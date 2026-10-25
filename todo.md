# To do

## ublk zero-copy

Today each request is copied once between the kernel and the daemon's buffer. With `UBLK_F_SUPPORT_ZERO_COPY` or `UBLK_F_AUTO_BUF_REG`, io_uring reads and writes the request's pages directly. libublk 0.4.8 and kernel 7.0 support both.

- Benefit: about 15% of one core at 1.6 GiB/s of 1 MiB local I/O. Under 3% for 4 KiB I/O. The S3 link and `fdatasync` set the speed, not the copy.
- Work: local reads and writes on the queue thread become `READ_FIXED`/`WRITE_FIXED`. Cache misses and writes to chunks that are not local must finish on the queue ring, because the daemon no longer has the data in its buffer. Then a codex review and the crash test again.
- Before starting: profile the 4-writer 1 MiB test with `perf`. Skip if the copy is under about 10% of CPU.

## Other open items

- Idle write plus `fdatasync` is 19% slower than v0.2a (1629 to 1317/s).
- FLUSH during a large upload is slow: the data file sync waits behind the upload's writeback.
- A page that GC deletes between commit and repair is lost on a power cut (also in v0.2a).
- Without conditional writes in S3, two GCs can still overlap.
- No crash-test baseline run on v0.2a.
