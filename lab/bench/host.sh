#!/bin/bash
# mica on the host: fill a disk with random files, save it in S3, then read
# it back cold (empty cache) and warm. Every read pass checks every file.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
DISK=${DISK:-lab-bench}
MNT=/mnt/mica-lab/$DISK
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
source "$LAB/lab.env"
mkdir -p "$OUT"
exec > >(tee -a "$OUT/host.log") 2>&1

now() { date +%s.%N; }
since() { printf "%.2f" "$(echo "$(now) - $1" | bc)"; }
step() { # label command...
    local label=$1 t
    shift
    t=$(now)
    "$@" > /tmp/mica-lab-step.out 2>&1 || { echo "FAILED: $label"; cat /tmp/mica-lab-step.out; exit 1; }
    echo "$label: $(since "$t") s"
}
attach_mount() { mica disk attach "$DISK" && "${ROOT[@]}" mount "$DISK" ext4; }
umount_detach() { "${ROOT[@]}" umount "$DISK" && mica disk detach "$DISK"; }
cold() { "$LAB/lab.sh" cold "$DISK" >/dev/null; }

python3 "$BENCH/sampler.py" "$OUT/host-samples.tsv" &
sampler=$!
trap 'kill $sampler 2>/dev/null' EXIT

echo "== mica on the host $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1), disk ${DISK_MIBPS} MiB/s"
echo "   commit $(git -C "$LAB/.." rev-parse --short HEAD)$(git -C "$LAB/.." diff --quiet -- src || echo '+dirty')"

# Write
step "disk create (20 GiB)" mica disk create "$DISK" --size 20G
step "first attach + mount (formats ext4)" attach_mount
t0=$(now); echo "fill start $t0"
python3 "$BENCH/gen.py" "$MNT"
echo "fill: $(since "$t0") s"
step "sync" sync -f "$MNT"
step "record hashes (16 readers, local data)" python3 "$BENCH/verify.py" record "$MNT" "$OUT/hashes.txt" 16
step "unmount (fstrim + umount)" "${ROOT[@]}" umount "$DISK"
step "detach (waits for the upload)" mica disk detach "$DISK"
echo "start of fill until all data is in S3: $(since "$t0") s"

# Read
round() { # label readers
    step "[$1] attach + mount" attach_mount
    step "[$1] first byte of a 64 MiB file" head -c 1 "$MNT/large/d000/f000000"
    step "[$1] read one 64 MiB file" cat "$MNT/large/d000/f000001"
    step "[$1] read 100 small files one by one" bash -c "for f in \$(ls $MNT/small/d039 | head -100); do cat $MNT/small/d039/\$f > /dev/null; done"
    "${ROOT[@]}" drop-caches
    step "[$1] read + check all files, $2 readers" python3 "$BENCH/verify.py" check "$MNT" "$OUT/hashes.txt" "$2"
    tail -1 /tmp/mica-lab-step.out
    step "[$1] unmount + detach (no file changes)" umount_detach
}
cold; round "cold" 1
cold; round "cold" 8
cold; round "cold" 32
# The background fill: attach with an empty cache, and wait until the cache holds the whole disk.
if [[ ${SKIP_FILL:-0} != 1 ]]; then
cold
fills() { "${ROOT[@]}" logs 2000 | grep -c "disk $DISK: background fill of .* done"; }
before=$(fills)
t=$(now); ATTACH_ARGS=" " attach_mount >/dev/null
until (( $(fills) > before )); do sleep 1; done
echo "background fill: whole disk local after $(since "$t") s ($("${ROOT[@]}" logs 400 | grep "disk $DISK: background fill of" | tail -1 | sed 's/.*background fill of/fill of/'))"
"${ROOT[@]}" drop-caches
step "[after fill] read + check all files, 8 readers" python3 "$BENCH/verify.py" check "$MNT" "$OUT/hashes.txt" 8
tail -1 /tmp/mica-lab-step.out
umount_detach >/dev/null
fi
cold; step "disk warm (8 at a time)" mica disk warm "$DISK"
cold; step "disk warm --parallel 32" mica disk warm "$DISK" --parallel 32
"${ROOT[@]}" drop-caches; round "warm" 1
"${ROOT[@]}" drop-caches; round "warm" 8
mica status | tail -6
echo DONE
exit
}
