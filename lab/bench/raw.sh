#!/bin/bash
# Raw capacity under the lab caps: the shaped S3 link, and the local disk
# with the same 300 MiB/s limit that mica-lab.service has.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
LAB=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$LAB/results/$(date +%F)-local}/raw.log
source "$LAB/lab.env"
mkdir -p "$(dirname "$OUT")"
exec > >(tee -a "$OUT") 2>&1
echo "== raw capacity $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1), disk ${DISK_MIBPS} MiB/s"

ping -c 10 -q 172.31.99.10 | tail -1
warp() {
    docker run --rm --network mica-lab minio/warp:v1.1.4 "$1" --host 172.31.99.10:9000 \
        --access-key micalab --secret-key micalab-secret --bucket warp-test --obj.size 4MiB \
        --concurrent "$2" --duration 20s --quiet 2>&1 | grep -E "Average|Reqs:" | sed "s/^/  [$1 c=$2] /"
}
for c in 1 4 16 32; do warp put $c; warp get $c; done

s5() { docker run --rm --network mica-lab -v /dev/shm:/shm -e AWS_ACCESS_KEY_ID=micalab -e AWS_SECRET_ACCESS_KEY=micalab-secret \
    peakcom/s5cmd:v2.3.0 --endpoint-url http://172.31.99.10:9000 "$@"; }
head -c 1G /dev/urandom > /dev/shm/lab-r1g
for c in 1 4 16; do
    t=$(date +%s.%N); s5 cp -c $c -p 50 /shm/lab-r1g s3://mica-lab/raw/r1g >/dev/null
    echo "  s5cmd upload 1GiB, $c streams: $(echo "$(date +%s.%N) - $t" | bc) s"
done
for c in 1 4 16; do
    t=$(date +%s.%N); s5 cat -c $c s3://mica-lab/raw/r1g > /dev/null
    echo "  s5cmd download 1GiB, $c streams: $(echo "$(date +%s.%N) - $t" | bc) s"
done
s5 rm s3://mica-lab/raw/r1g >/dev/null; rm -f /dev/shm/lab-r1g

dir=$HOME/.cache/mica-lab/fio; mkdir -p "$dir"
cap="/dev/nvme0n1:${DISK_MIBPS}mb"
fio_run() { # rw bs depth [extra]
    docker run --rm --device-read-bps "$cap" --device-write-bps "$cap" -v "$dir:/work:Z" mica-lab-tools \
        fio --name=x --filename=/work/f --size=4G --rw=$1 --bs=$2 --iodepth=$3 --ioengine=libaio --direct=1 \
        --runtime=20 --time_based --output-format=json ${4:-} 2>/dev/null |
        jq -r --arg n "$1 $2 qd$3" '.jobs[0] | "  disk \($n): \((.read.bw_bytes+.write.bw_bytes)/1048576|floor) MiB/s \((.read.iops+.write.iops)|floor) IOPS, lat \((.read.lat_ns.mean+.write.lat_ns.mean)/1000|floor) us"'
}
for x in "randread 4k 32" "randwrite 4k 32" "read 1M 16" "write 1M 16" "randread 4k 1" "randwrite 4k 1"; do fio_run $x; done
docker run --rm --device-write-bps "$cap" -v "$dir:/work:Z" mica-lab-tools \
    fio --name=s --filename=/work/g --size=256M --rw=write --bs=4k --fdatasync=1 --runtime=15 --time_based --output-format=json 2>/dev/null |
    jq -r '.jobs[0] | "  disk 4k write+fdatasync: \(.write.iops|floor)/s, lat \(.write.lat_ns.mean/1000|floor) us"'
rm -rf "$dir"
exit
}
