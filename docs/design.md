# Design

This is the full design of mica: formats, rules and the reasons for them. For an overview with diagrams, read [architecture.md](architecture.md) first. For failures, see [reliability.md](reliability.md).

mica makes a VM disk that lives in S3.

A node attaches the disk and gets a local block device. The device is a normal Linux disk. A VM can boot from it, the host can mount it, and a container can use it as a volume.

The node reads data from S3 only when the disk needs it. The node writes changes to its local SSD and uploads them to S3 in the background. After a detach, the disk can attach again on any other node.

## 1. Use cases

**Sandboxes.** A sandbox (a VM or a container) starts on any node. The controller stops it when it is inactive. Later, it starts again on a different node. The disk moves with the sandbox, but the node does not copy the full disk.

**Labs.** A VM starts from an image and must boot fast. The user then installs many packages, which writes many small files. The user leaves, and the controller stops the VM.

**Plain disks.** A disk is a volume for data. The host mounts it at a path, or a container gets it as an extra volume. Later, the disk attaches on another node with the same data.

## 2. Guarantees

1. When a guest FLUSH completes, the data survives a crash of mica or a reboot of the node.
2. While uploads keep up, data older than about 5 minutes is in S3. It survives the loss of the node. Uploads have no deadline, so on a slow link mica shows the disk as **behind**. With `wait_when_behind`, guest writes wait instead, and the bound holds.
3. After a detach, all data is in S3. Nothing is lost.
4. mica never deletes local data until S3 has it.
5. mica detects a second writer, but cannot prevent one without conditional writes. The controller makes sure that one node owns a disk. mica reads a claim back, and checks the marker every minute and at every commit.
6. A failed local sync stops the disk. mica never reports a flush as done when it may not be.

## 3. Architecture

```text
            S3 (R2, Ceph or Garage)
   ┌─────────────────────────────────────┐
   │ chunks      immutable, by hash      │
   │ manifests   immutable, by hash      │
   │ head        current state of a disk │
   │ attached    which node owns a disk  │
   └──────────────────▲──────────────────┘
                      │ GET on miss, PUT on checkpoint
┌─────────────────────┴──────────────────────────────┐
│ mica daemon                                        │
│                                                    │
│  ublk ──> Disk engine ──> chunk map (RAM)          │
│                 │                                  │
│                 ├── dirty chunks   (local SSD)     │
│                 ├── chunk cache    (local SSD)     │
│                 └── checkpoint     (every 3 min)   │
└─────────────────────┬──────────────────────────────┘
                      ▼
            /dev/mica/<disk-id>  ──>  VM, host mount or container
```

The Disk engine does not know about ublk. ublk only turns kernel block requests into calls to `read`, `write`, `flush` and `discard`. We can test the full engine without a block device.

mica uses no conditional S3 writes (`If-Match`, `If-None-Match`). Garage ignores them, and Ceph support depends on the version. mica works the same way on all three stores.

## 4. Chunks

A disk is a list of **chunks**. A chunk is 4 MiB. A 100 GiB disk has 25,600 chunks.

In S3, a chunk is an immutable object. Its key is the SHA-256 of its content. Two equal chunks are one object, so an image and all its clones share the same chunks.

An all-zero chunk is never stored. The map marks it as zero.

## 5. S3 layout

```text
chunks/<first 2 hex>/<chunk hash>     chunk data, immutable
manifests/<manifest hash>             chunk map of a disk, immutable
disks/<disk-id>/head                  current manifest of the disk, overwritten
disks/<disk-id>/attached              owner of the disk, present only while attached
snapshots/<name>                      a named manifest, immutable
```

Each hash is a SHA-256, written as 64 lowercase hex characters:

- **Chunk hash:** SHA-256 of the 4 MiB of chunk data, exactly as the guest wrote it.
- **Manifest hash:** SHA-256 of the full manifest file, header included.
- **`<first 2 hex>`:** the first 2 characters of the chunk hash. It splits the chunks into 256 folders, which keeps the local folder store fast.

In `head` and in snapshots, `"sha256:54b..."` is a manifest hash.

Example:

```text
chunks/3f/3fa9c2...e71b       4 MiB of data whose SHA-256 is 3fa9c2...e71b
manifests/54b0d1...a902       manifest file whose SHA-256 is 54b0d1...a902
```

After each GET, mica computes the SHA-256 of the data and compares it with the key. If they differ, mica rejects the data.

An image is a snapshot. There is no separate image type.

### Manifest

A manifest is a small binary file. It lists every chunk of the disk. It does not point to a parent for data, so a node reads one manifest to know the full disk.

```text
magic        "MICA"
version      u32
chunk_size   u32          4 MiB
disk_size    u64          a multiple of chunk_size
seq          u64          seq of the commit that wrote this manifest
time         u64          Unix time of that commit
parent       [u8; 32]     manifest this one came from (for history and GC only)
disk_id      u16 length, then UTF-8 bytes
chunks       [[u8; 32]; disk_size / chunk_size]    all zero = zero chunk
```

mica rounds the disk size up to a multiple of the chunk size.

A 100 GiB disk has a manifest of about 800 KiB.

Manifests are immutable and keyed by their hash. This gives three benefits:

- Every checkpoint stays a restore point until GC. To list them, start at `head` and follow `parent`.
- A snapshot or clone is a pointer to a manifest. It copies no data.
- All clones of an image share one manifest until they write.

### Head

```json
{
  "manifest": "sha256:54b...",
  "profile": [0, 1, 812, 813, 5120],
  "seq": 42,
  "node": "node-23",
  "time": "2026-09-26T10:00:00Z"
}
```

`seq` increases by one on each commit. `profile` is the read profile (see section 14).

### Attached marker

```json
{
  "node": "node-23",
  "node_id": "<content of /var/lib/mica/node-id>",
  "since": "2026-09-26T09:00:00Z"
}
```

`node_id` is a random ID. mica makes it on its first start. mica does not use `/etc/machine-id`, because hosts made from the same image often share it. If the local SSD is wiped, the ID is lost with the local data. This is correct, because the node no longer has the data of the disk.

### Snapshot

```json
{ "manifest": "sha256:54b...", "profile": [0, 1, 812, 813, 5120] }
```

## 6. Local layout

```text
/var/lib/mica/
  node-id                          random ID of this node
  cache/<first 2 hex>/<chunk hash>   clean chunks, shared by all disks, can be evicted
  disks/<disk-id>/
    state                          status of the disk: attached or orphaned
    dirty/<index>.chunk            chunk the guest changed
    dirty/<index>.frozen           chunk that a checkpoint uploads now
    dirty/<index>.tmp              chunk being made; deleted after a crash
```

A file in `dirty/` is local data that S3 does not have. mica deletes it only after a commit that includes it.

The base manifest is kept in RAM only. After a restart, mica reads `head` from S3 again.

## 7. Chunk map

At attach, mica reads the manifest and makes one entry for each chunk in RAM:

```rust
struct ChunkSlot {
    base: Option<ContentHash>,   // content in the last commit; None = zeros
    working: Option<File>,       // dirty/<index>.chunk
    frozen: Option<File>,        // dirty/<index>.frozen
}
```

The first present one holds the current content: `working`, then `frozen`, then `base`. Each slot has its own lock.

A 100 GiB disk has 25,600 entries. This is small.

## 8. Read path

1. Split the request by chunk.
2. If the chunk has a `.chunk` file, read it.
3. If the chunk has a `.frozen` file, read it.
4. If the base is zero, return zeros.
5. If the base chunk is in the local cache, read it from the cache.
6. If not, GET the chunk from S3. Check its SHA-256. Write it to the cache. Then read it.

When many requests miss the same chunk, mica sends one GET.

## 9. Write path

1. Split the request by chunk.
2. If the chunk has no `.chunk` file, make one as a copy of the current content:
   - From the `.frozen` file, if there is one.
   - From the base chunk in the cache. If the base is not in the cache, GET it from S3 first.
   - As a sparse file of zeros, if the base is zero or the write covers the full chunk.
3. Write the data to the `.chunk` file.
4. Return to the guest. Do not sync.

The copy uses `copy_file_range`. On XFS and btrfs this is a reflink and costs nothing. mica makes the copy as `.tmp`, syncs it, then renames it. Thus a crash never leaves a half-copied `.chunk`.

The first write to a chunk that is not in the cache waits for one GET. Later writes to the chunk do not. If a write covers a full chunk, mica does not fetch the base. `mkfs`, `cp` and `dd` do this often.

## 10. Discard and write zeroes

- A discard over a full chunk makes `.chunk` a sparse file of zeros. The checkpoint stores it as a zero chunk, so nothing is uploaded.
- A discard over part of a chunk does nothing. A discard is only a hint. Zeroing part of a chunk would download it and upload it again, which costs more than it frees.
- Write-zeroes must zero every byte, so it also zeroes parts of chunks.

The device tells the kernel that its discard granularity is one chunk, so trims arrive as whole chunks. `mica disk unmount` runs `fstrim` before it unmounts. Thus space that deleted files freed is not uploaded, and GC frees it in S3.

A disk is thin: `create` writes only the manifest and the head. A chunk uses space only after the guest writes it.

## 11. Flush

mica tells the kernel the device has a volatile write cache. The kernel then sends FLUSH and FUA to mica.

On FLUSH:

1. `fdatasync` each `.chunk` file changed since the last FLUSH.
2. `fsync` the `dirty/` directory if files were made or deleted.
3. Return to the guest.

A FUA write is a write plus `fdatasync` of that file.

A FLUSH never touches S3.

## 12. Checkpoint

A checkpoint uploads the dirty chunks and commits a new manifest. It starts when one of these occurs:

- 3 minutes passed since the last checkpoint started, and at least one chunk is dirty.
- The dirty space on the local SSD reached the high-water mark.
- A detach or snapshot asks for it.

Only one checkpoint runs at a time. An idle disk makes no S3 calls.

Steps:

1. **Freeze.** Rename each `<index>.chunk` to `<index>.frozen`. The guest continues to write. A new write to a frozen chunk first copies the frozen file to a new `<index>.chunk`, then writes.
2. **Upload.** Hash each frozen chunk. PUT it to `chunks/`. Skip all-zero chunks, and chunks already in the cache (S3 has them). Run many PUTs in parallel.
3. **Manifest.** Copy the base manifest. Set the new hashes. PUT it to `manifests/`.
4. **Check owner.** GET `attached`. It must have our `node_id`.
5. **Commit.** PUT the new `head` with `seq + 1` and the current read profile.
6. **Repair.** HEAD every committed chunk. Upload a missing one again from its frozen file. GC can delete a chunk between our upload and our commit; see "Garbage collection".
7. **Clean up.** Move each frozen file into `cache/<hash>` if no new `<index>.chunk` exists. If one exists, delete the frozen file.

If step 4 fails, another node owns the disk. mica stops all commits, fails all I/O on the device and reports an error. mica keeps the local data.

If a step fails for a network error, mica tries again later. The dirty files stay on the local SSD. If the oldest write that is not in S3 is older than 5 minutes, mica shows the disk as **behind**.

A chunk that the guest writes many times, such as the journal, goes up at most once per checkpoint.

### Cost

For one checkpoint: one PUT for each dirty chunk, one PUT for the manifest and one PUT for the head. A 100 GiB disk adds 800 KiB for the manifest.

## 13. Attach and detach

### Attach

1. GET `attached`.
   - If it is missing, PUT ours with a random claim ID. Wait 1 s, then GET it again. If it has another claim ID, another node claimed at the same moment: stop with an error.
   - If it has our `node_id`, continue. mica restarted on this node.
   - If it has another `node_id`, stop with an error. Use `--force` only when the other node is down.
2. GET `head`, then its manifest.
3. Make the chunk map.
4. If `disks/<disk-id>/` exists on this node, load the dirty files into the map.
5. Start the prefetch of the profile.
6. Make the ublk device. Make the link `/dev/mica/<disk-id>` to it.

### Use the device

The device is a normal block device. Use the stable link `/dev/mica/<disk-id>`, because the `ublkbN` number can change from one attach to the next.

- **VM.** Give the device to QEMU, cloud-hypervisor or Firecracker as a raw disk.
- **Host mount.** Run `mount /dev/mica/<disk-id> /some/path`.
- **Container volume.** Mount the device on the host, then bind-mount the path into the container:

  ```bash
  mount /dev/mica/disk-123 /srv/volumes/disk-123
  docker run -v /srv/volumes/disk-123:/data ...
  ```

  Or give the device to the container with `--device /dev/mica/disk-123`, and mount it inside the container.

mica runs as root, because it makes ublk devices.

### Mount and unmount

Two helper commands do attach and mount in one step:

```bash
mica disk mount disk-123 /srv/volumes/disk-123
mica disk unmount disk-123
```

`mica disk mount <disk-id> <path>`:

1. Attach the disk.
2. If the disk has no data yet (all chunks are zero), make an ext4 filesystem on it. Use `--fs xfs` for XFS. mica never formats a disk that has data.
3. Mount the device at the path. Make the path if it does not exist.

A new filesystem has a top folder owned by root. So when mica formats a disk, it gives the top folder to the caller (the sudo user, when run with sudo). `--owner USER[:GROUP]` gives it to another user, on every mount. mica never changes the owner of the files inside. Only root can give a disk to another user.

`mica disk unmount <disk-id | path>`:

1. Unmount the filesystem.
2. Detach the disk, and wait until the upload completes.

For a container, run `mica disk mount` on the host, then bind-mount the path.

When `mica.service` is installed, a mount is a systemd unit, `mica-mount@<disk-id>.service`. The unit attaches and mounts on start, and unmounts and detaches on stop. Its mount path is in `/etc/mica/mounts/<disk-id>.toml`. Thus:

- After a reboot, systemd attaches and mounts the disk again.
- At shutdown, systemd unmounts the disk and uploads its data before it stops mica.
- `systemctl status mica-mount@disk-123` shows one mount. `systemctl list-units 'mica-mount@*'` shows all.

The unit only wants `mica.service`. It does not require it, so a restart of mica does not unmount the disks.

Without the service, `mica disk mount` mounts directly and warns that the mount does not come back after a reboot.

### Detach

Detach always waits. When it succeeds, S3 has all data and the disk is free. A controller that must not block runs detach in the background itself, and still gets the result.

1. Remove the ublk device. If the device is open or mounted, stop with an error. Stop the VM, unmount the filesystem or stop the container first. mica checks this with an `O_EXCL` open of the device.
2. Run checkpoints until one succeeds. Network errors are retried.
3. Delete `disks/<disk-id>/dirty/`. S3 has all of it now.
4. Delete `attached`, if it has our `node_id`.
5. Delete `disks/<disk-id>/`. Keep the chunks in `cache/`.

Step 3 comes before step 4. After a crash, a disk folder without chunks is safe to delete. Chunks without our marker are not safe to upload.

If the client disconnects, the daemon still completes steps 2 to 5. Until then, `attached` stays in S3, and other nodes cannot attach the disk.

### Daemon restart

Devices use ublk user recovery. When the daemon exits, the kernel keeps `/dev/ublkbN` and holds its I/O. The next daemon takes the device over:

1. On SIGTERM, the daemon runs one last checkpoint for each disk (at most 60 s), and exits without deleting its devices.
2. The new daemon finds the disk folder. The `state` file has the ublk device number.
3. If the marker is ours, the new daemon opens the disk from its local files and takes the device over. The kernel sends the held I/O again.

Users of the device see a pause of about one second. No acknowledged write is lost: each one is in a local chunk file. `mica service restart` and a daemon crash both work this way.

If the marker is not ours, nobody will serve the device. mica deletes it, so its users get I/O errors instead of waiting forever.

An attach of a disk that is still uploading on this node waits until the upload is done. An attach of a disk that is already attached on this node returns its device.

### Node restart

After a reboot, the ublk devices are gone. mica finds each `disks/<disk-id>/` on the local SSD. For each disk:

- If the folder has no chunks, S3 has everything. mica deletes our marker, if there is one, and deletes the folder.
- If the marker has our `node_id`, mica runs the rest of the detach. The controller then starts the VM where it wants.
- If the marker is missing or has another `node_id`, another node may own the disk now. mica does not touch S3. mica marks the disk as **orphaned** in `state`, keeps the local data and reports it. An operator deletes the data or copies it out.

### Why this is safe without conditional writes

The controller makes sure that one node owns a disk. The `attached` marker catches mistakes: a missing detach, or a second attach by a script.

One gap stays. A node can pass the check in step 4 of a checkpoint, and then another node can force an attach before the first node commits. Then the first node can write the head one more time. This does not corrupt the disk, because each manifest is a full disk. The head points to one full disk or the other. The first node sees the new marker on its next check and stops.

Rule for the controller: use `--force` only when the old node is powered off or fenced.

## 14. Fast boot: read profile

A cold boot reads many chunks from all over the disk. One GET at a time, this takes tens of seconds.

1. While a disk is attached, mica records the order in which the guest reads chunks the first time. It keeps the first 512 chunks (2 GiB). Reads from the prefetch are not recorded. If they were, the profile would never change.
2. Each checkpoint writes the list into `head`. The list is a few KB. At detach, mica writes `head` also when only the profile changed.
3. At attach, mica GETs the chunks in the profile, 32 at a time, in the recorded order. Reads from the guest go first.

For labs, the image profile comes from its first boot, so every later boot is fast. For sandboxes, the profile comes from the last session, so the working set of the user is ready first.

The chunk cache is shared by all disks on a node. When many VMs on a node use the same image, only the first boot gets the chunks from S3.

## 15. Snapshot and clone

**Snapshot.** If `snapshots/<name>` exists, stop with an error. Run a checkpoint. Then PUT `snapshots/<name>` with the manifest and profile of the new head.

The snapshot is crash-consistent. For a clean filesystem snapshot, the controller freezes the guest filesystem first.

**Clone.** `mica disk create <new-id> --from-snapshot <name>` makes a new disk from a snapshot. PUT `disks/<new-id>/head` with the manifest and profile of the snapshot. No chunks are copied. If the new disk is larger, write a new manifest with zero chunks at the end.

**Resize.** `mica disk resize <disk-id> --size <size>` makes a disk larger. mica adds zero chunks to the end of the manifest and commits it. Then grow the filesystem with `resize2fs` or `xfs_growfs`. mica does not make a disk smaller.

**Delete.** `mica disk delete <disk-id>` deletes the head of a disk. It refuses while the disk is attached on any node. `mica snapshot delete <name>` deletes a snapshot record. Both leave chunks and manifests in S3, because other disks and snapshots may share them. GC frees them later. Disks made from a deleted snapshot keep working.

**List.** `mica disk ls` and `mica snapshot ls` list the names with one S3 request. The details need two or three small reads per item, so they ask first. `-y` skips the question.

**Image.** Make a disk, install the OS, boot it once, detach it and take a snapshot. Clone the snapshot for each VM.

## 16. Garbage collection

Checkpoints leave old chunk versions in S3. Deleted disks and snapshots leave chunks and manifests too. `mica gc` deletes them. Someone runs it by hand, or a controller runs `mica gc -y`.

1. Read the `head` of every disk and the record of every snapshot.
2. For each disk, keep its current manifest and the checkpoints before it, by `parent`, up to `gc_keep_checkpoints` (default 5). For each snapshot, keep its manifest.
3. The live chunks are the chunks of the kept manifests.
4. List `chunks/` and `manifests/`. Delete each object that is not live and was written more than `gc_grace_hours` ago (default 24).
5. Just before each delete, read the object's age again.

If GC cannot read a head, a snapshot or a kept manifest, it stops and deletes nothing.

**Why the grace period.** A checkpoint uploads chunks, then writes the manifest, then the head. GC can read the head between these steps. The new chunks are then not live yet, but they are young, so GC keeps them.

**Rule for checkpoints.** A checkpoint skips the upload of a chunk only when the current manifest of the disk already has it. It uploads any other chunk again, even when S3 may have it. That chunk may be garbage that GC is about to delete, and the new write makes it young.

**Rule for create from a snapshot.** After it writes the new head, create checks that the snapshot still exists. If the snapshot was deleted meanwhile, create deletes the new head and fails.

**One GC at a time.** The daemon runs one GC at a time. Across nodes, GC writes `gc/lock` and refuses to start while a lock younger than 6 hours exists. The lock is not atomic without conditional writes. This is safe: two GCs at the same time each delete only garbage. The lock only stops needless work.

`mica gc` counts the garbage first and asks before it deletes. `-y` deletes without the question.

## 17. Local space

**Cache.** The cache has a size limit. When full, mica deletes the least recently used chunks. It can delete any cache chunk, because S3 has all of them.

`mica cache status` shows how much the cache holds, and how much of it the attached disks use. `mica cache prune` deletes the chunks that no attached disk uses. A pruned chunk is downloaded again if a disk needs it later.

**Dirty space.** Dirty space also has a limit. At the high-water mark, a checkpoint starts. At the limit, new writes wait until a checkpoint frees space. The guest sees a slow disk, not an I/O error.

## 18. Defaults

| Setting | Default |
|---|---|
| Chunk size | 4 MiB, stored in the manifest |
| Checkpoint interval | 3 minutes |
| Maximum data loss | About 5 minutes while uploads keep up (`max_unsaved_minutes`) |
| Wait when behind | Off (`wait_when_behind`) |
| Marker check | Every minute, and at every commit |
| Profile length | 512 chunks |
| Prefetch parallelism | 32 GETs |
| Upload parallelism | 16 PUTs |
| Dirty space high-water mark | 50% of the dirty limit |

After the first tests, review the chunk size. Measure two things:

- The boot time with a real image. Smaller chunks make cold boots faster.
- The upload size after a large package install. A small write uploads the full chunk, so smaller chunks upload less.

## 19. Code layout

See [src/code-map.md](../src/code-map.md) for each file, the order to read them, and the main flows.

### Daemon

mica runs as a systemd service. The CLI and the controller send commands to it on the Unix socket `/run/mica.sock`.

The config file is `/etc/mica/config.toml`. It holds the S3 endpoint, bucket and keys, the local paths and the limits.

### Setup and service

`mica service setup` asks for the settings. Each prompt shows the current value, and Enter keeps it. `mica service setup --use-config-file` asks nothing: it takes the config file as it is, and fails with a list of the missing fields. Both then:

1. Load `ublk_drv`, and make it load at boot.
2. Create the `mica` group.
3. Check for reflink support on the data folder.
4. Test the bucket with a put, a get and a delete.
5. Save the config with mode 0600.
6. Copy the binary to `/usr/local/bin/mica`.
7. Install `mica.service` and `mica-mount@.service`, enable and start mica. A running mica restarts.
8. Offer to add the user who ran sudo to the `mica` group.

Setup reads only the file, not the environment, because a systemd service does not get the shell environment.

`mica service install | uninstall | enable | disable | start | stop | restart | status | logs` manage the service:

- `stop` refuses while disks are attached, because their devices would pause until mica starts again.
- `restart` is safe with attached disks.
- `uninstall` refuses while any disk is attached, mounted by mica, or has local data.

### The mica group

The daemon runs as root. Its socket is `root:mica` with mode 0660, so members of the `mica` group use mica without sudo. The CLI does not need the config file, which only root can read.

A member must not become root through mica:

- `mica disk mount` goes through the daemon, which starts `mica-mount@<disk>` as root. The CLI of a member never mounts.
- Mounts go only below `mount_roots` (default `/mnt`, `/srv`, `/media`). Paths with `..`, and symlinks that point outside, are refused.
- Mounts use `nosuid` and `nodev`.
- `--force` needs root, because taking a disk from another node can lose its data.

`mica.service` is `Type=notify`: `systemctl start` returns when mica answers. The daemon raises its open-file limit, because it keeps one open file per dirty chunk.

The engine (`Disk` in `src/disk.rs`) has five calls: `read`, `write`, `discard`, `write_zeroes` and `flush`. The S3 wrapper (`Bucket` in `src/bucket.rs`) has `get`, `put`, `delete` and `exists`, plus listings and ranged reads for `ls` and GC. It counts every request.

### Crates

- `libublk` for the ublk frontend.
- `tokio` for the engine and S3 calls.
- `object_store` for S3 with a custom endpoint.
- `smol` for the executor that `libublk` drives on each queue thread.
- `io-uring`, for the control ring that `libublk` needs on each thread.
- `dialoguer`, for the setup prompts.
- `sha2`, `bytes`, `serde_json`, `toml`, `clap`, `humantime`.

`libublk` runs its own io_uring loop on each queue thread. Each request is spawned on the tokio runtime, and the queue thread waits on its join handle.

The daemon runs in its own mount namespace. The libublk README requires this: a mount of the device inside the namespace of the daemon can deadlock when the daemon exits.

## 20. Not built yet

- A Docker volume plugin or a Kubernetes CSI driver. For v1, use `mica disk mount` on the host and bind-mount the path.
