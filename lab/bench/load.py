#!/usr/bin/env python3
"""O_DIRECT load on several block devices at once: one process per device,
some threads per process. Prints the total MiB/s and I/O per second.

    load.py <read|write|randread|randwrite> <block KiB> <threads> <MiB per device> <seconds> <device>...

A sequential pass covers the first <MiB per device> of each device, again
and again, until <seconds> pass; with 0 seconds, once. Each thread takes
its own stripe. A random load with 0 seconds makes as many operations as a
pass would. Any error or short transfer fails the run.
"""
import mmap
import os
import random
import sys
import threading
import time
from multiprocessing import Pool


def worker(device, mode, block, size, index, threads, end, counts):
    try:
        counts[index] = transfer(device, mode, block, size, index, threads, end)
    except Exception as error:
        counts[index] = error


def transfer(device, mode, block, size, index, threads, end):
    write = "write" in mode
    fd = os.open(device, (os.O_WRONLY if write else os.O_RDONLY) | os.O_DIRECT)
    buffer = mmap.mmap(-1, block)
    if write:
        buffer.write(os.urandom(block))
    blocks = size // block
    position, done = index, 0
    while (position < blocks) if end is None else (time.monotonic() < end):
        if mode.startswith("rand"):
            offset = random.randrange(blocks) * block
        else:
            offset = (position % blocks) * block
        position += threads
        moved = os.pwritev(fd, [buffer], offset) if write else os.preadv(fd, [buffer], offset)
        if moved != block:
            raise OSError(f"{device}: {moved} of {block} bytes at {offset}")
        done += 1
    os.close(fd)
    return done


def run(args):
    device, mode, block, threads, size, seconds = args
    end = time.monotonic() + seconds if seconds > 0 else None
    counts = [0] * threads
    workers = [
        threading.Thread(target=worker, args=(device, mode, block, size, i, threads, end, counts))
        for i in range(threads)
    ]
    for thread in workers:
        thread.start()
    for thread in workers:
        thread.join()
    for count in counts:
        if isinstance(count, Exception):
            raise count
    return sum(counts)


def main():
    mode, block, threads = sys.argv[1], int(sys.argv[2]) << 10, int(sys.argv[3])
    size, seconds, devices = int(sys.argv[4]) << 20, float(sys.argv[5]), sys.argv[6:]
    start = time.monotonic()
    with Pool(len(devices)) as pool:
        ops = sum(pool.map(run, [(device, mode, block, threads, size, seconds) for device in devices]))
    elapsed = time.monotonic() - start
    print(f"{ops} ops {ops / elapsed:.0f} IOPS {ops * block / elapsed / 2**20:.0f} MiB/s")


if __name__ == "__main__":
    main()
