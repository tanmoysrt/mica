# How mica works

mica gives a Linux machine a block device whose data lives in an S3 bucket.
The machine keeps the newest writes and a cache on its local disk. A disk
can move to another machine: detach it on one machine, attach it on the
other, and the new machine downloads only the data that it reads.

This document explains the design from the start. Each section uses only
the words of the sections before it.

## The parts

- **Node.** A Linux machine that runs mica. It has a local SSD.
- **Bucket.** An S3 bucket. It holds the data of the disks. Each disk can
  use its own bucket, with its own credentials.
- **Daemon.** One `mica daemon` on each node, run by systemd as root. It
  serves the attached disks.
- **Device.** For each attached disk, the daemon makes a ublk block device,
  `/dev/ublkbN`, with the link `/dev/mica/<disk>`. ublk is a Linux driver
  that sends the block requests of a device to a program.
- **CLI.** The `mica` command. It sends each request to the daemon on a
  Unix socket, `/run/mica.sock`. Only root can use the socket.

```text
 program or VM ──► /dev/mica/data ──► ublk driver ──► mica daemon ──► S3 bucket
                                                           │
                                                           ▼
                                                     local SSD: newest writes, cache
```

## Words

- **Chunk.** The disk is a row of chunks of the same size: 64 KiB to 4 MiB,
  256 KiB by default. A chunk is the unit of upload and download.
- **Entry.** The name of the stored content of one chunk: a birth (the
  transaction that wrote it) and the SHA-256 of its content. Birth 0 is the
  zero entry: a chunk of zeros. Zero chunks are never stored.
- **Page.** The entries of 1,024 chunks, as one object.
- **Root.** The entries of all pages of one version of the disk.
- **Transaction.** One upload of changed chunks, pages and a root. Each
  transaction makes a new version. Transactions have numbers: 1, 2, 3 and so on.
- **Head.** One small object that names the current version, the owner and
  the snapshots. It is the only object that mica writes more than once.
- **Snapshot.** A name for one version, kept until it is deleted.
- **Owner.** The node that has the disk attached. Only the owner writes.
- **Local chunk.** A chunk whose current content is in the data file of
  the node. Guest writes go to local chunks.
- **Unsaved chunk.** A local chunk whose content is not in the bucket yet.
- **Cache.** Files of clean chunks on the node, by hash. A clean chunk has
  the same content as an object in the bucket.

## The layout in the bucket

```text
<prefix>/d/<disk>/head
<prefix>/d/<disk>/<transaction>/root
<prefix>/d/<disk>/<transaction>/p/<page>
<prefix>/d/<disk>/<transaction>/c/<chunk>
```

The transaction number has 16 hexadecimal digits. Each object goes into the
folder of the transaction that wrote it, and is never written again. Thus
an entry `(birth, hash)` of chunk `k` names the key `<birth>/c/<k>`.

| Object | Content | Compressed |
|---|---|---|
| Chunk | the bytes of the chunk | zstd level 1 |
| Page | `MPAG`, a count, 1,024 entries of 40 bytes | zstd level 3 |
| Root | `MICA`, version 2, chunk size, disk size, transaction, parent, time, then one entry for each page | no |
| Head | JSON | no |

A disk of 1 TiB with 256 KiB chunks has 4,096 pages. Its root has 160 KiB.
A page is never stored when all its entries are zero. So a new disk has
an empty root, and a disk uses space in the bucket only for chunks that
are not zeros.

## The head is the lock

S3 has conditional writes. `If-None-Match: *` writes only if the key does
not exist. `If-Match: <etag>` writes only if the object did not change
since it was read. mica writes the head only with one of these two. Before
`mica disk create`, mica checks that the bucket obeys both, and refuses a
bucket that does not.

The head holds:

```json
{
  "id": "4f0c…",            // random, for the life of the disk
  "transaction": 41,        // the current version
  "next": 42,               // the folder of the next transaction
  "owner": { "node": "node-23", "id": "b0a0…", "since": 1794094644 },
  "lost": [],               // folders of transactions that never finished
  "snapshots": { "before-upgrade": { "transaction": 37, "time": 1794090000 } },
  "size": 10737418240, "chunk_size": 262144,
  "limits": { "cache_bytes": 1073741824, "unsaved_bytes": 2147483648 },
  "profile": [544, 545, 546],
  "time": 1794094700
}
```

To **claim** a disk, a node reads the head and writes it back with
`If-Match`, with itself as the owner and `next` plus one. If another node
wrote the head between the read and the write, the write fails, and the
claim fails. A claim refuses a disk that another node owns. `--force` takes
the disk anyway: use it only when the other node is off.

The owner writes the head at the end of each transaction, again with
`If-Match`. When the write fails, another node took the disk. The old owner
then stops the disk: it refuses all I/O and keeps its local data.

A forced claim cannot know if the old owner is still uploading. So the new
owner adds the old `next` to `lost`. The old owner can only write into that
folder, because it cannot change the head. The new owner deletes the
objects in the lost folders.

## A write

1. The guest writes 4 KiB at some offset. The ublk driver gives the request
   to a queue thread of the daemon.
2. The write falls into one chunk. When the chunk is local, the queue
   thread writes the data into the data file at once, without a copy of the
   data in memory: the request pages go to the file directly.
3. When the chunk is not local, the daemon first makes it local: it copies
   the current content of the chunk into the data file, from the cache or
   from the bucket. Then it writes the new data.
4. The chunk is now unsaved.

The data file is one sparse file for the whole disk. A chunk that is not
local is a hole in it. So the file uses space only for local chunks.

### Flush

A guest flush asks for durable data. mica syncs the data file, then
appends the chunks that became local since the last flush to the **dirty
log**. The dirty log is the list of local chunks. Two files, `log-a` and
`log-b`, hold it: a rewrite goes to the file not in use, so one file is
always complete.

Order matters here. The log never lists a chunk before its data is durable.
After a power loss, mica punches out all ranges of the data file that the
log does not list: they can hold half-written data.

Between a write and the next flush, a **recent bitmap** in `/run` lists the
chunk. It is a file in memory, one bit for each chunk. When the daemon
crashes, the bitmap and the page cache survive, so the next daemon keeps
these chunks. A reboot removes both.

### Transaction

Every 30 seconds, or when 512 MiB waits, a transaction runs:

1. Take each unsaved chunk that was not written in the last second. Read
   it under its lock. A chunk of zeros gets the zero entry. A chunk with
   the same hash as before needs no upload.
2. Upload the other chunks into the folder `next`, 64 at a time.
3. Upload each changed page, and then the new root.
4. Write the head with `If-Match`: `transaction = next`, `next + 1`.
5. Free the objects that no version needs any more (see below).

The head write is the commit. Before it, the new objects are not part of
any version. A crash before it leaves only objects in the folder `next`,
and the next claim adds that folder to `lost`.

A chunk that the guest writes during its upload stays unsaved. The next
transaction uploads it again.

### Saved chunks leave the data file

After a transaction, saved chunks stay local. A next write into them needs
no copy, and on ext4 or XFS it overwrites blocks that exist, so a flush
syncs less. When the saved local chunks are more than the cache budget of
the disk, the oldest ones move into the cache, and their range in the data
file becomes a hole again. Thus the data file holds the unsaved data and
the newest saved data, and the cache holds the rest.

## A read

1. A local chunk: read from the data file.
2. A zero chunk: zeros, at once.
3. A chunk in the cache: read from the cache file.
4. Else download the chunk, check its hash, put it into the cache, and
   read it.

Many reads of one chunk share one download. The cache of the node is by
hash, so many disks from one image download each chunk once for the node:
a file of another disk is linked, not copied.

A read of local or cached data runs on the queue thread, without a copy:
the file data goes into the request pages directly.

### Downloads before the reads

- **Profile.** The head holds the first 64 MiB of chunks that the guest read
  in its last session. At attach, these download first, eight at a time.
  So a boot or a database start reads mostly from the cache.
- **Read ahead.** Two sequential reads start a read ahead of 1 MiB, which
  grows to 8 MiB.
- **Fill.** When the guest is quiet for 250 ms, the rest of the disk
  downloads into the cache, until the cache is 90 % full. `--no-fill`
  stops it.

Reads that wait always come first. The node has 128 download slots: 16
for read ahead, 32 for fill, and the rest for reads.

## Freeing objects without a garbage collector

Each object belongs to one disk. Only the owner of the disk deletes its
objects. So mica can decide when an object is free from the head alone,
with no listing and no scan.

One fact makes this possible: the versions of a disk form one line. An
object that version `n` replaced is still used by an older version only
if a snapshot holds that older version.

**Free rule.** After transaction `n`, look at each chunk and page that `n`
replaced. Let `K` be the newest snapshot before `n`. When the replaced
object was born after `K`, no snapshot uses it: delete it. Then delete the
root of the version before `n`, unless it is a snapshot. The root goes
last, so a crash in the middle leaves a root that names the work left.

**Merge rule.** To delete snapshot `S`, let `N` be the next snapshot after
`S`, or the current version. Let `P` be the snapshot before `S`. Delete
each object of `S` that `N` does not have and that was born after `P`.
Only the roots of `S` and `N` are compared, and only the pages that differ.

Each transaction thus deletes exactly what it replaced, at once. A disk
with no snapshots holds exactly one version in the bucket.

## Snapshots and restore

`mica snapshot create db before-upgrade` names the current version. On an
attached disk, it is a transaction of all unsaved chunks whose head also
adds the name. It costs no copy.

mica has no rollback. To go back, make a new disk from a snapshot:

```bash
mica disk restore db before-upgrade db-old
```

The restore copies the root, the pages and the chunks of the snapshot into
the folders of the new disk with S3 `CopyObject`. The bucket copies the
data. The node downloads nothing. The new disk is independent: a delete of
the old disk does not touch it.

## Moving a disk

**Detach** (or `mica disk unmount`, which first runs `fstrim` and
`umount`):

1. Refuse when a program still has the device open.
2. Remove the device.
3. Run transactions until no chunk is unsaved.
4. Move the saved local chunks into the cache.
5. Write the head with no owner, and delete the local folder.

When detach returns, the bucket has all data, and any node can attach the
disk.

**Attach** claims the disk, opens its local data, and makes the device. A
disk that was never written has `"blank": true`, and `mica disk mount`
formats it.

The local folder records the `id` of its disk and the version of its file
layout. A mica of another layout refuses the folder. An attach also
refuses local data of another disk with the same name: one from another
bucket, or one that was deleted and made again. It keeps such data as
**orphaned**.

## Crashes and restarts

| Event | What happens |
|---|---|
| The daemon stops (`systemctl restart mica`) | Each device stays in the kernel, paused. The daemon runs one last transaction and exits. The next daemon takes each device over with ublk user recovery. The guest sees a pause of about one second. |
| The daemon crashes | Same, without the last transaction. The kernel sends the requests in flight again to the next daemon. The recent bitmap keeps the chunks written since the last flush. |
| The node reboots | The devices and `/run` are gone. The local data stays. The next `mica disk attach` (or a mount at boot) goes on from it. |
| Power loss | Same as a reboot. Writes after the last flush can be lost, as on any disk with a write cache. The dirty log makes sure that no torn data comes back. |
| The bucket is down | Reads of local and cached data work. Writes go on until the unsaved data reaches its limit, then wait. Transactions retry. |
| Another node takes the disk | The old owner fails its next head write, stops the disk, and keeps its local data as orphaned. |
| A local sync fails | The disk stops for good. Linux reports a write error only once, so a later sync that works proves nothing. |
| A second daemon starts on the same folders | It refuses to start. Each daemon locks its data folder and its run folder. A stopping daemon can still run its last transactions, and two daemons on one folder would punch out the chunks of each other. |

## Budgets

Each disk has two budgets in its head, so they follow the disk:

- `cache_bytes`: clean chunks kept on the node.
- `unsaved_bytes`: data not in the bucket yet. Guest writes wait at this limit.

The cache budget also limits the saved chunks in the data file. So a disk
uses at most `unsaved_bytes + 2 × cache_bytes` of the local SSD.

`mica disk set-limits` changes them. A disk is **behind** when its oldest
unsaved write is older than five minutes.

## Credentials

Each request that needs the bucket carries its credentials: a TOML file
with `--store`, or the `S3_*` variables. The daemon keeps the credentials
of an attached disk in memory and in `/run/mica/disks/<disk>/store.toml`,
mode 0600, for a restart of the daemon. A reboot removes them.

For a laptop or a desktop, `mica service add-disk` keeps the file in
`/etc/mica/disks/<disk>.toml` and mounts the disk at each boot.

## The code

| Module | Job |
|---|---|
| `store/format.rs` | Hashes, entries, pages, roots, and their encodings |
| `store/bucket.rs` | The S3 calls, with counts of requests and bytes |
| `store/head.rs` | The head |
| `store/disk_store.rs` | The keys and objects of one disk |
| `store/cleanup.rs` | The free rule and the merge rule |
| `owner.rs` | The claim of a disk |
| `local/data_files.rs` | The sparse data file |
| `local/dirty_log.rs` | The dirty log and the recent bitmap |
| `local/cache.rs` | The chunk cache of the node |
| `local/folder.rs` | The local folder of a disk and its state |
| `transfers.rs` | The download and upload slots of the node |
| `disk/` | The engine of an attached disk: I/O, transactions, prefetch |
| `ublk/` | The block device, its queue threads, and the zero-copy requests |
| `daemon/` | The daemon, its operations, and its API |
| `cli/` | The `mica` command, mount and unmount, the systemd units |
