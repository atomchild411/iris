# Bringing virtio to IRIS — a design sketch

Written 2026-09-19, before any code. Nothing here is implemented. The point is
to decide the shape while three guests with virtio drivers and full source are
sitting on this disk.

## Why now

IRIS already boots guests that ship virtio drivers: NetBSD 10.2 and 11.0, and
Linux (Debian 7 reaches its installer). Each of those talks to emulated 1993
hardware today — a WD33C93A pushed a byte at a time, a Seeq 80c03, a serial
port. Every one of those is expensive to emulate faithfully and slow to drive.

The long game is different, and it is the reason to care: once IRIS speaks
virtio, **we can write IRIX drivers for it**. IRIX has no virtio, so this is
the one guest where we supply both halves. Everything our C code does through
`irisx` today — X, GL, MIT-SHM — is a private transport we invented. virtio is
the same idea with a specification, three reference implementations to check
against, and drivers we do not have to write for two of the three guests.

## Transport: virtio-mmio, legacy, in a GIO slot

virtio-pci is the common transport and the Indy has no PCI. **virtio-mmio** is
a flat register window, which is exactly what a GIO expansion aperture is.

The version question is already answered for us. NetBSD's
`sys/dev/virtio/virtio_mmio.c:167`:

    ver = bus_space_read_4(sc->sc_iot, sc->sc_ioh, VIRTIO_MMIO_VERSION);
    if (ver != 1) {
            aprint_error_dev(vsc->sc_dev, "unknown version 0x%02x; giving up\n", ver);
            return;
    }

Version 1 only — the legacy layout, with `GUEST_PAGE_SIZE` (0x028),
`QUEUE_ALIGN` (0x03c) and `QUEUE_PFN` (0x040). Linux accepts both 1 and 2.

That settles a worry from earlier: **legacy virtio uses guest-native
endianness**, so a big-endian MIPS guest does no byte swapping. The
per-descriptor swap tax I expected from virtio 1.0's little-endian rule does
not apply if we implement version 1. Implement version 1; it is what NetBSD
requires and what costs the guest least.

## Where it plugs in

`gio::ExpansionCard` (committed as part of the slot-ledger work) is already the
right hook: anything that is a `BusDevice` decoding a range inside a slot can
be installed before `Machine::new`. A virtio-mmio card satisfies that exactly
as irisx does, and the two can coexist in different slots.

What a card needs from the emulator is what `iris-hostbridge` already takes:

- guest **physical** memory for descriptor and buffer access — the `Physical`
  bus, so writes bump jitv2's page generations
- an **interrupt line** — the slot's IOC source, driven from
  `INTERRUPT_STATUS`/`INTERRUPT_ACK`

So the crate boundary mirrors `iris-hostbridge`: a `iris-virtio` crate that
knows nothing about IRIS internals, plus a thin adapter in the emulator.

## What is genuinely different from irisx

irisx is connection-oriented: `open(arg) -> (Read, Write)`, byte streams, plus
a pin table of long-lived guest pages a host service reads in place. That pin
model is why MIT-SHM works — an X client writes its segment outside the X
protocol entirely, and nothing submits a descriptor when it does.

A virtqueue is the opposite: the driver hands buffers over and gets them back.
Ownership ping-pongs. **This is not a reason to convert irisx**, and it is why
"replace irisx with virtio" is the wrong instinct — MIT-SHM has no virtqueue
shape. The two should coexist: virtio for request/response device traffic,
irisx for the streams and pinned regions it was built for.

## Order of work

1. **The MMIO transport shell.** Magic, version, device/vendor id, feature
   negotiation, one queue, `QUEUE_PFN`, `QUEUE_NOTIFY`, interrupt status/ack.
   No device behind it yet. Verifiable immediately: a Linux or NetBSD kernel
   will probe it and either attach or print why not.
2. **virtio-blk first** (`ld_virtio` on NetBSD, `virtio_blk` on Linux). The
   simplest device, the biggest win, and the easiest to check: the guest reads
   a disk image we already have. This is also where the performance argument
   lives — it skips the WD33C93A entirely.
3. **virtio-net** (`if_vioif` / `virtio_net`), against the NAT backend we
   already run.
4. **virtio-console** (`viocon`) — cheap once the transport exists, and useful
   as a second channel that is not the SCC.
5. **Only then IRIX.** By that point the transport is proven by two guests
   whose drivers we did not write, so an IRIX driver is debugging one side
   instead of two.

## Booting

The SGI PROM knows nothing about virtio, so a guest cannot boot from a virtio
disk. The kernel has to come from SCSI or the volume header, with virtio
carrying the root filesystem or data. That is not a limitation worth fighting:
NetBSD already boots a kernel from the volume header and mounts root
elsewhere.

## Test assets already on disk

- `netbsd-10.2-src/` — full kernel source, including `dev/virtio/` and the
  device drivers (`ld_virtio.c`, `if_vioif.c`, `viocon.c`, `vioscsi.c`,
  `vio9p.c`, `viornd.c`, `viomb.c`), all of which attach to the generic
  `virtio` bus and therefore ride the MMIO transport.
- NetBSD 10.2 and 11.0 `GENERIC32_IP2x` kernels, both booting.
- Debian 7 `r4k-ip22`, reaching its installer.
- Linux's `sound/mips/hal2.c` fetch showed how cheap it is to pull single
  reference files when needed.

NetBSD is the better first target despite Linux's wider virtio use: we have its
complete source locally, its driver is legacy-only so there is one code path,
and `GENERIC32_IP2x` already carries every device driver we would exercise.

## The guest-tools question

Everything we compile *for* IRIX — the irisx driver and daemon, the GL shim,
the hostcall trap stub, and eventually IRIX virtio drivers — is one body of
work with one toolchain and one target, and none of it belongs in the emulator
repository. Splitting it out is a prerequisite for IRIX virtio drivers rather
than a consequence: the moment there is an IRIX virtio driver there is no
question about where it lives.

## Open questions, not yet decided

- Does `ExpansionCard` need to grow snapshot participation before a virtio
  card is useful, or is "not snapshotted" acceptable for a first cut? The slot
  hook documents the limitation; a virtio-blk device with in-flight requests
  makes it sharper.
- One card per device, or one card multiplexing several device types? Real
  virtio-mmio is one device per register window; QEMU instantiates many
  windows. With three GIO slots we cannot have many. A single card exposing
  several windows at different offsets inside one slot aperture is probably
  the answer, but it is a spec deviation to check against both drivers.
- Does the 64 KB bus mapping granule constrain the window layout? Each virtio
  device wants 0x200 bytes; packing several into one granule is fine for the
  bus but needs the decode to be exact.
