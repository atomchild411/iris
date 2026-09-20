# The three builds we actually use

Settled 2026-09-19 after noticing that the flag string we had been copying
around for weeks contained two flags that did nothing.

## The sets

```bash
# A — graphics: IRIX desktop, Quake, X11, anything that draws
cargo build --release --features jitv2,lightning,rex-jit,idle-pause,chd

# B — headless: NetBSD on serial, network and SCSI bringup, long unattended runs
cargo build --release --features jitv2,lightning,idle-pause,chd

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
