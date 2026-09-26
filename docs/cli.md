# CLI reference

```text
mica status                          Show attached disks
mica disk      ls | create | attach | mount | unmount | detach | resize | delete
mica snapshot  ls | create | delete
mica gc                              Delete data that no disk or snapshot needs
mica cache     status | prune        Show and prune the local chunk cache
mica service   setup | start | stop | restart | status | logs | enable | disable | install | uninstall
```

Every command except `mica service` talks to the daemon on `/run/mica.sock`. Run them as root, or as a member of the `mica` group (see [operations.md](operations.md#permissions)).

**Global option:** `--config <file>` (default `/etc/mica/config.toml`).

**Sizes** take plain bytes or `K`, `M`, `G`, `T` (powers of 1024): `20G`, `512M`.

**Questions.** Commands that delete, or that read many objects from S3, ask first. `-y` skips the question. Without a terminal, a delete needs `-y`.

## status

Shows the disks attached on this node, and the S3 requests since mica started.

```text
$ mica status
DISK     STATE      DEVICE             SIZE    NOT IN S3   NOTE
disk-1   attached   /dev/mica/disk-1   10GiB   14MiB

S3 requests since mica started:
  Class A:   284 PUT, 1 LIST
  Class B:   96 GET, 1 HEAD
  Free:      1 DELETE
  Data:      340MiB downloaded, 1.1GiB uploaded
```

- **NOT IN S3:** local data that the next checkpoint uploads.
- **NOTE:** `upload is behind` when unsaved data is older than 5 minutes; `taken by another node` when another node took the disk.

## disk

### disk ls

Lists all disks in the bucket. Listing the names is one S3 request. The details need three small reads per disk, so it asks first.

```text
$ mica disk ls
Found 3 disks. Their details need 9 reads from S3. Continue? yes
DISK     SIZE    ATTACHED ON     LAST SAVED
disk-1   10GiB   tanmoy-laptop   3 minutes ago
disk-2   20GiB   -               2 days ago
```

| Option | Meaning |
|---|---|
| `-y`, `--yes` | Do not ask. Without a terminal and without `-y`, it prints only the names. |

### disk create

```bash
mica disk create disk-1 --size 20G
mica disk create sandbox-7 --from-snapshot base-image
mica disk create sandbox-8 --from-snapshot base-image --size 40G
```

| Option | Meaning |
|---|---|
| `--size <SIZE>` | Size of an empty disk, or a larger size for a disk from a snapshot |
| `--from-snapshot <SNAPSHOT>` | Start from this snapshot. No data is copied. |

A disk is thin: it uses no space until it is written. Disk names use letters, digits, `-`, `_` and `.`.

### disk attach

Makes `/dev/mica/<disk>` on this node. Use it to give a raw disk to a VM.

```bash
mica disk attach disk-1
```

| Option | Meaning |
|---|---|
| `--force` | Take the disk from another node. Root only. Use it only when that node is down: see [reliability.md](reliability.md#fencing-without-conditional-writes). |

Attaching a disk that is already attached here returns its device. Attaching a disk that is still uploading here waits for the upload.

### disk mount

Attaches and mounts a disk. A disk that has never been written is formatted first.

```bash
mica disk mount disk-1 /mnt/disk1
mica disk mount disk-1 /srv/data --fs xfs --owner www-data
```

| Option | Meaning |
|---|---|
| `--fs <ext4\|xfs>` | Filesystem for a new disk. Default: `ext4`. |
| `--owner <USER[:GROUP]>` | Owner of the disk's top folder, on every mount. Default: the caller, for a new disk only. Only root can give a disk to another user. |
| `--force` | As for `attach`. Root only. |

With `mica.service` installed, the mount is the systemd unit `mica-mount@<disk>`. It comes back after a reboot, and systemd unmounts and uploads it before shutdown. Mounts go only below the folders in `mount_roots` (default `/mnt`, `/srv`, `/media`), with `nosuid` and `nodev`.

### disk unmount

Trims, unmounts and detaches a disk. It returns when S3 has all its data.

```bash
mica disk unmount disk-1
mica disk unmount /mnt/disk1
```

The argument is a disk name or a mount path. `fstrim` runs first, so space freed by deleted files is not uploaded.

### disk detach

Removes the device, uploads all local data, and releases the disk. It refuses while the device is open or mounted.

```bash
mica disk detach disk-1
```

### disk resize

Makes a detached disk larger. Grow the filesystem afterwards (`resize2fs` or `xfs_growfs`).

```bash
mica disk resize disk-1 --size 40G
```

A disk cannot be made smaller.

### disk delete

Deletes a disk. It refuses while the disk is attached on any node.

```bash
mica disk delete disk-1
```

| Option | Meaning |
|---|---|
| `-y`, `--yes` | Do not ask |

Only the disk's head is deleted. Its snapshots, and disks made from them, keep working. `mica gc` frees the space later.

## snapshot

### snapshot ls

```text
$ mica snapshot ls -y
SNAPSHOT     SIZE    FROM DISK   CREATED
base-image   10GiB   builder     2 days ago
```

### snapshot create

Saves the current state of a disk under a name. If the disk is attached here, a checkpoint runs first, so the snapshot has its latest data.

```bash
mica snapshot create disk-1 base-image
```

A snapshot is crash-consistent. For a clean filesystem snapshot, stop writes in the guest first (for example `fsfreeze`).

### snapshot delete

```bash
mica snapshot delete base-image
```

Disks made from the snapshot keep working. `mica gc` frees its space later.

## gc

Deletes chunks and manifests that no disk, snapshot or kept checkpoint needs. It counts first, then asks.

```text
$ mica gc
Kept: 3 disks, 2 snapshots, 14 manifests, 4812 chunks
Garbage: 167 chunks (668MiB) and 30 manifests (2.3MiB)
Delete them? [y/N]
Deleted 167 chunks and 30 manifests, and freed 670.3MiB
```

| Option | Meaning |
|---|---|
| `--grace <TIME>` | Keep objects written within this time: `30s`, `10m`, `2h`, `1d`. Default: `gc_grace_hours` (24 h). Below 1 hour, every disk must be detached. |
| `-y`, `--yes` | Do not ask |

## cache

### cache status

```text
$ mica cache status
Used:                     1.2GiB of 50GiB
Chunks:                   301
Used by attached disks:   68MiB
Unused:                   1.1GiB
```

### cache prune

Deletes cached chunks that no attached disk uses. S3 has them all; a disk that needs one later downloads it again.

| Option | Meaning |
|---|---|
| `-y`, `--yes` | Do not ask |

## service

These commands need root.

| Command | Does |
|---|---|
| `setup` | Asks for the settings, tests the bucket, installs and starts `mica.service`. See [operations.md](operations.md#setup). |
| `setup --use-config-file` | The same, without questions. Fails with a list of missing fields. |
| `start` | Starts mica |
| `stop [--force]` | Stops mica. Refuses while disks are attached: their devices would pause until mica starts again. |
| `restart [--force]` | Restarts mica. Attached disks pause for about a second and keep working. |
| `status` | systemd state, attached disks and S3 requests |
| `logs [-f]` | Logs of mica and its mount units |
| `enable` / `disable` | Start mica at boot, or not |
| `install` | Writes `mica.service` and `mica-mount@.service` |
| `uninstall [--purge]` | Removes the units. Refuses while any disk is attached, mounted, or has local data. `--purge` also removes the config and the local cache. |

## For controllers

A controller can use the same socket as the CLI: send one JSON line, read one JSON line back.

```bash
echo '{"command":"attach","disk":"disk-1","force":false}' | socat - UNIX-CONNECT:/run/mica.sock
{"ok":true,"error":null,"result":{"blank":false,"device":"/dev/mica/disk-1","ublk":"/dev/ublkb2"}}
```

The requests are the `Request` enum in [src/api.rs](../src/api.rs).
