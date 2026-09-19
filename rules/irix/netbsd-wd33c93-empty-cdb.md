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

## Next step

Re-run with `IRIS_DEBUG_LOG=scsi` to confirm the "Empty CDB!" line and capture
the register writes NetBSD makes before the command, then compare against
`wd33c93.c`'s command issue path (`wd33c93_select` / `SEND_CMD` and the
`SBIC_CMD_SEL_ATN_XFER` auto-transfer path). The question to answer is which
register sequence NetBSD uses to deliver CDB bytes that we are not collecting.

## Status

Not yet reported upstream; it sits behind the timer bug, which is also not yet
reported. Upstream's `docs/wd33c93a.md` already cites NetBSD's `wd33c93.c` as a
reference, so a divergence from it is likely to be taken seriously.

Found with a **local test patch** to the IP7 ticket handling that is not in
`atom-wip` and is not proposed for upstream -- it exists only to get past the
timer bug so later bugs are reachable.
