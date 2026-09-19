# Black-holed regions make absent hardware look present to a probing driver

NetBSD 10.2/sgimips, booted headless with no graphics board and nothing in any
GIO slot, reports two devices that are not there:

    hpc1 at gio0: SGI HPC3 (IOPLUS mezzanine)
    hpc1: using EXP0's DMA channel
    hpc2 at gio0: SGI HPC1.5 (GIO slot)

Both come from `Physical::build_device_map`, which points three address ranges
at `BlackHoleRegion` instead of leaving them unmapped:

| range | comment in the source |
|---|---|
| `0x02080000`-`0x02090000` | "Mystery Black Hole" |
| `0x1F980000`-`0x1F990000` | "2nd hpc" |
| `0x1FB00000`-`0x1FB80000` | HPC1 region, with a full explanation |

`0x1FB00000` is where the HPC3 IOPLUS mezzanine lives, and `0x1F980000` is the
classic HPC1.5 address (inside GIO slot 1). So each phantom maps exactly onto
one black hole.

## Why the probe succeeds

Not the value. `BlackHoleRegion` returns all-ones, which is what a driver
usually reads off an unterminated bus:

    BusRead8::ok(0xFF)   BusRead16::ok(0xFFFF)
    BusRead32::ok(0xFFFFFFFF)   BusRead64::ok(0xFFFFFFFFFFFFFFFF)

What matters is that the access **completes instead of raising a bus error**.
A `badaddr()`-style probe asks only whether the access faults, so suppressing
the fault is exactly what convinces it the hardware is there. Changing the
returned value would not help.

(The HPC1 comment in `physical.rs` says the black hole "reads as zero". It
does not -- it reads as all-ones. The comment is wrong on that detail.)

## Why the HPC1 hole exists

Documented in place, and it is a real fix, not an accident: IRIX probes
`0x1FB0xxxx` during normal operation and usually tolerates the bus error, but
once `vidtomem` activates the VINO capture pipeline a kernel access there
escalates to "PANIC: IRIX Killed due to Bus Error". The black hole prevents
that without implementing HPC1.

So the two goals are in direct tension: IRIX needs these accesses **not** to
fault, and a probing driver reads "does not fault" as "device present". The
`0x1F980000` hole has no such justification recorded -- just "2nd hpc".

## How much it matters

Cosmetic so far. NetBSD attaches the phantom HPCs but finds no children
(`sq at hpc1 ... not configured`) and does not crash. The risk is a driver
that attaches to a phantom and then drives it.

Unrelated to the Count==Compare stall in
`netbsd-sgimips-hangs-on-a-missed-compare.md`, which is what actually blocks
the boot.

## Before changing anything

Any fix has to keep the IRIX vidtomem panic fixed. Options, none tried:
implement enough HPC1 to answer honestly; make the suppression conditional on
the access pattern IRIX makes; or leave it and accept phantoms on other
guests. Needs upstream's view -- the tension is theirs to resolve.
