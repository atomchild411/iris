# The three builds we actually use

Settled 2026-09-19 after noticing that the flag string we had been copying
around for weeks contained two flags that did nothing.

## The sets

```bash
# A — graphics: IRIX desktop, Quake, X11, anything that draws
cargo build --release --features jitv2,lightning,rex-jit,idle-pause,chd,mips4

# B — headless: NetBSD on serial, network and SCSI bringup, long unattended runs
cargo build --release --features jitv2,lightning,idle-pause,chd,mips4

# C — debug: packet logs, breakpoints, tracebacks
cargo build --release --features jitv2,developer,rex-jit,chd,mips4
```

`mips4` is right for every guest we currently run — IP28 is an R10000 and the
Indy configs are R5000, all MIPS IV. **Drop it, and only it, when building for
the R4400 config**; see the section below for why it cannot simply go in
`default`.

Two omissions worth checking against a build you already have, because both
are silent:

- **`idle-pause`** is in A and B and the IP28 build had been missing it. The
  symptom is not a failure, it is the emulator holding ~376% host CPU while
  the guest sits idle at a login prompt.
- **`mips4`** costs ~20% of integer throughput when absent (below).

`chd` is in all three on purpose. It is one dependency, and it is the
difference between an IRIX `.chd` image loading and `fatal: CHD image support
not compiled in`. Leaving it out of some builds only produces a confusing
failure later.

## Add `mips4` when the guest CPU is one — which is all of them but the R4400

Measured 2026-09-22 on the IP28 (R10000, IRIX 6.5.7, MIPSpro `-Ofast -mips4`
binaries), four arms of five reps each, fresh disk clone and fresh boot per
arm, timed by **host** wall clock for a fixed workload:

| build | Dhrystone 50M, warm | Whetstone 1M |
|---|---|---|
| without `mips4` | 41.8 s | 18.5 s |
| with `mips4`    | **34.3 s** | 19.5 s |

**About 20% on integer code, nothing on this FP code.** The win is MIPS IV's
integer `MOVZ`/`MOVN` conditional moves, which let MIPSpro emit branch-free
sequences that jitv2 then compiles instead of bailing to the interpreter.
Whetstone was already fully covered, hence flat.

Without the flag nothing is *wrong* — the interpreter gates on the runtime
`C::MIPS4` and absorbs every MIPS IV instruction correctly. It is purely
compilation coverage, which is why it went unnoticed: the only symptom is
being slower.

### The flag does not mean what its Cargo.toml comment says

The comment claims it is a "decode gate" that makes an R4400 build "correctly
raise Reserved Instruction". It does not. **45 of its 47 `cfg` sites are
inside `src/jitv2/`**; the other two are a feature listing and a stats gate.
The interpreter gates on the model const instead (`C::MIPS4`, 17 uses in
`mips_exec.rs`), and **jitv2 consults that const zero times.**

So the two engines gate the ISA on different axes — jitv2 at build time, the
interpreter at run time — and they can only agree by coincidence:

| model | `MIPS4` const | our configs |
|---|---|---|
| `R4400Cache` | `false` | `iris-atomchild-hostx.toml.r4400` |
| `R5000Cache` | `true`  | `iris-atomchild-hostx.toml`, `iris-657.toml` |
| `R10000Cache`| `true`  | `ip28irix.toml` |

**Therefore `mips4` must not go in `default`.** One binary serves all three
configs; a `mips4` binary pointed at the r4400 config would have jitv2 execute
`MOVZ`/`COP1X` where the interpreter raises Reserved Instruction. Pass it
explicitly for R5000/R10000 guests, and build without it for the R4400 config.

The real fix is to make jitv2 gate on `C::MIPS4` like the interpreter does,
after which the flag would only mean "compile the emitters in" and could be on
everywhere. The plumbing is contained: `lookup_semantics` /
`lookup_cp1_semantics` are pure `fn(raw: u32)` and would take a `mips4: bool`,
with three real call sites in `codegen.rs` plus the analyzer's.

## Two flags we were passing for nothing

We had been building with
`jitv2,opcodefusion,idle-pause,rex-jit,lightning,tlbvmap`. Verified by
comparing banners, that is **identical** to `jitv2,lightning,rex-jit,idle-pause`:

- `lightning = ["opcodefusion"]` — lightning already implies it.
- `tlbvmap` is in `default`, and its own comment says it is always on and the
  flag exists only so tooling that passes it still compiles. Passing it
  explicitly changes nothing.

Both builds print:

    iris: build features: jitv2 opcodefusion idle-pause rex-jit lightning tlbvmap chd

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
`idle-pause` buys.
