#!/usr/bin/env python3
"""Fill a folder with random, non-zero files of mixed sizes.

Random bytes come from an AES-CTR stream (openssl), so no two files share
content and nothing dedups or compresses.
"""
import os
import random
import subprocess
import sys
import time

root = sys.argv[1]
rng = random.Random(42)
stream = subprocess.Popen(
    ["openssl", "enc", "-aes-128-ctr", "-pass", "pass:mica", "-nosalt", "-pbkdf2", "-in", "/dev/zero"],
    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=8 << 20,
).stdout

KIB, MIB = 1 << 10, 1 << 20
classes = [
    # name, count, min size, max size, files per folder
    ("large", 48, 64 * MIB, 64 * MIB, 48),
    ("medium", 2000, 256 * KIB, 2 * MIB, 500),
    ("small", 40000, 4 * KIB, 128 * KIB, 1000),
]

def write_file(path, size):
    with open(path, "wb") as f:
        left = size
        while left:
            n = min(left, 4 * MIB)
            f.write(stream.read(n))
            left -= n

total_files = total_bytes = 0
start = time.time()
for name, count, lo, hi, per_dir in classes:
    t = time.time()
    nbytes = 0
    for i in range(count):
        folder = os.path.join(root, name, f"d{i // per_dir:03d}")
        if i % per_dir == 0:
            os.makedirs(folder, exist_ok=True)
        size = rng.randint(lo // 4096, hi // 4096) * 4096 if lo != hi else lo
        write_file(os.path.join(folder, f"f{i:06d}"), size)
        nbytes += size
    dt = time.time() - t
    print(f"{name}: {count} files, {nbytes / MIB:.0f} MiB in {dt:.1f}s ({nbytes / MIB / dt:.0f} MiB/s)", flush=True)
    total_files += count
    total_bytes += nbytes
dt = time.time() - start
print(f"total: {total_files} files, {total_bytes / MIB:.0f} MiB in {dt:.1f}s ({total_bytes / MIB / dt:.0f} MiB/s)", flush=True)
