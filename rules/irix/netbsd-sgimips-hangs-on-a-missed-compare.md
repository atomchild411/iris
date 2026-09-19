# A Count crossing inside the Compare write kills the IP7 timer for good

NetBSD 10.2/sgimips boots, probes the device tree, identifies the machine as
Indy (Guinness), then stops forever at

    scsibus0: waiting 2 seconds for devices to settle...

with every log line stamped `[ 1.0000000]`. The clock is not advancing.

Proven with upstream's own `developer_ip7` instrumentation on a clean
`upstream/main` build. `src/mips_core.rs` is untouched by `atom-wip`.

## The trace

The entire boot makes **five** Compare writes:

    #1 lastread=0x268f034a count=0x268f035c compare=0x26940c5a delta= 329982 FUTURE fired=0 cross=0
    #2 lastread=0x2691768a count=0x269b689b compare=0x2699156a delta=-152369 LATE!  fired=2 cross=1
    #3 lastread=0x269b732e count=0x269b733f compare=0x26a07c3e delta= 329983 FUTURE fired=2 cross=1
    #4 lastread=0x26a08584 count=0x26a08e99 compare=0x26a5854e delta= 325301 FUTURE fired=3 cross=2
    #5 lastread=0x26a099c6 count=0x26a5a69c compare=0x26aa8e5e delta= 321474 FUTURE fired=5 cross=3

Write #5 arms an ordinary ~9.7 ms deadline. Nothing follows: `fired` stays at
5, Compare is never written again, and the machine ends at
`Count=0x59753c35 Compare=0x26aa8e5e` with `Cause IP:________`.

Note #2: a genuinely late deadline, classified `LATE!` and delivered
correctly. **The read-to-write race is real but is already handled.** It is
not what breaks the boot.

## Root cause

In `write_cp0` reg 11:

    let ticket = self.arm_ip7_sequence();   // ip7_seq = 5, shared = 5
    let count_before = self.count_now();    // CROSS -> claim_ip7(5)
                                            //   CAS(5 -> CONSUMED) succeeds
    self.cp0_compare = value;
    self.cp0_cause &= !CAUSE_IP7;                        // ack clears IP7
    self.hot.interrupts.fetch_and(!(CAUSE_IP7 as u64));  // ...here too
    ...
    self.schedule_compare_timer();          // one-shot ticket = self.ip7_seq = 5

Two faults compound:

1. The crossing IP7 that `count_now()` raises is cleared two lines later by
   the write's own acknowledgement. That tick is lost.
2. The crossing consumed the ticket the new one-shot is about to use.
   `claim_ip7` CASes the **shared** sequence to `IP7_SEQ_CONSUMED`, but
   `self.ip7_seq` still holds 5, so `schedule_compare_timer` arms with ticket
   5 against a shared value of `CONSUMED`. When that one-shot fires,
   `claim_ip7` fails and returns `TimerReturn::Delete`.

The one-shot deletes itself, nothing re-arms it, and **the timer is dead for
the rest of the run.** The guest then waits on a deadline no longer has any
source, which looks like a hang.

The trigger is narrow, which is why IRIX does not hit it: the old deadline
must be crossed *inside the Compare-write handler's own `count_now()`* while
the new deadline is in the future. NetBSD's read/compute/write tick pattern
reaches it on the fifth tick.

## What an earlier draft of this note got wrong

It blamed `schedule_compare_timer`'s full-wrap arm (~109 s at 33 MHz) for a
deadline already behind Count. That arm is correct and the wrap was a
*symptom* observed after the timer was already dead -- Count simply kept
running past a Compare nobody would ever rewrite. The `LATE!` classification
handles the real race. Measure before blaming.

## Not a NetBSD bug -- checked against the source

`sys/arch/mips/mips/mips3_clockintr.c:65` (NetBSD 10.2, extracted to
`netbsd-10.2-src/`) is well formed, and carries explicit missed-tick recovery:

    ci->ci_next_cp0_clk_intr += (uint32_t)(ci->ci_cycles_per_hz & 0xffffffff);
    mips3_cp0_compare_write(ci->ci_next_cp0_clk_intr);

    /* Check for lost clock interrupts */
    new_cnt = mips3_cp0_count_read();
    if ((ci->ci_next_cp0_clk_intr - new_cnt) & 0x80000000) {
            ci->ci_next_cp0_clk_intr = new_cnt + curcpu()->ci_cycles_per_hz;
            mips3_cp0_compare_write(ci->ci_next_cp0_clk_intr);
            curcpu()->ci_ev_count_compare_missed.ev_count++;
    }

It notices Compare falling behind Count, re-bases from the current Count and
counts the event -- exactly the defence that would recover here. But it runs
**inside the interrupt handler**, and entering the handler needs an interrupt.
Once the one-shot deletes itself no interrupt ever arrives, so the recovery
can never run. NetBSD is robust against missed ticks and helpless against a
timer that has ceased to exist.

The source also explains the double fire in the trace: NetBSD writes Compare
*before* reading Count, so write #4's hptimer and write #5's cross-detector
hold different tickets and both claim -- `fired` 3 -> 5 for a single tick.

### Latent, separate: the ACK heuristic is testing a stale value here

Because NetBSD writes Compare before reading Count, `count_last_guest_read` is
from a previous iteration when the write lands. The case-1 test
(`compare == count_last_guest_read`) therefore compares against a stale value
for this guest. It did not misfire in this trace (`ack=0` throughout), but a
coincidental match would silently swallow a real deadline as an
acknowledgement.

## IRIX is not exposed -- measured, not assumed

Same instrumented build (unpatched), IRIX 6.5 booted to the login prompt:

    43663 Compare writes   fired=43663   ack=0   late=2   cross=13248

**Exactly one interrupt per write, across a whole boot**, despite 13,248
crossings. The crossing machinery is not itself the problem: for IRIX a
crossing is detected during an ordinary Count read *outside* the Compare
write, so it delivers the tick, the handler runs, and the handler's own
Compare write re-arms. That is the design working.

The `lastread` column is the difference:

    IRIX    lastread=0x13c4e45a count=0x13c4e46b     17 ticks apart
    NetBSD  lastread=0x2d1fe77f count=0x2d24d95f    ~323552 ticks apart

IRIX reads Count immediately before writing Compare
(`compare = read_count() + delta`). NetBSD writes Compare first, so its last
read is a full tick old and the crossing lands *inside* the write handler --
where the IP7 it raises is cleared two lines later by that write's own ack,
and the ticket it consumes is the one the next one-shot is about to use.

So the rule is: a guest is exposed if and only if it writes Compare before
reading Count. Linux does `write_c0_compare(read_c0_count() + delta)` like
IRIX, so it is safe for the same reason.

### A measurement error worth not repeating

An earlier draft claimed upstream delivers two IP7 per tick to NetBSD, from
`fired=396` against 200 writes. That run was on the **local test patch**,
which re-arms the sequence after `count_now()` and so hands the one-shot a
ticket the crossing did not consume -- letting both deliver. The doubling is
most likely the patch's own artifact. Unpatched, NetBSD dies after five ticks,
so there is no long unpatched run in which to measure the real ratio. Do not
quote a doubling figure for upstream without one.

## Fix sketch, untested

`schedule_compare_timer` should arm against the sequence as it stands, not a
stale copy -- re-arm the sequence after `count_now()`, or have it take the
ticket it should use. Separately, a crossing IP7 raised inside the Compare
write is going to be cleared by that same write's ack, so `count_now()` should
probably not deliver one from there at all.

Needs upstream's view: both halves are theirs, and the ticket protocol is
load-bearing for the ack semantics documented in that handler.

## Reproducing

    mkvh build netbsd-boot.img --size 64M --bootfile netbsd \
        netbsd=netbsd-INSTALL32_IP2x.ecoff

Attach as SCSI 1, headless, NVRAM with serial console. Build with
`--features developer_ip7`. At the PROM take option 5, then

    boot -f scsi(0)disk(1)rdisk(0)partition(8)netbsd

Trace saved at `netbsd/ip7-trace.txt`.
