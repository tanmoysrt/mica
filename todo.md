# To do

v2 (branch `v2`) is the per-disk storage design: see `docs/design.md`.

## Docker volume plugin

A Docker volume driver, so that `docker volume create -d mica` makes a mica disk. Only a plan; no code yet.

```bash
docker volume create -d mica app-data -o size=20G -o cache-size=512M -o unsaved-size=4G
docker volume create -d mica sandbox-7 -o from=base-disk:base-image
docker run -v app-data:/data --volume-driver mica ...
docker volume rm app-data          # deletes the mica disk
```

Shape:

- `mica docker serve` in the same binary, run by `mica-docker.service` as root. It listens on `/run/docker/plugins/mica.sock`, where Docker finds plugins. It is a client of the daemon socket, like the CLI. The daemon does not change.
- The unit has `Wants=mica.service`, `After=mica.service` and `Before=docker.service`. At shutdown Docker stops its containers first, so the volumes are unmounted before the plugin and mica stop. `mica service install` writes the unit.
- A small HTTP server in the plugin (POST with a JSON body, one request per connection), not a new HTTP crate.
- The volume name is the disk name.

Docker calls:

| Call | What mica does |
|---|---|
| `Create` | `disk create` with the options `size`, `chunk-size`, `cache-size`, `unsaved-size` and `interval`, or `disk restore` with `from=<disk>:<snapshot>`. Default size 1G. A disk that already exists is used as it is, so existing disks become volumes. |
| `Mount` | Attach, format a disk that was never written, and mount at `/mnt/mica-docker/<name>`. Give Docker the `data` folder below it: `lost+found` would make the volume not empty, and Docker copies image data only into an empty volume. Count users by container ID, so that a second container shares the mount. |
| `Unmount` | When the last container leaves: `fstrim`, unmount, then detach in the background. Docker waits only about 30 s for a plugin call, and an upload can take longer. A `Mount` or `Remove` of the same volume waits for that detach. |
| `Remove` | `disk delete`, which also deletes the snapshots of the disk. |
| `Get`, `List`, `Path` | `Get` is one head read (`disk info`). `List` is `disk ls`: the disks of every bucket that the node knows. `Path` comes from local state. |
| `Capabilities` | Scope `global`: any node can attach the disk, and the claim keeps it on one node at a time (on a store without conditional writes, only if users attach it on one node). |

Details:

- Mount directly in the plugin. Docker owns the life of the mount, and mounts the volume again when it restarts a container after a reboot.
- Keep the container IDs per volume in `/run/mica-docker/state.json`. After a plugin restart, an `Unmount` must not unmount a volume that another container still uses. `/run` and the mounts both go away at a reboot.
- One lock per volume, so a slow cold attach does not block other volumes.
- The filesystem for new disks is a plugin flag (`--fs`, default ext4), not a volume option: the option would have to be kept until the first mount.
- Attach through the daemon API (`docs/api.md`): `blank` in the answer says when to format.

Lab checks: create and run; two containers on one volume; `docker volume rm`; a plugin restart while a container runs; a reboot with `--restart always` containers; an existing disk as a volume; a volume on a second node while the first has it.

## ublk zero copy follow-ups

Zero copy is done (`plans/ublk-zero-copy.md`, results in `lab/results/2026-10-30-zerocopy*`).

- [x] Fixed-file registration of the data file: within 4 % of a plain fd in each job (2026-11-09), not kept.
- [x] Queue count: 4 queues beat 8 and 16 on a 16-CPU laptop (2026-11-09), so `MAX_QUEUES` stays 4. With 2 submitters on CPUs of one queue, one queue thread is the limit. Benchmarks must read a range that fits in the page cache, or the reads go to the SSD and the numbers jump. [Benchmark caveats](https://github.com/ublk-org/libublk-rs#benchmarking-caveat-one-submitter-measures-one-queue)
- [ ] FUA writes into local chunks still copy the request through `/dev/ublkcN`. Serve them on the queue thread only if a profile shows the copy.
- [ ] Idle write plus `fdatasync` with the no-iowait wait: 833/s against 1045/s for zero copy in the lab (wide spread), the same on the VM. Watch this number.
- When libublk changes, compare the event loop in `src/ublk/queue.rs` with `libublk::wait_and_handle_io_events`.

## Other open items

- On btrfs, the data and cache files are NOCOW now (2026-11-09): random writes 100k to 188k IOPS, synced writes up to 1,700/s. A flush still spends about 55 µs outside the sync (queue thread, tokio, a blocking thread); a flush on the queue thread could save it, not tried. Check XFS and ext4 on a server.
- A store without conditional writes (Garage): mica reads the head before each head write, but two machines that read at the same moment can both win. Only users, or an orchestrator, keep such a disk on one machine.
- Linux 7.0 logs a UBSAN array-index warning in `io_buffer_register_bvec` (io_uring/rsrc.c) once per boot, when the first mica device starts (ublk auto buffer registration). Seen on Fedora 7.0.12 and Ubuntu 7.0.0. The data checks pass. Follow the upstream io_uring fix.
- Not tested: Hetzner and Wasabi (no account). Their documents do not say if PutObject keeps `If-Match` and `If-None-Match`: `mica bucket add` probes it.
- Lab note: RustFS shares the btrfs and SSD with the lab daemon, so an upload slows the daemon's syncs. A loop-mounted XFS for RustFS did not help.
