# To do

## ublk zero copy (done, 2026-10-30)

Done as in `plans/ublk-zero-copy.md`: `UBLK_F_AUTO_BUF_REG` and `UBLK_F_USER_COPY`, no per-tag buffers. Results are in `lab/results/2026-10-30-zerocopy*`, and in the progress page.

Still open:

- [ ] Benchmark fixed-file registration for stable backing files. [Loop example](https://github.com/ublk-org/libublk-rs/blob/main/examples/loop.rs)
- [ ] Measure per-queue CPU use with single and multiple submitters. Try extra threads per queue only for saturated workloads, and check shallow-I/O latency. [Benchmark caveats](https://github.com/ublk-org/libublk-rs#benchmarking-caveat-one-submitter-measures-one-queue)
- [ ] Evaluate `UBLK_F_BATCH_IO`. Treat upstream null-target gains as framework measurements, not expected mica gains. [Upstream benchmarks](https://lwn.net/Articles/1046200/)
- [ ] FUA writes into local chunks still copy the request through `/dev/ublkcN`. Serve them on the queue thread only if a profile shows the copy.

## Idle queue threads count as iowait (done, 2026-10-30)

A queue thread waits in `io_uring_enter` with requests in flight (the ublk fetch commands and the eventfd read). The kernel then counted the wait as iowait: each attached disk added up to 4 to `procs_blocked`, `/proc/pressure/io` read about 95 % and `mpstat` about 64 % iowait on an idle host. The load average did not change (io_uring waits are interruptible). The daemon itself used about 10 ms of CPU per minute with 4 idle disks.

- Done: the queue runs libublk's event loop with its own wait, which passes `IORING_ENTER_NO_IOWAIT` (Linux 6.15). With 8 idle disks: 0 blocked tasks and 0 % I/O pressure.
- Lab A/B (lab/results/2026-10-30-zerocopy-flush-cold, -cold-flush2): queue-depth-1 local reads and writes the same or faster, cold reads the same, FLUSH p50 the same (720 µs). Idle write+fdatasync was 833/s against 922/s for v0.2c and 1045/s for zero copy (5 runs each, wide spread), from a heavier p99. Watch this number.
- VM check (4 vCPU, Ubuntu 26.04, kernel 7.0, 4 rounds against zero copy): 0 blocked tasks and under 1 % I/O pressure with 4 idle disks (16 and 99.9 % before). Idle write+fdatasync 993/s against 1004/s, queue-depth-1 and throughput tests the same within noise. Data check, 3 daemon kills mid-I/O, graceful restart (0.13 s) and idle detach (0.06 s) pass.
- When libublk changes, compare `handle_io_events` in `src/ublk_queue.rs` with `libublk::wait_and_handle_io_events`.

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
