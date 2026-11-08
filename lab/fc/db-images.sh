#!/bin/bash
# The images of bench/dbwrites.sh: a root disk with MariaDB and sysbench
# (`GUEST=mariadb lab/lab.sh fc-image` makes its rootfs), and a 10 GiB data
# disk with a 3 GB sysbench database: 12 tables of 1 million rows.
set -euo pipefail
LAB=$(cd "$(dirname "$0")/.." && pwd)
CACHE=${MICA_LAB_CACHE:-$HOME/.cache/mica-lab}; IMAGES=${IMAGES:-/var/tmp/mica-lab-db}
TAR=$CACHE/fc/rootfs-mariadb.tar
[[ -f $TAR ]] || GUEST=mariadb "$LAB/lab.sh" fc-image
mkdir -p "$IMAGES"; T=$(mktemp -d "$IMAGES/tree.XXXX")
truncate -s 4G "$IMAGES/root.ext4"; truncate -s 10G "$IMAGES/data.ext4"
# One fakeroot session, so mke2fs keeps the owners of the files.
fakeroot sh -c "tar -xpf '$TAR' -C '$T' \
  && echo '/dev/vdb /var/lib/mysql ext4 noatime,nofail,x-systemd.device-timeout=10s 0 0' > '$T/etc/fstab' \
  && mkdir -p '$T/etc/systemd/system/mariadb.service.d' \
  && printf '[Unit]\nRequiresMountsFor=/var/lib/mysql\n' > '$T/etc/systemd/system/mariadb.service.d/mount.conf' \
  && mke2fs -q -F -t ext4 -L lab-data -E root_owner=\$(stat -c %u:%g '$T/var/lib/mysql') -d '$T/var/lib/mysql' '$IMAGES/data.ext4' \
  && rm -rf '$T/var/lib/mysql'/* && mke2fs -q -F -t ext4 -L lab-root -d '$T' '$IMAGES/root.ext4'"
chmod -R u+w "$T" && rm -rf "$T"
echo "images in $IMAGES. Fill the database once with: lab/bench/dbwrites.sh prepare"
