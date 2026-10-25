#!/bin/sh
# Applies DOWN_RATE, UP_RATE and RTT_MS to eth0, then waits.
set -eu
delay_us=$((RTT_MS * 500))

tc qdisc del dev eth0 root 2>/dev/null || true
tc qdisc del dev eth0 ingress 2>/dev/null || true
ip link del ifb0 2>/dev/null || true

shape() { # device rate
    tc qdisc add dev "$1" root handle 1: htb default 10
    tc class add dev "$1" parent 1: classid 1:10 htb rate "$2" ceil "$2" burst 256k cburst 256k quantum 60000
    tc qdisc add dev "$1" parent 1:10 handle 10: netem delay "${delay_us}us" limit 100000
}

# Download: what s3 sends.
shape eth0 "$DOWN_RATE"
# Upload: what s3 receives, redirected through ifb0 so it can be queued.
ip link add ifb0 type ifb
ip link set ifb0 up
tc qdisc add dev eth0 handle ffff: ingress
tc filter add dev eth0 parent ffff: matchall action mirred egress redirect dev ifb0
shape ifb0 "$UP_RATE"

echo "shaped: down $DOWN_RATE, up $UP_RATE, rtt ${RTT_MS}ms"
exec sleep infinity
