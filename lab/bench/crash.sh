#!/bin/bash
# Power-cut test. The crash daemon keeps its data folder on a device that
# can drop every write (mica-lab-root crash). Each run:
#   1. a writer fills the disk with self-describing blocks and FLUSHes
#   2. at a random time, every write to the data folder is lost from then on
#   3. the writer and the daemon are killed, the ublk device deleted, and the
#      data folder mounted again as the device had it: a reboot
#   4. the daemon starts, serves the disk again, and a detach uploads all
#   5. the disk is attached again and every block is checked
#   bench/crash.sh <results folder>
# RUNS, BLOCKS (4 KiB each), FS (btrfs, xfs or ext4), BASELINE (mica-<name>).
# MODE=daemon instead kills only the daemon, mid-write: the next daemon
# takes the device over, the writer goes on, and every write that returned
# must be there, flushed or not.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
REPO=$(dirname "$LAB")
OUT=${1:-$LAB/results/$(date +%F)-crash}
RUNS=${RUNS:-20}
BLOCKS=${BLOCKS:-65536}
DISK=lab-crash
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
IO=$LAB/crash/crashio.py
mica() { "${ROOT[@]}" crash-cli "$@"; }
STORE=$LAB/crash/store.toml
WORK=$OUT/work
mkdir -p "$WORK"
exec > >(tee -a "$OUT/crash.log") 2>&1

wait_daemon() { until mica status >/dev/null 2>&1; do sleep 0.2; done; }
# A restarted daemon serves the disk again with the data that the last run left.
wait_attached() { until mica status --json 2>/dev/null | jq -e --arg d "$DISK" 'any(.disks[]; .disk == $d)' >/dev/null; do sleep 0.5; done; }
attach() {
    mica disk attach "$DISK" >/dev/null
    "${ROOT[@]}" own "$DISK" >/dev/null
}

echo "== crash test $(date -Is) | ${MODE:-power} | ${FS:-btrfs} | ${BASELINE:-current build} | $RUNS runs, $((BLOCKS / 256)) MiB"
"${ROOT[@]}" crash down >/dev/null 2>&1
# A fresh bucket prefix each time: nothing left from an earlier test.
sed -i "s/^prefix = .*/prefix = \"crash-$(date +%s)\"/" "$STORE"
"${ROOT[@]}" crash setup "${FS:-btrfs}" || exit 1
"${ROOT[@]}" crash deploy ${BASELINE:-} || exit 1
"${ROOT[@]}" crash start
wait_daemon
mica bucket add --store "$STORE" >/dev/null || exit 1
mica disk detach "$DISK" >/dev/null 2>&1
mica disk delete "$DISK" >/dev/null 2>&1
# Short intervals, so that cuts often land in the middle of a transaction.
mica disk create "$DISK" --size 1G --interval 3s >/dev/null || exit 1
attach
python3 "$IO" prefill "/dev/mica/$DISK" "$BLOCKS" "$WORK/state" || exit 1
mica disk detach "$DISK" >/dev/null
attach

daemon_run() { # run log
    local run=$1 log=$2 writer delay
    python3 "$IO" write "/dev/mica/$DISK" "$BLOCKS" "$log" $((run * 1000000000)) &
    writer=$!
    delay=$((2 + RANDOM % 19))
    sleep "$delay"
    "${ROOT[@]}" crash kill >/dev/null
    "${ROOT[@]}" crash start
    wait_daemon
    sleep 3
    kill -9 "$writer"
    wait "$writer" 2>/dev/null
    printf 'run %2d: daemon killed after %2d s, ' "$run" "$delay"
    python3 "$IO" check "/dev/mica/$DISK" "$BLOCKS" "$WORK/state" "$log" acked
}

failed=0
for run in $(seq "$RUNS"); do
    if [[ ${MODE:-power} == daemon ]]; then
        daemon_run "$run" "$WORK/run-$run.log" || failed=$((failed + 1))
        "${ROOT[@]}" crash logs 400 > "$WORK/run-$run.daemon.log"
        continue
    fi
    log=$WORK/run-$run.log
    python3 "$IO" write "/dev/mica/$DISK" "$BLOCKS" "$log" $((run * 1000000000)) &
    writer=$!
    delay=$((2 + RANDOM % 19))
    sleep "$delay"
    # The writer stops first: a FLUSH it finished after the cut would count
    # as durable, though the cut dropped its data. Power stops both at once.
    kill -STOP "$writer"
    "${ROOT[@]}" crash cut >/dev/null
    kill -9 "$writer"
    wait "$writer" 2>/dev/null
    "${ROOT[@]}" crash reboot >/dev/null || { echo "run $run: reboot failed"; exit 1; }
    started=$(date +%s.%N)
    "${ROOT[@]}" crash start
    wait_daemon
    wait_attached
    mica disk detach "$DISK" >/dev/null || { echo "run $run: detach failed"; failed=$((failed + 1)); }
    drained=$(echo "$(date +%s.%N) - $started" | bc)
    attach
    printf 'run %2d: cut after %2d s, %4.1f s to upload and detach, ' "$run" "$delay" "$drained"
    python3 "$IO" check "/dev/mica/$DISK" "$BLOCKS" "$WORK/state" "$log" || failed=$((failed + 1))
    "${ROOT[@]}" crash logs 400 > "$WORK/run-$run.daemon.log"
    grep -iE "error|warn|recover|repair" "$WORK/run-$run.daemon.log" | grep -v "devices are paused" | tail -5 | sed 's/^/    /'
done

mica disk detach "$DISK" >/dev/null
mica disk delete "$DISK" >/dev/null
"${ROOT[@]}" crash down >/dev/null
echo "DONE: $failed of $RUNS runs lost data"
exit
}
