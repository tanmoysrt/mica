# To do

## ublk zero-copy

Today each request is copied once between the kernel and the daemon's buffer. With `UBLK_F_SUPPORT_ZERO_COPY` or `UBLK_F_AUTO_BUF_REG`, io_uring reads and writes the request's pages directly. libublk 0.4.8 and kernel 7.0 support both.

- Benefit: about 15% of one core at 1.6 GiB/s of 1 MiB local I/O. Under 3% for 4 KiB I/O. The S3 link and `fdatasync` set the speed, not the copy.
- Work: local reads and writes on the queue thread become `READ_FIXED`/`WRITE_FIXED`. Cache misses and writes to chunks that are not local must finish on the queue ring, because the daemon no longer has the data in its buffer. Then a codex review and the crash test again.
- Before starting: profile the 4-writer 1 MiB test with `perf`. Skip if the copy is under about 10% of CPU.

Explore:

- [ ] Prefer `UBLK_F_AUTO_BUF_REG` over manual buffer registration. Probe support and preserve a fallback. [Upstream discussion](https://lwn.net/Articles/1020732/)
- [ ] Benchmark fixed-file registration for stable backing files separately from fixed-buffer zero-copy. [Loop example](https://github.com/ublk-org/libublk-rs/blob/main/examples/loop.rs)
- [ ] Measure per-queue CPU use with single and multiple submitters. Try extra threads per queue only for saturated workloads, and check shallow-I/O latency. [Benchmark caveats](https://github.com/ublk-org/libublk-rs#benchmarking-caveat-one-submitter-measures-one-queue)
- [ ] Evaluate `UBLK_F_BATCH_IO` independently after zero-copy. Treat upstream null-target gains as framework measurements, not expected Mica gains. [Upstream benchmarks](https://lwn.net/Articles/1046200/)
- [ ] Remeasure 4 KiB workloads rather than assume negligible gains. Test compressed cold reads, FUA/FLUSH, and daemon recovery for correctness. [Android evaluation](https://lpc.events/event/18/contributions/1710/attachments/1440/3070/LPC2024_ublk_zero_copy.pdf)
- [ ] Defer shared-memory zero-copy unless a cooperating client requires it.

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
