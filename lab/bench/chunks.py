# Changed chunks per upload interval, from a block_bio_queue trace.
# Usage: python3 chunks.py <trace>... Skips the first minute (warm-up).
import re, sys
LINE = re.compile(r"(\d+\.\d+): block_bio_queue: \d+,\d+ (\S+) (\d+) \+ (\d+)")
SIZES = {"256K": 256 << 10, "1M": 1 << 20, "4M": 4 << 20}
INTERVALS = [30, 60, 300, 900]
MONTH = 30 * 86400
for path in sys.argv[1:]:
    writes = []
    for line in open(path):
        m = LINE.search(line)
        if m and "W" in m[2] and "D" not in m[2] and int(m[4]) > 0:
            writes.append((float(m[1]), int(m[3]) * 512, int(m[4]) * 512))
    start = writes[0][0] + 60
    span = writes[-1][0] - start
    writes = [w for w in writes if w[0] >= start]
    total = sum(w[2] for w in writes)
    print(f"\n{path.split('/')[-1]}: {span/60:.1f} min, {total/span*60/2**20:.0f} MiB written per minute")
    print(f"  {'interval':>8} {'chunk':>5} {'chunks/round':>12} {'uploads/month':>14} {'R2 $':>7} {'S3 $':>7}")
    for interval in INTERVALS:
        rounds = int(span // interval)
        if rounds == 0:
            continue
        for name, size in SIZES.items():
            per_round, counts = [], []
            for r in range(rounds):
                lo, hi = start + r * interval, start + (r + 1) * interval
                chunks = set()
                for t, off, n in writes:
                    if lo <= t < hi:
                        chunks.update(range(off // size, (off + n - 1) // size + 1))
                counts.append(len(chunks)); per_round.append(len(chunks) + len({c // 1024 for c in chunks}) + 2)
            avg = sum(per_round) / rounds
            month = avg * MONTH / interval
            print(f"  {interval:>7}s {name:>5} {sum(counts) / rounds:>12.0f} {month/1e6:>13.2f}M {max(month - 1e6, 0) * 4.5e-6:>7.2f} {month * 5e-6:>7.2f}")
