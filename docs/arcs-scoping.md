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

## Phase 0 result: what IRIX actually calls

Measured, not guessed. `IRIS_ARCS_TRACE=1` logs every jump whose target is a
firmware vector entry (`src/arcs_trace.rs`); the trace below is a real IRIX
6.5.22 boot on the Indy, from the maintenance menu through to multi-user.

Arming alone confirmed the interface a third independent time: **35 entries, 33
populated, and the two null slots are exactly 8 (`ReturnFromMain`) and 19
(`Signal`)** — the two NetBSD marks absent on sgimips.

**IRIX uses seven entries. Thirty-one calls, all of them before the kernel
starts.**

| calls | entry | also used by NetBSD? |
|------:|-------|---|
| 14 | `GetMemoryDescriptor` | yes |
| 6 | `GetChild` | yes |
| 4 | `Read` | yes |
| 3 | `GetPeer` | yes |
| 2 | `Open` | bootloader only |
| 1 | `Close` | bootloader only |
| 1 | `FlushAllCaches` | **never** |

The sequence is legible end to end: walk the component tree
(`GetChild(NULL)`, then `GetChild`/`GetPeer` alternating); `Open`; two full
passes over the memory map; `Open` again, then four `Read`s — 5 bytes, then
0x2f, then 0x60, then **0x3300f0** straight into `0xffffffff88004094`, which is
the ELF header, the program headers, and the kernel image, landing in the
`0x88…` window; `Close`; and finally `FlushAllCaches` immediately before
entering the kernel, which is exactly what you would do having just written
3 MB of code through the data cache.

### Three things this changes

**`FlushAllCaches` is required.** It is in NetBSD's dead list — one of 23
entries no NetBSD code calls anywhere — so an ARCS built from NetBSD's usage
alone would have omitted it, and the omission would have surfaced as a kernel
running against a stale instruction cache. Precisely the class of bug that is
agony to diagnose.

**`GetEnvironmentVariable` is never called.** NetBSD's single hottest entry, 15
call sites, and IRIX does not touch it during boot at all. Whatever IRIX needs
from the environment it gets another way. That removes what the earlier scoping
called "where the boot will actually stall" — for IRIX, at least.

**`Seek` is never called either**, though NetBSD's bootloader uses it. IRIX
reads the kernel sequentially.

The memory-descriptor cursors are worth noting for the implementation: the
returned pointers are `…4c4, …4e0, …4a8, …898, …87c, …830` — not monotonic and
not a single reused buffer, so the PROM returns pointers to seven distinct
static descriptors. A cursor token is still a legitimate implementation, but
the real one does not behave that way.

### What the trace does not cover

The PROM's own menu and command monitor do not go through the vector table, and
neither does anything after the kernel is up — the count stops at 31 and stays
there through multi-user. This is IP24 with 6.5.22; the ARCS contract is the
same on IP28, but the trace should be repeated there once it boots.

Union of what IRIX and NetBSD need, which is what to implement: the seven
above, plus `GetEnvironmentVariable`, `GetSystemId`, `Write`, `GetReadStatus`,
`Seek`, `Reboot`, `PowerDown`, `EnterInteractiveMode`.

## Phasing

**Phase 0 — measure. Done**, see above. `src/arcs_trace.rs`, armed with
`IRIS_ARCS_TRACE=1`; `summary` instead of `1` suppresses the per-call lines.

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
