#!/bin/bash
# FLUSH cost on the host, without a guest: fio with 4 KiB writes and an
# fdatasync after each, straight on the mica device.
#   1. idle: nothing else runs
#   2. upload: a 2 GiB write was just made, its checkpoint runs
#   3. rewrite: random 4 KiB writes over chunks the last checkpoint saved
#   bench/flush.sh <results folder>     (LABEL names the build)
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
DISK=lab-flush
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT"
exec > >(tee -a "$OUT/flush.log") 2>&1
dev=/dev/mica/$DISK

sw() { # label args...
    local label=$1
    shift
    echo "  $label: $(python3 "$BENCH/syncwrite.py" "$dev" "$@")"
}
settle() { "$LAB/lab.sh" save "$DISK"; }

echo "== flush on the host $(date -Is) | ${LABEL:-}"
mica disk create "$DISK" --size 8G >/dev/null
mica disk attach "$DISK" >/dev/null
"${ROOT[@]}" own "$DISK" >/dev/null
sw "idle, write+fdatasync" 6144 256 15
dd if=/dev/urandom of="$dev" bs=1M count=2048 oflag=direct status=none
sw "during upload, write+fdatasync" 6400 256 15
settle
sw "rewrite saved chunks, randwrite+fdatasync" 0 2048 15 rand
sw "rewrite saved chunks, randwrite, no sync" 0 2048 15 rand --nosync
settle
mica disk detach "$DISK" >/dev/null
mica disk delete "$DISK" >/dev/null
echo DONE
exit
}
