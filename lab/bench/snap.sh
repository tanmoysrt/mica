#!/bin/bash
# Firecracker memory snapshots on mica. One VM boots from a mica disk, starts
# an app that holds 384 MiB in RAM, and is paused and snapshotted: the memory
# file goes to a second mica disk, the root disk gets a mica snapshot. Then
# each restore variant starts from a clone of that root disk.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
# One benchmark at a time: a second run waits here.
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-local}
CACHE=$HOME/.cache/mica-lab
RUN=$CACHE/fc/run
SRC=lab-snap
MEM=lab-mem
MEMDIR=/mnt/mica-lab/$MEM
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
mica() { "$LAB/lab.sh" mica "$@"; }
vm() { "$LAB/lab.sh" fc-ssh "$@"; }
api() { curl -s --fail-with-body --unix-socket "$RUN/api.sock" -H 'Content-Type: application/json' -X "$1" "http://localhost$2" ${3:+-d "$3"}; }
source "$LAB/lab.env"
mkdir -p "$OUT"
exec > >(tee -a "$OUT/snap.log") 2>&1

now() { date +%s.%N; }
since() { printf "%.2f" "$(echo "$(now) - $1" | bc)"; }
s3_in() { mica status | awk '/Data:/{print $2}'; }
s3_in_mib() { # bytes downloaded so far, in MiB
    echo '{"command":"status"}' | socat - UNIX-CONNECT:/run/mica-lab.sock | jq '.result.s3.bytes_in / 1048576 | floor'
}
cold() { mica cache prune -y >/dev/null; "${ROOT[@]}" drop-caches; }
stop_vm() {
    [[ -f $RUN/pid ]] && kill "$(cat "$RUN/pid")" 2>/dev/null
    sleep 0.5
    rm -f "$RUN/pid" "$RUN/api.sock"
}
trap 'stop_vm; "${ROOT[@]}" tap down' EXIT

echo "== memory snapshots on mica $(date -Is) | $(docker logs mica-lab-shaper 2>&1 | tail -1), disk ${DISK_MIBPS} MiB/s"

# 1. Source VM: boot, start the app, pause, snapshot.
FC_DISK=$SRC "$LAB/lab.sh" fc-disk >/dev/null
FC_DISK=$SRC "$LAB/lab.sh" fc-up
cat > "$RUN/app.py" <<'EOF'
import hashlib, http.server, os
data = bytearray(os.urandom(384 << 20))
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = hashlib.blake2b(data, digest_size=16).hexdigest().encode()
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args): pass
http.server.HTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
EOF
scp -q -i "$CACHE/fc/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$RUN/app.py" root@172.30.0.2:/root/
vm 'systemd-run --unit=app python3 /root/app.py' >/dev/null 2>&1
until want=$(curl -s -m 5 http://172.30.0.2:8080/) && [[ -n $want ]]; do sleep 0.5; done
echo "app ready; hash $want"
vm 'free -m | sed -n 2p' | sed 's/^/    guest memory: /'

mica disk create "$MEM" --size 4G >/dev/null
mica disk attach "$MEM" >/dev/null
"${ROOT[@]}" mount "$MEM" ext4 >/dev/null
t=$(now); api PATCH /vm '{"state":"Paused"}'; echo "pause: $(since "$t") s"
t=$(now); api PUT /snapshot/create "{\"snapshot_type\":\"Full\",\"snapshot_path\":\"$MEMDIR/vmstate\",\"mem_file_path\":\"$MEMDIR/mem\"}"
echo "snapshot create (memory file on a mica disk): $(since "$t") s"
ls -ls --block-size=M "$MEMDIR" | sed 's/^/    /'
t=$(now); mica snapshot create "$SRC" "$SRC-base" >/dev/null; echo "mica snapshot of the root disk (VM paused): $(since "$t") s"
stop_vm
mica disk detach "$SRC" >/dev/null
sync -f "$MEMDIR"
"${ROOT[@]}" umount "$MEM" >/dev/null
t=$(now); before=$(echo '{"command":"status"}' | socat - UNIX-CONNECT:/run/mica-lab.sock | jq '.result.s3.bytes_out')
mica disk detach "$MEM" >/dev/null
after=$(echo '{"command":"status"}' | socat - UNIX-CONNECT:/run/mica-lab.sock | jq '.result.s3.bytes_out')
echo "upload of the memory disk: $(since "$t") s, $(( (after - before) >> 20 )) MiB uploaded (memory file is 2048 MiB)"

# 2. Restores.
n=0
restore() { # label ; env PREFETCH=<chunks>
    local label=$1 disk t0 in0
    n=$((n + 1))
    disk=$SRC-r$n
    in0=$(s3_in_mib)
    t0=$(now)
    mica disk create "$disk" --from-snapshot "$SRC-base" >/dev/null
    mica disk attach "$disk" >/dev/null
    "${ROOT[@]}" own "$disk"
    ln -sfn "/dev/mica/$disk" "$RUN/root-drive"
    if [[ -n ${PREFETCH:-} ]]; then mica disk attach "$MEM" --prefetch "$PREFETCH" >/dev/null; else mica disk attach "$MEM" >/dev/null; fi
    "${ROOT[@]}" mount "$MEM" ext4 >/dev/null
    "${ROOT[@]}" tap up
    local t_attach
    t_attach=$(since "$t0")
    rm -f "$RUN/api.sock"
    setsid firecracker --api-sock "$RUN/api.sock" > "$RUN/console-restore.log" 2>&1 < /dev/null &
    echo $! > "$RUN/pid"
    until [[ -S $RUN/api.sock ]]; do sleep 0.01; done
    local t1 t_load t_ssh t_app got
    t1=$(now)
    if ! api PUT /snapshot/load "{\"snapshot_path\":\"$MEMDIR/vmstate\",\"mem_backend\":{\"backend_type\":\"File\",\"backend_path\":\"$MEMDIR/mem\"},\"resume_vm\":true}"; then
        echo "[$label] snapshot load failed"
        tail -3 "$RUN/console-restore.log"
        stop_vm
        "${ROOT[@]}" umount "$MEM" >/dev/null
        mica disk detach "$MEM" >/dev/null
        mica disk detach "$disk" >/dev/null
        mica disk delete "$disk" -y >/dev/null
        return 1
    fi
    t_load=$(since "$t1")
    until "$LAB/lab.sh" fc-ssh -o ConnectTimeout=1 true 2>/dev/null; do sleep 0.05; done
    t_ssh=$(since "$t1")
    got=$(curl -s -m 120 http://172.30.0.2:8080/)
    t_app=$(since "$t1")
    [[ $got == "$want" ]] || echo "[$label] WRONG app response: '$got'"
    echo "[$label] attach $t_attach s | load $t_load s | ssh $t_ssh s | app response $t_app s | S3 fetched $(( $(s3_in_mib) - in0 )) MiB"
    stop_vm
    "${ROOT[@]}" umount "$MEM" >/dev/null
    mica disk detach "$MEM" >/dev/null
    mica disk detach "$disk" >/dev/null
    mica disk delete "$disk" -y >/dev/null
}

cold; restore "File, cold cache"
restore "File, warm cache"
cold; PREFETCH=512 restore "File, cold cache + mica read-profile prefetch (512 chunks)"
cold; t=$(now); mica disk warm "$MEM" >/dev/null; echo "disk warm of the memory disk: $(since "$t") s"
restore "File, memory disk fully warmed first"

mica status | tail -6
echo DONE
exit
}
