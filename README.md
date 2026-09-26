<p align="center">
  <img src="assets/mica-icon.svg" alt="" width="112" height="112">
</p>

<h1 align="center">mica</h1>

<p align="center"><strong>Disks that follow your VMs and containers.</strong></p>

mica gives a VM, container, or Linux server a disk it can use on any of your machines. Stop it on one machine and start it on another with the same files, without copying the whole disk first. mica fetches data as it is needed and saves changes in the background.

The disk's shared copy lives in an S3-compatible bucket. Each machine uses its local SSD to keep reads and writes fast.

## What you can do

- **Move a workload between machines.** Detach its disk on one machine, then attach it on another. The new machine downloads only the data it reads.
- **Start many VMs from one image.** Make a snapshot, then give each VM its own disk without copying the image's data.
- **Use a disk like any other Linux disk.** Give it to a VM as a block device, or mount it for a host or container.

## Quick start

You need Linux 6.0 or later, systemd, an S3-compatible bucket, and a local SSD. To build mica, you also need Rust and libclang (`clang-devel` on Fedora; `libclang-dev` on Debian and Ubuntu).

```bash
cargo install --git https://github.com/tanmoysrt/mica
sudo ~/.cargo/bin/mica service setup  # configure storage and start mica

sudo mica disk create data-1 --size 20G
sudo mica disk mount data-1 /mnt/data-1
echo hello | sudo tee /mnt/data-1/hello.txt
sudo mica disk unmount data-1
```

On another machine running mica with the same bucket, mount the disk and read the file:

```bash
sudo mica disk mount data-1 /mnt/data-1
sudo cat /mnt/data-1/hello.txt
```

Unmounting waits until the changes are saved to the bucket, so the disk is ready to move. For configuration and permissions, see the [operations guide](docs/operations.md).

## How it works

mica keeps a disk's data in a shared bucket and uses local storage for speed. It fetches data when needed and uploads changes in the background. When you detach a disk, mica finishes uploading before another machine uses it. See the [architecture guide](docs/architecture.md) for the full design.

## Commands

```text
mica status                          show disks attached here
mica disk      ls | create | attach | mount | unmount | detach | resize | delete
mica snapshot  ls | create | delete
mica gc                              remove unneeded data from the bucket
mica cache     status | prune
mica service   setup | start | stop | restart | status | logs | …
```

The [CLI reference](docs/cli.md) has examples and options for every command.

## Documentation

| Page | What it covers |
|---|---|
| [Architecture](docs/architecture.md) | How reads, writes, snapshots, and uploads work |
| [Reliability](docs/reliability.md) | Data safety and failure recovery |
| [Design](docs/design.md) | Storage formats and design decisions |
| [CLI](docs/cli.md) | Commands and examples |
| [Operations](docs/operations.md) | Setup, configuration, permissions, and troubleshooting |
| [Code map](src/code-map.md) | Where to find the implementation |

## Project status

mica is under development and should not be used for any production workload.

## License

MIT. See [LICENSE](LICENSE).
