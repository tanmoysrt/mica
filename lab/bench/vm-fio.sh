#!/bin/bash
# fio in a Firecracker guest on a mica disk: right after the guest writes a
# 4 GiB file (the upload of it may run), and again when nothing waits for
# upload. Separates the cost of overlapping uploads from steady I/O speed.
#   bench/vm-fio.sh <results folder>
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
export FC_DISK=${FC_DISK:-lab-fio}
mica() { "$LAB/lab.sh" mica "$@"; }
vm() { "$LAB/lab.sh" fc-ssh "$@"; }
mkdir -p "$OUT"
exec > >(tee -a "$OUT/vm-fio.log") 2>&1
trap '"$LAB/lab.sh" fc-down >/dev/null 2>&1' EXIT

echo "== guest fio $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1) | ${LABEL:-}"
"$LAB/lab.sh" fc-disk >/dev/null
"$LAB/lab.sh" fc-up >/dev/null
# With "settle", each test starts when nothing waits for upload.
settle() {
    [[ ${1:-} == settle ]] || return 0
    vm sync
    "$LAB/lab.sh" save "$FC_DISK"
}
matrix() { # [settle]
    for x in "randread 4k 32" "randwrite 4k 32" "randread 4k 1" "randwrite 4k 1"; do
        set -- $x ${MODE:-}
        settle "${4:-}"
        vm "fio --name=x --filename=/root/fio.dat --size=4G --rw=$1 --bs=$2 --iodepth=$3 --ioengine=libaio --direct=1 --runtime=15 --time_based --output-format=json" 2>/dev/null |
            jq -r --arg n "$1 $2 qd$3" '.jobs[0] | "  \($n): \((.read.iops+.write.iops)|floor) IOPS, lat \((.read.lat_ns.mean+.write.lat_ns.mean)/1000|floor) us"'
    done
    settle "${MODE:-}"
    vm "fio --name=s --filename=/root/fsync.dat --size=256M --rw=write --bs=4k --fdatasync=1 --runtime=15 --time_based --output-format=json" 2>/dev/null |
        jq -r '.jobs[0] | "  4k write+fdatasync: \(.write.iops|floor)/s"'
}
vm "fio --name=lay --filename=/root/fio.dat --size=4G --rw=write --bs=1M --iodepth=16 --ioengine=libaio --direct=1" >/dev/null 2>&1
echo "[right after a 4 GiB write]"
matrix
vm sync
"$LAB/lab.sh" save "$FC_DISK"
echo "[nothing waits for upload, before each test]"
MODE=settle matrix
"$LAB/lab.sh" fc-down >/dev/null
mica disk detach "$FC_DISK" >/dev/null
mica disk delete "$FC_DISK" >/dev/null
echo DONE
exit
}
