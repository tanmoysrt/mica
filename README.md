<p align="center">
  <img src="assets/mica-icon.svg" alt="" width="112" height="112">
</p>

<h1 align="center">mica</h1>

<p align="center"><strong>Block devices whose data lives in an S3 bucket.</strong></p>

mica gives a Linux machine a disk, `/dev/mica/<name>`, whose data lives in an S3 bucket. The machine keeps the newest writes and a cache on its local SSD, so reads and writes run at local speed. The bucket holds the full disk, so the disk can move to another machine, and its size has no limit but the bucket.

- **Move a disk.** Detach it on one machine and attach it on another. The new machine downloads only what it reads.
- **Snapshots that cost nothing.** A snapshot names a version. A restore makes a new disk from it, copied inside the bucket.
- **A data disk for a VM.** Give `/dev/mica/<name>` to Firecracker, QEMU or a container, or mount it on the host.
- **A large disk for a laptop.** mica attaches it at each boot, and fstab mounts it. Use it with no sudo.

## Quick start

You need Linux 6.16 or later with `ublk_drv`, systemd, and an S3 bucket with conditional writes (AWS S3, Cloudflare R2, MinIO and RustFS have them). To build mica, you also need Rust and libclang.

```bash
cargo install --git https://github.com/tanmoysrt/mica
sudo cp ~/.cargo/bin/mica /usr/local/bin/
sudo mica service install

cp store.example.toml s3.toml && chmod 600 s3.toml   # then fill in the bucket
sudo mica disk create data-1 --size 20G --store s3.toml   # mica keeps the bucket
sudo mica disk attach data-1                      # makes /dev/mica/data-1
sudo mkfs.ext4 /dev/mica/data-1
sudo mkdir -p /mnt/data-1 && sudo mount -o discard /dev/mica/data-1 /mnt/data-1
echo hello | sudo tee /mnt/data-1/hello.txt
sudo umount /mnt/data-1 && sudo mica disk detach data-1
```

`detach` shows the upload, and returns when the bucket has all data. Then the disk is ready to move. On another machine, give the bucket once:

```bash
sudo mica disk attach data-1 --store s3.toml
sudo mount -o discard /dev/mica/data-1 /mnt/data-1
cat /mnt/data-1/hello.txt
```

## Documentation

| Page | What it covers |
|---|---|
| [How mica works](docs/design.md) | The layout in the bucket, writes, reads, transactions, snapshots, crashes |
| [Running mica](docs/operations.md) | Install, commands, laptops, chunk sizes, problems |
| [The daemon API](docs/api.md) | The requests and answers on the socket, for programs |

## Project status

mica is under development. Do not use it for data that you cannot lose.

## License

MIT. See [LICENSE](LICENSE).
