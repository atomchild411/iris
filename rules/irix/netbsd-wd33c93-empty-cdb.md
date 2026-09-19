# NetBSD's WD33C93 driver gets "last command ignored" -- we saw an empty CDB

With the IP7 timer working (see
`netbsd-sgimips-hangs-on-a-missed-compare.md`), NetBSD 10.2/sgimips gets past
the SCSI settle and then aborts into the kernel debugger:

    [   3.0245535] next: aborting asr 0xc0 csr 0x40
    [   3.0418753] kernel: breakpoint trap
    Stopped in pid 0.18 (system) at 88074d2c:  jr  ra
    db>

## Decoding it

From `netbsd-10.2-src/usr/src/sys/dev/ic/wd33c93reg.h`:

    asr 0xc0 = SBIC_ASR_INT (0x80) | SBIC_ASR_LCI (0x40)   "last command ignored"
    csr 0x40 = SBIC_CSR_CMD_ERR                            "end with error"

`wd33c93.c:2144` is the `default:` arm of the driver's state machine --
"Something unexpected happened -- deal with it". The driver has no case for
being told its command was ignored here, so it aborts.

## Where LCI comes from on our side

`src/wd33c93a.rs` raises `asr::LCI` in exactly one place: `process_scsi_command`
when the CDB is empty.

    if cdb.is_empty() {
        dlog!(self.log_module(), "WD33C93A({}): Empty CDB!", self.id);
        self.update_asr(0, asr::LCI);
        self.queue_interrupt(Some(command_phase::DISCONNECTED),
                             scsi_status::INVALID_COMMAND);

So NetBSD issued a command and we assembled **no CDB bytes at all**. This is a
gap in how we collect the CDB, not a protocol disagreement about its contents.

The function already carries a comment about Linux's driver addressing the LUN
through IDENTIFY rather than the legacy CDB LUN field, so this chip's
driver-specific paths are known to vary. NetBSD's is a third one we do not
handle.

## The CDB path, found

`scsi regs` from the monitor at the abort:

    03-0E CDB       : 00 00 00 00 00 00  00 00 00 00 00 00
    10 CMD_PHASE    : 00
    15 DEST_ID      : 01
    17 SCSI_STATUS  : 40      (SBIC_CSR_CMD_ERR -- the csr NetBSD reported)
    18 COMMAND      : 20

Command `0x20` is `SBIC_CMD_XFER_INFO` (`wd33c93reg.h:326`), the initiator
level-II "Transfer Info". So NetBSD is **not** using `SBIC_CMD_SEL_ATN_XFER`
(0x08), the combined select-and-transfer that reads the CDB out of registers
0x03-0x0E. It drives the bus phase by phase and pushes CDB bytes through the
DATA register during COMMAND phase -- which is why the CDB registers are zero.

We do implement that: `wd33c93a.rs` has a PIO/DBR path for exactly this. Its
gate is the problem:

    if cmd == cmd::TRANSFER_INFO
        && (phase == command_phase::SELECTED          // 0x10
            || phase == command_phase::IDENTIFY_SENT  // 0x20
            || phase == command_phase::COMMAND_START) // 0x30
        && !state.use_dma()

Those three values are IRIX's state sequence -- the comment above the block
says so ("SELECTED (MESG_OUT), IDENTIFY_SENT (CDB after 0x8a), COMMAND_START
(write CDB re-issue)"). The dump shows `CMD_PHASE = 0x00`
(`command_phase::DISCONNECTED`), which is none of them, so the block is
skipped, the command falls through to the worker, and the worker looks for a
CDB nobody assembled.

**So the CDB-over-DATA-port path exists but its phase gate is cut to IRIX's
sequence.** NetBSD's does not match it.

Caveat on the evidence: `scsi regs` was read *after* the abort, so
`CMD_PHASE = 0x00` may be post-reset rather than the value at the moment of
the command. Confirming that wants the phase captured at the command write --
`log scsi on` from the monitor before booting the kernel, which enables the
`XFER_INFO PIO deferred phase=...` line in that same block.

### A false step worth recording

A first attempt used `IRIS_DEBUG_LOG=scsi` and found zero "Empty CDB!" lines,
which looked like the empty-CDB theory collapsing. It was not: `dlog!` is
**runtime**-gated on `devlog_is_active`, and `IRIS_DEBUG_LOG` had silently
done nothing -- `main.rs` wraps the whole thing in
`if let Some(dl) = DEVLOG.get()`, so when the `OnceLock` is unset the variable
is ignored without a word. Absence of a log line proves nothing until you have
checked the logging is on. Use `log scsi on` from the monitor instead.

## Status

Not yet reported upstream; it sits behind the timer bug, which is also not yet
reported. Upstream's `docs/wd33c93a.md` already cites NetBSD's `wd33c93.c` as a
reference, so a divergence from it is likely to be taken seriously.

Found with a **local test patch** to the IP7 ticket handling that is not in
`atom-wip` and is not proposed for upstream -- it exists only to get past the
timer bug so later bugs are reachable.
