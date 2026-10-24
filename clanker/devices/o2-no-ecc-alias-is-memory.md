# The O2's no-ECC alias at physical 0x00_8000_0000 is memory, not PCI

Status: rule (fixed 2026-10-24, branch o2-no-ecc-alias).

**Symptom.** On the O2, IRIX's `netstat -ni` printed its header and spun at
~90% CPU forever; Netscape, which runs it at startup, never opened. `/dev/mem`
and `/dev/kmem` returned 0xffffffff for about one page in six (odd frames
only), while the monitor read the right data at the same addresses.
IRIX 6.5.22 on an R5000 O2 panicked with "init died (why = 1, what = 0x1)"
early in boot; the same fix lets it boot to the desktop.

**Cause.** IRIX's `page_mapin` maps some pages (by cache colour) through a
temporary K2 TLB entry whose PFN points into CRIME's no-ECC alias of memory:
frame 1 became PFN 0x80001, physical 0x00_8000_1000 (CRIME 1.5 spec, table
3-1: 1 GB no-ECC memory at 0x00_8000_0000, the same RAM as 0x00_4000_0000).
The CPU hands the bus physical addresses as 32 bits, and the O2 bus decodes
0x8000_0000 and up as MACE's 64-bit PCI window (OpenBSD's 0x2_8000_0000, also
truncated there). The read went to PCI, master-aborted and returned all ones.
netstat reads `ifnet` through `/dev/kmem` (address & 0x7fffffff), got
0xffffffff, and walked from 0x7fffffff forever, ignoring the ENXIO.

**Fix.** `MipsExecutor::phys_alias` folds a physical window onto another
before the 32-bit cut, for TLB-mapped and XKPHYS translations; the O2 sets
`(NO_ECC_BASE, RAM_WINDOW_SIZE, RAM_BASE)`. The two ranges differ only in bits
39:32, so no 32-bit decode on the bus can tell them apart.

**How it was found.** A `developer` build (breakpoints are stripped from
`lightning` builds, see debug.md): a breakpoint after `page_mapin` in the
`/dev/mem` read showed a K2 address for bad frames and K1 for good ones; a
breakpoint after `uiomove` plus `tlb dump` showed the PFN. A perl loop over
`/dev/mem` comparing against the monitor finds bad pages in seconds.
