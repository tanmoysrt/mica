#!/bin/bash
# Data check of the ublk paths: datacheck.py on a disk with a small cache
# and a small pending-write budget, so requests take every path: local
# chunks, parts never written, cache misses to S3, writes into chunks that
# are not local, FUA, discard, write-zeroes. Then a reattach and a full read.
#   bench/datacheck.sh <results folder> [ops]     (LABEL names the build)
# Parsed as one block, so an edit while it runs cannot break it.
{
set -euo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
OPS=${2:-4000}
DISK=lab-datacheck
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT"
exec > >(tee -a "$OUT/datacheck.log") 2>&1
dev=/dev/mica/$DISK
model=$(mktemp)
rm "$model"
trap 'rm -f "$model"' EXIT
attach() { mica disk attach "$DISK" >/dev/null && "${ROOT[@]}" own "$DISK" >/dev/null; }

echo "== data check $(date -Is) | ${LABEL:-}"
mica disk create "$DISK" --size 512M --cache-size 32M --dirty-size 64M >/dev/null
attach
python3 "$BENCH/datacheck.py" run "$dev" "$model" "$OPS" 1
mica disk checkpoint "$DISK" >/dev/null
python3 "$BENCH/datacheck.py" run "$dev" "$model" "$OPS" 2
mica disk detach "$DISK" >/dev/null
attach
python3 "$BENCH/datacheck.py" check "$dev" "$model"
python3 "$BENCH/datacheck.py" run "$dev" "$model" "$OPS" 3
mica disk detach "$DISK" >/dev/null
mica disk delete "$DISK" -y >/dev/null
echo DONE
exit
}
