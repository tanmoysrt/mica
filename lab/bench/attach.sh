#!/bin/bash
# Attach latency: the wall time of repeated attach and detach of one disk.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
LAB=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$LAB/results/$(date +%F)-local}
DISK=${1:-lab-fc}
RUNS=${RUNS:-10}
mkdir -p "$OUT"
exec > >(tee -a "$OUT/attach.log") 2>&1
echo "== attach $DISK x$RUNS $(date -Is) | $(git -C "$LAB/.." rev-parse --short HEAD)$(git -C "$LAB/.." diff --quiet -- src || echo '+dirty') | ${LABEL:-}"
for _ in $(seq "$RUNS"); do
    start=$(date +%s%N)
    error=$("$LAB/lab.sh" mica disk attach "$DISK" 2>&1 >/dev/null)
    wall=$(( ($(date +%s%N) - start) / 1000 ))
    jq -nc --argjson wall "$(echo "scale=1; $wall / 1000" | bc)" --arg error "$error" '{wall: $wall} + (if $error == "" then {} else {error: $error} end)'
    "$LAB/lab.sh" mica disk detach "$DISK" > /dev/null
done | tee /tmp/mica-lab-attach.out
jq -s 'map(.wall) | sort | {runs: length, p50: .[length/2|floor], p95: .[(length*0.95)|floor], min: .[0], max: .[-1]}' /tmp/mica-lab-attach.out | jq -c '. + {stage: "wall ms"}'
exit
}
