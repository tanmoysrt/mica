# Operations

How to install mica on a node, configure it, and run it day to day.

## Requirements

- Linux 6.0 or later with the `ublk_drv` module. User recovery (restart without downtime) needs a kernel that supports `UBLK_F_USER_RECOVERY_REISSUE`.
- systemd.
- An S3-compatible bucket: Cloudflare R2, Ceph RGW or Garage. mica does not need conditional writes.
- A local SSD for `/var/lib/mica`. XFS or btrfs is best: their reflink copies make chunk copies free.
- `mkfs.ext4` or `mkfs.xfs`, and `fstrim`, for `mica disk mount`.
- To build: Rust, and libclang (`clang-devel` on Fedora, `libclang-dev` on Debian and Ubuntu).

## Setup

Install mica from GitHub, then run setup as root:

```bash
cargo install --git https://github.com/tanmoysrt/mica
sudo ~/.cargo/bin/mica service setup
```

`sudo` does not search `~/.cargo/bin`, so give the full path. Setup copies the binary to `/usr/local/bin/mica`; after that, `mica` works everywhere.

Setup asks for the S3 settings. Each prompt shows the current value; Enter keeps it. The secret key is never shown. Then setup:

1. Loads `ublk_drv`, and makes it load at boot.
2. Creates the `mica` group.
3. Checks that the data folder supports reflink copies.
4. Tests the bucket with a put, a get and a delete, and names the step that fails.
5. Saves `/etc/mica/config.toml` with mode 0600, because it holds the secret key.
6. Copies the binary to `/usr/local/bin/mica`.
7. Installs `mica.service` and `mica-mount@.service`, and starts mica. A running mica restarts without downtime.
8. Offers to add you to the `mica` group.

**Without questions**, for automation: write the config file first, then

```bash
sudo mica service setup --use-config-file
```

It fails with a list of every missing field. Setup reads only the file, never the shell environment, because a systemd service does not get that environment.

**To upgrade**, install the new version and run setup again. Attached disks keep working through the restart:

```bash
cargo install --git https://github.com/tanmoysrt/mica --force
sudo ~/.cargo/bin/mica service setup --use-config-file
```

## Configuration

`/etc/mica/config.toml`:

```toml
socket = "/run/mica.sock"
data_dir = "/var/lib/mica"
cache_limit_gib = 50
disk_cache_limit_mib = 1024
local_reserve_mib = 1024
dirty_limit_gib = 20
checkpoint_interval_secs = 30
checkpoint_after_mib = 512
max_unsaved_minutes = 5
wait_when_behind = false
mount_roots = ["/mnt", "/srv", "/media"]
gc_keep_checkpoints = 5
gc_grace_hours = 24
prefetch_on_attach = true
background_fill = true
memory_high = ""

[s3]
endpoint = "https://<account>.r2.cloudflarestorage.com"
bucket = "my-bucket"
access_key_id = "..."
secret_access_key = "..."
region = "auto"
prefix = ""
conditional_writes = false
```

| Setting | Default | Meaning |
|---|---|---|
| `socket` | `/run/mica.sock` | The daemon's Unix socket |
| `data_dir` | `/var/lib/mica` | Local chunks, cache and state. Cache and dirty chunks must be on one filesystem. |
| `cache_limit_gib` | 50 | Host safeguard for total clean-cache data |
| `disk_cache_limit_mib` | 1024 | Default clean cache per disk, overridden by `--cache-size` |
| `local_reserve_mib` | 1024 | Filesystem headroom; reclaim cache and pause writes below this |
| `dirty_limit_gib` | 20 | Local data not yet in S3, **for each attached disk**. At half, a checkpoint starts early; at the limit, writes wait. |
| `checkpoint_interval_secs` | 30 | Time between checkpoints of a busy disk. Keep it well below `max_unsaved_minutes`. |
| `checkpoint_after_mib` | 512 | A checkpoint also starts when this much local data is not in S3. So uploads run while the guest writes, and a detach has little left to upload. |
| `max_unsaved_minutes` | 5 | A disk is **behind** when data not in S3 is older than this |
| `wait_when_behind` | false | When behind, guest writes wait until uploads catch up. The data-loss bound then always holds; on a slow link, writes are slow. |
| `mount_roots` | `/mnt`, `/srv`, `/media` | `mica disk mount` only mounts below these folders |
| `gc_keep_checkpoints` | 5 | Checkpoints of each disk that GC keeps as restore points |
| `gc_grace_hours` | 24 | GC never deletes objects written in this time |
| `prefetch_on_attach` | true | At attach, download the chunks of the read profile: what the disk read first in its last session, up to 64 MiB |
| `background_fill` | true | After that, download every other chunk of the disk, behind all reads, until the cache is nearly full. `mica disk attach --no-fill` turns it off for one attach. |
| `memory_high` | empty | `MemoryHigh=` of `mica.service`, such as `2G`. Empty means no limit. |
| `s3.endpoint` | – | S3 endpoint URL |
| `s3.bucket` | – | Bucket name |
| `s3.access_key_id`, `s3.secret_access_key` | – | Credentials |
| `s3.region` | `auto` | Region; `auto` works for R2 |
| `s3.prefix` | empty | Key prefix, so that one bucket can hold many mica setups |
| `s3.conditional_writes` | false | The store honors `If-None-Match: *` on PUT. Then a claim is one atomic create, and an attach takes about 25 ms, not 1 s. True for R2, AWS S3, MinIO and RustFS. Keep it false for Garage. |

When you run `mica daemon` by hand during development, empty S3 fields come from the environment: `S3_ENDPOINT`, `BUCKET_NAME`, `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY`, `S3_REGION`, `S3_PREFIX`.

## Permissions

The daemon runs as root. Its socket is `root:mica`, mode 0660. To use mica without sudo:

```bash
sudo usermod -aG mica $USER
newgrp mica
```

A member of the group must not become root through mica, so:

- `mica disk mount` asks the daemon, which mounts through a systemd unit. The member's own process never mounts.
- Mounts go only below `mount_roots`. Paths with `..`, and symlinks that lead outside, are refused.
- Mounts use `nosuid` and `nodev`.
- `--force`, and `--owner` for another user, need root.
- The config file, which holds the secret key, stays readable by root only. The CLI does not need it.

## Running a node

### Persistent mounts

With the service installed, `mica disk mount` creates `mica-mount@<disk>.service`:

- At boot, systemd starts mica, then attaches and mounts the disk.
- At shutdown, systemd unmounts the disk and uploads its data, then stops mica.
- `systemctl status mica-mount@disk-1` shows one mount. `systemctl list-units 'mica-mount@*'` shows all.

`mica disk unmount` stops and removes the unit.

### Containers

Mount on the host, then bind-mount the path:

```bash
mica disk mount data-1 /srv/volumes/data-1
docker run -v /srv/volumes/data-1:/data my-image
```

### VMs

Attach the raw device and give it to the VMM:

```bash
mica disk attach sandbox-7
qemu-system-x86_64 ... -drive file=/dev/mica/sandbox-7,format=raw,if=virtio,cache=none
```

Stop the VM before `mica disk detach`.

### Images for fast boots

```bash
mica disk create builder --size 20G
mica disk attach builder
# install the OS on /dev/mica/builder, boot it once, stop it
mica disk detach builder
mica snapshot create builder base-image
mica disk create sandbox-1 --from-snapshot base-image
```

The first boot records a read profile, and every disk made from the snapshot carries it. With `mica disk attach <disk> --prefetch <chunks>`, those chunks download in the background at attach. A node that keeps the base image's chunks in its cache boots such disks without any downloads.

### Warming a node

To make every sandbox from an image start without downloads, warm the node once:

```bash
mica disk warm base-image
```

The chunks stay in the cache while any attached disk uses them. `mica cache prune` keeps them for as long as one such disk is attached.

### Cleaning up

- `mica gc` deletes data that nothing needs: old chunk versions, deleted disks and snapshots. Run it from time to time, for example daily from a timer or your controller (`mica gc -y`).
- `mica cache prune` frees local cache space. The cache also trims itself at its limit, so this is optional.

## Memory

`systemctl status mica` counts the page cache of the data files against mica. That memory is free for any program that needs it. mica's own memory is small: about 15 MiB idle, and about 80 MiB during a large upload.

To cap the page cache anyway, set `memory_high = "2G"` and run `mica service setup --use-config-file`. Above the limit, the kernel reclaims mica's cache and slows mica down; it never kills it.

## Performance

Measured in a local lab: RustFS behind a 100 MiB/s link with 1 ms round trip, and a local disk limited to 300 MiB/s. The full results, with v0.1 next to v0.2, are at <https://sites.tanmoysarkar.me/research/mica-v0-2-optimization/>.

| Test | v0.1 | v0.2 |
|---|---|---|
| Attach of one disk | 1,035 ms | 24 ms |
| 4 disks attached at the same time | 4.2 s | 0.06 s |
| Cold Firecracker boot (Ubuntu), attach until ssh answers | 4.6 s | 2.8 s |
| S3 download during that boot | 250 MiB | 136 MiB |
| Cold sequential read, 1 reader | 69 MiB/s | 91 MiB/s (the link) |

Cold reads, the background fill and uploads are limited by the network to S3. Warm reads are limited by the local disk.

## Cost

On Cloudflare R2: storage $0.015 per GB-month, Class A requests (PUT, LIST) $4.50 per million, Class B (GET, HEAD) $0.36 per million, DELETE and egress free.

| Action | Class A | Class B | Cost |
|---|---|---|---|
| Idle attached disk | 0 | 0 | $0 |
| Create and mount a new disk | 3 | ~11 | – |
| Write 1 GiB of new data, 256 KiB chunks | 4,096 | – | $0.018 |
| Write 1 GiB of new data, 4 MiB chunks | 256 | – | $0.0012 |
| A checkpoint after a small write | ~4 | ~5 | – |
| Stop and start a sandbox (Ubuntu, cold cache, 256 KiB chunks) | ~6 | ~550 | $0.0002 |
| A disk busy all day, every day (a checkpoint every 30 s) | ~350,000 / month | ~430,000 / month | ~$1.70 / month |
| 1 TB of stored data | – | – | ~$15 / month |

Storage is the main cost. Chunks are compressed, and disks made from one snapshot share its chunks, so an image is stored once. Small chunks and frequent checkpoints cost more requests: set `--chunk-size 4M` and a longer `checkpoint_interval_secs` for disks where that matters more than cold reads. `mica status` shows the requests since mica started.

## Troubleshooting

| Message | Cause and fix |
|---|---|
| `mica is not running` | Start it: `sudo mica service start`. See `mica service logs`. |
| `no permission to use mica` | Join the `mica` group, or use sudo. |
| `disk X is attached on Y` | Detach it on node Y. If node Y is gone for good, `mica disk attach X --force` as root. |
| `/dev/ublkbN is in use` | Unmount it, or stop the VM or container first. |
| `upload is behind` in `mica status` | Uploads are slower than writes, or S3 is unreachable. Check the network and `mica service logs`. |
| `taken by another node` | Another node took the disk. The local data is kept as orphaned in `/var/lib/mica/disks/<disk>`. |
| `stopped: local sync failed` | The local SSD failed a sync. The disk is stopped and its local data kept. Check the SSD (`dmesg`). S3 has the last commit. |
| `was attached on X at the same moment` | Two nodes attached the disk at once, and the other won. Check your controller. |
| `mount paths must be inside …` | Mount below `mount_roots`, or add the folder to `mount_roots`. |
| `cannot stop: disks are attached here` | Unmount or detach the disks, or use `--force` and accept that their devices pause. |

## Uninstall

```bash
sudo mica service uninstall          # keeps the config and the local cache
sudo mica service uninstall --purge  # removes them too
```

It refuses while any disk is attached, mounted by mica, or has local data that may not be in S3.

## Per-disk storage budgets

A disk can expose 50 GiB while retaining only 500 MiB of clean data locally: `mica disk create small-cache --size 50G --cache-size 500M --dirty-size 256M`. Reads outside its cache download from S3. The provisioned size bounds block I/O; there is no separate maximum-size setting.

Clean caches are independent per disk. This makes accounting and eviction predictable, at the cost of storing duplicate clean chunks locally when disks share an image. S3 chunks and metadata pages remain shared. Explicit limits live in the disk head; disks without explicit limits use `disk_cache_limit_mib` and `dirty_limit_gib` on the current node. Clones use their own supplied limits or node defaults.

After committing writes, mica copies saved chunks to that disk's clean cache, removes them from the dirty log, and frees their data-file ranges. A reconciler runs every second to trim caches, retry reclamation, and request checkpoints under space pressure. `cache_limit_gib` is the aggregate host safeguard. `local_reserve_mib` is free filesystem space to preserve; if clean-cache eviction cannot restore it, new writes wait while checkpointing continues. This is a periodic safeguard, not a filesystem quota: in-flight I/O, metadata, checkpoint copies, open evicted files, and other applications can consume additional space.

Upgrading does not change the root, page, chunk, or dirty-log formats. Existing heads without limits use the new per-disk default of 1 GiB. The old shared `cache/` remains disposable and is included in host accounting and `mica cache prune`; new reads populate `disk-cache/<disk>/`. Set per-disk limits explicitly before moving to a host whose defaults differ.
