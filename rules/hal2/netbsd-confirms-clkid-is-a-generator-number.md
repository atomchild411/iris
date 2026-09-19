# NetBSD's haltwo settles CLKID: it is a generator number, not an index

A codec's CLKID field (CTRL1 bits 4:3) selects which of HAL2's three Bresenham
clock generators feeds it. Whether the value is the generator *number* (1..3)
or a zero-based *index* was previously inference from Linux's `hal2.c` plus our
own traces -- which is why the fix for it was held back as unproven.

NetBSD/sgimips answers it, in source and at runtime, and agrees with the
generator-number reading.

## In the source

`netbsd-10.2-src/usr/src/sys/arch/sgimips/hpc/haltwo.c:405`, setting up
playback:

    /* Setup samplerate to HW */
    haltwo_write_indirect(sc, HAL2_IREG_BRES1_C1,
        play->sample_rate == 44100 ? 1 : 0, 0);
    haltwo_write_indirect(sc, HAL2_IREG_BRES1_C2, inc, 0xFFFF);   /* inc = 4 */
    ...
    /* Set PBUS channel, Bresenham clock source, number of channels to HW */
    haltwo_write_indirect(sc, HAL2_IREG_DAC_C1,
        (0 << HAL2_C1_DMA_SHIFT) |
        (1 << HAL2_C1_CLKID_SHIFT) |
        (play->channels << HAL2_C1_DATAT_SHIFT), 0);

It configures **BRES1** and then selects it with **CLKID = 1**. An index would
have needed CLKID = 0.

## At runtime

GENERIC32_IP2x under an **unmodified upstream** build. NetBSD announces:

    audio0 at haltwo0: playback
    audio0: slinear_be:16 2ch 48000Hz, blk 4096 bytes (21.3ms) for playback

and upstream's own `hal2 status` shows:

    BRES1: sel=0 (48000 Hz master)  inc=4  modctrl=65535  -> 48000Hz
    BRES2: sel=1 (44100 Hz master)  inc=1  modctrl=65535  -> 44100Hz
    Codec A: ch=0 bres=2 rate=44100Hz mode=stereo
      ctrl1=0x0208
    cpal underruns (samples): 579690

`ctrl1 = 0x0208`, so `CLKID = (ctrl1 >> 3) & 3 = 1`. NetBSD set BRES1 to the
48000 Hz it asked for and left BRES2 at its 44100 default. Upstream resolves
CLKID=1 to **BRES2 / 44100 Hz** -- one generator off from the one the driver
configured and announced. The underrun count is the consequence.

## What this changes

The CLKID fix (`5e76e78` in atomchild) was rejected for upstreaming with the
caveat "that assertion is inference from Linux's `hal2.c` and our own trace,
not a datasheet". That caveat no longer applies: a second, independent driver
both documents the intent in a comment ("Bresenham clock source") and exhibits
it at runtime.

It also gives the fix a **reproduction that does not involve Quake** and does
not touch the `ALsetparams` wedge: boot GENERIC32_IP2x, read `hal2 status`,
compare Codec A's decoded rate against the rate NetBSD prints at attach.

Still not established by this: the `reclock_active` mirror half of that commit,
which remains untested. Do not describe the whole commit as proven.

## Reproduced on two major releases

NetBSD 11.0 (GENERIC32_IP2x, built 2026-07-30) behaves identically to 10.2:
same `HAL2 revision 4.1.0`, same `48000Hz` announcement, same `ctrl1=0x0208`,
and upstream still resolves it to `bres=2 rate=44100Hz`. Two independently
maintained releases a year apart agree, so this is a property of our decode,
not of one version's quirks.

## Aside

`haltwo` attaching at all is its own result -- it reads our HAL2 as
"revision 4.1.0" and configures a stream, so the register surface is good
enough for an independent driver.

Needs the local IP7 timer patch to get this far; see
`rules/irix/netbsd-sgimips-hangs-on-a-missed-compare.md`.
