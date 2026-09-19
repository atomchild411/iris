# The seeq-enet thread stops pumping, a few MB into a bulk transfer

Open as of 2026-09-19. Found with NetBSD/sgimips 10.2. **Not fixed** — two
attempts failed, both recorded below, because the first diagnosis was wrong and
knowing *why* it was wrong is most of the value here.

## Symptom

A large HTTP download into the guest stops dead after a few megabytes and never
recovers. NetBSD's `ftp` shows `- stalled -` until its own timeout.

The stall point is random — 1.9, 3.2, 3.7, 5.0, 8.4, 15.8 MB across runs of the
same file — so it is a race, not a boundary. Small transfers (a 3 KB `SHA512`)
always work, which is why nothing else had noticed.

**The whole interface dies, not the connection.** After a stall, in the same
boot: DNS fails, `ping` gets no replies, and every later `ftp` fails instantly.
Only a reboot recovers it. So a retry loop cannot work — each boot buys you a
few MB, which is why a `ftp -R` resume loop still stopped dead at 8 MiB and then
burned 600 attempts without moving.

## What is actually broken

The guest still *thinks* it is sending. `ping -c 3` reports 3 transmitted. But:

    > net status icmp
    ICMP NAT (0 entries):
      (none)

**We never see those frames.** The guest's transmit ring is not being drained.
So this is not an RX problem and not an interrupt problem — the `seeq-enet`
thread has stopped pumping in both directions. Everything else follows from
that: no TX drained, no RX delivered, no interrupt raised.

Supporting state at the wedge (monitor, `lightning` build):

    intpend=false
    rx_cmd=0xfe rx_stat=0x30 tx_cmd=0x0f tx_stat=0x88
    threads: running
    HPC3 intstat = 00000000

    client:65535 → 151.101.193.6:80 age=165s
      srv_seq=0x40505270 srv_acked=0x404fcf44 in_flight=33580
      cli_win=33580 cli_seq=0xdd29dc4d rtx=25/33580B

`rx_stat=0x30` is `GOOD|END` with `OLD` clear — an unread status — and
`intpend=false` with the HPC3 latch clear. If the enet thread were alive it
would raise the line on its very next tick. It does not, and `threads: running`
only reports a flag, not liveness.

The NAT side is healthy and behaving correctly: `in_flight == cli_win` means the
guest's window is full so we stop reading from the server, and `rtx=25` is being
resent every RTO into a guest that is no longer listening.

## Where to look next

`Seeq8003::start`'s enet thread, in this order each iteration: `pump_tx`, then
up to `RX_DRAIN_CAP` `pump_rx` calls, then `lock_state!(state_enet)`. The
suspect is a lock, not logic — the DMA channel locks that `pump_tx`/`pump_rx`
take internally. The code already carries a comment about having broken one
ABBA deadlock here:

> Old: SeeqState → chan lock (enet thread) / Old: chan lock → SeeqState (CPU
> thread reading CTRL)

`seeq status` still answers at the wedge, so `SeeqState` itself is free. That
points at the enet thread being blocked *before* it takes `SeeqState` — i.e. in
a DMA channel lock held by the CPU thread.

**The measurement to make first:** a counter incremented at the top of the enet
loop, printed by `seeq status`. That settles "thread is blocked" versus "thread
is looping but doing nothing" in one run, and neither attempt below made it.

## Two fixes that did not work

Both were built on the theory that CLRINT loses an RX interrupt — `net.rs`
carries a FIXME saying exactly that, which is what made it convincing:

> IRIX writes CLRINT which clears *all* pending interrupts, silently dropping
> the RX interrupt.

**Attempt 1** — stop `reset_interrupt` forging `OLD`, and re-evaluate the line
straight away:

```rust
st.intpend = false;
if let Some(ref cb) = self.callback { cb.set_interrupt(false); }
Self::raise_interrupt(&mut st, false, &self.callback);
```

Broke networking outright: `dhcpcd` never completes a lease, stopping at
`delaying IPv4 for 0.9 seconds`. Re-asserting from inside the driver's own
CLRINT write, while it is still in its handler, is not the same as a level line
coming back up later.

**Attempt 2** — same, but re-arm from the enet thread instead of inside the
write, so the driver is never re-entered:

```rust
let rx_unread = (st.rx_stat & rx_stat::OLD) == 0
    && (st.rx_cmd & st.rx_stat & 0x1f) != 0
    && !st.intpend;
if did_something || rx_unread { Self::raise_interrupt(&mut st, dma_irq, &callback); }
```

DHCP survived this one, and the download still wedged at 3.7 MB. That is what
exposed the real problem: at the wedge `rx_unread` is *true by every term* —
`OLD` clear, `rx_cmd & rx_stat & 0x1f == 0x10`, `intpend` false — and the line
still never goes up. Code that should run every millisecond was not running at
all. Hence the check on the transmit side, and the empty ICMP table.

Both reverted. The FIXME is probably a real bug; it is just not this one.

## Reproducing

About a minute, no sysinst needed:

1. Boot the NetBSD installer, main menu → Utility menu → Run /bin/sh.
2. `dhcpcd sq0`
3. `ftp -a -q 20 -o /dev/null http://cdn.NetBSD.org/pub/NetBSD/NetBSD-10.2/sgimips/binary/sets/base.tgz`
4. It stalls within ~4 MB. Then from the monitor (`nc 127.0.0.1 8888`):
   `seeq status`, `hpc3 status`, `net status tcp`, and — the telling one —
   `ping` in the guest followed by `net status icmp`.

`log net on` produces nothing in a non-`developer` build: `dlog_dev!` is
compiled out, and `lightning` and `developer` are mutually exclusive. Budget a
separate non-lightning build for any packet log.

## Does IRIX hit this?

Unknown, and worth settling before assuming it is NetBSD-specific. IRIX has not
shown it in a lot of use, but IRIX has also not been asked to pull 180 MB over
HTTP in one go. The same test — a large `ftp` to `/dev/null` — would answer it.
