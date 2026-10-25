# mica lab

A local test bench for mica. It has three parts:

- **S3:** RustFS in Docker, behind a capped link. `lab.env` sets the download rate, the upload rate and the round trip. The shaper container applies them in the RustFS network namespace, so the host needs no root for this part.
- **Lab daemon:** a second mica daemon, separate from the main `mica.service`. It has its own socket (`/run/mica-lab.sock`), its own data folder (`/var/lib/mica-lab`) and its own bucket. A cgroup `io.max` limit on `mica-lab.service` caps its local SSD (`DISK_MIBPS`, `DISK_IOPS`).
- **VM:** Firecracker boots Ubuntu 24.04 (or Alpine 3.22 with `GUEST=alpine`) from a mica disk (`/dev/mica/lab-fc`, 100 GiB, thin). You reach it with `ssh` on `fc-tap0` (172.30.0.2). The VM has no internet access.

## One-time setup

The root steps are in `mica-lab-root`. Read it, then install a root-owned copy and allow only that file:

```bash
sudo install -o root -g root -m 0755 lab/mica-lab-root /usr/local/sbin/mica-lab-root
echo 'tanmoy ALL=(root) NOPASSWD: /usr/local/sbin/mica-lab-root' | sudo tee /etc/sudoers.d/mica-lab
sudo chmod 0440 /etc/sudoers.d/mica-lab
sudo visudo -cf /etc/sudoers.d/mica-lab
```

`mica-lab-root deploy` (and `install`) copies `target/release/mica` to `/usr/local/libexec/mica-lab/mica`, and `mica-lab.service` runs it as root. Thus, a user who can write to the repository can run code as root. This cannot be avoided when you test development builds of a root daemon.

## Use

```bash
lab/lab.sh up                 # RustFS, cargo build, lab daemon with disk limits
lab/lab.sh fc-disk            # lab-fc: 100 GiB mica disk with the guest rootfs
lab/lab.sh fc-up              # boot the VM (IO_ENGINE=Async|Sync, VCPUS, MEM_MIB)
lab/lab.sh fc-ssh fio --version
lab/lab.sh fc-down
lab/lab.sh mica status        # the mica CLI against the lab daemon
lab/lab.sh shape              # apply new lab.env values live
lab/lab.sh down
```

## Link and disk profiles

`lab.env` holds the defaults: 100 MiB/s in each direction, 1 ms round trip, 300 MiB/s for the lab daemon's disk. Each value can come from the environment:

```bash
RTT_MS=20 lab/lab.sh shape    # 20 ms round trip, applied live
lab/lab.sh shape              # back to lab.env
```

## Benchmarks

Each script writes a log to `lab/results/<date>-local/`, or to the folder you give. Only one runs at a time: a second one waits for the first. Every read pass checks every file against its checksum.

| Script | What it measures |
|---|---|
| `bench/raw.sh` | The lab itself: S3 with warp and s5cmd, the disk with fio |
| `bench/objsize.sh` | S3 throughput and latency for 64 KiB, 256 KiB and 4 MiB objects |
| `bench/attach.sh <disk>` | Attach latency, with stage timings |
| `bench/attach-parallel.sh <disks…>` | Several attaches at the same time |
| `bench/host.sh` | A 7.93 GiB fill on a host mount, the upload, cold and warm reads, the background fill |
| `bench/vm.sh` | Firecracker: cold and warm boot, fio in the guest, a fill in the guest, cold reads, a clone |
| `bench/snap.sh` | Firecracker memory snapshots on a mica disk, restored with the File backend: cold, warm, with prefetch |
| `bench/suite.sh <folder>` | host, vm and snap, then vm with Alpine |
| `bench/compare.sh <ms> <folder>` | The suite for v0.2 and for v0.1 at one round trip |

To run a v0.1 build: `lab/lab.sh deploy <binary>` with `PREFIX=""`, and `SKIP_FILL=1 ATTACH_ARGS=" "` for the suite. The two versions use different formats, so they use different bucket prefixes.
