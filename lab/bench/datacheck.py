#!/usr/bin/env python3
"""Random I/O on a block device, checked against a model of its content.

    datacheck.py run   <device> <model file> <ops> [seed]
    datacheck.py check <device> <model file>

`run` mixes O_DIRECT reads and writes of 4 KiB to 1 MiB at any 4 KiB
offset, so requests span chunks and parts that were never written. It also
sends FUA writes (O_DSYNC), FLUSH, discards and write-zeroes. Each read is
compared with the model. At the end it reads the whole device. The model
starts from <model file> if it exists, else from zeros (a new disk), and is
saved there. `check` only reads the whole device, for example after a
reattach, when the reads come from S3.

A discard zeroes only the whole chunks it covers (CHUNK_KIB, default 256).
"""
import fcntl
import mmap
import os
import random
import struct
import sys

BLOCK = 4096
MAX_IO = 1 << 20
CHUNK = int(os.environ.get("CHUNK_KIB", "256")) << 10
BLKDISCARD = 0x1277
BLKZEROOUT = 0x127F
BLKGETSIZE64 = 0x80081272


class Device:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR | os.O_DIRECT)
        self.fua = os.open(path, os.O_RDWR | os.O_DIRECT | os.O_DSYNC)
        self.size = struct.unpack("Q", fcntl.ioctl(self.fd, BLKGETSIZE64, b"\0" * 8))[0]
        self.buffer = mmap.mmap(-1, MAX_IO)

    def read(self, offset, length):
        view = memoryview(self.buffer)[:length]
        assert os.preadv(self.fd, [view], offset) == length
        return bytes(view)

    def write(self, offset, data, fua=False):
        self.buffer[: len(data)] = data
        view = memoryview(self.buffer)[: len(data)]
        assert os.pwritev(self.fua if fua else self.fd, [view], offset) == len(data)

    def range_ioctl(self, request, offset, length):
        fcntl.ioctl(self.fd, request, struct.pack("QQ", offset, length))


def compare(device, model, offset, length, what):
    got = device.read(offset, length)
    if got != model[offset : offset + length]:
        bad = next(i for i in range(length) if got[i] != model[offset + i])
        sys.exit(f"MISMATCH {what} at {offset}+{length}: first bad byte at {offset + bad}")


def random_range(size):
    length = random.randint(1, MAX_IO // BLOCK) * BLOCK
    offset = random.randrange(0, (size - length) // BLOCK + 1) * BLOCK
    return offset, length


def run(device, model, ops):
    counts = {}
    for _ in range(ops):
        offset, length = random_range(device.size)
        kind = random.choices(["read", "write", "fua", "flush", "discard", "zeroes"], [40, 35, 10, 3, 6, 6])[0]
        counts[kind] = counts.get(kind, 0) + 1
        if kind == "read":
            compare(device, model, offset, length, "read")
        elif kind in ("write", "fua"):
            data = random.randbytes(length)
            device.write(offset, data, fua=kind == "fua")
            model[offset : offset + length] = data
        elif kind == "flush":
            os.fsync(device.fd)
        elif kind == "discard":
            device.range_ioctl(BLKDISCARD, offset, length)
            start, end = -(-offset // CHUNK) * CHUNK, (offset + length) // CHUNK * CHUNK
            if end > start:
                model[start:end] = bytes(end - start)
        else:
            device.range_ioctl(BLKZEROOUT, offset, length)
            model[offset : offset + length] = bytes(length)
    return counts


def check_all(device, model):
    offset = 0
    while offset < device.size:
        length = min(random.randint(1, MAX_IO // BLOCK) * BLOCK, device.size - offset)
        compare(device, model, offset, length, "full read")
        offset += length


def main():
    mode, device, model_path = sys.argv[1], Device(sys.argv[2]), sys.argv[3]
    random.seed(sys.argv[5] if len(sys.argv) > 5 else None)
    if os.path.exists(model_path):
        model = bytearray(open(model_path, "rb").read())
        assert len(model) == device.size, "model and device differ in size"
    else:
        model = bytearray(device.size)
    counts = run(device, model, int(sys.argv[4])) if mode == "run" else {}
    os.fsync(device.fd)
    open(model_path, "wb").write(model)
    check_all(device, model)
    print(f"ok: {mode}, {device.size >> 20} MiB checked", " ".join(f"{k}={v}" for k, v in sorted(counts.items())))


if __name__ == "__main__":
    main()
