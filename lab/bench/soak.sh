#!/bin/bash
# Many attached disks over time. Each round: a writer on every disk, GC during
# the writes, a latency probe, then every block is checked. One round in three
# restarts the daemon mid-write. Half the disks detach and attach again.
#   bench/soak.sh <results folder>     (DISKS, ROUNDS, ROUND_SECS, BLOCKS)
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
OUT=${1:-$LAB/results/$(date +%F)-soak}
DISKS=${DISKS:-32}
ROUNDS=${ROUNDS:-6}
ROUND_SECS=${ROUND_SECS:-90}
BLOCKS=${BLOCKS:-16384}
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
IO=$LAB/crash/crashio.py
PROBE=lab-soak-probe
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT/work"
exec > >(tee -a "$OUT/soak.log") 2>&1

name() { echo "lab-soak-$1"; }
attach() { mica disk attach "$1" >/dev/null && "${ROOT[@]}" own "$1" >/dev/null; }
wait_daemon() { until mica status >/dev/null 2>&1; do sleep 0.3; done; }
wait_attached() { until [[ -e /dev/mica/$1 ]] && [[ -w /dev/mica/$1 ]]; do "${ROOT[@]}" own "$1" >/dev/null 2>&1; sleep 0.3; done; }
mem_available() { awk '/^MemAvailable/ {print int($2 / 1024)}' /proc/meminfo; }
kernel_mem() { awk '/^(Slab|KernelStack|PageTables|Percpu)/ {sum += $2} END {print int(sum / 1024)}' /proc/meminfo; }
daemon_rss() { awk '/^VmRSS/ {print int($2 / 1024)}' "/proc/$(systemctl show -p MainPID --value mica-lab)/status"; }
report() { echo "  $1: daemon RSS $(daemon_rss) MiB, kernel $(kernel_mem) MiB, available $(mem_available) MiB, threads $(ls "/proc/$(systemctl show -p MainPID --value mica-lab)/task" | wc -l)"; }

echo "== soak $(date -Is) | ${LABEL:-} | $DISKS disks, $ROUNDS rounds of $ROUND_SECS s"
wait_daemon
report "before attach"
kernel_before=$(kernel_mem); available_before=$(mem_available)
mica disk create "$PROBE" --size 1G >/dev/null; attach "$PROBE"
for i in $(seq "$DISKS"); do
    mica disk create "$(name "$i")" --size 1G >/dev/null
    attach "$(name "$i")"
done
report "attached, idle"
echo "  in all, $((DISKS + 1)) disks: $(($(kernel_mem) - kernel_before)) MiB kernel, $((available_before - $(mem_available))) MiB available"
for i in $(seq "$DISKS"); do
    python3 "$IO" prefill "/dev/mica/$(name "$i")" "$BLOCKS" "$OUT/work/state-$i" >/dev/null &
done
wait
report "prefilled"

failed=0
for round in $(seq "$ROUNDS"); do
    echo "-- round $round $(date +%T)"
    writers=()
    for i in $(seq "$DISKS"); do
        python3 "$IO" write "/dev/mica/$(name "$i")" "$BLOCKS" "$OUT/work/log-$i-$round" $((round * 1000000000)) &
        writers+=($!)
    done
    sleep $((ROUND_SECS / 3))
    report "writing"
    echo "  probe, write+fdatasync: $(python3 "$BENCH/syncwrite.py" "/dev/mica/$PROBE" 0 256 10)"
    started=$(date +%s.%N)
    mica gc -y --grace 1h >/dev/null 2>&1 || echo "  gc failed"
    printf '  gc: %.1f s\n' "$(echo "$(date +%s.%N) - $started" | bc)"
    if ((round % 3 == 0)); then
        started=$(date +%s.%N)
        "${ROOT[@]}" restart >/dev/null
        wait_daemon
        printf '  daemon restart under load: %.1f s\n' "$(echo "$(date +%s.%N) - $started" | bc)"
    fi
    sleep $((ROUND_SECS / 3))
    kill -9 "${writers[@]}" 2>/dev/null
    wait 2>/dev/null
    for i in $(seq 2 2 "$DISKS"); do
        mica disk detach "$(name "$i")" >/dev/null || { echo "  detach $i failed"; failed=$((failed + 1)); }
    done
    for i in $(seq 2 2 "$DISKS"); do attach "$(name "$i")"; done
    for i in $(seq "$DISKS"); do
        python3 "$IO" check "/dev/mica/$(name "$i")" "$BLOCKS" "$OUT/work/state-$i" "$OUT/work/log-$i-$round" acked > "$OUT/work/check-$i-$round" 2>&1 || {
            echo "  disk $i: $(tail -1 "$OUT/work/check-$i-$round")"
            failed=$((failed + 1))
        }
    done
    echo "  checked: $(cat "$OUT"/work/check-*-"$round" | grep -c ' 0 bad') of $DISKS disks with 0 bad"
    report "after check"
done

"${ROOT[@]}" logs 5000 > "$OUT/daemon.log"
echo "  log: $(grep -c ' ERROR ' "$OUT/daemon.log") errors, $(grep -c ' WARN ' "$OUT/daemon.log") warnings"
for i in $(seq "$DISKS"); do
    mica disk detach "$(name "$i")" >/dev/null
    mica disk delete "$(name "$i")" -y >/dev/null
done
mica disk detach "$PROBE" >/dev/null; mica disk delete "$PROBE" -y >/dev/null
report "all detached"
rm -rf "$OUT/work"
echo "DONE: $failed failures"
exit
}
