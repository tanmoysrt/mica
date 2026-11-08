# The daemon API

The `mica` command and any other program, such as an orchestrator, talk to the daemon in the same way. This page lists each request and its answer. [operations.md](operations.md) explains the commands for people.

## The socket

The daemon listens on a Unix socket, `/run/mica.sock` by default (the `socket` setting). The socket belongs to root, with mode 0600, because a request can make devices and carry bucket keys.

A connection carries one request and one answer. Each is one line of JSON:

```text
$ echo '{"command":"status"}' | sudo socat - UNIX-CONNECT:/run/mica.sock
{"ok":true,"result":{"node":"node-23","version":"0.2.0","disks":[],"tasks":[]}}
```

An error has `"ok": false` and a message for people:

```json
{"ok":false,"error":"there is no disk web (see `sudo mica disk ls`) ...","result":null}
```

## The bucket of a request

A request names its bucket in one of three ways:

| Field | Meaning |
|---|---|
| `store` | The bucket with its keys: `endpoint`, `bucket`, `prefix`, `region`, `access_key_id`, `secret_access_key`. The daemon keeps a bucket that works. |
| `bucket` | The name of a bucket that the daemon knows: its bucket name, its name and prefix (`my-disks/laptop`), or its full place. |
| neither | The daemon finds the disk in all the buckets that it knows. A new disk goes to the only bucket that it knows. |

The **place** of a bucket is `<endpoint>/<bucket>/<prefix>`. Two configs with the same place reach the same disks. New keys for a known place replace the old ones, and the attached disks of that place use them at once.

```json
{"command":"attach","disk":"web","store":{"endpoint":"https://<account>.r2.cloudflarestorage.com","bucket":"my-disks","prefix":"","region":"auto","access_key_id":"...","secret_access_key":"..."}}
{"command":"attach","disk":"web","bucket":"my-disks"}
{"command":"attach","disk":"web"}
```

## Requests

Sizes are in bytes, and times are Unix seconds. A time of 0 means never.

| `command` | Fields | Result |
|---|---|---|
| `status` | | `node`, `version`, `disks` (see below), `tasks` |
| `list` | | `buckets` (places), `disks`: each disk as in `info`, with `bucket`, `cache_bytes` on this node, and `local` (its entry in `status` when it is attached here, else null) |
| `info` | `disk` | `disk`, `size`, `chunk_size`, `transaction`, `time` (of the last save), `owner` (host name or null), `limits`, `snapshots`, `deleted`, `bucket`, `local` |
| `create` | `disk`, `size`, `chunk_size` (null: 1 MiB), `limits` | `disk`, `size`, `chunk_size`, `warning` (null, or why the bucket cannot stop two nodes) |
| `attach` | `disk` | `device`, such as `/dev/mica/web`, and `blank` (true for a disk that was never written) |
| `detach` | `disk` | `detached`. It returns when the bucket has all data. |
| `set` | `disk`, `size` (null: no change), `limits` | as `info` |
| `release` | `disk` | `released_from`: the host name of the old owner |
| `delete` | `disk` | `deleted` |
| `snapshot-create` | `disk`, `snapshot` | `disk`, `snapshot`, `transaction` |
| `snapshot-delete` | `disk`, `snapshot` | `disk`, `deleted` |
| `restore` | `disk`, `snapshot`, `target` | `disk` (the target), `from`, `snapshot`, `copied_objects` |
| `bucket-add` | needs `store` | `bucket`, `disks` (how many it holds), `warning` |
| `bucket-list` | | `buckets`: `bucket` (place), `endpoint`, `name`, `prefix`, `access_key_id`, `attached_here` |
| `bucket-forget` | `name` | `forgotten`: the place |

`limits` has `cache_bytes`, `unsaved_bytes` and `interval_seconds`. In `create` and `set`, each is optional: a missing one keeps the old value, or for a new disk the default (512 MiB, 4 GiB, 300 s).

A disk attached on another node is refused with the host name of that node, so that a controller can send the request there.

## The status of a disk

Each entry of `disks` in `status`:

| Field | Meaning |
|---|---|
| `disk`, `device`, `bucket` | The name, the device link, and the place of the bucket |
| `detaching` | True while a detach uploads the last data |
| `rates` | Bytes each second over the last 5 seconds: `read`, `written` (by the device user), `uploaded`, `downloaded` (to and from the bucket) |
| `status.unsaved_bytes` | Data that is only on this node |
| `status.local_bytes` | Data in the data file, saved or not |
| `status.cache_bytes` | Data in the cache |
| `status.limits` | As in `info` |
| `status.transaction`, `status.saved_time` | The current version, and the time of its save |
| `status.behind` | True when the oldest unsaved write is older than three intervals |
| `status.stopped` | Null, or why the disk stopped: all its I/O fails, and its local data stays |
| `status.requests` | Requests and bytes to the bucket since the attach: `get`, `put`, `list`, `delete`, `copy`, `bytes_in`, `bytes_out` |
| `status.io` | Counters of the write path, for profiling |

Each entry of `tasks` is a long operation, such as the copy of a restore: `disk`, `what`, `done` and `total`. A program can show progress from it.
