# Fix concept for the IP7 timer death: do not deliver a crossing from the Compare write

The bug is in `netbsd-sgimips-hangs-on-a-missed-compare.md`. This is the fix
shape, prototyped and measured but **not committed to any branch** -- it
changes upstream's IP7 ticket protocol, which is load-bearing for the ack
semantics documented in that handler, so it wants his agreement first.

## The change

`count_now()` becomes a wrapper over `materialize_count(deliver_crossing)`.
Every existing caller keeps today's behaviour (`true`). Exactly one path opts
out -- the Compare *write* in `write_cp0` reg 11:

    let ticket = self.arm_ip7_sequence();
    let count_before = self.materialize_count(false);
    self.cp0_compare = value as u32 as u64;

and the crossing block gains the guard:

    if deliver_crossing && dist_to_compare != 0 && (dist_to_compare as u64) <= ticks {

## Why suppress it there specifically

A crossing IP7 raised inside the Compare write is useless and harmful:

- **Useless**: two lines later that same write acknowledges IP7, clearing it
  from `cp0_cause` and from `hot.interrupts`. The guest never sees it.
- **Harmful**: `claim_ip7` CASes the shared sequence to `IP7_SEQ_CONSUMED`, but
  `self.ip7_seq` keeps its value, so `schedule_compare_timer` arms the one-shot
  with a ticket already spent. On its first fire it cannot claim, returns
  `TimerReturn::Delete`, and the timer is gone for the run.

The signal it was carrying is not lost: the write's own classifier already
handles an overrun deadline explicitly (case 2, "the guest asked for a real
deadline that we blew through"), and delivers IP7 itself. That is the right
place for it -- after the ack, with the write's own fresh ticket.

The crossing detector keeps working everywhere else, including the Linux
`c0_compare_int_usable()` probe it was added for: that polls Count in its own
loop, not inside a Compare write.

## Measured

Same instrumented build, NetBSD 10.2 GENERIC32_IP2x:

    guest / build                     writes  fired  cross   outcome
    NetBSD, upstream                  5       5      3       timer dead, hang
    NetBSD, ticket re-arm (earlier)   200     396    198     boots, 2x delivery
    NetBSD, this concept              200     199    1       boots, 1:1
    IRIX, upstream                    200     199    1       login
    IRIX, this concept                200     199    1       login, clean shutdown

The `late=1` in the NetBSD run is the case-2 classifier doing its job on a
genuinely overrun deadline, which is the outcome we want to keep.

An earlier local patch re-armed the sequence *after* `count_now()` so the
one-shot got a live ticket. It unblocks the boot but lets both the crossing
and the one-shot deliver -- 2x the interrupts, so hardclock runs fast. That
patch is a test enabler, not a fix. This one restores the 1:1 ratio IRIX has.

## What is still unproven

- Only two guests and one workload each. No long soak, no SMP (there is none
  here), no snapshot/restore across the change.
- `materialize_count(false)` still advances the anchor and the memo. That is
  deliberate -- the write needs a current Count to classify against -- but it
  means a crossing that happens exactly there is not merely deferred, it is
  dropped. Case 2 is what catches the case that matters; a crossing that is
  *not* an overrun deadline is by definition one the ack was about to clear.
- The ACK heuristic (`compare == count_last_guest_read`) is untouched and
  remains fragile for a guest that writes Compare before reading Count. See
  the parent note.
