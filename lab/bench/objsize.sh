#!/bin/bash
# S3 throughput and latency by object size: the cost side of smaller chunks.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
LAB=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$LAB/results/$(date +%F)-local}
source "$LAB/lab.env"
mkdir -p "$OUT"
exec > >(tee -a "$OUT/objsize.log") 2>&1
echo "== S3 by object size $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1)"
for size in 64KiB 256KiB 4MiB; do
    for c in 1 8 32 64; do
        for op in ${OPS:-put get}; do
            docker run --rm --network mica-lab minio/warp:v1.1.4 "$op" --host 172.31.99.10:9000 \
                --access-key micalab --secret-key micalab-secret --bucket warp-size --obj.size "$size" \
                --concurrent "$c" --duration 15s $([[ $op == get ]] && echo --objects 400) --quiet 2>&1 |
                awk -v tag="$op $size c=$c" '/Average:/{a=$0} /Reqs: Avg:/{r=$0} END{gsub(/^ *\* /,"",a); gsub(/^ *\* /,"",r); print tag ": " a " | " r}'
        done
    done
done
exit
}
