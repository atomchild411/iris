# IRIX 6.5.22 `shutdown -i0` hangs at the EcWatch daemon

Observed 2026-09-19 on the 6.5.22m image, on a disk clone. **Pre-existing** —
confirmed against two different builds, so do not go looking for it in whatever
you changed today.

## What it looks like

`/etc/shutdown -y -g0 -i0` from a serial root login runs the `K` scripts and
then stops dead:

```
The system is shutting down.
Please wait.
PID file /tmp/socks5.pid-1080 does not exist
/usr/freeware/apache/sbin/apachectl stop: httpd (no pid file) not running
httpd (no pid file) not running
Stopping Form & Vision Eclipse EcWatch daemon
```

and stays there. Seen for 16 minutes before giving up, host CPU pegged at 100%.

The monitor's `perf snapshot` gives the signature:

| | |
|---|---|
| `CPU running` | true, cycles climbing |
| `fastticks` | **frozen**, not increasing |

A spinning CPU with a dead timer — the same shape as the NetBSD REX3 console
hang in [`netbsd-console-on-rex3.md`](netbsd-console-on-rex3.md). Whether the
two share a cause is unknown; nobody has looked.

## It is not a regression

Reproduced identically on:

- a build of the current tree, and
- `/tmp/featchk/release/iris`, built earlier from the tree *before* the
  `mips_cache_v2` `C_IST` change
  ([`pr-drafts/pr-cache-index-store-tag.md`](../../pr-drafts/pr-cache-index-store-tag.md)),

both hanging at the same line with the same frozen `fastticks`. That control
run is the only reason we know the cache change is innocent — run one before
blaming a hang on your own work.

## Why it matters

The standing rule is to shut IRIX down properly rather than killing the
emulator, because an unclean halt can leave the image with boot problems. This
hang means `-i0` does not reliably get you there on this image. Until someone
diagnoses it:

- **Work on a clone** (`./snapshot.sh save`, or `cp -c`) for anything that ends
  in a shutdown, so a forced kill costs nothing.
- The guest *is* still alive when it hangs — it is one daemon's stop script
  spinning, not a dead kernel — so the filesystem has already been synced by
  the `K` scripts that ran before it.

## Worth trying next

Nothing here has been tested; these are the obvious leads.

- Disable the EcWatch init script and see whether shutdown then completes,
  which would confirm it is that daemon rather than the run-level transition.
- `init 0` directly, instead of `/etc/shutdown`.
- Check whether the frozen `fastticks` precedes the hang or follows it — if the
  timer dies first, the daemon is a victim and not the cause, and this is
  really a timer bug.
