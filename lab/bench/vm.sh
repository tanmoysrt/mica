#!/bin/bash
# Firecracker on a 100 GiB mica disk: make the disk, boot cold and warm,
# fio in the guest, fill the guest with random files, save, read back cold,
# then boot a clone of a snapshot.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
export FC_DISK=${FC_DISK:-lab-fc}
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
vm() { "$LAB/lab.sh" fc-ssh "$@"; }
source "$LAB/lab.env"
mkdir -p "$OUT"
exec > >(tee -a "$OUT/vm.log") 2>&1

now() { date +%s.%N; }
since() { printf "%.2f" "$(echo "$(now) - $1" | bc)"; }
step() {
    local label=$1 t
    shift
    t=$(now)
    "$@" > /tmp/mica-lab-step.out 2>&1 || { echo "FAILED: $label"; cat /tmp/mica-lab-step.out; exit 1; }
    echo "$label: $(since "$t") s"
}
s3_bytes() { mica status | awk '/Data:/{print $2, $4}'; }
boot() { # label
    local before
    before=$(s3_bytes)
    step "[$1] attach + boot until ssh answers" "$LAB/lab.sh" fc-up
    echo "    S3 downloaded/uploaded before: $before, after: $(s3_bytes)"
}
shutdown() { # label
    step "[$1] guest reboot (Firecracker exits)" "$LAB/lab.sh" fc-down
    step "[$1] detach (waits for the upload)" mica disk detach "$FC_DISK"
}
cold() { mica cache prune -y >/dev/null; "${ROOT[@]}" drop-caches; }

python3 "$BENCH/sampler.py" "$OUT/vm-samples.tsv" &
sampler=$!
trap 'kill $sampler 2>/dev/null; "$LAB/lab.sh" fc-down >/dev/null 2>&1' EXIT

echo "== Firecracker on mica $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1), disk ${DISK_MIBPS} MiB/s"
echo "   commit $(git -C "$LAB/.." rev-parse --short HEAD)$(git -C "$LAB/.." diff --quiet -- src || echo '+dirty') | $(firecracker --version | head -1) | ${VCPUS:-2} vCPU, ${MEM_MIB:-2048} MiB, io_engine ${IO_ENGINE:-Async}"

[[ -f $HOME/.cache/mica-lab/fc/rootfs-${GUEST:-ubuntu}.tar ]] || step "build guest rootfs tarball" "$LAB/lab.sh" fc-image
step "fc-disk: create 100 GiB disk + write rootfs (mke2fs -d)" "$LAB/lab.sh" fc-disk
step "first detach (uploads the rootfs)" mica disk detach "$FC_DISK"

cold; boot "cold boot, empty cache"
vm 'cat /proc/cmdline; df -h /; uname -r' | sed 's/^/    /'
shutdown "after cold boot"

boot "warm boot"
fio_run() { # rw bs depth
    vm "fio --name=x --filename=/root/fio.dat --size=4G --rw=$1 --bs=$2 --iodepth=$3 --ioengine=libaio --direct=1 --runtime=20 --time_based --output-format=json" 2>/dev/null |
        jq -r --arg n "$1 $2 qd$3" '.jobs[0] | "  guest \($n): \((.read.bw_bytes+.write.bw_bytes)/1048576|floor) MiB/s \((.read.iops+.write.iops)|floor) IOPS, lat \((.read.lat_ns.mean+.write.lat_ns.mean)/1000|floor) us"'
}
step "guest: lay out a 4 GiB fio file" vm "fio --name=lay --filename=/root/fio.dat --size=4G --rw=write --bs=1M --iodepth=16 --ioengine=libaio --direct=1"
for x in "randread 4k 32" "randwrite 4k 32" "read 1M 16" "write 1M 16" "randread 4k 1" "randwrite 4k 1"; do fio_run $x; done
vm "fio --name=s --filename=/root/fsync.dat --size=256M --rw=write --bs=4k --fdatasync=1 --runtime=15 --time_based --output-format=json" 2>/dev/null |
    jq -r '.jobs[0] | "  guest 4k write+fdatasync: \(.write.iops|floor)/s, lat \(.write.lat_ns.mean/1000|floor) us"'
vm "rm -f /root/fio.dat /root/fsync.dat; fstrim -v /"

scp -q -i "$HOME/.cache/mica-lab/fc/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    "$BENCH/gen.py" "$BENCH/verify.py" root@172.30.0.2:/root/
t0=$(now)
step "guest: fill 7.93 GiB of random files" vm "python3 /root/gen.py /data"
step "guest: sync" vm "sync"
step "guest: record hashes (2 readers)" vm "python3 /root/verify.py record /data /root/hashes.txt 2"
shutdown "after fill"
echo "start of fill until all data is in S3: $(since "$t0") s"

cold; boot "cold boot after fill"
"${ROOT[@]}" drop-caches
for r in 1 8; do
    step "[cold] guest: read + check all files, $r readers" vm "echo 3 > /proc/sys/vm/drop_caches; python3 /root/verify.py check /data /root/hashes.txt $r"
    tail -1 /tmp/mica-lab-step.out
    [[ $r == 1 ]] && { shutdown "between cold reads"; cold; boot "cold boot for 8 readers"; }
done
shutdown "after cold reads"

step "snapshot $FC_DISK" mica snapshot create "$FC_DISK" lab-fc-base
step "clone from snapshot" mica disk create lab-fc-clone --from-snapshot lab-fc-base
FC_DISK=lab-fc-clone boot "clone boot, cache shared with $FC_DISK"
FC_DISK=lab-fc-clone step "[clone] guest: read + check all files, 8 readers" "$LAB/lab.sh" fc-ssh "python3 /root/verify.py check /data /root/hashes.txt 8"
tail -1 /tmp/mica-lab-step.out
FC_DISK=lab-fc-clone shutdown "clone"
mica status | tail -6
echo DONE
exit
}
