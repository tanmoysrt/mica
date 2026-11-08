# Use mica

This page explains each task after the first disk. [Get started](getting-started.md) installs mica and makes that first disk.

Each command talks to the mica daemon, so each one needs sudo. Most answers end with the next command to run. Add `--json` to a `status`, `disk`, `snapshot` or `bucket` command to get the answer of the daemon as JSON: [The daemon API](api.md) describes each answer.

## Buckets

The first command on a bucket needs its place and its keys. That command can be `mica bucket add`, the first `mica disk create`, or the first `mica disk attach` of a disk that another machine made. mica keeps the bucket when the command works. `mica bucket add` and `mica disk create` also test that the bucket keeps conditional writes, so use `mica bucket add` for a bucket that is new to you. Later commands need only the name of the disk: mica finds the disk in the buckets that it knows.

```bash
sudo mica bucket add --store s3.toml       # test the bucket and keep its keys
sudo mica bucket ls                        # the buckets of this machine
sudo mica bucket forget my-disks           # delete the keys from this machine
```

A new disk goes to the bucket that mica knows. When mica knows more than one, name it with `--bucket` and no keys: the bucket name, such as `my-disks`, or the bucket and its prefix, such as `my-disks/laptop`.

```bash
sudo mica disk create db --size 50G --bucket my-disks/servers
```

mica keeps the keys in `/var/lib/mica/buckets/`, readable by root only. These files are the only copy of the keys on the machine. `mica bucket forget` refuses a bucket that has a disk attached.

**Rotate the keys** of a bucket: give the new keys once, such as with `sudo mica bucket add --store new.toml`. The attached disks of that bucket use them at once.

**Give the bucket without a file.** mica takes each value from the first of these that has it:

1. A flag: `--endpoint`, `--bucket`, `--prefix`, `--region`, `--access-key-id`, `--secret-access-key`.
2. A variable: `OBJECT_STORAGE_ENDPOINT`, `OBJECT_STORAGE_BUCKET`, `OBJECT_STORAGE_PREFIX`, `OBJECT_STORAGE_REGION`, `OBJECT_STORAGE_ACCESS_KEY_ID`, `OBJECT_STORAGE_SECRET_ACCESS_KEY`.
3. The file of `--store`. See [store.example.toml](../store.example.toml).

Do not give the secret key as a flag on a shared machine: other users can see the flags of a process in `ps`.

## Disks

```bash
sudo mica disk create data --size 500G     # the bucket stores only the data that you write
sudo mica disk attach data                 # makes /dev/mica/data
sudo mkfs.ext4 /dev/mica/data              # only for a new disk
sudo mkdir -p /mnt/data
sudo mount -o discard /dev/mica/data /mnt/data
```

Mount with `discard`. Then the bucket also frees the chunks of the files that you delete. mica frees only whole chunks.

To give the disk to another machine, unmount it and detach it. The detach uploads all changes, and shows the speed and the time left. When it returns, any machine can attach the disk.

```bash
sudo umount /mnt/data
sudo mica disk detach data
```

`mica disk detach` refuses a disk that is mounted, or open in a program.

**See your disks.** `sudo mica status` shows the disks of this machine: the read, write, upload and download speed, the unsaved data, the cache and the last save. `sudo mica disk ls` shows the disks of each bucket that the machine knows, and where each one is attached:

```console
$ sudo mica disk ls
Bucket https://<account>.r2.cloudflarestorage.com/my-disks/laptop
DISK     SIZE  WHERE                     UNSAVED  CACHE  LAST SAVE   SNAPSHOTS
photos   1T    ● here: /dev/mica/photos  2.5G     120M   17 s ago    1
web      50G   ● on server-2             -        0      2 min ago   0
backups  2T    ○ detached                -        512M   3 days ago  3
```

`● here` is a disk attached on this machine, `● on <host>` a disk that another machine has, and `○ detached` a disk that any machine can attach.

## A disk for a VM

Give the device to the VM, and do not mount it on the host:

```bash
sudo mica disk attach vm-data
firecracker ...                            # drive path_on_host = /dev/mica/vm-data
sudo mica disk detach vm-data              # after the VM stops
```

The best use is a data disk. The VM boots from a small local root disk, and keeps its data, such as a database, on the mica disk. A VM can also boot from a mica disk. On a new machine, mica first downloads the chunks that the last boot read, so the VM starts fast.

Any program that takes a block device, such as QEMU or a container, can use it in the same way.

## A large disk for a laptop

mica can give a laptop a folder that holds terabytes, with the data in a bucket. You need sudo once, to set it up:

```bash
sudo mica service install
sudo mica disk create photos --size 1T --chunk-size 4M --cache-size 20G --store s3.toml
sudo mica disk attach photos
sudo mkfs.ext4 /dev/mica/photos
echo "/dev/mica/photos $HOME/photos ext4 discard,nofail,x-systemd.requires=mica.service 0 0" | sudo tee -a /etc/fstab
mkdir -p ~/photos && sudo mount ~/photos && sudo chown "$USER:" ~/photos
```

After that, use `~/photos` with no sudo. At each boot, mica attaches the disk before systemd mounts it.

At shutdown, mica uploads what it can in one minute, and the rest stays on the laptop. A shutdown never waits for a full upload, and the next boot continues from the local data. Give the disk a large cache, so that most reads stay local when the laptop is offline. Without a network, reads of data that is not on the laptop fail, and writes wait when the unsaved data reaches its limit.

## Snapshots and restore

A snapshot is a name for the current version of a disk. It copies nothing. On an attached disk, it uploads all changes first.

```console
$ sudo mica snapshot create photos before-cleanup
✔ Took snapshot before-cleanup of disk photos
```

mica has no rollback. To go back, make a new disk from a snapshot. The bucket copies the objects, so nothing downloads, and the new disk does not depend on the old one:

```console
$ sudo mica disk restore photos before-cleanup photos-old
✔ Made disk photos-old from snapshot before-cleanup of disk photos
  Attach it: sudo mica disk attach photos-old
```

`sudo mica disk info photos` lists the snapshots of a disk. `sudo mica snapshot delete photos before-cleanup` deletes one, with the objects that only it kept.

## Limits and chunk size

Each disk has three limits. They are stored with the disk, so they follow it to other machines.

| Limit | Flag | Default | What it does |
|---|---|---|---|
| Cache | `--cache-size` | 512M | The data from the bucket that this machine keeps |
| Unsaved | `--unsaved-size` | 4G | The data that is only on this machine. Writes wait at this limit. |
| Interval | `--interval` | 5m | How often mica uploads the changes. It uploads earlier when half the unsaved limit is full. |

`mica disk create` sets them, and `mica disk set` changes them. `mica disk set` can also make a detached disk larger. A disk uses at most the unsaved limit plus two times the cache limit on the local SSD.

A long interval costs fewer requests: a chunk that changes ten times in one interval uploads once. A crash of the daemon or a reboot loses nothing, because the changes are on the local SSD. Only the loss of the machine loses the changes since the last save.

The **chunk size** is fixed when you make the disk. Each upload sends each changed chunk, so the chunk size sets the number of requests:

- **1 MiB**, the default, suits most disks: files, databases, boot disks.
- **4 MiB** suits large files, such as photos, video and backups. It also makes a busy database about four times cheaper. See [Speed and cost](benchmarks.md#the-cost-of-a-50-gb-disk).
- **256 KiB or less** makes cold random reads cheaper, but a busy disk makes more requests.

## Restart, upgrade and crashes

`sudo systemctl restart mica` keeps the disks attached and the filesystems mounted. Each device pauses for about one second.

To upgrade, replace the binary and run `sudo mica service install` again: it restarts mica with the new binary.

After a crash, systemd starts mica again, and mica takes the devices over. After a reboot, mica attaches each disk that the machine had. When the bucket cannot be reached at boot, mica tries again every 10 seconds. A mount with `nofail` does not stop the boot.

## Commands

| Command | What it does |
|---|---|
| `mica status [--watch]` | The disks of this machine: speed, unsaved data, cache, last save |
| `mica disk ls` | The disks in each known bucket, and where each one is attached |
| `mica disk info <disk>` | One disk: where it is, its limits and its snapshots |
| `mica disk create <disk> --size <size>` | A new disk. Options: `--chunk-size`, `--cache-size`, `--unsaved-size`, `--interval` |
| `mica disk attach <disk>` | Make `/dev/mica/<disk>` on this machine |
| `mica disk detach <disk>` | Upload all changes and remove the device |
| `mica disk set <disk>` | New limits, or a larger size for a detached disk |
| `mica disk restore <disk> <snapshot> <new disk>` | A new disk from a snapshot, copied inside the bucket |
| `mica disk release <disk>` | Free a disk whose machine is gone for good. Its changes since the last save are lost. |
| `mica disk delete <disk>` | Delete a detached disk and all its snapshots |
| `mica snapshot create <disk> <name>` | Keep the current version under a name |
| `mica snapshot delete <disk> <name>` | Delete a snapshot |
| `mica bucket add` | Test a bucket and keep its keys |
| `mica bucket ls` | The buckets that this machine keeps keys for |
| `mica bucket forget <bucket>` | Delete the keys of a bucket from this machine |
| `mica service install` | Start mica now and at each boot. After an upgrade, restart it. |
| `mica service uninstall` | Stop mica and delete the keys of its buckets. The cache stays. |
| `mica service status` | Whether mica runs, and its last log lines |

`mica <command> --help` shows all options of a command.

## Settings of the machine

The settings of the machine are in `/etc/mica/config.toml`: the socket, the data folder and the run folder. The file is optional. See [config.example.toml](../config.example.toml) for each setting and its default.
