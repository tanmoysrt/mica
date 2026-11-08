# Running mica

This guide shows how to install mica, how to use disks, and what to do
when something goes wrong. [design.md](design.md) explains how mica works.

## Requirements

- Linux 6.16 or later, with the `ublk_drv` module. mica uses the zero-copy
  and user-recovery features of ublk.
- systemd, and root for the daemon.
- A local SSD for `/var/lib/mica`.
- An S3 bucket with conditional writes (`If-Match` and `If-None-Match`).
  AWS S3, Cloudflare R2, MinIO and RustFS have them. `mica disk create`
  checks the bucket first, and refuses a bucket without them.

## Install

```bash
cargo install --git https://github.com/tanmoysrt/mica
sudo cp ~/.cargo/bin/mica /usr/local/bin/
sudo mica service install       # writes mica.service, starts it, and starts it at boot
```

The node settings are in `/etc/mica/config.toml`. The file is optional:
see [config.example.toml](../config.example.toml) for each setting and its
default.

## The bucket of a disk

Each command that uses the bucket needs its place and its keys. mica
takes each value from the first of these that gives it:

1. A flag: `--endpoint`, `--bucket`, `--prefix`, `--region`,
   `--access-key-id`, `--secret-access-key`.
2. A variable: `OBJECT_STORAGE_ENDPOINT`, `OBJECT_STORAGE_BUCKET`,
   `OBJECT_STORAGE_PREFIX`, `OBJECT_STORAGE_REGION`,
   `OBJECT_STORAGE_ACCESS_KEY_ID`, `OBJECT_STORAGE_SECRET_ACCESS_KEY`.
3. A TOML file: `--store <file>`, else `/etc/mica/store.toml` when it exists.

A machine with one bucket keeps it in `/etc/mica/store.toml`, and needs no
flags:

```bash
sudo cp store.example.toml /etc/mica/store.toml
sudo chmod 600 /etc/mica/store.toml      # then fill it in
```

Do not give the secret key as a flag on a shared machine: other users see
the flags of a process in `ps`.

Two disks can use two buckets with two sets of credentials. The daemon
keeps the credentials of an attached disk in its folder,
`/var/lib/mica/disks/<disk>/store.toml`, root only. A detach deletes them.

## Use a disk

```bash
sudo mica disk create data --size 500G      # no space is used until it is written
sudo mica disk attach data                  # makes /dev/mica/data
sudo mkfs.ext4 /dev/mica/data               # only for a new disk
sudo mount -o discard /dev/mica/data /mnt/data
```

Mount with `discard`: then the bucket also deletes the data of deleted
files. To move the disk to another node, unmount it and detach it:

```bash
sudo umount /mnt/data
sudo mica disk detach data                  # returns when the bucket has all data
```

On the other node, `mica disk attach data` and `mount` again.

For a VM, give the device to the VM, and do not mount it on the host:

```bash
sudo mica disk attach vm-data
firecracker ...  # drive path_on_host = /dev/mica/vm-data
sudo mica disk detach vm-data               # after the VM stops
```

The main use is a data disk: the VM boots from a local root disk, and the
mica disk holds its data, such as a database. A VM can also boot from a
mica disk.

`mica disk detach` refuses a disk that is mounted or open.

### Commands

| Command | What it does |
|---|---|
| `mica status` | The disks attached on this node, their unsaved data and cache |
| `mica disk ls` | The disks in the bucket |
| `mica disk info <disk>` | One disk, its limits and snapshots |
| `mica disk create <disk> --size 20G [--chunk-size 1M] [--cache-size 512M] [--unsaved-size 4G] [--interval 5m]` | A new, empty disk |
| `mica disk attach <disk> [--force]` | Makes `/dev/mica/<disk>`. `--force` takes the disk from a node that is off. |
| `mica disk detach <disk>` | Uploads all data and gives the disk free |
| `mica disk set <disk> [--size] [--cache-size] [--unsaved-size] [--interval]` | New limits, or a larger size (detached only) |
| `mica disk restore <disk> <snapshot> <new disk>` | A new disk from a snapshot |
| `mica disk delete <disk> [--force]` | Deletes the disk and its snapshots |
| `mica snapshot create / delete <disk> <name>` | Snapshots. A create uploads all changes first. |
| `mica service install / uninstall / status` | The systemd unit `mica.service` |

`--json` prints the answer of the daemon as JSON. `mica status --json`
also shows the requests and bytes of each disk since its attach.

### Choose the chunk size

The chunk size is fixed when the disk is made. A transaction uploads each
changed chunk, so the chunk size sets the number of requests.

- **1 MiB** (the default) suits most disks: files, databases, boot disks.
- **4 MiB** suits large files, such as photos, video and backups. Fewer
  chunks mean fewer requests.
- **256 KiB or less** makes cold random reads cheaper, but a busy disk
  makes more requests.

## A disk on a laptop or a desktop

mica can give a laptop a large disk whose data lives in a bucket. sudo is
needed once, to set it up:

```bash
sudo mica service install
sudo mica disk create photos --size 1T --chunk-size 4M --cache-size 20G
sudo mica disk attach photos
sudo mkfs.ext4 /dev/mica/photos
echo "/dev/mica/photos $HOME/photos ext4 discard,nofail,x-systemd.requires=mica.service 0 0" | sudo tee -a /etc/fstab
mkdir -p ~/photos && sudo mount ~/photos && sudo chown "$USER:" ~/photos
```

After that, use `~/photos` with no sudo. At each boot, mica attaches the
disk again before systemd mounts it.

At shutdown, systemd unmounts the disk, and mica runs a last transaction.
The local data stays on the laptop. So a shutdown never waits for a full
upload, and the next boot goes on from the local data. Give the disk a
large cache, so that most reads stay local when the laptop is offline.
Without a network, reads of data that is not local fail, and writes wait
when the unsaved data reaches its limit.

## Restart, upgrade and crashes

`sudo systemctl restart mica` keeps the disks attached and the file
systems mounted. Each device pauses for about a second. Use it to upgrade
mica: replace the binary, then restart.

After a crash, systemd starts mica again, and mica takes the devices over.
After a reboot, mica attaches each disk that the node held again. When the
bucket cannot be reached at boot, mica tries again every 10 seconds. A
mount with `nofail` does not stop the boot: mount it when the disk is back.

## When something goes wrong

| Message | Meaning | What to do |
|---|---|---|
| `disk … is attached on <node>` | Another node owns the disk | Detach it there. When that node is gone for good, use `--force`. |
| `stopped: another node took the disk` | A forced attach took the disk | The local data is kept as orphaned in `/var/lib/mica/disks/<disk>`. Copy out what you need, then delete the folder. |
| `… has orphaned local data` | The folder holds data that is not in the bucket | Same as above. |
| `the local data … is of another disk` | A disk with this name was deleted and made again | The old data is kept as orphaned. |
| `this store cannot hold mica disks` | The bucket ignores conditional writes | Use another bucket or provider. |
| `upload is behind` in `mica status` | Unsaved data missed three transactions | Check the network and the bucket. Writes wait at the unsaved limit. |
| `stopped: a local sync failed` | The local SSD failed a write | The disk refuses I/O. Check the SSD. |
| `not served yet, tried again` in the log | At start, the bucket could not be reached | mica tries again every 10 seconds. Check the network. |

`journalctl -u mica -f` shows the log of the daemon.
