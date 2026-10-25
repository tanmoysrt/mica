#!/usr/bin/env python3
"""Hash every file under a folder with N readers.

  verify.py record <root> <hashes> <readers>   write "<blake2b> <path>" lines
  verify.py check  <root> <hashes> <readers>   read all files, compare
"""
import hashlib
import os
import sys
from multiprocessing import Pool


def digest(path):
    h = hashlib.blake2b(digest_size=16)
    with open(path, "rb", buffering=0) as f:
        while block := f.read(1 << 20):
            h.update(block)
    return h.hexdigest()


def check(item):
    path, want = item
    return path if digest(path) != want else None


def main():
    mode, root, hashes, readers = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    if mode == "record":
        paths = sorted(os.path.join(d, f) for d, _, fs in os.walk(root) for f in fs if "lost+found" not in d)
        with Pool(readers) as pool:
            sums = pool.map(digest, paths, chunksize=16)
        with open(hashes, "w") as out:
            for path, s in zip(paths, sums):
                out.write(f"{s} {os.path.relpath(path, root)}\n")
        print(f"recorded {len(paths)} files")
    else:
        items = []
        with open(hashes) as f:
            for line in f:
                s, rel = line.rstrip("\n").split(" ", 1)
                items.append((os.path.join(root, rel), s))
        # Round-robin over readers, like `split -n r/N`.
        with Pool(readers) as pool:
            bad = [p for p in pool.imap_unordered(check, items, chunksize=16) if p]
        print(f"checked {len(items)} files, {len(bad)} bad")
        for p in bad[:10]:
            print("  BAD", p)


if __name__ == "__main__":
    main()
