# A bulk transfer wedges the SEEQ permanently, a few MB in

Open as of 2026-09-19. Found with NetBSD/sgimips 10.2; **not fixed** — one
attempt made it worse, recorded below so the next try starts further along.

## Symptom

A large HTTP download into the guest stops dead after a few megabytes and never
recovers. NetBSD's `ftp` shows `- stalled -` and sits there forever. It is not
a truncation: nothing times out, nothing errors, the connection just stops.

The stall point is random — 1.9 MB, 3.2 MB, 5 MB, 15.8 MB across runs of the
same file — so it is a race, not a boundary. Small transfers (a 3 KB file,
`SHA512`) always work, which is why nothing else had noticed.

## The state at the stall

`net status tcp` and `seeq status` from the monitor, sampled 14 s apart:

    client:65535 → 151.101.193.6:80 age=165s
      srv_seq=0x40505270 srv_acked=0x404fcf44 in_flight=33580
      cli_win=33580 cli_seq=0xdd29dc4d rtx=25/33580B
    rx_cmd=0xfe rx_stat=0xb0 tx_cmd=0x0f tx_stat=0x88

Read it as:

- `in_flight == cli_win` — the guest's receive window is exactly full, so
  `poll_tcp` stops reading from the server. Correct behaviour.
- `rtx=25` — 25 segments are queued for retransmit and being resent every RTO.
- `age` climbs and **nothing else moves at all**. The guest sends no ACKs.
- `rx_stat=0xb0` = `OLD|GOOD|END`, every single sample. `OLD` set means "the
  driver has read this status". It never goes back to `0x30` (a new frame), so
  our retransmits are not being delivered — `RxPumpResult::Refused` — because
  the guest's RX DMA channel is no longer armed.

So the guest's driver is asleep waiting for an interrupt, and every frame we
try to hand it is refused, and a refusal raises no interrupt. Deadlock.

## Why the interrupt was lost

`net.rs` already carries a FIXME describing the race, and this is it:

> When a gateway reply is generated synchronously while draining TX frames, it
> can arrive at IRIX while the TX completion interrupt handler is still
> running. IRIX writes CLRINT which clears *all* pending interrupts, silently
> dropping the RX interrupt.

`Seeq8003::reset_interrupt` (CLRINT) does:

```rust
st.rx_stat |= rx_stat::OLD;
st.tx_stat |= tx_stat::OLD;
```

`OLD` means "the driver has read this status", and the one place entitled to
set it is `read_rx_reg`, when the driver actually reads RSTAT. Setting it in
CLRINT forges an acknowledgement for a status the driver never saw. For TX that
is survivable. For RX it is terminal: the frame is already in the ring, the
driver is never told, so it never re-arms RX DMA, so every later frame is
refused, so nothing ever raises the line again.

The FIXME's workaround — deferring synchronously-generated replies by one loop
iteration — only covers replies IRIS makes itself. Bulk data from a real server
is pushed from `poll_tcp` at arbitrary times and is not covered, which is why
a download is what exposes this.

## The fix that did not work

The obvious correction — stop forging `OLD`, and re-evaluate the line after
clearing the latch so a still-unread, still-enabled status re-asserts:

```rust
pub fn reset_interrupt(&self) {
    let mut st = lock_state!(self.state);
    st.intpend = false;
    if let Some(ref cb) = self.callback { cb.set_interrupt(false); }
    Self::raise_interrupt(&mut st, false, &self.callback);
}
```

**This breaks networking outright.** NetBSD's `dhcpcd` never completes a lease:
it gets as far as `delaying IPv4 for 0.9 seconds` and stops, leaving
`rx_stat=0x80 tx_stat=0x08`. Reverted.

Why it fails is not yet established. Two candidates worth separating before
trying again:

1. Re-asserting the line from inside the CLRINT write, while the guest is still
   in its handler, is not the same as a level-triggered line coming back up
   later — the driver may sample the line before it finishes and lose its
   place. Re-arming from the enet thread on its next tick instead would avoid
   re-entering the driver's own write.
2. CLRINT is an HPC3 latch, not a SEEQ register. Clearing SEEQ status there may
   be modelling something real about the HPC3 path, in which case the fix
   belongs in the HPC3 side and not here.

The measurement to make first: whether the guest is taking *too many*
interrupts after the change (a storm) or none at all. That distinguishes the
two, and neither was measured before reverting.

## Reproducing

Cheap and reliable — about a minute, no sysinst needed:

1. Boot the NetBSD installer, main menu → Utility menu → Run /bin/sh.
2. `dhcpcd sq0`
3. `ftp -a -o /dev/null http://cdn.NetBSD.org/pub/NetBSD/NetBSD-10.2/sgimips/binary/sets/base.tgz`
4. Watch it stall; then from the monitor (`nc 127.0.0.1 8888`):
   `net status tcp` and `seeq status`.

`log net on` produces nothing in a non-`developer` build — `dlog_dev!` is
compiled out — so the monitor's `net status` is the only instrument available
in a `lightning` build, and `lightning` and `developer` are mutually exclusive.
Budget a separate non-lightning build if you want the packet log.

## Does IRIX hit this?

Not visibly, in a lot of use. Whether that is luck (different interrupt timing,
smaller transfers) or a genuinely different driver path is unknown, and worth
settling before assuming the fix is NetBSD-only.
