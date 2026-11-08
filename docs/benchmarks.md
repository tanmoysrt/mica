# Speed and cost

This page shows how fast a mica disk is, what it costs each month, and how the crash tests went. All numbers come from the local lab in [lab/](../lab/README.md), on one laptop. Run the same scripts to check them.

## In short

- **Reads** of a database are as fast as on a local file, also from a cold cache with 20 ms to the bucket.
- **Writes** of a database that flushes often run at 40 to 50 % of a local file. The flushes take the time.
- **A quiet 50 GB disk** costs about $0.60 a month on Cloudflare R2. **A database that writes all day** costs about $1 to $13 a month when its data fits in its memory, and about $25 with 4 MiB chunks when it does not.
- **Crashes:** in 6 power cuts and 4 kills of the daemon, no flushed write was lost.

## The setup

| Part | What it is |
|---|---|
| Machine | A laptop with an NVMe SSD, Linux 7.0 |
| VM | Firecracker, 2 vCPUs, 2 GiB of RAM. It boots from a local root disk, and its data disk is a mica disk. |
| Database | MariaDB with 3.1 GB of data: sysbench, 12 tables of 1 million rows, 8 threads, a 512 MiB buffer pool |
| Bucket | RustFS in a container on the same laptop, behind a link shaped to 800 Mbit/s, with a round trip of 1 ms or 20 ms |
| Local | The same disk image as a plain file on the same SSD |
| Cold | A new machine with an empty cache and no fill: each first read goes to the bucket |

The speed runs used the first settings of v2: chunks of 256 KiB and a save every 30 seconds. The defaults are now 1 MiB and 5 minutes, which upload less often.

## Database speed

sysbench transactions per second. Higher is better, except for the p95 latency.

| Run | Read-only tps | Read-write tps | Read-write p95 |
|---|---|---|---|
| Local file (btrfs) | 2,180 | 497–703 | 25–28 ms |
| mica, cold, round trip 1 ms | 2,131–2,203 | 217–242 | 119–128 ms |
| mica, cold, round trip 20 ms | 2,140 | 249 | 98 ms |
| mica, warm cache | 2,229–2,270 | 207–250 | 105–137 ms |
| Local file (ext4) | — | 421–485 | 31–36 ms |
| mica on ext4, warm cache | — | 213–224 | 72–83 ms |

A cold disk is slow only at the start. At 1 ms, the read-only run gave 1,056 tps in its first 10 seconds, and full speed after 30 seconds. At 20 ms, it gave 772 tps in the first 10 seconds, and full speed after 30 seconds.

### Boot

| Boot of the VM | ssh ready | MariaDB ready |
|---|---|---|
| Local root disk, mica data disk, cold | 2.2 s | 2.4 s |
| Root disk and data disk on mica, cold, no fill | 2.3 s | 7.3 s |
| Both on mica, new machine, with the read profile and fill | 2.3 s | 4.4 s |

The read profile is the list of chunks that the last boot read. A new machine downloads them first.

### Why writes are slower than local

The time goes to flushes. In a 60-second run, the syncs of the data file took 55 seconds: about 7,500 flushes of 7 ms each, one at a time. Flushes must run one at a time, because Linux reports a write error only once for each file. Flushes that wait at the same time share one sync.

A mica flush costs more than a flush of a plain file, for three reasons:

1. The VM writes into the page cache of the mica device. At each flush, these pages go through ublk into the data file, and then mica syncs the data file. The data passes the kernel two times.
2. A write into a chunk that is not on the machine first copies the full chunk: 256 KiB for a 16 KiB database page.
3. The data file is sparse. On ext4 or XFS, a write into a hole allocates blocks, so the flush must also commit the journal of the filesystem.

One change helped: saved chunks now stay on the machine, up to the cache limit. On ext4, this made read-write steady at 213–224 tps. Before it, three runs gave 217, 157 and 164 tps, with ten times more chunk copies. With 64 KiB chunks, read-write fell to 120–185 tps.

These runs were on a laptop filesystem that was 94 % full. A run on a server with ext4 or XFS on NVMe is still to do.

## Crash tests

**Daemon kill.** The daemon was killed with SIGKILL two times while a VM wrote files with fsync. After each takeover, all 400 synced files were correct, and again after a full download on a new machine.

**Power cut.** The data folder of the daemon sits on a device-mapper device that can drop every write from one moment on, as a power cut does. A writer fills a disk with blocks that name their own version, and flushes often. At a random moment, the writes stop reaching the device. Then the daemon is killed, the device is deleted, and the folder comes back as the device had it, as after a reboot. The daemon serves the disk again, a detach uploads all, and a check compares every block with the last flush.

| Test | Result |
|---|---|
| Power cuts | 6 of 6: no flushed write lost |
| Daemon kills in the same rig | 4 of 4: no flushed write lost |

These tests found two bugs, both fixed: small files of the daemon written without a sync, and two daemons on one data folder. [How mica works](design.md#crashes-and-restarts) explains the crash rules.

## The cost of a 50 GB disk

A bucket bills three things: the stored bytes, the requests, and at some providers the downloaded bytes. The stored bytes are cheap: 50 GB on Cloudflare R2 costs $0.60 a month. The requests are the part to watch.

Each save uploads each chunk that changed since the last save, once. So the bill follows the number of different chunks that change in each interval, not the number of writes. Two settings of the disk set it: the interval and the chunk size.

### How it was measured

A 3 GB MariaDB database ran sysbench at a fixed rate for 15 minutes. Most writes go to a few hot rows (a Pareto distribution), as in a real app. The kernel traced each write to the data disk, and a script counted the different chunks in each interval. A save also writes one page for each 1,024 chunks, a root and a head.

| Workload | Rate | Written each minute |
|---|---|---|
| Small app | 5 transactions and 2 inserts a second | 19 MiB |
| Business app | 50 transactions and 20 inserts a second | 383 MiB |
| Busy app | 200 transactions and 50 inserts a second | 1.1 GiB |

### Requests

| Workload, all day | First v2: 30 s, 256 KiB | Now: 5 min, 1 MiB | 5 min, 4 MiB |
|---|---|---|---|
| Small app | $67 | $19 | $7 |
| Business app | $923 | $108 | $25 |
| Busy app | $2,136 | $114 | $26 |

USD per month for the requests on Cloudflare R2, after the free million. Add $0.60 for the storage of 50 GB. AWS S3 costs about 15 % more for the same requests.

The new defaults cut the bill by 3 to 19 times. A longer interval helps, because a hot chunk that changes 10 times in 5 minutes uploads once. From the business app up, the bill stops growing with the load: about 2,900 of the 3,100 chunks of 1 MiB change in each 5-minute round. InnoDB writes its changed pages back about every 8 minutes, and the updates of secondary indexes fall all over the tables. So the cost is about one upload of the database for each interval.

For comparison, a 50 GB gp3 disk on AWS EBS costs $4 a month.

### Tune the database

The bill of a database depends most on one thing: does its data fit in its memory? The tuned runs lasted 60 minutes each, long enough for the redo log to fill and wrap. They set `innodb_log_file_size=2G`, `innodb_adaptive_flushing_lwm=50` and `innodb_max_dirty_pages_pct=90`. The 3 GiB buffer pool ran in a VM with 4 GiB of RAM.

| Run | Buffer pool | Written each minute | 1 MiB chunks changed in 5 min | R2, 1 MiB chunks | R2, 4 MiB chunks |
|---|---|---|---|---|---|
| Business app, default settings | 512 MiB | 383 MiB | 2,878 | $108 | $25 |
| Business app, 2 GiB redo log | 512 MiB | 322 MiB | 2,904 | $109 | $26 |
| Business app, 2 GiB redo log, data in memory | 3 GiB | 54 MiB | 138 | $1.10 | free |
| Busy app, default settings | 512 MiB | 1.1 GiB | 3,050 | $114 | $26 |
| Busy app, 2 GiB redo log, data in memory | 3 GiB | 226 MiB | 446 | $13 | $0.21 |

USD per month for the requests on Cloudflare R2, at a 5-minute interval. "Free" means fewer than the free million requests. The runs with default settings lasted 15 minutes.

A larger redo log alone changes nothing. A small buffer pool must write changed pages back to make room, so each round still changes almost the whole database. When the data fits in the buffer pool, InnoDB keeps the changed pages in memory and writes them back rarely. Most writes then go to the redo log, which is sequential and changes few chunks. The bill falls about 100 times for the business app, and about 9 times for the busy app.

### Providers

| Provider | Works with mica | Storage only | Laptop | Business app |
|---|---|---|---|---|
| Cloudflare R2 | Yes: documented `If-Match` and `If-None-Match` | $0.60 | $0.60 | $108 |
| AWS S3 Standard | Yes: documented | $1.15 | $2.36 | $126 |
| Hetzner Object Storage | Probably: not tested | $7.01 | $7.01 | $7.01 |
| Wasabi | Not tested | $7.99 | $7.99 | $7.99 |
| Your own Garage server | Yes, with a warning: it ignores conditional writes | the server | the server | the server |
| Backblaze B2 | No: it refuses conditional writes | $0.28 | — | — |

USD per month, November 2026, with a 5-minute interval and 1 MiB chunks. The laptop writes 1 GB and reads 3 GB from the bucket each day. R2 includes 10 GB, one million writes and ten million reads free each month, and has no download fee. Hetzner is a flat €6.49 for up to 1 TB, with no request fees. Wasabi bills 1 TB at minimum, for at least 90 days.

### What to pick

- **A laptop or a quiet data disk:** Cloudflare R2, about $0.60 a month for 50 GB. The first 10 GB are free.
- **A database that writes all day:** give it a buffer pool that holds its data, and a 2 GiB redo log. In the lab, this cut the bill of the business app from $108 to about $1 a month on R2. When the data cannot fit in memory, make the disk with 4 MiB chunks, for about $25 a month. Or use a store with no request fees, such as Hetzner, or a Garage server of your own.

`mica bucket add` tests a new store first, and tells you if it works with mica.

## Run the benchmarks yourself

The lab runs RustFS in Docker behind a shaped link, a second mica daemon, and Firecracker VMs. [lab/README.md](../lab/README.md) explains the setup and each script.

## Sources

- [Cloudflare R2 pricing](https://developers.cloudflare.com/r2/pricing/) and [R2 S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/)
- [Amazon S3 pricing](https://aws.amazon.com/s3/pricing/) and [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
- [Hetzner Object Storage](https://www.hetzner.com/storage/object-storage/) and [its supported actions](https://docs.hetzner.com/storage/object-storage/supported-actions/)
- [Wasabi pricing](https://wasabi.com/pricing)
- [Amazon EBS pricing](https://aws.amazon.com/ebs/pricing/)
- [Garage](https://garagehq.deuxfleurs.fr/)
- [Backblaze B2 pricing](https://www.backblaze.com/cloud-storage/pricing), and reports that B2 refuses conditional writes: [ClickHouse #127483](https://github.com/ClickHouse/ClickHouse/issues/127483), [put.gd #267](https://github.com/patrick-hudson/put.gd-desktop/pull/267)
- [ublk in the Linux kernel documentation](https://docs.kernel.org/block/ublk.html)
