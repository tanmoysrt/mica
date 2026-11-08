#!/bin/bash
# Attach several disks at the same time; report the wall time of the slowest.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
LAB=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$LAB/results/$(date +%F)-local}
exec > >(tee -a "$OUT/attach.log") 2>&1
echo "== parallel attach of $* $(date -Is) | ${LABEL:-}"
for round in 1 2 3; do
    start=$(date +%s%N)
    for disk in "$@"; do
        "$LAB/lab.sh" mica disk attach "$disk" > /dev/null 2> "/tmp/mica-lab-pa-$disk" &
    done
    wait
    echo "round $round: all $# attached after $(( ($(date +%s%N) - start) / 1000000 )) ms; errors: $(cat /tmp/mica-lab-pa-* | grep -c error)"
    for disk in "$@"; do "$LAB/lab.sh" mica disk detach "$disk" > /dev/null; done
done
exit
}
