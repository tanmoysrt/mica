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

Each command that uses the bucket needs its credentials. Write them into
a file, and keep it private:

```bash
cp store.example.toml ~/photos.toml
chmod 600 ~/photos.toml
```

Then give the file with `--store ~/photos.toml`, or set `MICA_STORE`, or
set the variables `S3_ENDPOINT`, `S3_BUCKET`, `S3_PREFIX`, `S3_REGION`,
`S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`.

Two disks can use two buckets with two sets of credentials. The daemon
keeps the credentials of an attached disk only in memory and in `/run`.

## Use a disk

```bash
export MICA_STORE=~/photos.toml
sudo -E mica disk create photos --size 500G      # no space is used until it is written
sudo -E mica disk mount photos /mnt/photos       # attach, format when new, mount
sudo -E mica status
sudo -E mica disk unmount photos                 # trim, unmount, upload all, detach
```

On another node with the same bucket file:

```bash
sudo -E mica disk mount photos /mnt/photos
```

For a VM, attach the disk and give the device to the VM:

```bash
sudo -E mica disk attach vm-data                 # makes /dev/mica/vm-data
firecracker ...  # drive path_on_host = /dev/mica/vm-data
sudo -E mica disk detach vm-data                 # after the VM stops
```

The main use is a data disk: the VM boots from a local root disk, and the
mica disk holds its data, such as a database. A VM can also boot from a
mica disk.

### Commands

| Command | What it does |
|---|---|
| `mica status` | The disks attached on this node, their unsaved data and cache |
| `mica disk ls` | The disks in the bucket |
| `mica disk info <disk>` | One disk, its budgets and snapshots |
| `mica disk create <disk> --size 20G [--chunk-size 256K] [--cache-size 1G] [--unsaved-size 4G]` | A new, empty disk |
| `mica disk attach <disk> [--force] [--no-fill]` | Makes `/dev/mica/<disk>` |
| `mica disk mount <disk> <path> [--fs ext4] [--no-fill]` | Attach, format when new, mount |
| `mica disk unmount <disk or path>` | Trim, unmount, detach |
| `mica disk detach <disk>` | Upload all data and give the disk free |
| `mica disk commit <disk>` | Run a transaction now |
| `mica disk set-limits <disk> [--cache-size] [--unsaved-size]` | New budgets, kept in the head |
| `mica disk resize <disk> --size 40G` | A larger disk (detached only) |
| `mica disk restore <disk> <snapshot> <new disk>` | A new disk from a snapshot |
| `mica disk delete <disk>` | Delete the disk and its snapshots |
| `mica snapshot ls / create / delete <disk> <name>` | Snapshots |
| `mica service install / uninstall / start / stop / restart / status / logs` | The systemd unit |
| `mica service add-disk <disk> <path>` / `remove-disk <disk>` | Mount a disk at each boot |

`--json` prints the answer of the daemon as JSON. `mica status --json`
also shows the requests and bytes of each disk since its attach.

### Choose the chunk size

The chunk size is fixed when the disk is made.

- **256 KiB** (the default) suits most disks: files, boot disks, mixed use.
- **64 KiB** suits databases with random writes. A write into a chunk that
  is not local first copies the whole chunk, so small chunks copy less and
  upload less.
- **1 MiB to 4 MiB** suits large files that are read and written in order,
  such as video or backups. Fewer chunks mean fewer requests.

## A disk on a laptop or a desktop

mica can give a laptop a large disk whose data lives in a bucket. sudo is
needed once, to set it up:

```bash
sudo mica service install
sudo mica --store ~/photos.toml disk create photos --size 1T --cache-size 20G
sudo mica --store ~/photos.toml service add-disk photos ~/photos
```

`add-disk` copies the bucket file to `/etc/mica/disks/photos.toml` (root
only) and writes `mica-disk-photos.service`. The disk mounts at `~/photos`
at each boot, and the folder belongs to you. After that, use `~/photos`
with no sudo.

At shutdown, the unit only unmounts. The daemon runs a last transaction,
and the local data stays on the laptop. So a shutdown never waits for a
full upload, and the next boot goes on from the local data. Give the disk
a large cache, so that most reads stay local when the laptop is offline.
Without a network, reads of data that is not local fail, and writes wait
when the unsaved data reaches its limit.

## Restart, upgrade and crashes

`sudo systemctl restart mica` keeps the disks attached and the file
systems mounted. Each device pauses for about a second. Use it to upgrade
mica: replace the binary, then restart.

After a crash, systemd starts mica again, and mica takes the devices over.
After a reboot, the local data stays. Attach or mount the disk again, or
let `add-disk` do it.

## When something goes wrong

| Message | Meaning | What to do |
|---|---|---|
| `disk … is attached on <node>` | Another node owns the disk | Detach it there. When that node is gone for good, use `--force`. |
| `stopped: another node took the disk` | A forced attach took the disk | The local data is kept as orphaned in `/var/lib/mica/disks/<disk>`. Copy out what you need, then delete the folder. |
| `… has orphaned local data` | The folder holds data that is not in the bucket | Same as above. |
| `the local data … is of another disk` | A disk with this name was deleted and made again | The old data is kept as orphaned. |
| `this store cannot hold mica disks` | The bucket ignores conditional writes | Use another bucket or provider. |
| `upload is behind` in `mica status` | Unsaved data is older than five minutes | Check the network and the bucket. Writes wait at the unsaved limit. |
| `stopped: a local sync failed` | The local SSD failed a write | The disk refuses I/O. Check the SSD. |

`mica service logs -f` shows the log of the daemon.
