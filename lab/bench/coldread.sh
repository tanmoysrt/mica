#!/bin/bash
# Cold reads on the host: every read goes to S3 (empty cache, no prefetch,
# no fill), at the round trip RTT_MS (default 20 ms).
#   1. 128 MiB sequential, 1 MiB reads, queue depth 1 (read-ahead helps)
#   2. 64 random 1 MiB reads, queue depth 1 (each one waits for S3)
#   bench/coldread.sh <results folder>     (LABEL names the build, ROUNDS)
# Parsed as one block, so an edit while it runs cannot break it.
{
set -euo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
DISK=lab-coldread
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT"
exec > >(tee -a "$OUT/coldread.log") 2>&1
dev=/dev/mica/$DISK

attach() { mica disk attach "$DISK" --prefetch 0 --no-fill >/dev/null && "${ROOT[@]}" own "$DISK" >/dev/null; }
cold() { mica disk detach "$DISK" >/dev/null; mica cache prune -y >/dev/null; "${ROOT[@]}" drop-caches; attach; }
random_reads() {
    python3 - "$dev" <<'PY'
import mmap, os, random, sys, time
fd = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECT)
buffer = mmap.mmap(-1, 1 << 20)
random.seed(7)
offsets = random.sample(range(256), 64)
start = time.monotonic()
for offset in offsets:
    assert os.preadv(fd, [buffer], offset << 20) == 1 << 20
print(f"{(time.monotonic() - start) / 64 * 1000:.0f} ms each")
PY
}

trap 'mica disk detach "$DISK" >/dev/null 2>&1; mica disk delete "$DISK" -y >/dev/null 2>&1; RTT_MS= "$LAB/lab.sh" shape >/dev/null 2>&1' EXIT
RTT_MS=${RTT_MS:-20} "$LAB/lab.sh" shape >/dev/null
echo "== cold reads at ${RTT_MS:-20} ms round trip, $(date -Is) | ${LABEL:-}"
mica disk create "$DISK" --size 1G >/dev/null
attach
dd if=/dev/urandom of="$dev" bs=1M count=256 oflag=direct status=none
for round in $(seq "${ROUNDS:-2}"); do
    cold
    start=$(date +%s.%N)
    dd if="$dev" of=/dev/null bs=1M count=128 iflag=direct status=none
    seq_s=$(echo "$(date +%s.%N) - $start" | bc)
    cold
    printf '  round %d: sequential 128 MiB in %5.2f s = %5.1f MiB/s | 64 random 1 MiB reads: %s\n' \
        "$round" "$seq_s" "$(echo "128 / $seq_s" | bc -l)" "$(random_reads)"
done
echo DONE
exit
}
