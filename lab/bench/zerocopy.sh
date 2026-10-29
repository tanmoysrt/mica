#!/bin/bash
# Daemon memory and CPU of the ublk data path: load on 8 disks at once.
# For each phase: throughput, daemon CPU per operation, daemon RSS.
#   1. fill: 1 MiB writes into new chunks (the path through tokio)
#   2. 1 MiB reads and 4 KiB random reads of local chunks (the queue thread)
#   3. 1 MiB rewrites and 4 KiB random rewrites of local chunks
#   bench/zerocopy.sh <results folder>     (LABEL names the build)
# The lab disk limit is off while it runs: the copy, not the SSD, is measured.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
DISKS=${DISKS:-8}
MIB=${MIB:-256}
SECONDS_PER_PHASE=${PHASE_SECONDS:-20}
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT"
exec > >(tee -a "$OUT/zerocopy.log") 2>&1
source "$LAB/lab.env"

pid=$(systemctl show -p MainPID --value mica-lab.service)
cpu_ticks() { awk '{print $14 + $15}' "/proc/$pid/stat"; }
rss_mib() { awk -v key="$1:" '$1 == key {printf "%.0f", $2 / 1024}' "/proc/$pid/status"; }
names=() devices=()
for i in $(seq "$DISKS"); do names+=("lab-zc$i") devices+=("/dev/mica/lab-zc$i"); done

phase() { # label load.py args...
    local label=$1 before after result ops
    shift
    before=$(cpu_ticks)
    result=$(python3 "$BENCH/load.py" "$@" "${devices[@]}") || { echo "  $label: load failed"; exit 1; }
    after=$(cpu_ticks)
    ops=${result%% ops*}
    printf '  %-28s %-34s cpu %6.2f s  %5.1f us/op  rss %5s MiB\n' "$label" "${result#* ops }" \
        "$(echo "($after - $before) / 100" | bc -l)" "$(echo "($after - $before) * 10000 / $ops" | bc -l)" "$(rss_mib VmRSS)"
}

echo "== zero-copy data path, $DISKS disks, $(date -Is) | ${LABEL:-}"
# Only disks that this run made are deleted at the end.
created=()
cleanup() {
    for name in "${created[@]}"; do
        mica disk detach "$name" >/dev/null 2>&1
        mica disk delete "$name" -y >/dev/null
    done
    "${ROOT[@]}" limits "$DISK_MIBPS" "$DISK_IOPS" >/dev/null
}
trap cleanup EXIT
"${ROOT[@]}" limits 0 0 >/dev/null
for name in "${names[@]}"; do
    mica disk create "$name" --size 4G >/dev/null || { echo "  cannot create $name; does it exist?"; exit 1; }
    created+=("$name")
    mica disk attach "$name" >/dev/null && "${ROOT[@]}" own "$name" >/dev/null || exit 1
done
echo "  rss after attach: $(rss_mib VmRSS) MiB"
phase "fill, 1M write, new chunks" write 1024 4 "$MIB" 0
for name in "${names[@]}"; do mica disk checkpoint "$name" >/dev/null; done
phase "1M read, local" read 1024 4 "$MIB" "$SECONDS_PER_PHASE"
phase "4K randread, local" randread 4 8 "$MIB" "$SECONDS_PER_PHASE"
phase "1M write, local" write 1024 4 "$MIB" "$SECONDS_PER_PHASE"
phase "4K randwrite, local" randwrite 4 8 "$MIB" "$SECONDS_PER_PHASE"
echo "  peak rss: $(rss_mib VmHWM) MiB"
echo DONE
exit
}
