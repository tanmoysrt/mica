# ublk zero-copy

## Goal

Remove the I/O buffers of the daemon. Now each tag has a 512 KiB `IoBuf`. One disk has 4 queues × 64 tags, so up to 128 MiB per disk. With many attached disks on a host, this memory is the main cost. The CPU saving is secondary.

## Design

The device gets `UBLK_F_AUTO_BUF_REG` and `UBLK_F_USER_COPY`. libublk 0.4.8 supports this pair (`BufDesc::Slice` is valid with `USER_COPY`, see `io.rs:494`).

- `UBLK_F_AUTO_BUF_REG`: before a request reaches the daemon, the kernel registers the request pages as a fixed buffer in the queue ring, at index `tag`. Nothing is copied.
- `UBLK_F_USER_COPY`: the daemon reads or writes the request pages through `/dev/ublkcN` at `ublk_user_copy_pos(queue, tag, offset)`. This is one copy, used only on the slow paths.

Each request path:

| Path | Now | New |
|---|---|---|
| Local read (`queue_io::read_local`) | `Read` into `IoBuf` | `ReadFixed` into buffer `tag` |
| Local read, part that was never written | `memset` in `IoBuf` | `ReadFixed` from `/dev/zero` into buffer `tag` |
| Local write (`queue_io::write_local`) | `Write` from `IoBuf` | `WriteFixed` from buffer `tag` |
| Read through tokio (cache miss, chunk not local) | `copy_from_slice` into `IoBuf` | tokio returns the `Vec`, the queue ring writes it to `/dev/ublkcN` |
| Write through tokio (FUA, chunk not local) | `to_vec()` of `IoBuf` | the queue ring reads `/dev/ublkcN` into a new `Vec`, then tokio |
| FLUSH, DISCARD, WRITE_ZEROES | no data | no change |

The slow-path buffers live only while a request is in flight. Their size follows the load, not the device. A slow path now does one copy, not two (`Vec` to `IoBuf` to request pages).

For a fixed buffer that the kernel registered, the address in `ReadFixed` and `WriteFixed` is the offset in the request, not a pointer. All operations on buffer `tag` must complete before the commit, because the commit unregisters the buffer. `serve_tag` already waits for them.

The slow paths keep the copy. The other option was this: tokio makes the chunks local, and then the queue thread tries the local path again. But the local plans use `try_lock`, so a busy lock or an eviction between the two steps still needs the copy. That design also splits `disk.read` and `write_part` into two steps. The copy costs about 1 µs per 4 KiB, and these paths wait for S3 or `fdatasync`. Look at FUA writes into local chunks again only if a profile shows their copy.

`src/disk.rs` and `src/local_data.rs` do not change. The work is in `src/ublk_device.rs` and `src/queue_io.rs`.

## Steps

1. Measure the baseline on the current build. Attach 8 disks, run 4 KiB random I/O and 1 MiB sequential I/O on each disk, and record daemon RSS and CPU. Pages of an `IoBuf` count in RSS only after the kernel writes to them, so RSS can be much less than 128 MiB per disk.
2. Probe the lab kernel (7.0.12). Make a device with `AUTO_BUF_REG | USER_COPY`, serve one `ReadFixed` from a file, one `ReadFixed` from `/dev/zero`, and one `/dev/ublkcN` copy, and check the data. Stop here if the kernel refuses the pair.
3. Change `serve_tag`. Fetch and commit with `BufDesc::AutoReg { index: tag }`, and remove the `IoBuf`. Register a sparse buffer table of size `QUEUE_DEPTH` on each queue ring, if libublk does not do this already.
4. Change `read_local` and `write_local` to `ReadFixed` and `WriteFixed` with the buffer offset of each part. Replace the `memset` with a `ReadFixed` from `/dev/zero`, opened once for each queue.
5. Change the tokio paths in `handle_request` to copy through `/dev/ublkcN` on the queue ring. Open the char device once for each queue.
6. Set `UBLK_AUTO_BUF_REG_FALLBACK`. Without it, a failed registration fails the request, and the daemon is not told. With it, the request arrives with `UBLK_IO_F_NEED_REG_BUF` set. The daemon then serves it with user copy and logs it. Count these requests in the stress test.
7. Run the unit tests, strict Clippy, and a codex review (`-m gpt-6.1-sol -c model_reasoning_effort=medium`).
8. Run the crash test: `MODE=power` and `MODE=daemon`, on btrfs, XFS and ext4.
9. Do an A/B against the current build: step 1 again, plus `lab/bench/flush.sh` and the cold-read test. Save the current build as `~/.cache/mica-lab/mica-v0.2c` first.
10. Update `todo.md`, `docs/architecture.md`, `src/code-map.md`, and the site page.

## Existing devices

The daemon serves only zero-copy devices. All disks are purged before the new build runs, so the daemon does not recover devices made by an older build. A rollback to an older build also needs a purge.

## Risks

- A kernel without `AUTO_BUF_REG` (before 6.16) cannot run mica. Check the kernel on production hosts.
- `USER_COPY` and `SUPPORT_ZERO_COPY` are not available for unprivileged devices. mica runs as root, so this does not apply now.
- A wrong offset in `ReadFixed` or `WriteFixed` corrupts data without an error. The crash test and `lab/bench/verify.py` must cover multi-chunk requests and requests with unwritten parts.

## Expected result

- Memory: the per-disk buffer memory goes from up to 128 MiB to almost 0. Step 1 shows the real figure.
- CPU: less than 3% at 4 KiB I/O, about 15% of one core at 1.6 GiB/s of 1 MiB I/O.
- Cold reads, FLUSH and S3 traffic: no change.

## Effort

Steps 1 and 2: 1 day. Steps 3 to 6: 1.5 to 2 days. Steps 7 to 9: 1.5 to 2 days.
