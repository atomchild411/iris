# CD changer `cdrom-eject` does not trigger a `mediad` remount

**Finding (5.3, and expected on 6.5):** `iris-ci cdrom-eject <id>` cycles
the SCSI CD changer to the next disc at the *device* level, but the guest's
`mediad` does **not** notice the media change and keeps the previous disc's
filesystem mounted at `/CDROM` (stale — you'll see the old disc's contents,
or I/O errors). On real hardware the drive raises a media-change /
unit-attention that `mediad` polls; iris's changer eject doesn't deliver
that signal to the guest.

**Symptom:** after `cdrom-eject 4`, `ls /CDROM` still shows the *old* disc.

**Workaround:** remount by hand after every eject (the CD device is
`/dev/dsk/dks0d4s7` for SCSI id 4, EFS, read-only):

```bash
ic cdrom-eject 4
ic run "umount /CDROM"
ic run "mount -t efs -o ro /dev/dsk/dks0d4s7 /CDROM"
```

## 6.5.7 addendum: the manual remount can fail; bounce `mediad` instead

On a 6.5.7 IP28 guest the hand-mount above did **not** work, for three
stacked reasons:

- `/dev/dsk/dks0d4s7` **did not exist**. `/dev/dsk` is a symlink to `/hw/disk`,
  which had no `d4` entry, and `MAKEDEV` does not create one — the node is
  published by `mediad` when it sees the media.
- Mounting the hwgraph block node directly
  (`/hw/scsi_ctlr/0/target/4/lun/0/disk/volume/block`) failed `Resource busy`:
  `mediad` holds the generic scsi node (`fuser` shows it).
- `-t efs` on that node reported `Wrong filesystem type efs`.

What works is to **restart `mediad` and let it mount the disc itself**:

```bash
/etc/init.d/mediad stop ; sleep 2 ; /etc/init.d/mediad start ; sleep 12
mount | grep CDROM      # /dev/dsk/dks0d4s7 on /CDROM type efs (ro,nosuid,noquota)
```

It creates the device node *and* mounts, so the stale-disc problem and the
missing-node problem go away together.

## Swapping discs without `--ci`

The monitor (port 8888) does the same job as `iris-ci cdrom-eject`, so a guest
started without `--ci` can still cycle media:

```
scsi list 4                      # show the changer queue
scsi add 4 path/to/disc.iso      # append, becomes "next"
scsi eject 4                     # advance to it
```

**Paths must not contain spaces** — the monitor's argument parser splits on
them and reports `File not found` on the first word. Clone the image to a
space-free name first (`cp -c` on APFS is free).

This came up cycling the 3 Developer's Toolbox CDs during a 5.3 add-on
install (`rules/irix/irix-install.md` §11). The base-OS install via the changer
(6.5.22 recipe) doesn't hit it because `inst` itself reopens the
distribution path after each swap rather than relying on a `/CDROM` mount.

Related: shell driving is csh — see the csh-redirect memory note; use
`egrep` (5.3 `grep` has no `-E`), and a wedged `? ` continuation needs
Ctrl-D, not Ctrl-C.
