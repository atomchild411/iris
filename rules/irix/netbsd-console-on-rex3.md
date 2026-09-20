# Getting NetBSD's console onto REX3

Investigated 2026-09-19 on NetBSD 11.0/sgimips. **Partly working**: NetBSD now
draws its boot to the graphics head through REX3, then hangs. Everything below
is what it took to get that far, because almost none of it is guessable.

## `console=g` is the switch — not `ConsoleOut`

On IP22 NetBSD does **not** read the PROM's `ConsoleOut`. It emulates ARCS
itself and synthesises the answer from our `console` NVRAM variable
(`sgimips/sgimips/arcemu.c`):

```c
if (strstr(sgienv.gfx, "dead") != NULL)
        return "serial(0)";
switch (nvram.console) {
case 'd': case 'D': case 's': case 'S':  return "serial(0)";
case 'g': case 'G':                      return "video()";
}
```

and `sgimips/sgimips/console.c` then does an exact match:

```c
if (strcmp(consdev, "video()") == 0)     /* empty parens */
```

So:

- **`setenv console g`** is what moves NetBSD to the graphics head.
- Setting `ConsoleOut=video()` achieves nothing, and `video(0)` is worse than
  nothing: it matches neither `gio_video_init` nor `zs_serial_init`, so the
  kernel ends up with *no* console and userland output vanishes entirely. That
  wasted an earlier session on 10.2.
- If the `gfx` variable ever contains `dead`, serial wins regardless. Ours is
  unset, which is fine.

Remember `rtc save` from the IRIS monitor after any `setenv`, and take a copy of
the NVRAM first — `console=g` also moves the *PROM* to the graphics head, so
serial stops being a way back in. Keep a serial-console NVRAM to restore.

## It works, then hangs

With `console=g`, REX3 GO goes from 1238 (PROM only) to **127133**, and the
NetBSD boot is visible on screen. Then:

| | |
|---|---|
| `CPU running` | true, cycles climbing ~250 MIPS |
| `REX3 GO` | frozen at 127133 |
| `fastticks` | **495, not increasing** |
| sshd | never starts |

A spinning CPU with a dead timer. It drew, then wedged. Not a rendering
problem — that part works.

## Without `console=g` nothing can ever draw, and it is a NetBSD bug

Worth knowing so nobody burns a day on it. With the console on serial you can
still allocate a screen by hand, but it will never be drawn to.

`wsconscfg 0` — **not** `wsconscfg -t 80x25 0`; newport's only screen type is
named `default` (160x64), and a bad type name makes
`wsdisplay_screentype_pick` return NULL, which surfaces as the very misleading
`WSDISPLAYIO_ADDSCREEN: Device not configured`.

That succeeds, `/dev/ttyE0` becomes openable, writes to it return success — and
nothing appears, with REX3 GO never moving. In `dev/wscons/wsdisplay_vcons.c`:

```c
if (vd->active == NULL) {
        vd->active = scr;
        SCREEN_VISIBLE(scr);        /* marked visible... */
}
if (existing) { ... } else {
        SCREEN_INVISIBLE(scr);      /* ...and immediately unmarked */
}
```

A non-console screen is created with `existing == 0`, so it ends up as
`vd->active` *and* flagged invisible. `wsdisplay_addscreen` then calls
`show_screen` to fix exactly that, and `vcons_switch_screen` refuses:

```c
oldscr = vd->active;                /* == the screen we are trying to show */
if (oldscr != NULL) SCREEN_INVISIBLE(oldscr);
if (scr == oldscr) return;          /* returns before SCREEN_VISIBLE(scr) */
```

The screen stays invisible for good, vcons gates every draw, and
`newport_putchar` is never reached. On real hardware newport *is* the console,
`newport_cnattach` passes `existing=1`, and the path is never taken — which is
why this has survived.

An untested workaround: switch VTs from the emulated keyboard (Ctrl+Alt+F2)
after `wsconscfg 1`, which would make `scr != oldscr` and let
`vcons_switch_screen` reach `SCREEN_VISIBLE`.

## Useful while debugging this

- `REX3 GO` in the monitor's `perf snapshot` is the fastest "is the guest
  drawing at all" check. 1238 is what the PROM alone produces on this config.
- `rex fbdump <dir>` writes `rgb.bin`/`ci.png`; newport draws into the **CI**
  (8-bit) planes, not RGB, so check both.
- The guest's own geometry: an `ioctl(WSDISPLAYIO_GINFO)` on `/dev/ttyE0`
  reported `1282x1024 depth=24`, which ruled out the theory that the VC2 video
  timing walk in `newport_attach_common` had failed and left a 0x0 screen. The
  VC2 table is fine: `VideoEntryPtr = 0400` and RAM there is walkable.
- Keep sshd up. Every experiment here kills the console one way or another, and
  ssh is what makes it safe to try.
