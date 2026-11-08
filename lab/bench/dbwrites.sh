#!/bin/bash
# How many chunks a database changes in each upload interval: the request
# side of the bill. A Firecracker VM runs MariaDB with sysbench at a fixed
# rate; the kernel traces each write to the data disk; chunks.py counts the
# different chunks per interval and prices them.
#   bench/dbwrites.sh <label> <rw tps> <insert tps> [results folder]
#   bench/dbwrites.sh prepare      once, after fc/db-images.sh: fills the database
# MIN (default 15), POOL (InnoDB buffer pool, default 512M), MEM_MIB (2048),
# TUNED=1 (a 2G redo log and lazy page writes). The images come from
# fc/db-images.sh. The data disk is a local file, so no bucket is needed.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
exec 9> /tmp/mica-lab-bench.lock
flock 9
BENCH=$(cd "$(dirname "$0")" && pwd); LAB=$(dirname "$BENCH")
LABEL=$1 RW=${2:-0} INSERTS=${3:-0} OUT=${4:-$LAB/results/$(date +%F)-dbwrites}
MIN=${MIN:-15} IMAGES=${IMAGES:-/var/tmp/mica-lab-db}
CACHE=${MICA_LAB_CACHE:-$HOME/.cache/mica-lab}
SUDO=(sudo); [[ -n ${SUDO_PASSWORD_FILE:-} ]] && SUDO=(sh -c 'sudo -S -p "" "$@" < "$SUDO_PASSWORD_FILE"' sudo)
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
SSH=(ssh -i "$CACHE/fc/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@172.30.0.2)
T=/sys/kernel/tracing/instances/mica-chunks
SB="sysbench --db-driver=mysql --mysql-user=sb --mysql-password=sb --mysql-db=sbtest --tables=12 --table-size=1000000"
RUN=$CACHE/fc/run-db
mkdir -p "$OUT" "$RUN"

cp --reflink=never "$IMAGES/data.ext4" "$IMAGES/data-run.ext4"
LOOP=$("${SUDO[@]}" losetup --find --show "$IMAGES/data-run.ext4"); "${SUDO[@]}" chown "$USER:" "$LOOP"
read -r MAJ MINOR < <(stat -c '%t %T' "$LOOP"); DEV=$(( (16#$MAJ << 20) | 16#$MINOR ))
cleanup() {
    "${SSH[@]}" -o ConnectTimeout=2 'systemctl stop mariadb; sync; reboot' 2>/dev/null
    sleep 3; [[ -f $RUN/pid ]] && kill "$(cat "$RUN/pid")" 2>/dev/null
    "${ROOT[@]}" tap down
    "${SUDO[@]}" sh -c "echo 0 > $T/events/block/block_bio_queue/enable 2>/dev/null; rmdir $T 2>/dev/null; losetup -d $LOOP"
    rm -f "$IMAGES/data-run.ext4"
}
trap cleanup EXIT
"${SUDO[@]}" mkdir -p "$T"
"${SUDO[@]}" sh -c "echo 8192 > $T/buffer_size_kb; echo 0 > $T/tracing_on; echo 'dev == $DEV' > $T/events/block/block_bio_queue/filter; echo 1 > $T/events/block/block_bio_queue/enable"

"${ROOT[@]}" tap up
sed -e "s|@KERNEL@|$CACHE/fc/vmlinux-6.1.155|" -e "s|@ROOT@|$IMAGES/root.ext4|" -e "s|@DATA@|$LOOP|" -e "s|@MEM_MIB@|${MEM_MIB:-2048}|" \
    "$LAB/fc/vm-db.json.in" > "$RUN/vm.json"
rm -f "$RUN/api.sock"
setsid firecracker --api-sock "$RUN/api.sock" --config-file "$RUN/vm.json" > "$RUN/console.log" 2>&1 < /dev/null &
echo $! > "$RUN/pid"
until "${SSH[@]}" -o ConnectTimeout=1 mysqladmin ping >/dev/null 2>&1; do sleep 0.5; done
if [[ $LABEL == prepare ]]; then
    "${SSH[@]}" "mysql -e \"CREATE DATABASE IF NOT EXISTS sbtest; CREATE USER IF NOT EXISTS sb@localhost IDENTIFIED BY 'sb'; GRANT ALL ON sbtest.* TO sb@localhost\" \
        && $SB --threads=4 oltp_read_write prepare | tail -1 && systemctl stop mariadb && sync && du -sh /var/lib/mysql/sbtest"
    mv "$IMAGES/data-run.ext4" "$IMAGES/data.ext4"
    exit
fi
tune="sed -i 's/^innodb_buffer_pool_size=.*/innodb_buffer_pool_size=${POOL:-512M}/' /etc/mysql/mariadb.conf.d/90-lab.cnf"
[[ -n ${TUNED:-} ]] && tune+="; printf '[mysqld]\ninnodb_log_file_size=2G\ninnodb_adaptive_flushing_lwm=50\ninnodb_max_dirty_pages_pct=90\n' > /etc/mysql/mariadb.conf.d/91-tuned.cnf"
"${SSH[@]}" "$tune; systemctl restart mariadb; mysql -e 'select @@innodb_buffer_pool_size, @@innodb_log_file_size'" | tee "$OUT/$LABEL.settings"
# A warm buffer pool, so the trace has the steady writes.
"${SSH[@]}" "$SB --threads=4 --time=120 --rate=$RW --rand-type=pareto oltp_read_write run > /dev/null"

"${SUDO[@]}" sh -c "echo > $T/trace; cat $T/trace_pipe > '$OUT/$LABEL.trace' & echo \$! > '$RUN/cat.pid'; echo 1 > $T/tracing_on"
"${SSH[@]}" "$SB --threads=4 --rate=$RW --rand-type=pareto --time=$((MIN * 60)) oltp_read_write run > /tmp/rw.txt &
    $SB --threads=2 --rate=$INSERTS --time=$((MIN * 60)) oltp_insert run > /tmp/ins.txt & wait
    grep -h -E 'transactions:|95th' /tmp/rw.txt /tmp/ins.txt" | tee "$OUT/$LABEL.sysbench"
"${SUDO[@]}" sh -c "echo 0 > $T/tracing_on; sleep 1; kill \$(cat '$RUN/cat.pid'); chown $USER: '$OUT/$LABEL.trace'"
python3 "$BENCH/chunks.py" "$OUT/$LABEL.trace" | tee "$OUT/$LABEL.chunks"
exit
}
