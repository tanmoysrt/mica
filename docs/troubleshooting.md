# Fix a problem

Find the message that you see, then do what the last column says. To see the log of the daemon, run `journalctl -u mica -f`, or `sudo mica service status` for its last lines.

## Messages of a command

| Message | What it means | What to do |
|---|---|---|
| `disk … is attached on <host>` | Another machine has the disk | Detach it there. When that machine is gone for good, run `sudo mica disk release <disk>`. Its changes since the last save are lost. |
| `there is no disk …` | No known bucket holds a disk with this name | Check the name with `sudo mica disk ls`. For a disk in a new bucket, give the bucket once with `--store`. |
| `this machine knows no bucket …` | No command gave this bucket yet | Add it: `sudo mica bucket add --store <file>`. |
| `this machine knows more than one bucket` | A new disk needs a bucket, and the machine knows several | Name one with `--bucket`, such as `--bucket my-disks`. |
| `… names more than one bucket` | Two buckets have this name, with other prefixes | Give the bucket and its prefix, such as `my-disks/laptop`, as `sudo mica bucket ls` shows it. |
| `this bucket cannot hold mica disks` | The bucket refuses conditional writes, such as Backblaze B2 | Use another bucket or provider. |
| `warning: this bucket ignores conditional writes` | The bucket accepts the conditions but does not keep them, such as Garage | Attach each disk on one machine only. mica cannot stop two machines here. |
| `disk … is in use` at detach | A filesystem is mounted, or a program has the device open | Unmount it, or stop the program, then detach again. |
| `… has orphaned local data` | The machine has local data of this disk that is not in the bucket | See [orphaned data](#orphaned-data). |
| `the local data … is of another disk` | A disk with this name was deleted and made again | The old data is kept as orphaned. See [orphaned data](#orphaned-data). |

## States in `mica status`

| State | What it means | What to do |
|---|---|---|
| `ok` | All is well | Nothing. |
| `upload behind` | The unsaved data missed three saves | Check the network and the bucket. Writes wait when the unsaved data reaches its limit. |
| `stopped: another machine took the disk` | Somebody else wrote the head of the disk | The disk refuses all I/O. Its local data is kept as orphaned. See [orphaned data](#orphaned-data). |
| `stopped: a local sync failed` | The local SSD failed a write | The disk refuses all I/O. Check the SSD. |

## Messages in the log

| Message | What it means | What to do |
|---|---|---|
| `not served yet, tried again` | At start, the bucket could not be reached | Nothing. mica tries again every 10 seconds. Check the network. |
| `no keys for the bucket …` | The keys of the bucket of an attached disk were forgotten | Give them again with `sudo mica bucket add`. mica tries again every 10 seconds. |
| `UBSAN: array-index-out-of-bounds in io_uring/rsrc.c` (kernel log) | A warning of Linux 7.0 when the first ublk device starts | Nothing. It shows once per boot, and the data checks pass with it. |

## Other problems

**An fstab mount of a mica disk does not happen at boot.** systemd does not know `/dev/mica/<disk>`. Run `sudo mica service install` again: it writes the udev rule that names the device. Then check that the fstab line has `nofail,x-systemd.requires=mica.service`.

**`mica is not running`.** Start it with `sudo mica service install`, and look at `sudo mica service status` for the reason.

**Reads are slow on a new machine.** The first read of each chunk downloads it. mica fills the cache when the disk is quiet. A larger cache keeps more of the disk local: `sudo mica disk set <disk> --cache-size 20G`.

## Orphaned data

mica never deletes local data that the bucket does not have. When it cannot use such data, it keeps it as orphaned in `/var/lib/mica/disks/<disk>`. Copy out what you need, then delete the folder. The disk in the bucket is not touched, and you can attach it again.
