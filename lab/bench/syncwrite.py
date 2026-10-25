#!/usr/bin/env python3
"""4 KiB O_DIRECT writes on a block device, each followed by fdatasync
(or not, with --nosync), for some seconds. Prints writes/s and latency.

    syncwrite.py <device> <offset MiB> <size MiB> <seconds> [seq|rand] [--nosync]
"""
import mmap
import os
import random
import sys
import time

device, offset, size, seconds = sys.argv[1], int(sys.argv[2]) << 20, int(sys.argv[3]) << 20, float(sys.argv[4])
pattern = sys.argv[5] if len(sys.argv) > 5 else "seq"
sync = "--nosync" not in sys.argv
fd = os.open(device, os.O_WRONLY | os.O_DIRECT)
buffer = mmap.mmap(-1, 4096)
buffer.write(os.urandom(4096))
blocks = size // 4096
latencies = []
position = 0
end = time.monotonic() + seconds
while time.monotonic() < end:
    block = random.randrange(blocks) if pattern == "rand" else position % blocks
    position += 1
    start = time.monotonic()
    os.pwrite(fd, buffer, offset + block * 4096)
    if sync:
        os.fdatasync(fd)
    latencies.append(time.monotonic() - start)
latencies.sort()
pick = lambda q: latencies[min(len(latencies) - 1, int(q * len(latencies)))] * 1e6
print(f"{len(latencies) / seconds:.0f} writes/s, p50 {pick(0.5):.0f} us, p99 {pick(0.99):.0f} us, max {latencies[-1] * 1e3:.0f} ms")
