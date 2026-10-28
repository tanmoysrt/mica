# Code map

This page tells you where each part of mica is. Read [docs/architecture.md](../docs/architecture.md) first. It explains how mica works. This page explains the code.

## Read in this order

1. **`main.rs`** is the entry point. The `daemon` command starts the daemon. All other commands are CLI commands.
2. **`commands.rs`**, **`disk_commands.rs`** and **`snapshot_commands.rs`** hold the CLI commands. Each command sends requests to the daemon.
3. **`api.rs`** is the Unix socket protocol. It receives a request and calls the daemon.
4. **`daemon.rs`** owns the attached disks. It attaches, detaches, and finishes work left from a crash.
5. **`disk.rs`** is the disk engine: read, write, discard and flush. This is the most important file.
6. **`checkpoint.rs`** uploads unsaved chunks and commits a new root.
7. **`ublk_device.rs`** connects the kernel block device to the disk engine.

Then read the helper files in the table below when you need them.

## Files

### Entry and control

| File | What it holds |
|---|---|
| `main.rs` | CLI parsing. Starts the daemon or runs a CLI command. Isolates the mount namespace of the daemon. |
| `commands.rs` | Top-level CLI commands, the questions before lists and deletes, size parsing (`20G`). |
| `disk_commands.rs` | `mica disk ls / create / attach / mount / unmount / detach / set-limits / resize / delete`. |
| `snapshot_commands.rs` | `mica snapshot ls / create / delete`. |
| `output.rs` | Text output: tables, sizes and ages. |
| `api.rs` | `Request` enum, socket server, socket client, and the dispatch of each request. |
| `daemon.rs` | `Daemon`: attach, detach, drain (upload until done), status, checkpoint loop. |
| `daemon_recovery.rs` | Daemon restart: takes over paused devices, finishes detaches after a reboot, and the last checkpoint on SIGTERM. |
| `setup.rs` | `mica service setup`: prompts, machine checks, the mica group, bucket test, binary install, service start. |
| `service.rs` | `mica service …` commands, and the text of `mica.service` and `mica-mount@.service`. |
| `managed_mount.rs` | Daemon side: mount and unmount through `mica-mount@<disk>`, with the mount path checks. CLI side: direct mounts without the service, and the unit's start and stop steps. |
| `systemd.rs` | `systemctl` calls, unit files, and the readiness message to systemd. |
| `config.rs` | `/etc/mica/config.toml`. Empty S3 fields come from the environment (`.env` names). |

### Disk engine

| File | What it holds |
|---|---|
| `disk.rs` | `Disk` and `ChunkSlot`. Loads the slots of a page on first use. Splits block I/O into chunk parts. Makes a chunk local on first write. FLUSH, read passes, held copies, and the freeing of saved chunks. Feeds read-ahead. Waits when unsaved data is at its limit. |
| `checkpoint.rs` | `Disk::checkpoint`: stage and upload each unsaved chunk, release uploads that a GC check proves safe, save changed pages and the root, check owner, write head, repair after a GC, finish. |
| `prefetch.rs` | Read-ahead for sequential reads, the startup prefetch from the read profile, and the background fill. |
| `read_profile.rs` | Records the first reads of a session (up to 64 MiB of chunks). |
| `local_data.rs` | `LocalData`: the data files, the hold file, and the dirty log (`DirtyLog`) with the `recent` bitmap in /run (`RecentMap`). Punches out unlisted ranges at open. |
| `cache_manager.rs` | Per-disk cache instances and host capacity reconciliation. |
| `disk_limits.rs` | Persisted clean-cache and pending-write budgets. |
| `disk_resize.rs` | Online engine growth and budget updates. |
| `disk_operations.rs` | Serialized lifecycle operations and storage reconciliation. |
| `chunk_cache.rs` | Clean chunks of one disk. Downloads from S3 in three classes (reads, likely reads, fill), decompresses and checks the hash, checks files found at start, evicts old chunks, prunes unused ones. |

### S3 objects

| File | What it holds |
|---|---|
| `bucket.rs` | `Bucket`: get, put, delete, exists. `keys` gives the key of each object type. |
| `manifest.rs` | Binary formats of the root (`Manifest`) and of a `Page`: encode, decode, load, save, grow. Chunk size limits. |
| `page_store.rs` | `PageStore`: verified pages of all disks, in memory, in local files and in S3. |
| `codec.rs` | zstd compression of chunk and page objects. |
| `records.rs` | JSON records: `Head`, `AttachedMarker`, `Snapshot`. |
| `catalog.rs` | Operations that only touch S3: create, clone, snapshot, resize, delete, list, load a disk. |
| `gc.rs` | `GarbageCollector`: finds the live roots, pages and chunks, and deletes old objects that are not live. `GcActivity`: what a checkpoint sees of GC. |
| `ownership.rs` | The `attached` marker: find the owner, claim with a read-back, release, and the watcher that checks it every minute. |
| `content_hash.rs` | `ContentHash` (SHA-256), hex and `sha256:` forms, zero check. |

### Node and host

| File | What it holds |
|---|---|
| `node.rs` | `NodeIdentity`: the random node ID in `<data_dir>/node-id`, and the hostname. |
| `local_state.rs` | `DiskFolder`: `<data_dir>/disks/<disk-id>/`, its `state` file (attached or orphaned). |
| `local_io.rs` | Plans reads and writes that the queue thread can serve alone, from local data. |
| `queue_io.rs` | Does those reads and writes with io_uring on the queue thread. |
| `ublk_device.rs` | `UblkDevice`: start, recover and stop `/dev/ublkbN`. Maps each ublk request to a `Disk` call. Makes `/dev/mica/<disk-id>`. |
| `mount_tools.rs` | CLI helpers: `mkfs`, `mount`, `umount`, and search in `/proc/self/mounts`. |
| `blocking.rs` | `blocking()`: runs file I/O on the tokio blocking pool. |

## Main flows

### A guest write

`ublk_device.rs` `handle_request` → `Disk::write` → `Disk::write_part`: `make_local` copies the base from the cache if needed, `hold_if_staged` keeps a copy for the running checkpoint, the data goes into the data file, and `finish_write` marks the data file and lists the chunk (`DirtyLog::note_write`).

### A guest read

`Disk::read` reads local chunks from the data files under a read pass, then the others from `chunk_cache.rs` (which gets them from S3 when needed). A chunk with no content is zeros.

### A guest flush

`Disk::flush` syncs the data files written since the last FLUSH, then appends the newly local chunks to the dirty log and syncs it. It never touches S3.

### A checkpoint

`daemon.rs` `run_checkpoints` starts it every 30 s, at 512 MiB of unsaved data, or when local space runs low. Then `checkpoint.rs`: `GcActivity::read` → `stage_and_upload` (`stage` and `upload` for each chunk, `release_safe` once a second) → `commit_root` (pages, then root) → `confirm_ownership` → `Head::save` → `GcActivity::finished` → repair if a GC ran → `finish` (saved chunks, `unstage`, `free_saved_chunks`).

### Attach

`Daemon::attach` → `ownership::claim_then` (claim, and `Catalog::load` of head and root) → `Disk::open` (adds chunks left by a crash, with their pages) → `UblkDevice::start` → checkpoint loop, marker watcher and `prefetch::run` start.

### Detach

`Daemon::detach` checks that the device is not in use, then stops it. `Daemon::drain` then runs checkpoints until one succeeds. It copies the local chunks into the cache, deletes the local data, releases the marker, and deletes the disk folder.

### Daemon restart

On SIGTERM, `Daemon::shut_down` runs a last checkpoint and hands each device over (`UblkDevice::hand_over`). The new daemon's `recover_disks` finds the ublk device number in each `state` file, and `take_over` recovers the device with `UblkDevice::recover`.

### Setup

`setup::run` → `load_ublk_module` → `ask_config` (or `Config::read_complete_file`) → `test_bucket` → `Config::save` → `install_binary` → `service::install` → `start_service`.

### A mount with the service

The CLI sends a `Mount` request. The daemon (`managed_mount::mount_with_unit`) checks the path, saves a `MountSpec`, then runs `systemctl enable --now mica-mount@<disk>`. The unit calls the hidden `mica mount-unit start|stop <disk>` commands, which call `unit_start` and `unit_stop`.

### Restart after a reboot

`Daemon::recover_disks` looks at each disk folder without a live device:

- A folder without chunks is deleted, and our marker is released.
- If the marker is ours, `drain` uploads the chunks.
- If the marker is not ours, the folder is marked orphaned and kept.

## Concurrency rules

- Each chunk has its own async lock (`ChunkSlot` in `disk.rs`). A write holds it until the data is in the file. Thus a checkpoint cannot read a chunk while a write goes into it.
- Only one checkpoint runs at a time (`checkpoint_running`).
- File I/O runs on the blocking pool (`blocking.rs`).
- ublk queue threads run a smol executor. A request that only needs local data is served on the queue thread with io_uring (`local_io.rs`, `queue_io.rs`). Other requests go to tokio in `on_tokio` in `ublk_device.rs`.
- A local write holds its chunk locks until the data is in the files, the same rule as the tokio path.
- A queue thread sleeps inside io_uring. Only an io_uring completion wakes it. When tokio finishes a request, it writes to an eventfd (`QueueWaker`) that the queue always reads. The result goes into a channel before this write, so the thread never wakes too early.
- The daemon runs in its own mount namespace with slave propagation. Mounts made in the daemon stay inside it. Unmounts on the host still reach it.
