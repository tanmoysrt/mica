#!/usr/bin/env python3
"""4 KiB O_DIRECT random reads at queue depth 1 on a block device.

    randread.py <device> <size MiB> <seconds>
"""
import mmap
import os
import random
import sys
import time

device, size, seconds = sys.argv[1], int(sys.argv[2]) << 20, float(sys.argv[3])
fd = os.open(device, os.O_RDONLY | os.O_DIRECT)
buffer = mmap.mmap(-1, 4096)
latencies = []
end = time.monotonic() + seconds
while time.monotonic() < end:
    start = time.monotonic()
    os.preadv(fd, [buffer], random.randrange(size // 4096) * 4096)
    latencies.append(time.monotonic() - start)
latencies.sort()
pick = lambda q: latencies[min(len(latencies) - 1, int(q * len(latencies)))] * 1e6
print(f"{len(latencies) / seconds:.0f} reads/s, p50 {pick(0.5):.0f} us, p99 {pick(0.99):.0f} us")
