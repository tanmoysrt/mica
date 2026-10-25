#!/bin/bash
# v0.2 against v0.1 at one link round trip: the full suite for each build.
#   bench/compare.sh <rtt ms> <results folder>
# v0.1 runs with its own defaults (180 s checkpoints, no fill), under the
# empty bucket prefix; v0.2 under "v2z". The lab ends on v0.2 at 1 ms.
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd)
LAB=$(dirname "$BENCH")
RTT=$1
OUT=$2
BASE=${BASE_BINARY:-$HOME/.cache/mica-lab/mica-v0.1}
mica() { "$LAB/lab.sh" mica "$@"; }
mkdir -p "$OUT"

clean() {
    for disk in $(mica disk ls -y 2>/dev/null | awk 'NR>1 {print $1}'); do
        mica disk detach "$disk" >/dev/null 2>&1
        mica disk delete "$disk" -y >/dev/null
    done
    for name in $(mica snapshot ls -y 2>/dev/null | awk 'NR>1 {print $1}'); do
        mica snapshot delete "$name" -y >/dev/null
    done
}

RTT_MS=$RTT "$LAB/lab.sh" shape
OPS=get "$BENCH/objsize.sh" "$OUT"

clean
"$BENCH/suite.sh" "$OUT/v0.2"

cp "$LAB/config.toml" "$OUT/config-v0.2.toml"
sed -i 's/^checkpoint_interval_secs = .*/checkpoint_interval_secs = 180/' "$LAB/config.toml"
PREFIX="" "$LAB/lab.sh" deploy "$BASE"
clean
SKIP_FILL=1 ATTACH_ARGS=" " "$BENCH/suite.sh" "$OUT/v0.1"
clean

cp "$OUT/config-v0.2.toml" "$LAB/config.toml"
PREFIX=v2z "$LAB/lab.sh" deploy
RTT_MS=1 "$LAB/lab.sh" shape
echo "compare done"
exit
}
