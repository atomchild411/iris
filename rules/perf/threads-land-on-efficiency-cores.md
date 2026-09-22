# IRIS threads land on macOS efficiency cores

Observed 2026-09-22 on an M4 Pro (4 E-cores + 8 P-cores), from Activity
Monitor's CPU History while benchmarking IP28: sustained work sitting on the
efficiency cores with the performance cores largely idle.

**There is no thread QoS, affinity or scheduling-policy code anywhere in
`src/`** — zero matches for `qos`, `QOS_CLASS`, `thread_policy_set` or
`THREAD_AFFINITY_POLICY`. Threads spawned without a QoS class get
`QOS_CLASS_UNSPECIFIED`/legacy treatment, and macOS is free to park them on
E-cores.

## What is and is not available

Apple Silicon does **not** honour `THREAD_AFFINITY_POLICY` — there is no hard
pinning, by design. The lever that works is the QoS class:

```
pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0)
```

set from inside each thread that should be treated as foreground work. The
CPU thread and the REX3 processor thread are the obvious candidates; the
jitv2 compile thread pool is arguably `QOS_CLASS_USER_INITIATED`.

This is macOS-only and belongs behind `#[cfg(target_os = "macos")]` with a
no-op elsewhere — unlike the ISA-level work, which stays target-independent
through Cranelift.

## Why it matters beyond throughput

Benchmarks: an E-core run and a P-core run of the same build differ by more
than most of the changes we measure, and nothing currently pins which one you
get. Any A/B must therefore be run back-to-back under comparable conditions —
which is what the 2026-09-22 `mips4` and MADD measurements did — and even
then this is an unquantified source of variance in every number in
`rules/perf/`.

Not yet implemented; no measurement of what it would buy.

Related: [`bench-first-numbers.md`](bench-first-numbers.md),
[`guest-cpu-time-accounting-undercounts.md`](guest-cpu-time-accounting-undercounts.md).
