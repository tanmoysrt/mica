# mica documentation

Start with the page that fits what you want to do. Each page stands alone, and links to the next one.

| You want to | Read |
|---|---|
| Try mica for the first time | [Get started](getting-started.md): install, a first disk, and a move to another machine |
| Use mica every day | [Use mica](usage.md): buckets, VMs, laptops, snapshots, limits, and every command |
| Fix something | [Fix a problem](troubleshooting.md): each message, what it means, and what to do |
| Know if mica fits your use | [Speed and cost](benchmarks.md): database speed, crash tests, and the monthly bill of each provider |
| Understand the design | [How mica works](design.md): chunks, transactions, the head as a lock, snapshots without a garbage collector, crashes |
| Control mica from a program | [The daemon API](api.md): the JSON requests and answers on the Unix socket of the daemon |
| Run the tests and benchmarks | [The lab](../lab/README.md): RustFS, Firecracker VMs, crash tests |

## Words

These pages use each word with one meaning:

- **Disk:** a mica disk. Its data lives in a bucket. A machine that attaches it gets the device `/dev/mica/<disk>`.
- **Bucket:** an S3 bucket, with an optional prefix, that holds disks.
- **Machine:** a Linux computer that runs the mica daemon.
- **Unsaved data:** data that is only on the machine, not in the bucket yet.
- **Save:** one upload of the changes of a disk. [How mica works](design.md) calls it a transaction.
- **Cache:** a copy of data from the bucket, kept on the machine to read it fast.
