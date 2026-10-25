# Guest root filesystem for the lab VM. `lab.sh fc-image` exports it to a
# tarball, and `mke2fs -d` writes it onto the mica disk.
FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        systemd systemd-sysv udev openssh-server iproute2 iputils-ping fio xxhash python3 openssl \
        e2fsprogs xfsprogs util-linux procps kmod less \
    && rm -rf /var/lib/apt/lists/*
RUN echo lab-vm > /etc/hostname \
    && passwd -d root \
    && mkdir -p /root/.ssh && chmod 700 /root/.ssh \
    && sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config \
    && systemctl enable systemd-networkd ssh \
    && systemctl mask apt-daily.timer apt-daily-upgrade.timer \
    && printf '[Match]\nName=eth0\n\n[Network]\nAddress=172.30.0.2/30\nGateway=172.30.0.1\n' > /etc/systemd/network/10-eth0.network
COPY authorized_keys /root/.ssh/authorized_keys
