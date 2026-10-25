#!/bin/bash
# Attach latency: stage timings of repeated attach and detach of one disk.
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
# The wall time of the attach request, so builds without stage timings compare too.
for _ in $(seq "$RUNS"); do
    start=$(date +%s%N)
    reply=$(echo "{\"command\":\"attach\",\"disk\":\"$DISK\",\"force\":false}" | socat -t 60 - UNIX-CONNECT:/run/mica-lab.sock)
    wall=$(( ($(date +%s%N) - start) / 1000 ))
    jq -c --argjson wall "$(echo "scale=1; $wall / 1000" | bc)" '(.result.timings_ms // {error: .error}) + {wall: $wall}' <<<"$reply"
    "$LAB/lab.sh" mica disk detach "$DISK" > /dev/null
done | tee /tmp/mica-lab-attach.out
jq -s 'map(.wall) | sort | {runs: length, p50: .[length/2|floor], p95: .[(length*0.95)|floor], min: .[0], max: .[-1]}' /tmp/mica-lab-attach.out | jq -c '. + {stage: "wall ms"}'
jq -s 'map(select(.claim_and_load)) | map(.claim_and_load) | sort | select(length > 0) | {p50: .[length/2|floor], max: .[-1]}' /tmp/mica-lab-attach.out | jq -c '. + {stage: "claim_and_load ms"}'
exit
}
