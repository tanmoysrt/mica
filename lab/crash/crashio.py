#!/usr/bin/env python3
"""Writes and checks a disk for the crash test (lab/bench/crash.sh).

Every 4 KiB block names itself: a header with the block number, the number
of the write that wrote it, and a CRC of the whole block. So a read tells
which write a block holds, and a torn block shows.

    crashio.py prefill <device> <blocks> <state>
    crashio.py write <device> <blocks> <log> <first seq>
    crashio.py check <device> <blocks> <state> <log> [acked]

`write` runs until it is killed. Before each write it logs "W <seq> <block>
<count>"; after each FLUSH "F <seq>"; after each FUA write "D <seq>". Log
lines go straight to the file, so a kill loses none that were done.

`check` knows, from the state of the last check and the log, which writes
must be on the disk: each block must hold its last write before the last
FLUSH (or a FUA write), or a later one. Each check saves the state it saw.
With "acked" (after a daemon crash), every write that returned must be
there: all but the last one logged.
"""
import json
import mmap
import os
import random
import struct
import sys
import zlib

BLOCK = 4096
MAGIC = b"MICACRSH"
HEADER = struct.Struct("<8sQQI")  # magic, block, seq, crc of the rest
SIZES = [1, 1, 1, 1, 2, 4, 8, 16, 64, 256]


def aligned(size):
    return mmap.mmap(-1, size)


def fill(buffer, start, seq):
    """One write: `len(buffer) / BLOCK` blocks from `start`, all from `seq`."""
    count = len(buffer) // BLOCK
    buffer[:] = os.urandom(len(buffer))
    for index in range(count):
        offset = index * BLOCK
        body = bytes(buffer[offset + HEADER.size:offset + BLOCK])
        crc = zlib.crc32(struct.pack("<QQ", start + index, seq) + body)
        buffer[offset:offset + HEADER.size] = HEADER.pack(MAGIC, start + index, seq, crc)


def parse(block_bytes, expected_block):
    """The seq of a block; -1 for zeros, None for a bad block."""
    if block_bytes == bytes(BLOCK):
        return -1
    magic, block, seq, crc = HEADER.unpack_from(block_bytes)
    if magic != MAGIC or block != expected_block:
        return None
    body = block_bytes[HEADER.size:]
    if zlib.crc32(struct.pack("<QQ", block, seq) + body) != crc:
        return None
    return seq


def prefill(device, blocks, state_path):
    fd = os.open(device, os.O_WRONLY | os.O_DIRECT)
    step = 256
    buffer = aligned(step * BLOCK)
    for start in range(0, blocks, step):
        count = min(step, blocks - start)
        view = buffer if count == step else aligned(count * BLOCK)
        fill(view, start, 0)
        os.pwrite(fd, view, start * BLOCK)
    os.fsync(fd)
    os.close(fd)
    with open(state_path, "w") as state:
        json.dump([0] * blocks, state)


def write(device, blocks, log_path, first_seq):
    fd = os.open(device, os.O_WRONLY | os.O_DIRECT)
    fua = os.open(device, os.O_WRONLY | os.O_DIRECT | os.O_DSYNC)
    log = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    buffers = {count: aligned(count * BLOCK) for count in set(SIZES)}
    rng = random.Random(first_seq)
    seq = first_seq
    while True:
        seq += 1
        roll = rng.random()
        if roll < 0.08:
            os.fsync(fd)
            os.write(log, f"F {seq}\n".encode())
            continue
        count = rng.choice(SIZES)
        start = rng.randrange(0, blocks - count + 1)
        buffer = buffers[count]
        fill(buffer, start, seq)
        os.write(log, f"W {seq} {start} {count}\n".encode())
        if roll < 0.12:
            os.pwrite(fua, buffer, start * BLOCK)
            os.write(log, f"D {seq}\n".encode())
        else:
            os.pwrite(fd, buffer, start * BLOCK)


def check(device, blocks, state_path, log_path, acked=False):
    with open(state_path) as state:
        base = json.load(state)
    writes = {}  # block -> seqs, in order
    durable = {}  # block -> the seq that must be there at least
    last_flush = 0
    fua = set()
    entries = []
    with open(log_path) as log:
        for line in log:
            parts = line.split()
            if len(parts) == 4 and parts[0] == "W":
                entries.append((int(parts[1]), int(parts[2]), int(parts[3])))
            elif len(parts) == 2 and parts[0] == "F":
                last_flush = max(last_flush, int(parts[1]))
            elif len(parts) == 2 and parts[0] == "D":
                fua.add(int(parts[1]))
    last_write = max((seq for seq, _, _ in entries), default=0)
    for seq, start, count in entries:
        for block in range(start, start + count):
            writes.setdefault(block, []).append(seq)
            if seq < last_flush or seq in fua or (acked and seq < last_write):
                durable[block] = seq
    fd = os.open(device, os.O_RDONLY | os.O_DIRECT)
    step = 256
    buffer = aligned(step * BLOCK)
    seen = list(base)
    errors = []
    bad = 0
    for start in range(0, blocks, step):
        count = min(step, blocks - start)
        os.preadv(fd, [buffer], start * BLOCK)
        for index in range(count):
            block = start + index
            seq = parse(buffer[index * BLOCK:(index + 1) * BLOCK], block)
            floor = durable.get(block)
            allowed = [s for s in writes.get(block, []) if floor is None or s >= floor]
            if floor is None:
                allowed.append(base[block])
            if seq not in allowed:
                bad += 1
                if len(errors) < 20:
                    errors.append(f"block {block}: holds {seq}, must be one of {sorted(allowed)[-5:]} (durable {floor})")
            seen[block] = seq
    os.close(fd)
    with open(state_path, "w") as state:
        json.dump(seen, state)
    flushed = len(durable)
    print(f"checked {blocks} blocks: {len(entries)} writes, last FLUSH at {last_flush}, "
          f"{flushed} blocks had durable writes, {bad} bad")
    for error in errors:
        print("  " + error)
    return 1 if errors else 0


if __name__ == "__main__":
    command, device, blocks = sys.argv[1], sys.argv[2], int(sys.argv[3])
    if command == "prefill":
        prefill(device, blocks, sys.argv[4])
    elif command == "write":
        write(device, blocks, sys.argv[4], int(sys.argv[5]))
    elif command == "check":
        sys.exit(check(device, blocks, sys.argv[4], sys.argv[5], len(sys.argv) > 6 and sys.argv[6] == "acked"))
