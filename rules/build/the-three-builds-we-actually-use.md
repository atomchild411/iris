# The three builds we actually use

Settled 2026-09-19 after noticing that the flag string we had been copying
around for weeks contained two flags that did nothing.

## The sets

```bash
# A — graphics: IRIX desktop, Quake, X11, anything that draws
cargo build --release --features jitv2,lightning,rex-jit,chd

# B — headless: NetBSD on serial, network and SCSI bringup, long unattended runs
cargo build --release --features jitv2,lightning,chd

# C — debug: packet logs, breakpoints, tracebacks
cargo build --release --features jitv2,developer,rex-jit,chd
```

`chd` is in all three on purpose. It is one dependency, and it is the
difference between an IRIX `.chd` image loading and `fatal: CHD image support
not compiled in`. Leaving it out of some builds only produces a confusing
failure later.

## Two flags we were passing for nothing

We had been building with
`jitv2,opcodefusion,idle-pause,rex-jit,lightning,tlbvmap`. Verified by
comparing banners, that is **identical** to `jitv2,lightning,rex-jit,idle-pause`:

- `lightning = ["opcodefusion"]` — lightning already implies it.
- `tlbvmap` is in `default`, and its own comment says it is always on and the
  flag exists only so tooling that passes it still compiles. Passing it
  explicitly changes nothing.

## `mips4` dropped 2026-09-22 — ISA level follows the CPU now

**You no longer need to pass it, and passing it changes nothing in a real
run**, so it is gone from the sets above. It remains *declared* in Cargo.toml
so existing build commands and `r5k = ["mips4"]` keep working; all it does now
is seed the runtime flag before any CPU exists (unit tests that compile
without constructing an executor).

ISA level is a property of the CPU model, which is a **runtime** choice, so
both engines now read it from there: the interpreter from `C::MIPS4`
directly, jitv2 from `jitv2::isa`, which `MipsExecutor::new` publishes
`C::MIPS4` into as the CPU is constructed.

Verified end to end: a binary built with **no** `mips4` feature, booted on
IP28, reports `fpu 78/78` and `loadstore 29/29` — identical coverage *and*
performance to the feature build, because the R10000 turns it on at runtime.
The R4400 direction is covered by unit test at the gate (`jitv2::isa::tests`,
and `classify_cop1x_madd_follows_emitter_coverage` asserting both
polarities), **not** end to end: an R4400 pointed at an IP28 config never
gets far enough to answer the monitor.

### What it used to be, and what that cost

Its Cargo.toml comment claimed a "decode gate" that made an R4400 raise
Reserved Instruction. It never did: **45 of its 47 `cfg` sites were inside
`src/jitv2/`**, the interpreter gated on `C::MIPS4` instead (17 uses), and
jitv2 read that const **zero** times.

| model | `MIPS4` const | our configs |
|---|---|---|
| `R4400Cache` | `false` | `iris-atomchild-hostx.toml.r4400` |
| `R5000Cache` | `true` | `iris-atomchild-hostx.toml`, `iris-657.toml` |
| `R10000Cache`| `true` | `ip28irix.toml` |

One binary serves all three, so build-time and run-time could only agree by
coincidence. Both directions were live bugs. A `mips4` build on the r4400
config had jitv2 executing `MOVZ`/`COP1X` where the interpreter trapped. The
reverse is the one we actually paid: every IP28 run had an R10000 with MIPS IV
compilation off, worth ~20% of integer throughput —

| | Dhrystone 50M, warm | Whetstone 1M |
|---|---|---|
| ISA level off | 41.8 s | 18.5 s |
| ISA level on | **34.3 s** | 19.5 s |

— measured over four arms of five reps, fresh disk clone and fresh boot per
arm, timed by **host** wall clock (the guest's own `times()` is not
trustworthy; see
[`../perf/guest-cpu-time-accounting-undercounts.md`](../perf/guest-cpu-time-accounting-undercounts.md)).
The win is MIPS IV's *integer* `MOVZ`/`MOVN` conditional moves, which let
MIPSpro emit branch-free sequences jitv2 then compiles instead of bailing to
the interpreter. Whetstone was already fully covered, hence flat.

Nothing was ever *wrong* without it — the interpreter absorbed every MIPS IV
instruction correctly. That is exactly why it survived: the only symptom was
being slower.

## `idle-pause` dropped 2026-09-22

It is on its way out upstream ("does nothing for him"), and it does nothing
for us on IP28 either. Same binary, `IRIS_NO_IDLE=1` as the control, guest at
a login prompt, host CPU as a `ps -o cputime` **delta** over 60 s:

| | idle host CPU |
|---|---|
| idle-pause on | 325% of a core |
| idle-pause off | 341% of a core |

~5%, inside the noise — against the 2.6-2.8x it buys on the R4400 guests
measured in [`../perf/idle-pause-what-it-buys.md`](../perf/idle-pause-what-it-buys.md).
Not worth carrying or relying on. The IP28 still idles at ~3.3 host cores and
nobody has diagnosed why; `idle_profile_arm`/`idle_profile_report` in
`mips_exec.rs` is the tool if anyone wants to.

(Measuring trap, for whoever repeats this: macOS `ps -o %cpu` is an average
since exec, so a CPU-heavy boot dominates it and every arm reads ~330%
regardless. Use a `cputime` delta.)

## The banner under-reports

`build_features::banner()` enumerates a fixed list of ~28 features. **`rexdiag`
is not in it**, so a default build never shows `rexdiag` even though `default =
["tlbvmap", "rexdiag"]` has it on. Do not use the banner to prove a feature is
absent — only that a listed one is present. `chd` *is* enumerated, which is how
we confirmed a build genuinely lacked it.

## Why C exists, and why it cannot be fast

`lightning` and `developer` are a `compile_error!` — mutually exclusive. And
`dlog_dev!` is `developer`-gated, so in any `lightning` build the device logs
compile to nothing: `log net on` is accepted by the monitor, reports itself
enabled in `log status`, and writes an empty file.

That cost an afternoon while chasing the receive-channel stall, where the
monitor's `net status` / `seeq status` / `pdma status` were the only
instruments available. Those work in every build; the packet log does not.

C also deliberately omits `opcodefusion`: a fused pair's second instruction is
never independently dispatched, so a breakpoint on that address silently never
fires.

## What `rex-jit` actually costs

It is not "graphics on/off" — REX3 is always emulated. NetBSD's `newport0`
attaches to it even with the console on serial, and the PROM draws to it before
any OS runs. `rex-jit` only JIT-compiles REX3 draw shaders (31 cfg gates, five
cranelift crates); without it REX3 is interpreted, and there is a runtime
`rex jit off` that does the same thing to a build that has it.

So dropping it from B costs nothing at runtime for a guest that never draws —
it is build time and dependency weight only.

Related: [`../perf/idle-pause-work.md`](../perf/idle-pause-work.md) for what
`idle-pause` used to buy, and
[`../jitv2/instructions-jitv2-still-interprets.md`](../jitv2/instructions-jitv2-still-interprets.md)
for the emitter-coverage survey the ISA-level work came out of.
