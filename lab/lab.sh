#!/bin/bash
# Local mica lab: RustFS with a capped link, a separate mica daemon with a
# capped disk, and a Firecracker VM on a mica disk. Root steps go through
# /usr/local/sbin/mica-lab-root (see lab/README.md).
# Parsed as one block, so an edit while it runs cannot break it.
{
set -euo pipefail

LAB=$(cd "$(dirname "$0")" && pwd)
REPO=$(dirname "$LAB")
CACHE=${MICA_LAB_CACHE:-$HOME/.cache/mica-lab}
ROOT=(sudo -n /usr/local/sbin/mica-lab-root)
# The v2 socket is root only, so the CLI runs through the root helper.
MICA=(sudo -n /usr/local/sbin/mica-lab-root cli)
FC_DISK=${FC_DISK:-lab-fc}
FC_SIZE=${FC_SIZE:-100G}
# ubuntu (systemd) or alpine (OpenRC).
GUEST=${GUEST:-ubuntu}
ROOTFS=$CACHE/fc/rootfs-$GUEST.tar
KERNEL=$CACHE/fc/vmlinux-6.1.155
KERNEL_URL=https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155
SSH=(ssh -i "$CACHE/fc/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@172.30.0.2)

# shellcheck source=lab.env
source "$LAB/lab.env"
export DOWN_RATE UP_RATE RTT_MS

s5() {
    docker run --rm --network mica-lab -e AWS_ACCESS_KEY_ID=micalab -e AWS_SECRET_ACCESS_KEY=micalab-secret \
        peakcom/s5cmd:v2.3.0 --endpoint-url http://172.31.99.10:9000 "$@"
}

s3_up() {
    docker compose -f "$LAB/compose.yaml" up -d --build
    until s5 ls >/dev/null 2>&1; do sleep 1; done
    s5 ls s3://mica-lab >/dev/null 2>&1 || s5 mb s3://mica-lab
    docker logs mica-lab-shaper 2>&1 | tail -1
}

up() {
    s3_up
    cargo build --release --manifest-path "$REPO/Cargo.toml"
    "${ROOT[@]}" install
    "${ROOT[@]}" restart
    "${ROOT[@]}" limits "$DISK_MIBPS" "$DISK_IOPS"
    until "${MICA[@]}" status >/dev/null 2>&1; do sleep 0.2; done
    "${MICA[@]}" bucket add --store "$LAB/store.toml" >/dev/null
    "${MICA[@]}" status
}

down() {
    "${ROOT[@]}" stop || true
    docker compose -f "$LAB/compose.yaml" down
}

# New rates from lab.env, applied without restarting S3.
shape() {
    docker compose -f "$LAB/compose.yaml" up -d --force-recreate shaper
    "${ROOT[@]}" limits "$DISK_MIBPS" "$DISK_IOPS"
    docker logs mica-lab-shaper 2>&1 | tail -1
}

# Runs a mica build in the lab daemon: the current tree, or a given binary
# (such as a baseline build). A given binary goes to target/release, and
# src/main.rs is touched, so that the next cargo build replaces it again.
deploy() {
    if [[ -n ${1:-} ]]; then
        cp "$1" "$REPO/target/release/mica"
        touch "$REPO/src/main.rs"
    else
        cargo build --release --manifest-path "$REPO/Cargo.toml"
    fi
    "${ROOT[@]}" deploy
    "${ROOT[@]}" restart
    until "${MICA[@]}" status >/dev/null 2>&1; do sleep 0.2; done
}

# Uploads all changes of a disk now: a snapshot takes them all.
save() {
    local name=save-$$
    "${MICA[@]}" snapshot create "$1" "$name" >/dev/null && "${MICA[@]}" snapshot delete "$1" "$name" >/dev/null
}

# MiB downloaded and uploaded by the attached disks since their attach.
s3() {
    "${MICA[@]}" status --json | jq -r '[.disks[].status.requests] | "\((map(.bytes_in) | add // 0) / 1048576 | floor) \((map(.bytes_out) | add // 0) / 1048576 | floor)"'
}

# A cold start for a detached disk: no cache on the node, no page cache.
cold() {
    "${ROOT[@]}" drop-cache "$1"
    until "${MICA[@]}" status >/dev/null 2>&1; do sleep 0.2; done
    "${ROOT[@]}" drop-caches
}

fc_image() {
    mkdir -p "$CACHE/fc"
    [[ -f $KERNEL ]] || curl -sSfL -o "$KERNEL" "$KERNEL_URL"
    [[ -f $CACHE/fc/id_ed25519 ]] || ssh-keygen -q -t ed25519 -N "" -f "$CACHE/fc/id_ed25519"
    local context
    context=$(mktemp -d)
    case $GUEST in
        alpine) cp "$LAB/fc/Dockerfile.alpine" "$context/Dockerfile" ;;
        mariadb) cp "$LAB/fc/Dockerfile.mariadb" "$context/Dockerfile" ;;
        *) cp "$LAB/fc/Dockerfile" "$context/" ;;
    esac
    cp "$CACHE/fc/id_ed25519.pub" "$context/authorized_keys"
    docker build -q -t "mica-lab-guest-$GUEST" "$context" >/dev/null
    rm -rf "$context"
    local container
    container=$(docker create "mica-lab-guest-$GUEST" /bin/true)
    docker export "$container" > "$ROOTFS"
    docker rm "$container" >/dev/null
    echo "rootfs ($GUEST): $(du -h "$ROOTFS" | cut -f1)"
}

# A blank mica disk with the guest root filesystem written onto it.
fc_disk() {
    [[ -f $ROOTFS ]] || fc_image
    "${MICA[@]}" disk create "$FC_DISK" --size "$FC_SIZE"
    "${MICA[@]}" disk attach "$FC_DISK"
    "${ROOT[@]}" own "$FC_DISK"
    # Fedora's mke2fs cannot read tarballs, so unpack under fakeroot, which
    # keeps root ownership for mke2fs -d in the same session.
    local tree
    tree=$(mktemp -d "$CACHE/fc/tree.XXXX")
    fakeroot sh -c "tar -xpf '$ROOTFS' -C '$tree' && mke2fs -q -F -t ext4 -E nodiscard -L lab-root -d '$tree' /dev/mica/$FC_DISK"
    rm -rf "$tree"
    sync
    echo "$FC_DISK: ext4 root filesystem ($GUEST), $FC_SIZE"
}

fc_up() {
    local engine=${IO_ENGINE:-Async} run=$CACHE/fc/run
    mkdir -p "$run"
    "${MICA[@]}" disk attach "$FC_DISK" >/dev/null
    "${ROOT[@]}" own "$FC_DISK"
    "${ROOT[@]}" tap up
    # A fixed drive path, so a memory snapshot can restore onto a clone:
    # the restore only points this link at the clone's device.
    ln -sfn "/dev/mica/$FC_DISK" "$run/root-drive"
    sed -e "s|@KERNEL@|$KERNEL|" -e "s|@DRIVE@|$run/root-drive|" -e "s|@IO_ENGINE@|$engine|" -e "s|@INIT@|$([[ $GUEST == alpine ]] && echo /sbin/init || echo /lib/systemd/systemd)|" \
        -e "s|@VCPUS@|${VCPUS:-2}|" -e "s|@MEM_MIB@|${MEM_MIB:-2048}|" "$LAB/fc/vm.json.in" > "$run/vm.json"
    rm -f "$run/api.sock"
    local start
    start=$(date +%s.%N)
    setsid firecracker --api-sock "$run/api.sock" --config-file "$run/vm.json" > "$run/console.log" 2>&1 < /dev/null &
    echo $! > "$run/pid"
    until "${SSH[@]}" -o ConnectTimeout=1 true 2>/dev/null; do
        kill -0 "$(cat "$run/pid")" 2>/dev/null || { tail -20 "$run/console.log"; return 1; }
        sleep 0.2
    done
    echo "VM up (io_engine $engine): ssh after $(echo "$(date +%s.%N) - $start" | bc) s"
}

fc_down() {
    local run=$CACHE/fc/run
    if [[ -f $run/pid ]] && kill -0 "$(cat "$run/pid")" 2>/dev/null; then
        # Firecracker exits when the guest reboots; it has no power-off.
        "${SSH[@]}" -o ConnectTimeout=2 'sync; reboot' 2>/dev/null || true
        for _ in $(seq 150); do kill -0 "$(cat "$run/pid")" 2>/dev/null || break; sleep 0.2; done
        kill "$(cat "$run/pid")" 2>/dev/null || true
    fi
    rm -f "$run/pid"
    "${ROOT[@]}" tap down
}

usage() {
    cat >&2 <<EOF
usage: lab.sh <command>
  up            start RustFS, build mica, start the lab daemon with disk limits
  down          stop the lab daemon and RustFS
  shape         apply lab.env again (link rates, latency, disk limits)
  deploy [bin]  build and run the current tree, or run the given mica binary
  mica ...      the mica CLI against the lab daemon
  save <disk>   upload all changes of a disk now
  s3            MiB downloaded and uploaded by the attached disks
  cold <disk>   drop the node cache of a detached disk, and the page cache
  fc-image      build the guest kernel, ssh key and rootfs tarball
  fc-disk       make $FC_DISK ($FC_SIZE) with the root filesystem
  fc-up         boot Firecracker on /dev/mica/$FC_DISK (IO_ENGINE=Async|Sync)
  fc-ssh [cmd]  ssh into the VM
  fc-down       shut the VM down
EOF
    exit 2
}

case ${1:-} in
    up) up ;;
    down) down ;;
    shape) shape ;;
    deploy) shift; deploy "$@" ;;
    mica) shift; "${MICA[@]}" "$@" ;;
    save) save "$2" ;;
    s3) s3 ;;
    cold) cold "$2" ;;
    fc-image) fc_image ;;
    fc-disk) fc_disk ;;
    fc-up) fc_up ;;
    fc-ssh) shift; "${SSH[@]}" "$@" ;;
    fc-down) fc_down ;;
    *) usage ;;
esac
exit
}
