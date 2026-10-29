# To do

## ublk zero copy (done, 2026-10-30)

Done as in `plans/ublk-zero-copy.md`: `UBLK_F_AUTO_BUF_REG` and `UBLK_F_USER_COPY`, no per-tag buffers. Results are in `lab/results/2026-10-30-zerocopy*`, and in the progress page.

Still open:

- [ ] Benchmark fixed-file registration for stable backing files. [Loop example](https://github.com/ublk-org/libublk-rs/blob/main/examples/loop.rs)
- [ ] Measure per-queue CPU use with single and multiple submitters. Try extra threads per queue only for saturated workloads, and check shallow-I/O latency. [Benchmark caveats](https://github.com/ublk-org/libublk-rs#benchmarking-caveat-one-submitter-measures-one-queue)
- [ ] Evaluate `UBLK_F_BATCH_IO`. Treat upstream null-target gains as framework measurements, not expected mica gains. [Upstream benchmarks](https://lwn.net/Articles/1046200/)
- [ ] FUA writes into local chunks still copy the request through `/dev/ublkcN`. Serve them on the queue thread only if a profile shows the copy.

## Idle queue threads count as iowait

A queue thread waits in `io_uring_enter` with requests in flight (the ublk fetch commands and the eventfd read). The kernel then counts the wait as iowait. Each attached disk adds up to 4 to the load average and to `procs_blocked`, and `/proc/pressure/io` reads about 95 % on an idle host (8 lab disks, 2026-10-30; v0.2c and zero copy alike). Host monitoring sees an I/O stall that does not exist.

- Fix: wait with `IORING_ENTER_NO_IOWAIT` (Linux 6.15). libublk makes the wait in `wait_and_handle_io_events`, so this needs a libublk change.
- Lab: check I/O pressure before disks attach, or use `vmstat` `bo`/`bi`.

## Keep attached disks fully cached

Today the chunk cache evicts the least recently used chunks at `cache_limit_gib`, from any disk. An attached disk can lose its cached chunks, and its next reads go to S3 again. The background fill also stops for good once the cache reaches 90%, because eviction keeps it at 90%.

Idea: a cache mode with no LRU eviction. Every chunk of an attached disk stays local until the disk detaches. A cache GC removes chunks that no attached disk uses: after a detach, and when space runs low.

- Config: for example `cache_eviction = "lru" | "attached"`. This changes a policy, not an implementation.
- Admission: refuse an attach, or turn off its fill, when the cache cannot hold the disk's chunks. Count the chunks of each attached disk once, because disks share chunks.
- Detached disks: keep their chunks while space allows, so a disk that comes back attaches warm. The cache GC removes those first.
- Check the fill: with this mode it must fill every attached disk completely.

## Other open items

- On btrfs, a synced 4 KiB write costs about 50 µs more in a 1 GiB data file than in a new small file (idle write plus `fdatasync` 3% slower than v0.2a on a quiet host). Smaller data files do not help. Check XFS and ext4.
- In the lab, RustFS shares the btrfs and SSD with the lab daemon, so an upload slows the daemon's syncs. A loop-mounted XFS for RustFS did not help.
- A page that GC deletes between commit and repair is lost on a power cut (also in v0.2a).
- Without conditional writes in S3, two GCs can still overlap.
- No crash-test baseline run on v0.2a.
