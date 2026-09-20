# Bringing up IP32 (O2) in IRIS — what it would take

Assessment written 2026-09-19. Nothing implemented yet. Scope agreed with the
owner: **run the real O2 PROM** (not a synthetic ARCS), serial console only,
graphics explicitly out of scope.

## The PROM images we have

`ip32/ip32prom.rev4.18.bin` and `ip32/ip32prom.rev4.3.bin`, both 512 KiB, both
genuine IP32 firmware. Verified by parsing the section table rather than
trusting the filenames.

Rev 4.18:

| SHDR @ | name | length | version | load |
|---|---|---|---|---|
| 0x000008 | `sloader` | 0x04000 | 1.0 | 0xbfc00000 |
| 0x004008 | `env` | 0x00400 | 1.0 | — |
| 0x004408 | `post1` | 0x04d44 | 1.0 | 0xa0004000 (copied to RAM) |
| 0x009208 | `firmware` | 0x5fffc | **4.18** | 0x81000000 (.rodata +0x48e70) |
| 0x069208 | `version` | 0x00388 | 4.18 | ELF metadata |

SHDR layout, from the bytes: magic at +0, length at +4, type flags at +8,
NUL-padded name at +12, version string at +44, checksum at +52, then load
address and .rodata offset for `firmware`.

4.3 has the same five sections at the same offsets bar the last (0x68008).

**Rev 4.18 is the revision `ip32prom-decompiler` is tested against**, so the
published annotations line up with the image we hold.

## What tracing the reset path already told us

    bfc00000:  b      0xbfc00048        # jump over the SHDR
    bfc00048:  b      0xbfc003a8        # ...to the real entry
    bfc00100:  mtc0   a0,$17            # exception vector stub
    bfc00104:  lui    t8,0xb400
    bfc00108:  sd     a0,624(t8)        # 0xb4000270
    bfc0010c:  lui    t8,0xb400
    bfc00110:  addiu  t8,t8,512
    bfc00118:  sd     zero,0(t8)        # 0xb4000200

`0xb4000000` is uncached KSEG1 for physical `0x14000000` — **CRIME**. Against
NetBSD's `crimereg.h` those two offsets are `CRIME_MEM_ERROR_ECC_REPL` (0x270)
and `CRIME_MEM_CONTROL` (0x200): a memory-error exception handler, installed
before anything else runs.

Two facts fall out immediately:

- CRIME registers are **64-bit** — the PROM uses `sd`/`ld`, not `sw`/`lw`. Our
  bus plumbing must carry 64-bit accesses to this device.
- CRIME is live from the very first exception, so it cannot be stubbed out
  later; it is the first thing to build.

## The machine, from NetBSD's own GENERIC32_IP3x

```
crime0     at mainbus0 addr 0x14000000     memory + interrupt controller
crmfb0     at mainbus0 addr 0x16000000     framebuffer            [out of scope]
mace0      at mainbus0 addr 0x1f000000     I/O ASIC
  macepci0 +0x080000 → pci0 → ahc          Adaptec SCSI on real PCI
  mec0     +0x280000                       ethernet
  mavb0    +0x300000                       audio                  [out of scope]
  macekbc0 +0x320000                       keyboard               [out of scope]
  com0/1   +0x390000 / +0x398000           16550-style serial
  mcclock0 +0x3a0000                       RTC
```

Nothing of this exists in IRIS today: 17 source files mention IP22/IP24, and
`crime`/`mace` appear nowhere.

The serial console is a genuine simplification — `com` is a 16550 derivative,
far simpler than the Indy's Z85C30, and the PROM's built-in `env` section uses
the **same `console` variable** we already handle (`console=g` is its default,
`console=d` selects serial).

## Why the real PROM, and what that costs

NetBSD tries real ARCS first and only falls back to its own emulation:

```c
if (arcbios_init(ARCS_VECTOR) == 1) { ... arcemu_init(...) }
```

and `arcemu` only covers IP6/IP10/IP12/IP20/IP22. **IP32 needs real ARCS**, so
the PROM is not optional if the goal is stock NetBSD — and it is certainly not
optional for IRIX.

The consequence, and it is the important one: **virtio does not shrink this
job.** virtio removes devices the *guest OS* needs; the PROM POSTs the real
machine regardless. `post1` is ~20 KiB of memory test driven through CRIME, and
the firmware section will probe MACE and the PCI bridge. virtio's payoff comes
after the PROM is satisfied — it means we would not also have to emulate `mec`
and the `ahc` SCSI controller well enough for a running system, only well
enough for POST to not reject them.

## Licensing — settle this before writing code

- IRIS is **BSD-3-Clause**.
- `ip32prom-decompiler` and its annotations are **GPL-3.0**.

Reading the annotations to learn what a register does is fine. Copying comments,
structure, or generated assembly into IRIS is not. Any CRIME/MACE code must be
written independently, and commits that were informed by the annotations should
say so plainly. The PROM binaries themselves are SGI's; they stay out of the
repo exactly like `ip24prom.070-9101-011.bin` does.

## Suggested first milestone

Not "boot NetBSD" — too far. Instead: **get `sloader` to hand off to `post1`.**

That needs only:

1. A `MachineType::Ip32` profile: reset at 0xbfc00000, the IP32 physical map,
   PROM image loaded at 0x1fc00000.
2. CRIME as a 64-bit register file at 0x14000000 — enough of `CRIME_MEM_*` and
   the revision/ID register to get through early setup.
3. RAM behind CRIME's bank controls.

Success is observable without any console at all: trace the PC reaching
0xa0004000. That single milestone will also answer the real unknown — how much
of CRIME the memory test actually exercises — which is what determines whether
the rest is weeks or months.

After that, in order: MACE register file → `com0` (first console output, and
the first time the PROM talks to us) → RTC → then decide how much of PCI/`ahc`
POST insists on.

## Open questions

- How much does `post1` insist on? Unknown until we run it. It is the gate.
- Does the PROM require a framebuffer to be present even with `console=d`? The
  Indy PROM does not; the O2's `crt_option=1` in the default env suggests it at
  least looks.
- CPU: O2 shipped R5000/RM5200/RM7000/R10000/R12000. We have R5000, which is
  the cheapest target. RM7000 would need extra CP0 state, and the decompiled
  PROM branches on `CP0_PRID`, so it will notice.

Related: [`virtio-mmio-design.md`](virtio-mmio-design.md).
