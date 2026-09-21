# Implementing ARCS: scope

Assessment written 2026-09-20. Nothing implemented. The question is what it
would cost to boot an SGI kernel **without a boot PROM**, by providing the ARCS
firmware interface ourselves.

The immediate motivation is IP28, whose PROM stops in a secondary-cache
diagnostic testing SRAM behaviour this emulator deliberately does not model
(see `ip28-bringup.md`). But the value is general: a working ARCS means direct
kernel boot on every machine, and no more "we cannot try X because we have no
PROM for it".

## We already know the interface, from two independent directions

This is the part that makes the job tractable, and it is worth stating first.

**Observed.** `rules/testing/arcs-console-from-bare-metal.md` established the
layout by reading memory on a running machine, and the bare-metal test harness
already *calls* ARCS for its console and its disk log. Re-confirmed live on an
Indy while writing this:

```
phys 0x1000  = 53435241   SPB signature 'ARCS'
phys 0x101c  = 0000008c   FirmwareVectorLength = 140 = 35 entries
phys 0x1020  = a0001800   FirmwareVector
```

**Declared.** NetBSD's `sys/dev/arcbios/arcbios.h` gives every entry's index
and signature, and `sys/arch/sgimips/sgimips/arcemu.c` is a working ARCS
*emulator* for IP6/IP10/IP12/IP20/IP22 — the same job, BSD-licensed, ~650 lines
of which only 200–250 are ARCS-shaped (the rest is hardware NVRAM poking we
would not reproduce).

**They agree.** Dumping all 35 vector slots on the Indy, exactly two are zero:
**index 8 and index 19**. NetBSD marks exactly four entries "not on sgimips",
of which the two that fall inside a 35-entry table are `ReturnFromMain` (8) and
`Signal` (19). Nothing else is null. That is a complete, cross-validated map of
the interface before a line is written.

## What has to be built

### The vector table and its 13 live entries

Of 35 slots, **only 13 have any caller** in the whole of NetBSD's sgimips tree.
The rest can point at a stub that reports and halts — loudly, so that an
unexpected call is a finding rather than a mystery.

| Needed for a kernel | Needed additionally for a bootloader |
|---|---|
| `GetEnvironmentVariable` (30) | `Open` (23) |
| `GetMemoryDescriptor` (18) | `Close` (24) |
| `GetChild` (10), `GetPeer` (9) | `Seek` (28) |
| `GetSystemId` (17) | |
| `Read` (25), `Write` (27) | |
| `GetReadStatus` (26) | |
| `Reboot` (6), `PowerDown` (4), `EnterInteractiveMode` (7) | |

`Reboot`/`PowerDown`/`EnterInteractiveMode` only matter at shutdown.

### The data behind them

- **Memory descriptors.** `{Type, BasePage, PageCount}`, 4 KiB pages, iterated
  by passing the previous return back in until NULL. A static buffer reused as
  a cursor is fine — that is what arcemu does. Must be sorted ascending.
- **A component tree.** `GetChild(NULL)` → root, `GetPeer` along siblings. Two
  nodes suffice (one System, one Processor); arcemu publishes a flat array.
- **An environment.** See below — this is the real work.
- **An SPB** at physical `0x1000`. Only the signature and `FirmwareVector` are
  ever read; the rest of the block is ignored.

### Two traps worth naming in advance

**The memory-type enum is renumbered between SGI and ARC.** SGI has
`FreeContiguous`=2, `FreeMemory`=3, `BadMemory`=4, `LoadedProgram`=5,
`FirmwareTemporary`=6, `FirmwarePermanent`=7; the ARC port assigns those values
differently. Crib from the wrong branch and a kernel either panics on an
unknown descriptor — which at least fails loudly — or silently counts free RAM
as firmware-reserved, which does not. The component `Class`/`Type` numbers
diverge the same way.

**`GetEnvironmentVariable` is the interface.** Fifteen of roughly thirty call
sites in NetBSD are this one function, and what it returns decides the console,
the MAC address, the root device, single- versus multi-user, and the CPU
frequency — which is a hard panic if absent. Getting the vector table right is
the easy day; getting the environment right is where a boot will actually
stall.

### The 64-bit gap

IP28 runs IRIX64, so this matters. NetBSD's 32-bit bootstrap reads
`FirmwareVector` as a 32-bit load at SPB+0x20 via KSEG0; its 64-bit bootstrap
reads a **64-bit load at SPB+64** via XKPHYS (`0xa800000000001000`). The header
describes only the 32-bit layout — the 64-bit offset is a bare constant in
assembly. So the 64-bit SPB layout has to be established, and the obvious way
is to read it off the IP28 PROM the way the 32-bit one was read off the Indy.

The ELF loader is 32-bit only (`crate::elf::parse` → `Elf32`), though the
structures it fills already carry 64-bit addresses. `--load-elf` and
`Machine::load_elf_bytes` exist and work.

## What is genuinely unknown

Everything above describes what *NetBSD* needs. **IRIX is the actual target,
and no source may be consulted for it.** `sash` and `unix.IP28` may well call
entries NetBSD never touches — `GetDirectoryEntry`, `GetFileInformation` and
`Mount` are all plausible for a loader that reads a volume header.

That is measurable rather than guessable, and cheaply: the vector table lives
at a known address, so the emulator can log every call into it while the **real
PROM** boots IRIX on a machine that already works. Which is the same oracle
method that found every real defect during the IP28 work.

## Phasing

**Phase 0 — measure (half a day).** Log ARCS calls during a real IRIX boot on
the working Indy/Indigo2. Produces the exact set IRIX uses, including arguments
and return values. De-risks everything after it, and needs no new firmware.

**Phase 1 — 32-bit ARCS (one to two days).** SPB, vector table, dispatch, the
13 entries, environment, memory map, component tree. Validate by booting
NetBSD/sgimips on the **Indy** with our ARCS instead of the PROM — a machine
that already boots, so any failure is ours and is visible immediately.

**Phase 2 — 64-bit (about a day).** The 64-bit SPB layout, read off the IP28
PROM; ELF64 in the loader. Validate on IP28, which is where the payoff is: it
skips POST entirely, and the MRU bit stops mattering.

**Phase 3 — IRIX.** `sash`, then `unix.IP28`, against the Phase 0 trace.

Phases 0 and 1 are worth doing on their own merits whatever happens to IP28,
because they are validated against machines that already work.

## Estimate

Roughly **400–600 lines** for a complete kernel-booting ARCS, on the evidence
of arcemu with the hardware-specific parts removed and file I/O added back.
The risk is not in the line count; it is in Phase 0 telling us IRIX wants
something NetBSD never asks for.
