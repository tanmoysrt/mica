# Get started

This page installs mica, makes a first disk, and moves the disk to a second machine. It takes about ten minutes. [Use mica](usage.md) covers everything after that.

## What you need

- **Linux 6.16 or later**, with the `ublk_drv` module. Check it with `sudo modprobe ublk_drv`. Fedora 43, Ubuntu 25.10 and later have it.
- **systemd**, and sudo. The mica daemon runs as root.
- **A local SSD** with room for the cache and the newest writes: 5 GiB for each disk with the default limits.
- **An S3 bucket** and a key that can read, write and delete in it.
- **Rust and libclang**, to build mica. There are no release binaries yet. Get Rust from [rustup.rs](https://rustup.rs). libclang is the package `clang-devel` on Fedora and `libclang-dev` on Debian and Ubuntu.

The bucket must keep S3 conditional writes, because mica uses them as the lock of each disk:

| Provider | Works with mica |
|---|---|
| Cloudflare R2, AWS S3, MinIO, RustFS | Yes |
| Garage | Yes, with a warning: it ignores the conditions, so attach each disk on one machine only |
| Backblaze B2 | No: it refuses conditional writes |
| Others | mica tests the bucket when you add it |

For a first try, Cloudflare R2 costs nothing for the first 10 GB. [Speed and cost](benchmarks.md#the-cost-of-a-50-gb-disk) compares the providers.

## 1. Install mica

```bash
cargo install --git https://github.com/tanmoysrt/mica
sudo cp ~/.cargo/bin/mica /usr/local/bin/
sudo mica service install
```

`mica service install` starts the daemon now and at each boot. It also writes a udev rule, so that each disk gets the name `/dev/mica/<disk>`.

## 2. Give mica your bucket

Write a bucket file, `s3.toml`, with your values. Keep it private: `chmod 600 s3.toml`.

```toml
endpoint = "https://<account>.r2.cloudflarestorage.com"
bucket = "my-disks"
prefix = "laptop"          # optional: a folder in the bucket for the disks
region = "auto"
access_key_id = "<id>"
secret_access_key = "<key>"
```

Then give it to mica:

```console
$ sudo mica bucket add --store s3.toml
✔ Added bucket https://<account>.r2.cloudflarestorage.com/my-disks/laptop. It holds 0 disks
  Make a disk in it: sudo mica disk create <disk> --size 50G
```

mica tests the bucket first, and keeps it only when it works. The keys go to `/var/lib/mica/buckets/`, readable by root only. No later command on this machine needs `s3.toml`. Keep a copy for the next machine, in a safe place.

## 3. Make a disk and use it

```console
$ sudo mica disk create data --size 50G
✔ Made disk data: 50G in chunks of 1M
  Attach it: sudo mica disk attach data

$ sudo mica disk attach data
✔ Attached disk data as /dev/mica/data
  The disk is new. Make a filesystem on it once: sudo mkfs.ext4 /dev/mica/data
  Mount it: sudo mkdir -p /mnt/data && sudo mount -o discard /dev/mica/data /mnt/data
```

Run the two commands that mica shows. The disk is now a normal folder:

```bash
sudo mkfs.ext4 /dev/mica/data
sudo mkdir -p /mnt/data && sudo mount -o discard /dev/mica/data /mnt/data
echo hello | sudo tee /mnt/data/hello.txt
```

A 50G disk uses no space in the bucket until you write to it. With the `discard` option, the bucket also frees the chunks of the files that you delete.

To see the disk at work, run `sudo mica status`. Add `--watch` to show it again each second.

## 4. Move the disk to another machine

On the first machine, unmount the disk and detach it. The detach uploads the last changes and shows the time left:

```bash
sudo umount /mnt/data
sudo mica disk detach data
```

On the second machine, install mica (step 1), and copy `s3.toml` there in a safe way, such as with `scp`. Add the bucket, then attach the disk:

```bash
sudo mica bucket add --store s3.toml
sudo mica disk attach data
sudo mkdir -p /mnt/data && sudo mount -o discard /dev/mica/data /mnt/data
cat /mnt/data/hello.txt
```

The second machine downloads only the data that it reads. mica refuses to attach a disk that another machine still has: detach it there first.

## Next steps

- Mount the disk at each boot, or use it for a VM: [Use mica](usage.md).
- Pick the chunk size and the limits for your use: [Use mica](usage.md#limits-and-chunk-size).
- Learn what happens in a crash: [How mica works](design.md#crashes-and-restarts).

## Remove mica

Detach each disk first. Then:

```bash
sudo mica service uninstall
sudo rm /usr/local/bin/mica
```

The uninstall stops the daemon, removes the udev rule, and deletes the bucket keys of the machine. The cache stays in `/var/lib/mica`: delete that folder to free its space. The disks stay in the bucket. To delete a disk from the bucket, run `sudo mica disk delete <disk>` before the uninstall.
