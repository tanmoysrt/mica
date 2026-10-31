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
5. With conditional writes, an attach is an atomic claim: two nodes cannot both claim a free disk. Without them, mica detects a second writer, but cannot prevent one. The controller makes sure that one node owns a disk. mica checks the marker every minute and at every commit.
6. A failed local sync stops the disk. mica never reports a flush as done when it may not be.

## 3. Architecture

```text
            S3 (R2, Ceph or Garage)
   ┌─────────────────────────────────────┐
   │ chunks      immutable, by hash      │
   │ pages       immutable, by hash      │
   │ manifests   immutable, by hash      │
   │ head        current state of a disk │
   │ attached    which node owns a disk  │
   └──────────────────▲──────────────────┘
                      │ GET on miss, PUT on checkpoint
┌─────────────────────┴──────────────────────────────┐
│ mica daemon                                        │
│                                                    │
│  ublk ──> Disk engine ──> chunk map (RAM, by page) │
│                 │                                  │
│                 ├── dirty chunks   (local SSD)     │
│                 ├── chunk cache    (local SSD)     │
│                 ├── prefetch       (in background) │
│                 └── checkpoint     (every 30 s)    │
└─────────────────────┬──────────────────────────────┘
                      ▼
            /dev/mica/<disk-id>  ──>  VM, host mount or container
```

The Disk engine does not know about ublk. ublk only turns kernel block requests into calls to `read`, `write`, `flush` and `discard`. We can test the full engine without a block device.

mica needs no conditional S3 writes (`If-Match`, `If-None-Match`). Garage ignores them, and Ceph support depends on the version. When the store supports `If-None-Match` (R2, AWS S3, MinIO, RustFS), set `s3.conditional_writes = true`. Then an attach claims the disk with one atomic create, without a wait. See section 13.

## 4. Chunks

A disk is a list of **chunks** of one size. The size is set when the disk is made: a power of two from 64 KiB to 4 MiB, 256 KiB by default. A 100 GiB disk of 256 KiB chunks has 409,600 chunks.

In S3, a chunk is an immutable object. Its key is the SHA-256 of its content. Two equal chunks are one object, so an image and all its clones share the same chunks.

An all-zero chunk is never stored. The map marks it as zero.

The chunk size is a trade. A small chunk makes a cold read cheap: a 4 KiB read downloads one chunk. A small chunk also uploads less after a small write. A large chunk needs fewer S3 requests, and fewer local files, for the same data. 256 KiB fills a 100 MiB/s link with one GET at a time when the round trip is short.

## 5. S3 layout

```text
chunks/<first 2 hex>/<chunk hash>     chunk data, compressed, immutable
pages/<first 2 hex>/<page hash>       1,024 chunk hashes, compressed, immutable
manifests/<root hash>                 root of a disk: its page hashes, immutable
disks/<disk-id>/head                  current root of the disk, overwritten
disks/<disk-id>/attached              owner of the disk, present only while attached
snapshots/<name>                      a named root, immutable
gc/lock                               present only while a GC runs
gc/last                               the last GC that deleted objects, overwritten
```

Each hash is a SHA-256, written as 64 lowercase hex characters:

- **Chunk hash:** SHA-256 of the chunk data, exactly as the guest wrote it, before compression.
- **Page hash:** SHA-256 of the page file, before compression.
- **Root hash:** SHA-256 of the full root file, header included.
- **`<first 2 hex>`:** the first 2 characters of the hash. It splits the objects into 256 folders, which keeps the local folder store fast.

In `head` and in snapshots, `"sha256:54b..."` is a root hash.

Example:

```text
chunks/3f/3fa9c2...e71b       a chunk whose plain data has the SHA-256 3fa9c2...e71b
pages/81/81c0aa...04d2        a page whose plain file has the SHA-256 81c0aa...04d2
manifests/54b0d1...a902       a root file whose SHA-256 is 54b0d1...a902
```

**Compression.** A chunk or page object is one zstd frame: level 1 for chunks, level 3 for pages. mica decompresses each object, then computes the SHA-256 of the plain data and compares it with the key. If they differ, mica rejects the data. Random data grows by less than 0.1%. System files and memory pages shrink to a half or a third. The root is not compressed, so `ls` can read the disk size with a ranged GET.

An image is a snapshot. There is no separate image type.

### Root

A root is a small binary file. It lists the hash of every page of the disk. It does not point to a parent for data, so a node reads one root to know the full disk.

```text
magic        "MICA"
version      u32          2
chunk_size   u32          bytes, a power of two from 64 KiB to 4 MiB
disk_size    u64          a multiple of chunk_size
seq          u64          seq of the commit that wrote this root
time         u64          Unix time of that commit
parent       [u8; 32]     root this one came from (for history and GC only)
disk_id      u16 length, then UTF-8 bytes
pages        [[u8; 32]; ceil(disk_size / chunk_size / 1024)]    all zero = zero page
```

mica rounds the disk size up to a multiple of the chunk size. A 100 GiB disk of 256 KiB chunks has a root of about 12.5 KiB.

### Page

```text
magic        "MPAG"
count        u32          1024
chunks       [[u8; 32]; 1024]    all zero = zero chunk
```

Page `p` lists chunks `1024 × p` to `1024 × p + 1023`. The last page of a disk lists zero chunks past the end of the disk. A page of zero chunks is not stored: the root has a zero hash for it.

Roots and pages are immutable and keyed by their hash. This gives four benefits:

- Every checkpoint stays a restore point until GC. To list them, start at `head` and follow `parent`.
- A snapshot or clone is a pointer to a root. It copies no data.
- A commit writes only the pages it changes. All clones of an image share its pages until they write.
- A node reads a page only when the guest uses that part of the disk.

### Head

```json
{
  "manifest": "sha256:54b...",
  "profile": [0, 1, 812, 813, 81920],
  "seq": 42,
  "node": "node-23",
  "time": "2026-09-26T10:00:00Z"
}
```

`manifest` is the root hash. `seq` increases by one on each commit. `profile` is the read profile (see section 14).

### Attached marker

```json
{
  "node": "node-23",
  "node_id": "<content of /var/lib/mica/node-id>",
  "since": "2026-09-26T09:00:00Z",
  "claim_id": "<random, new for each attach>"
}
```

`node_id` is a random ID. mica makes it on its first start. mica does not use `/etc/machine-id`, because hosts made from the same image often share it. If the local SSD is wiped, the ID is lost with the local data. This is correct, because the node no longer has the data of the disk.

### Snapshot

```json
{ "manifest": "sha256:54b...", "profile": [0, 1, 812, 813, 81920], "disk": "builder", "time": "2026-09-26T10:00:00Z" }
```

### Last GC

```json
{ "run": "<random, new for each GC>", "node": "node-23", "finished": "2026-09-26T11:00:00Z" }
```

A GC writes `gc/last` after its last delete, and before it deletes `gc/lock`. Section 16 explains how checkpoints use it.

## 6. Local layout

```text
/var/lib/mica/
  node-id                          random ID of this node
  disk-cache/<disk>/<xx>/<hash>     clean chunks, independently limited per disk
  pages/<first 2 hex>/<page hash>    pages, not compressed, shared by all disks
  disks/<disk-id>/
    state                          status of the disk: attached or orphaned
    dirty/data-<n>                 sparse data files, 1 GiB of the disk each: local chunks at their disk offset
    dirty/log-a, dirty/log-b       the dirty log: the local chunks
    dirty/hold                     sparse: held copies of chunks that the running checkpoint uploads
```

Attach creates the current data files. Writes, FLUSH and checkpoints do not create files. Online growth can add data files, extend the held-copy file and remap the recent bitmap; it syncs the directory before publishing the larger size.

**The dirty log** has two files, used in turn. Each starts with a header: a magic, a generation, the list of chunks and a CRC. A FLUSH appends records, each with its own CRC. A rewrite writes the other file with the next generation and syncs it before it counts. After a crash, mica takes the whole file with the higher generation, up to its first bad record. A torn rewrite fails its CRC, so the older file counts.

**`recent`** is `/run/mica/sessions/<data dir>/<disk-id>.recent`: a page with the boot ID and a random ID, then one bit per chunk, mapped into memory. A chunk's bit is set when it becomes local and the log does not list it; a log rewrite stores each word anew. `/run` is a tmpfs: it survives a crash of mica, and a reboot clears it, like the page cache. mica trusts the bitmap only with the same boot ID, and only for a data folder whose log existed already. A detach deletes it. A bit set in memory costs no system call and no writeback, which matters: an append to a file for each new local chunk cut parallel writes from 1,651 to 421 MiB/s on btrfs.

**At open**, the local chunks are those that the log lists, and those that a trusted `recent` lists. mica punches out every other range of the data files: after a crash, such a range can hold writes that never became local. Thus every range that is not local is a hole, and reads as zeros.

An unsaved local chunk is local data that S3 does not have. mica frees it only after a commit that includes it.

The root is kept in RAM only. After a restart, mica reads `head` from S3 again.

A cache file is written without a sync. After a crash it can be torn, so mica checks the hash of each cache file that it finds at start, before its first read. A torn file is deleted and downloaded again. A page file has the same rule: mica checks its hash each time it reads it.

## 7. Chunk map

At attach, mica reads the root. It makes the entries of one page, one for each of its 1,024 chunks, when the guest first uses a chunk in that page:

```rust
struct ChunkSlot {
    base: Option<ContentHash>,   // content in the last commit; None = zeros
    local: bool,                 // the data file holds the current content
    unsaved: bool,               // the local content may differ from base
    staged: bool,                // read by the running checkpoint
    changed: bool,               // written since that read
    held: bool,                  // a copy of the staged content is kept
    verify: bool,                // found in the log after a restart
}
```

When `local` is set, the data file holds the current content; else `base` does, or zeros. Each slot has its own lock.

The node keeps loaded pages in RAM and in `pages/` on the local SSD. All disks of the node share them, so clones of one image load each page once.

The ublk queue thread never waits for a page. If the page of a chunk is not loaded, the request goes to the engine, which loads the page.

mica also keeps the set of unsaved chunks. A checkpoint works on this set, not on the full map.

## 8. Read path

1. Split the request by chunk.
2. If the chunk is local, read it from the data file.
3. If the base is zero, return zeros.
4. If the base chunk is in the local cache, read it from the cache.
5. If not, GET the chunk from S3. Decompress it, and check its SHA-256. Write it to the cache, without a sync. Then read it.

A read of local data holds a **read pass** from before it picks its sources until it ends. mica frees the space of a chunk that left the data files only after every pass that was open at that moment ends. Reads from the cache or S3 come after the local parts, without the pass, so a slow download never holds up the freeing of space.

When many requests miss the same chunk, mica sends one GET. Section 14 explains read-ahead and the order of downloads.

## 9. Write path

1. Split the request by chunk.
2. Lock the chunk.
3. If the chunk is not local, put its current content into the data file: copy the base chunk from the cache (GET it from S3 first if needed). Skip this if the base is zero, because the range is a hole, or if the write covers the full chunk.
4. If the running checkpoint read the chunk and no GC check has proved its upload safe yet, keep a copy of its content (see section 12).
5. Mark the chunk unsaved, and write the data into the data file.
6. Mark the data file for the next FLUSH. If the log does not list the chunk, set its bit in `recent`, and add it to the chunks that the next FLUSH logs.
7. Return to the guest. Do not sync.

The copy uses `copy_file_range`. On XFS and btrfs this is a reflink and costs nothing. On other filesystems it is a copy of one chunk.

The order matters. A chunk is listed only after its content is in the data file, and its data file is marked before it is listed. So a FLUSH that logs the chunk also syncs its content.

The first write to a chunk that is not in the cache waits for one GET. Later writes to the chunk do not. If a write covers a full chunk, mica does not fetch the base. `mkfs`, `cp` and `dd` do this often.

## 10. Discard and write zeroes

- A discard over a full chunk punches out its range of the data file. The checkpoint stores it as a zero chunk, so nothing is uploaded.
- A discard over part of a chunk does nothing. A discard is only a hint. Zeroing part of a chunk would download it and upload it again, which costs more than it frees.
- Write-zeroes must zero every byte, so it also zeroes parts of chunks.

The device tells the kernel that its discard granularity is one chunk, so trims arrive as whole chunks. `mica disk unmount` runs `fstrim` before it unmounts. Thus space that deleted files freed is not uploaded, and GC frees it in S3.

A disk is thin: `create` writes only the root and the head. A chunk uses space only after the guest writes it.

## 11. Flush

mica tells the kernel the device has a volatile write cache. The kernel then sends FLUSH and FUA to mica.

On FLUSH:

1. Take the chunks that the log does not list yet, then the data files written since the last FLUSH.
2. `fdatasync` those data files, at the same time.
3. Append the chunks to the dirty log, and `fdatasync` the log.
4. Return to the guest.

Step 1 takes the chunks first: a chunk in that list marked its data file before it was listed, so step 2 syncs its content. Step 3 comes after step 2, so the durable log never lists a chunk whose content a power loss can lose. Most FLUSHes skip step 3: a chunk stays in the log while it is local.

One FLUSH runs at a time. mica counts the writes done, and the writes that a FLUSH made durable. A FLUSH that finds its writes durable already returns at once, so FLUSHes that wait for each other share one sync.

A FUA write is a write and then the same steps for only its own chunks and data files, in the engine, under the same FLUSH lock. The queue thread never syncs. Linux reports a writeback error only once to each open file, so two syncs at the same time could each miss a failure that the other saw. With one FLUSH at a time, a failed sync stops the disk before the next sync can succeed.

A data file written before a crash is in the list of the next FLUSH, because it can hold writes that the guest did not flush.

Linux starts writeback late: after 10% of free memory is dirty, or after 30 s. A guest that writes 2 GiB and then flushes would wait for all 2 GiB in that one FLUSH, 16 s on a 125 MiB/s disk. So after each 16 MiB of writes, mica starts writeback of the unsynced data files with `sync_file_range(SYNC_FILE_RANGE_WRITE)`. That call does not wait, and does not take the writeback error that the FLUSH `fdatasync` must see. In the lab, the longest FLUSH after a 2 GiB write fell from 2.5–3.1 s to 0.3–0.8 s.

A FLUSH never touches S3.

## 12. Checkpoint

A checkpoint uploads the dirty chunks and commits a new root. It starts when one of these occurs:

- `checkpoint_interval_secs` (30 s) passed since the last checkpoint started, and at least one chunk is dirty.
- `checkpoint_after_mib` (512 MiB) of dirty data waits, or the dirty space reached half of its limit.
- A detach, a snapshot or `mica disk checkpoint` asks for it.

Only one checkpoint runs at a time. An idle disk makes no S3 calls. While the guest writes much data, checkpoints run one after the other, so the upload runs while the guest writes.

Steps:

1. **List.** Take the set of unsaved chunks. Chunks that the guest changes later go to the next checkpoint. Thus a busy writer cannot make a checkpoint last forever.
2. **Read GC activity.** HEAD `gc/lock`, then GET `gc/last`.
3. **Stage and upload**, for each listed chunk, up to 64 MiB of chunks at the same time:
   1. Read the chunk into memory under its lock, and mark it staged. The guest goes on writing it in place.
   2. Hash it. Compress it, and PUT it to `chunks/`. Skip all-zero chunks, and a chunk equal to its `base`.
   
   Meanwhile, once a second, read GC activity. If it is the same as in step 2, and no GC runs, the uploads that ended before this read are safe (see below): their chunks leave the stage. Once a GC shows up, every chunk stays staged to the end.
4. **Pages.** For each page with a changed chunk, copy the committed page, set the new hashes, and PUT it to `pages/`.
5. **Root.** Copy the committed root. Set the new page hashes. PUT it to `manifests/`.
6. **Check owner.** GET `attached`. It must have our `node_id`.
7. **Repair.** Read GC activity again. If a GC ran during the checkpoint, wait until it ends (at most 10 minutes; if it still runs, the checkpoint fails and is retried). Then HEAD every new page and chunk, and upload a missing one again, then look again. A chunk comes from its stage or held copy. If the copy is gone, the checkpoint fails and is retried.
8. **Commit.** PUT the new `head` with `seq + 1` and the current read profile. Then read GC activity once more, and repair again if a GC started since step 7. This covers a checkpoint longer than the grace period.
9. **Finish.** Each chunk that the guest did not write since its stage is saved. Drop held copies. Saved chunks stay in the data files up to the disk's cache budget. Over it, copy the oldest into the disk's clean cache, rewrite the log without them, wait for read passes, then punch out their ranges. The reconciler retries incomplete reclamation.

**Why an upload can be safe early.** GC deletes an object only if it was older than the grace period when GC checked it, just before the delete. A PUT makes the age new. So GC can delete our upload only if that GC was running when the PUT ended. A GC writes `gc/lock` before it lists anything. Thus if a read of GC activity after the PUT shows no lock and the same `gc/last`, no GC was running when the PUT ended, and none ran in between.

**Held copies.** The first write to a staged chunk keeps a copy of the staged content: in memory, up to 64 MiB for each disk, then in the `hold` file, a reflink where the filesystem can. The repair needs it only if a GC deleted the upload. Since a chunk leaves the stage about a second after its upload, few writes need a copy.

A **background checkpoint** (from the timer or the dirty limit) also leaves out the chunks written in the last second. The guest would write them again soon. When unsaved data reaches half of its limit, or the disk is behind, it takes all chunks. A detach, a snapshot and `mica disk checkpoint` always take all chunks.

When no chunk changed, steps 3 to 5 do nothing, and the root stays. Step 7 writes only the new read profile.

**After a restart**, every chunk that the log lists is local, and counts as unsaved until the next checkpoint reads it. If its hash equals the committed hash, nothing is uploaded. These chunks do not count toward the limit of unsaved data: most of them are saved.

If step 6 fails, another node owns the disk. mica stops all commits, fails all I/O on the device and reports an error. mica keeps the local data.

If a step fails for a network error, mica tries again later. The chunks stay unsaved on the local SSD. If the oldest write that is not in S3 is older than 5 minutes, mica shows the disk as **behind**.

A chunk that the guest writes many times, such as the journal, goes up at most once per checkpoint.

### Cost

For one checkpoint: one PUT for each dirty chunk, one PUT for each changed page, one PUT for the root and one PUT for the head. Also six small requests for the GC activity and one GET for the marker. A checkpoint of a 4 KiB write to a 100 GiB disk uploads one chunk, one page of 32 KiB and a root of 12.5 KiB, both before compression.

## 13. Attach and detach

### Attach

1. Claim `attached`:
   - With `s3.conditional_writes`, PUT ours with `If-None-Match: *`. If the PUT succeeds, the disk is ours. If a marker exists, GET it: with our `node_id`, continue (mica restarted on this node); with another `node_id`, stop with an error.
   - Without conditional writes, GET `attached`. If it is missing, PUT ours with a random claim ID. Wait 1 s, then GET it again. If it has another claim ID, another node claimed at the same moment: stop with an error. If it has our `node_id`, continue. If it has another `node_id`, stop with an error.
   - Use `--force` only when the other node is down. It writes the marker without a condition.
2. GET `head`, then its root. Without conditional writes, this runs during the 1 s wait, after our marker is written.
3. Open `disks/<disk-id>/dirty/`. Each chunk that the dirty log lists is local, with its page loaded.
4. Make the ublk device. Make the link `/dev/mica/<disk-id>` to it.
5. Start the checkpoint loop, the marker watcher and the prefetch (see "Fast boot").

Each disk has its own attach lock. The list of attached disks is locked only for short updates. Thus different disks attach at the same time, and status calls do not wait. Two attaches of the same disk wait for each other, and the second gets the device of the first.

With conditional writes, an attach takes three small requests and the start of the device: about 25 ms with a round trip of 1 ms. Without them, it takes about 1 s.

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
3. Copy the local chunks into `disk-cache/<disk>/`, a reflink where the filesystem can. Then delete `disks/<disk-id>/dirty/`. S3 has all of it now.
4. Delete `attached`, if it has our `node_id`.
5. Delete `disks/<disk-id>/`. Keep the chunks in `disk-cache/<disk>/`.

Step 3 comes before step 4. After a crash, a disk folder without chunks is safe to delete. Chunks without our marker are not safe to upload.

If the client disconnects, the daemon still completes steps 2 to 5. Until then, `attached` stays in S3, and other nodes cannot attach the disk.

### Daemon restart

Devices use ublk user recovery. When the daemon exits, the kernel keeps `/dev/ublkbN` and holds its I/O. The next daemon takes the device over:

1. On SIGTERM, the daemon runs one last checkpoint for each disk (at most 60 s), and exits without deleting its devices.
2. The new daemon finds the disk folder. The `state` file has the ublk device number.
3. If the marker is ours, the new daemon opens the disk from its local files and takes the device over. The kernel sends the held I/O again.

Users of the device see a pause of about one second. No acknowledged write is lost: the page cache still has each one, and the dirty log or `recent` lists its chunk. `mica service restart` and a daemon crash both work this way.

If the marker is not ours, nobody will serve the device. mica deletes it, so its users get I/O errors instead of waiting forever.

An attach of a disk that is still uploading on this node waits until the upload is done. An attach of a disk that is already attached on this node returns its device.

### Node restart

After a reboot, the ublk devices are gone. mica finds each `disks/<disk-id>/` on the local SSD. For each disk:

- If the folder has no chunks, S3 has everything. mica deletes our marker, if there is one, and deletes the folder.
- If the marker has our `node_id`, mica runs the rest of the detach. The controller then starts the VM where it wants.
- If the marker is missing or has another `node_id`, another node may own the disk now. mica does not touch S3. mica marks the disk as **orphaned** in `state`, keeps the local data and reports it. An operator deletes the data or copies it out.

### Why this is safe without conditional writes

The controller makes sure that one node owns a disk. The `attached` marker catches mistakes: a missing detach, or a second attach by a script.

One gap stays, also with conditional writes. A node can pass the owner check of a checkpoint, and then another node can force an attach before the first node commits. Then the first node can write the head one more time. This does not corrupt the disk, because each root is a full disk. The head points to one full disk or the other. The first node sees the new marker on its next check and stops.

Rule for the controller: use `--force` only when the old node is powered off or fenced.

## 14. Fast boot: prefetch, read-ahead and fill

A cold boot reads many chunks from all over the disk. One GET at a time, this is slow. mica downloads chunks before the guest asks for them, in three ways.

**Read profile.**

1. While a disk is attached, mica records the order in which the guest reads chunks the first time. It keeps the first 64 MiB of chunks: 256 chunks of 256 KiB. Reads from the prefetch are not recorded. If they were, the profile would never change.
2. Each checkpoint writes the list into `head`. The list is a few KB. At detach, mica writes `head` also when only the profile changed.
3. At attach, mica GETs the chunks of the profile, in the recorded order, 8 at a time. `--prefetch <chunks>` limits it to the first chunks, `--prefetch-parallel` changes the 8, and `prefetch_on_attach = false` turns it off.

For labs, the image profile comes from its first boot, so every later boot is fast. For sandboxes, the profile comes from the last session, so the working set of the user is ready first.

**Read-ahead.** When reads continue each other, mica fetches the chunks after them: 1 MiB ahead at first, and twice as much each time, up to 8 MiB. At most 64 chunks of one disk wait for read-ahead. Past that, new read-ahead is dropped, because it is only a hint.

**Background fill.** After the profile, mica downloads every other chunk of the last commit, in disk order. Then all reads are local, also if the link goes down. The fill stops when the cache is 90% full. `--no-fill` and `background_fill = false` turn it off.

**Priority.** The work ahead must never slow the guest. So the node has three classes of download:

| Class | Who | Rule |
|---|---|---|
| Read | the guest | up to 64 downloads on the node |
| Likely | read-ahead and the profile | up to 16; starts only while no read waits for a download slot, and while fewer than 8 reads download |
| Fill | background fill | up to 32 on the node, 8 MiB in flight for each disk; starts only after 250 ms without a new read download, and while no read and no likely download is in flight |

A fetch takes its slots before the download lock of its chunk. Thus a read of the same chunk never waits behind a fetch that waits for its turn. Every fetch stops when its disk closes.

Clean caches are independent per disk, so one disk cannot consume another disk's cache budget. Metadata pages and S3 objects remain shared.

## 15. Snapshot and clone

**Snapshot.** If `snapshots/<name>` exists, stop with an error. Run a checkpoint. Then PUT `snapshots/<name>` with the root and profile of the new head.

The snapshot is crash-consistent. For a clean filesystem snapshot, the controller freezes the guest filesystem first.

**Clone.** `mica disk create <new-id> --from-snapshot <name>` makes a new disk from a snapshot. PUT `disks/<new-id>/head` with the root and profile of the snapshot. No pages or chunks are copied. If the new disk is larger, write a new root with zero pages at the end. The clone has the chunk size of the snapshot.

**Resize.** `mica disk resize <disk-id> --size <size>` makes a disk larger. mica adds zero pages to the end of the root and commits it. The last page already lists zero chunks past the old end, so it does not change. Then grow the filesystem with `resize2fs` or `xfs_growfs`. mica does not make a disk smaller.

**Delete.** `mica disk delete <disk-id>` deletes the head of a disk. It refuses while the disk is attached on any node. `mica snapshot delete <name>` deletes a snapshot record. Both leave chunks, pages and roots in S3, because other disks and snapshots may share them. GC frees them later. Disks made from a deleted snapshot keep working.

**List.** `mica disk ls` and `mica snapshot ls` list the names with one S3 request. The details need two or three small reads per item, so they ask first. `-y` skips the question.

**Image.** Make a disk, install the OS, boot it once, detach it and take a snapshot. Clone the snapshot for each VM.

## 16. Garbage collection

Checkpoints leave old chunk and page versions in S3. Deleted disks and snapshots leave chunks, pages and roots too. `mica gc` deletes them. Someone runs it by hand, or a controller runs `mica gc -y`.

1. Read the `head` of every disk and the record of every snapshot.
2. For each disk, keep its current root and the checkpoints before it, by `parent`, up to `gc_keep_checkpoints` (default 5). For each snapshot, keep its root.
3. The live pages are the pages of the kept roots. GC reads each live page once. The live chunks are the chunks of the live pages.
4. List `chunks/`, `pages/` and `manifests/`. Delete each object that is not live and was written more than `gc_grace_hours` ago (default 24): chunks first, then pages, then roots.
5. Just before each delete, read the object's age again.
6. After the last delete, PUT `gc/last` with a new run ID. Then delete `gc/lock`.

If GC cannot read a head, a snapshot, a kept root or a live page, it stops and deletes nothing.

**Why the grace period.** A checkpoint uploads chunks and pages, then writes the root, then the head. GC can read the head between these steps. The new objects are then not live yet, but they are young, so GC keeps them.

**Rule for checkpoints.** A checkpoint skips the upload of a chunk only when the current root of the disk has it at the same place. It uploads any other chunk again, even when S3 may have it. That chunk may be garbage that GC is about to delete, and the new write makes it young. Pages are always uploaded.

**Why the repair step.** One race stays: GC reads the age of an old object, then a checkpoint uploads an object with the same hash, then GC deletes it. So a checkpoint must check its objects after such a GC. This needs a GC that ran during the checkpoint. The checkpoint sees one in two ways:

- `gc/lock` is present at the end: a GC runs now. The checkpoint waits for it to end, up to 10 minutes.
- `gc/last` changed: a GC ended during the checkpoint.

The checkpoint reads `gc/lock` before `gc/last`, never at the same time. A GC writes `gc/last` before it deletes its lock, so a GC that ends between the two reads shows in `gc/last`. Only then does the checkpoint HEAD each committed page and chunk. v0.1 did this for every checkpoint: one HEAD for each chunk.

**Rule for create from a snapshot.** After it writes the new head, create checks that the snapshot still exists. If the snapshot was deleted meanwhile, create deletes the new head and fails.

**One GC at a time.** The daemon runs one GC at a time. Across nodes, GC writes `gc/lock` and refuses to start while a lock younger than 6 hours exists. The lock is not atomic without conditional writes. This is safe: two GCs at the same time each delete only garbage. The lock only stops needless work.

`mica gc` counts the garbage first and asks before it deletes. `-y` deletes without the question.

## 17. Local space

**Cache.** The cache has a size limit. When full, mica deletes the least recently used chunks. It can delete any cache chunk, because S3 has all of them.

`mica cache status` shows how much the cache holds, and how much of it the attached disks use. `mica cache prune` deletes the chunks that no attached disk uses. A pruned chunk is downloaded again if a disk needs it later.

**Dirty space.** `dirty_limit_gib` limits unsaved data. At half of the limit, or at `checkpoint_after_mib`, a checkpoint starts. At the limit, new writes wait until a checkpoint saves chunks. The guest sees a slow disk, not an I/O error. Saved chunks move to the separately budgeted clean cache after commit, and their data-file ranges are freed.

## 18. Defaults

| Setting | Default |
|---|---|
| Chunk size | 256 KiB for a new disk (`--chunk-size`), stored in the root |
| Checkpoint interval | 30 s (`checkpoint_interval_secs`) |
| Early checkpoint | at 512 MiB of dirty data (`checkpoint_after_mib`) |
| Maximum data loss | About 5 minutes while uploads keep up (`max_unsaved_minutes`) |
| Wait when behind | Off (`wait_when_behind`) |
| Marker check | Every minute, and at every commit |
| Conditional claim | Off (`s3.conditional_writes`) |
| Profile length | 64 MiB of chunks |
| Prefetch on attach | The full profile (`prefetch_on_attach`) |
| Background fill | On (`background_fill`) |
| Downloads | 64 for reads, 16 for likely reads, 32 for the fill (8 MiB for each disk) |
| Upload parallelism | 64 MiB of chunks: 64 PUTs of 256 KiB, 16 of 4 MiB |
| Compression | zstd level 1 for chunks, level 3 for pages |

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

`mica.service` is `Type=notify`: `systemctl start` returns when mica answers. The daemon raises its open-file limit, because it keeps one data file open per GiB of each attached disk.

The engine (`Disk` in `src/disk.rs`) has five calls: `read`, `write`, `discard`, `write_zeroes` and `flush`. The S3 wrapper (`Bucket` in `src/bucket.rs`) has `get`, `put`, `delete` and `exists`, plus listings and ranged reads for `ls` and GC. It counts every request.

### Crates

- `libublk` for the ublk frontend.
- `tokio` for the engine and S3 calls.
- `object_store` for S3 with a custom endpoint.
- `smol` for the executor that `libublk` drives on each queue thread.
- `io-uring`, for the control ring that `libublk` needs on each thread.
- `dialoguer`, for the setup prompts.
- `zstd`, for chunk and page objects.
- `sha2`, `bytes`, `serde_json`, `toml`, `clap`, `humantime`.

`libublk` runs its own io_uring loop on each queue thread. Each request is spawned on the tokio runtime, and the queue thread waits on its join handle.

The daemon runs in its own mount namespace. The libublk README requires this: a mount of the device inside the namespace of the daemon can deadlock when the daemon exits.

## 20. Not built yet

- A Docker volume plugin or a Kubernetes CSI driver. For v1, use `mica disk mount` on the host and bind-mount the path.

## Online growth

Resize and other disk mutations use the same per-disk operation lock as attach and detach. Detached mutations claim the S3 ownership marker for their duration. Creating a head uses create-if-absent when the store supports conditional writes. Without conditional writes, the controller must still serialize operations across nodes.

An attached resize holds the checkpoint lock. It extends local backing files and the recent bitmap, writes a larger root and head after confirming ownership, publishes the larger engine size, then sends the ublk size update. Existing chunk locks remain valid while metadata pages are appended. Kernel-update failures are retried by repeating resize or during device recovery. Filesystem and hypervisor growth remain the caller's responsibility.
