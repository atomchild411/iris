# `inst` reports success for installs that did nothing

Four ways an IRIX `inst` run exits 0 having installed nothing. All four were
hit in one session putting MIPSpro 7.4 onto a 6.5.7 guest, and each looked
like a clean install until the artefact was checked.

**Verify by the artefact — does the file exist? — never by exit status.
And read the log; do not grep it.** A filter for `error|cannot|no matches`
misses two of the four cases below, because the word is *conflict*.

## 1. `open` displaces the `-f` source instead of adding to it

```
inst -f A          # then, at the prompt:
open B             # A is now invisible
install thing-from-A   -> "No matches for ... were found"
```

Despite the menu text ("Specify *additional* software locations"). Confirmed
with two sources as well as three. **Either give each distribution its own
`inst` invocation, or put both in one directory** (see 4).

## 2. A nonexistent subsystem

`install dev.hdr` where no such subsystem exists prints
`No matches for "dev.hdr" were found` and proceeds happily to `go`.

To find which subsystem actually owns a file, **read the media**, do not guess
at the guest. The `.idb` lines name the owner:

```sh
for f in media/*.iso; do strings -a "$f" | grep -m1 "usr/include/stdio\.h "; done
# f 0444 root sys usr/include/stdio.h ... irix_dev.sw.headers sum(26431) ...
```

That answered in one command what four rounds of guessing had not.

## 3. A version downgrade

```
2c. Allow downgrades by setting the "neweroverride" preference to "on"
```

Needs `inst -V neweroverride:on`. Reported as a *conflict*, so an error-only
grep misses it entirely.

## 4. READ-ONLY mode, because another `inst` still holds the lock

```
Inst 3.8 Main Menu (READ-ONLY)
ERROR: This command is not usable in Read-Only mode.
Another inst is currently running
```

A scripted `inst` whose stdin runs out **parks at the final prompt and keeps
the lock** even after its transaction completed. Check `ps -ef | grep inst`
and kill the stale one — safe once its log shows
`Installations and removals were successful` and the requickstart finished.

## The overlay/base deadlock, and why a merged directory fixes it

On a guest with release overlays applied, `eoe.sw.base` is newer than every
base CD, which produces a pair of mutually blocking refusals:

- base product — refused as a downgrade against the installed `eoe`
- overlay product — refused for a missing prerequisite (the base)

Neither installs alone; they must go in **one transaction**. Since `open` is
broken (1), the way to give `inst` both at once is to **copy both into a
single directory**:

```sh
mkdir /var/tmp/d-all
cp /path/base-cd/dist/irix_dev*      /var/tmp/d-all/   # irix_dev, irix_dev.sw, ...
cp /path/overlay-cd/dist/irix_dev_*  /var/tmp/d-all/   # irix_dev_657f, _657m, ...
inst -V neweroverride:on -f /var/tmp/d-all
```

This works because SGI names overlay products distinctly (`irix_dev_657m` vs
`irix_dev`, `dev_657m` vs `dev`), so they coexist in one directory. Two
*identically named* versions would collide and need the staged approach.

Verified end to end: `/usr/include/stdio.h` and `/usr/lib32/crt1.o` (which
resolves to `mips4/crt1.o`) both landed, and a test program compiled and ran.
