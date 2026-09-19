# What three guests say about the same emulator

Booting a guest that is not IRIX is the cheapest oracle we have: independently
written drivers, built from the same hardware documentation, disagreeing with
us where we are wrong. Three guests, same iris build (upstream/main plus the
local IP7 timer patch where noted).

| | IRIX 6.5 | NetBSD 10.2 / 11.0 | Debian 7 (Linux 3.2) |
|---|---|---|---|
| CP0 timer | fine | **dies after 5 ticks** | fine |
| SCSI | fine | **aborts to `db>`** | one 20 s timeout, recovers |
| reaches | login | kernel debugger | **Debian installer UI** |
| derived CPU clock | - | 66 MHz | 66 MHz |

## The timer exposure rule, confirmed on a third guest

A guest is exposed to the IP7 ticket bug
(`netbsd-sgimips-hangs-on-a-missed-compare.md`) only if it writes Compare
*before* reading Count. Linux does
`write_c0_compare(read_c0_count() + delta)` like IRIX, so its own Count read
consumes the crossing and the one-shot's ticket survives. Measured: Linux
timestamps advance normally (`4.44` -> `24.86` -> `25.17`) with no timer
patch needed. Only NetBSD, which writes Compare first, dies.

That is the prediction made from reading the code, tested against a guest that
had not yet been booted, and confirmed.

## SCSI: same area, three severities

Linux's `wd33c93` gets there, but not cleanly:

    scsi0: Aborting connected command - stopping DMA - sending wd33c93 ABORT
    command - flushing fifo - asr=80, sr=01, 0 bytes un-transferred
    (timeout=1000000)

A 20-second timeout with **0 bytes transferred**, then abort and recovery,
after which the disk attaches normally and the partition table reads
(`sda: sda9 sda11`). NetBSD, issuing `XFER_INFO` from a command phase we do
not expect, gets nothing and its state machine bails to `default: abort`.

So a command we fail to service is visible in both, and IRIX never shows it.
Whether Linux's timeout is the *same* defect as NetBSD's is not established --
the Linux path recovers and the phase is not captured. Worth `log scsi on`
during that 20 s window before claiming they are one bug.

## Not tested here

The Debian installer kernel carries no `hal2`, `newport` or sound drivers, so
Linux says nothing about the CLKID finding. NetBSD GENERIC is the guest for
that (see `rules/hal2/`).

## Recipes

Debian 7 mips `r4k-ip22`, kernel + initrd in one ECOFF image, no network
needed:

    curl -O http://archive.debian.org/debian/dists/wheezy/main/\
    installer-mips/current/images/r4k-ip22/netboot-boot.img
    mkvh build linux-boot.img --size 64M --bootfile linux linux=netboot-boot.img

Attach as SCSI 1, headless, NVRAM with serial console, then at the PROM take
option 5 and

    boot -f scsi(0)disk(1)rdisk(0)partition(8)linux

Our README's "Old Gentoo-mips livecd dies somewhere in kernel" is stale for
Linux generally: Debian 7 reaches the installer UI.
