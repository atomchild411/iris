# What `idle-pause` buys, measured

Measured 2026-09-19 on this machine, because "it saves CPU when the guest is
idle" is not an argument anyone should accept without numbers.

## Method

Guest booted and left sitting at a login prompt, doing nothing. Host CPU is the
`ps -o cputime` delta over a 90 s wall window, so it counts *every* IRIS thread,
not just the CPU thread. Emulated MIPS and guest Hz are `perf snapshot` cycle
and fastticks deltas over 45 s.

Two builds, identical but for the one flag:

    cargo build --release --features jitv2,lightning,rex-jit,idle-pause,chd
    cargo build --release --features jitv2,lightning,chd

## Results

**IRIX 6.5.22m, R4400, idle at the desktop login screen**

| | host CPU | emulated MIPS | guest Hz |
|---|---|---|---|
| idle-pause **off** | 322.3% of a core | 454.7 | 1000 |
| idle-pause **on** | 124.3% of a core | 154.3 | 1000 |
| | **2.6× less** | **2.9× less** | unchanged |

**NetBSD 11.0/sgimips, R4400, idle at a serial login prompt**

| | host CPU |
|---|---|
| idle-pause **off** | 303.2% of a core |
| idle-pause **on** | 106.7% of a core |
| | **2.8× less** |

Roughly two cores back, on both guests, for a machine that is doing nothing.

## The part that matters most

**Guest Hz is identical at 1000 either way.** The saving does not come out of
timekeeping — the guest's clock ticks at exactly the same rate parked or
spinning, because CP0 Count is materialised from the wall clock on read rather
than advanced by the run loop, and the compare timer fires on the hptimer
thread regardless. A guest that idled cheaply but drifted would be worthless;
this one does not drift.

Note also that it is not all-or-nothing: 154 MIPS while "idle" is still real
work. The detector only parks when the architectural state actually repeats
with interrupts enabled and none pending, so an idle loop that touches memory,
or a desktop with a blinking cursor, keeps running. It takes the free part and
leaves the rest.

## "It didn't work" — what was actually wrong

Two failure modes, both fixed and now upstream:

- **Parking with every interrupt masked.** Nothing could satisfy the wake
  condition, so the guest hung. Fixed by checking `IM` before parking
  (`1390bf6`, `rules/perf/idle-pause-must-not-park-with-interrupts-masked.md`).
- **Waiting out the slice.** A parked CPU only noticed an interrupt on its next
  1 ms boundary, so latency was terrible and it looked broken under load. Fixed
  by unparking from the raiser (`f1a561d`,
  `rules/perf/idle-pause-wake-the-parked-cpu.md`).

Before those two, "enable idle-pause and the guest hangs or crawls" is exactly
what you would see. Measure it again before believing any claim that it is
still broken — including this one, on different hardware.

See also [`idle-pause-work.md`](idle-pause-work.md) for the design and
[`../build/the-three-builds-we-actually-use.md`](../build/the-three-builds-we-actually-use.md)
for where it sits in the standard builds.
