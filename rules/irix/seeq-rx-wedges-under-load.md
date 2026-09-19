# SEEQ receive wedges under sustained load, and then the guest looks mute

Open as of 2026-09-19. Found with NetBSD/sgimips 10.2. **Not fixed.** Two
attempted fixes failed and two successive diagnoses were wrong; all four are
recorded, because the wrong turns are most of what is worth inheriting here.

## Symptom

A large HTTP download into the guest stops after a few megabytes and never
recovers. The stall point is random — 1.9, 3.2, 3.7, 5.0, 8.4, 15.8 MB across
runs of the same file — so it is a race, not a boundary. Small transfers always
work, which is why nothing had noticed.

Afterwards the interface looks completely dead: DNS fails, `ping` gets no
replies, new connections fail instantly. Only a reboot recovers it. A retry
loop therefore cannot work — each boot buys a few MB, which is why an
`ftp -R` resume loop stopped at exactly 8 MiB and then burned 600 attempts
without moving.

## What is measured

From inside the guest, `vmstat -i` sampled every 5 s across a stall:

    sq0 intr     3930   13     <- transfer running
    sq0 intr     3930   13
    sq0 intr     3930   13     <- stalled here
    ...
    sq0 intr     3947   11     <- 70 s later

So interrupts do not stop dead; they collapse from a steady rate to a trickle.
**The enet thread is alive.**

The decisive measurement is `ping` from the guest:

    ping: sendto: Host is down
    20 packets transmitted, 0 packets received, 100.0% packet loss

`EHOSTDOWN` from `sendto` is an **ARP** failure, not a transmit failure. The
guest never gets an ARP reply for the gateway, so the entry cannot be
revalidated, so the kernel refuses to transmit at all. Our NAT correspondingly
shows zero ICMP entries while the ping runs (checked three times, 4 s apart,
*during* the ping — see the retraction below for why that timing matters).

So the broken direction is **receive**. Everything else — no TX, no DNS, no new
connections, "the whole interface is dead" — is downstream of the guest not
receiving our ARP reply.

Host-side state at the wedge:

    intpend=false
    rx_cmd=0xfe rx_stat=0xb0 tx_cmd=0x0f tx_stat=0x88
    HPC3 intstat = 00000000

    client:65535 → 151.101.193.6:80 age=165s
      srv_seq=0x40505270 srv_acked=0x404fcf44 in_flight=33580
      cli_win=33580 cli_seq=0xdd29dc4d rtx=25/33580B

`in_flight == cli_win` is correct behaviour: the guest's window is full, so
`poll_tcp` stops reading from the server and resends the 25 queued segments
every RTO into a guest that is not listening. The NAT entry then disappears on
its own at the 300 s idle timeout; that is a consequence, not a cause.

## Two retracted diagnoses

**"CLRINT loses the RX interrupt."** `net.rs` carries a FIXME saying exactly
that, which is what made it convincing. Two fixes built on it (below) both
failed. It may still be a real bug; it is not this one.

**"The enet thread stops pumping."** Based on the guest reporting transmitted
pings while `net status icmp` showed no entries. **That evidence was bad**: ICMP
NAT entries expire after 30 s, and the table was read well after the pings had
finished. Re-run with the query during the ping, the table is still empty — but
the reason is `EHOSTDOWN`, i.e. the guest never put anything on the wire. And
`vmstat -i` shows the thread is alive. Retracted.

The lesson worth keeping: **a NAT table that is empty proves nothing unless you
read it inside the entry's lifetime.**

## Two fixes that did not work

**Attempt 1** — stop `reset_interrupt` forging `OLD`, and re-evaluate the line
immediately:

```rust
st.intpend = false;
if let Some(ref cb) = self.callback { cb.set_interrupt(false); }
Self::raise_interrupt(&mut st, false, &self.callback);
```

Broke networking outright: `dhcpcd` never completes a lease, stopping at
`delaying IPv4 for 0.9 seconds`. Re-asserting from inside the driver's own
CLRINT write, while it is still in its handler, is not a level line coming back
up later.

**Attempt 2** — same, but re-arm from the enet thread so the driver is never
re-entered:

```rust
let rx_unread = (st.rx_stat & rx_stat::OLD) == 0
    && (st.rx_cmd & st.rx_stat & 0x1f) != 0
    && !st.intpend;
if did_something || rx_unread { Self::raise_interrupt(&mut st, dma_irq, &callback); }
```

DHCP survived; the download still wedged at 3.7 MB. At that wedge `rx_unread`
was true by every term and the line still never rose — which is what prompted
(and mis-led) the second diagnosis. `HPC3 intstat` was not sampled under this
build, and should be: it distinguishes "we never raised the line" from "we
raised it and the HPC3 latch did not take it".

Both reverted. Only the `intpend` line in `seeq status` was kept.

## Where to look next, in order

1. **Is the RX DMA channel armed at the wedge?** `RxPumpResult::Refused` means
   it is not. A counter for Delivered/Refused/Nothing, printed by
   `seeq status`, separates "we are not trying" from "the guest will not take
   it" — and neither attempt above measured it.
2. **Does the HPC3 latch take the line?** Sample `hpc3 status` under attempt 2's
   build, with the line supposedly asserted.
3. Only then consider the CLRINT semantics again.

## Reproducing

About a minute, from a booted NetBSD guest (`netbsd10.toml`):

    ftp -a -q 90 -o /dev/null http://cdn.NetBSD.org/pub/NetBSD/NetBSD-10.2/sgimips/binary/sets/base.tgz &
    for i in 1 2 3 4 5 6 7 8 9 10; do vmstat -i | grep sq0; sleep 5; done
    ping -c 5 192.168.100.1

and from the monitor (`nc 127.0.0.1 8888`), *while* each of those is running:
`seeq status`, `hpc3 status`, `net status tcp`, `net status icmp`.

`log net on` produces nothing in a non-`developer` build — `dlog_dev!` is
compiled out, and `lightning` and `developer` are mutually exclusive. Budget a
separate non-lightning build for a packet log.

## Does IRIX hit this?

Unknown, and worth settling rather than assuming it is NetBSD-specific. IRIX
has not shown it, but IRIX has also never been asked to pull 180 MB over HTTP
in one go. The same test — a large transfer to `/dev/null` — would answer it.
