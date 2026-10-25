#!/bin/bash
# The full lab suite: host, Firecracker (Ubuntu and Alpine), memory snapshots.
#   bench/suite.sh <results folder>
# For a v0.1 build: SKIP_FILL=1 ATTACH_ARGS=" " bench/suite.sh <folder>
# Parsed as one block, so an edit while it runs cannot break it.
{
set -uo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd)
OUT=$1
mkdir -p "$OUT/alpine"
"$BENCH/host.sh" "$OUT"
"$BENCH/vm.sh" "$OUT"
"$BENCH/snap.sh" "$OUT"
GUEST=alpine FC_DISK=lab-fca "$BENCH/vm.sh" "$OUT/alpine"
exit
}
