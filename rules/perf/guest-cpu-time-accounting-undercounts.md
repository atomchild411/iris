# The guest's per-process CPU accounting undercounts by ~3x

Found 2026-09-22 on the IP28 guest (IRIX 6.5.7, R10000) while trying to
re-measure Dhrystone. It invalidates any benchmark that times itself with
`times()`, `clock()` or `getrusage()` rather than a wall clock.

## The measurement

A program that busy-loops 400M floating-point adds, reading every clock the
guest offers over the same interval, run over ssh with the host timing the
whole invocation:

```
HZ (sysconf)      : 100
time()     elapsed: 18 s        <- guest wall
gettimeofday      : 18.13 s     <- guest wall
times() elapsed rv: 18.13 s     <- guest tick counter, real time
times() utime     : 5.92 s      <- CPU charged to the process
times() stime     : 0.00 s
HOST wall-clock   : 18 s
```

Every **wall** clock agrees with the host to within 0.7%. Only `tms_utime` is
wrong, and it is wrong by 5.92/18.13 = **32.7%, almost exactly one third**.

The tick counter itself is fine: `times()`'s return value advances a full
18.13 s worth of ticks, so IRIS delivers ~1813 timer interrupts. The kernel
charges only 592 of them to the running process — and the remaining 1221 do
not appear in `tms_stime` either. They are going somewhere that is neither
user nor system time on the only runnable process.

## Why it fooled us

Dhrystone with `-DTIMES` divides by exactly this field (`dhry_1.c:133`,
`Begin_Time = time_info.tms_utime`), so its score comes out ~3x too high.
Whetstone uses `time()` and is unaffected — which is precisely how the bug
hid: the two benchmarks were cross-checked against the host clock, Whetstone
agreed, and `times()` was assumed to follow. It does not.

The earlier claim of **663 VAX MIPS, "~1.5x real hardware"** is therefore
wrong. Corrected for the 3x, the emulated IP28 lands roughly *at par* with a
real R10000/195, not ahead of it.

## What to do about it

Benchmark by **host wall clock over a fixed workload**, not by whatever the
guest reports about itself:

```sh
t0=$(date +%s); ssh guest 'cd /var/tmp/bench && echo 50000000 | ./dhry'; t1=$(date +%s)
echo "host wall: $((t1-t0)) s"
```

That is immune to every guest-clock question at once, and it is what the
2026-09-22 `mips4` A/B used.

## Still open

Nobody has yet found where the missing 2/3 of the ticks are charged. Worth
knowing because CPU-time accounting is not only a benchmarking concern — it
feeds `make`'s timing output, scheduler decisions, resource limits, and
anything calling `getrusage`. A guest whose processes appear to use a third of
the CPU they really use may mis-schedule under load.

Related: [`../testing/dhrystone-whetstone-on-an-irix-guest.md`](../testing/dhrystone-whetstone-on-an-irix-guest.md),
[`../build/the-three-builds-we-actually-use.md`](../build/the-three-builds-we-actually-use.md).
