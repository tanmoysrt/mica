use crate::daemon::{Daemon, Prefetch};
use crate::managed_mount::{self, FolderOwner};
use crate::ownership::{self, Owner};
use anyhow::{Context, Result, bail, ensure};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub const GROUP: &str = "mica";
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// One JSON line per request and per response, on the Unix socket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Create { disk: String, size: Option<u64>, from_snapshot: Option<String> },
    Snapshot { disk: String, name: String },
    Resize { disk: String, size: u64 },
    Attach {
        disk: String,
        force: bool,
        #[serde(default)]
        prefetch: Option<usize>,
        #[serde(default)]
        prefetch_parallel: Option<usize>,
    },
    Detach { disk: String },
    Mount { disk: String, path: PathBuf, filesystem: String, force: bool, owner: FolderOwner },
    Umount { target: String },
    Status,
    ListDisks,
    DescribeDisks { disks: Vec<String> },
    DeleteDisk { disk: String },
    ListSnapshots,
    DescribeSnapshots { names: Vec<String> },
    DeleteSnapshot { name: String },
    Gc { delete: bool, grace_secs: Option<u64> },
    Warm {
        disk: String,
        #[serde(default)]
        parallel: Option<usize>,
    },
    CacheStatus,
    CachePrune { delete: bool },
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Value,
}

/// Root and members of the `mica` group can use the socket.
pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let socket = daemon.config.socket.clone();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("listen on {}", socket.display()))?;
    let mode = match group_id(GROUP) {
        Some(gid) => {
            std::os::unix::fs::chown(&socket, Some(0), Some(gid))?;
            0o660
        }
        None => 0o600,
    };
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(mode))?;
    log::info!("listening on {}", socket.display());
    crate::systemd::notify_ready();
    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, daemon).await {
                log::warn!("api: {error:#}");
            }
        });
    }
}

/// Client side, used by the CLI.
pub async fn send(socket: &Path, request: &Request) -> Result<Value> {
    let stream = match connect(socket).await {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "no permission to use mica.\n\n\
             Run it with sudo, or join the mica group:\n  \
             sudo usermod -aG mica $USER\n  \
             newgrp mica"
        ),
        Err(_) => bail!("mica is not running.\n\nStart it with:\n  sudo mica service start"),
    };
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{}\n", serde_json::to_string(request)?).as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line).context("bad answer from the daemon")?;
    match response.ok {
        true => Ok(response.result),
        false => bail!("{}", response.error.unwrap_or_default()),
    }
}

/// During a restart the daemon is away for a few seconds. Wait for it,
/// so that the CLI and controllers do not fail for that.
async fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    let deadline = std::time::Instant::now() + RESTART_WAIT;
    loop {
        match UnixStream::connect(socket).await {
            Err(error) if error.kind() != std::io::ErrorKind::PermissionDenied && std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            result => return result,
        }
    }
}

async fn answer(stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
    let caller_uid = stream.peer_cred().map(|cred| cred.uid()).ok();
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let outcome = match serde_json::from_str::<Request>(&line) {
        Ok(request) => dispatch(&daemon, request, caller_uid).await,
        Err(error) => Err(error.into()),
    };
    let response = match outcome {
        Ok(result) => Response { ok: true, error: None, result },
        Err(error) => Response { ok: false, error: Some(format!("{error:#}")), result: Value::Null },
    };
    write.write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes()).await?;
    Ok(())
}

async fn dispatch(daemon: &Arc<Daemon>, request: Request, caller_uid: Option<u32>) -> Result<Value> {
    let caller_is_root = caller_uid == Some(0);
    match request {
        Request::Status => log::debug!("request: {request:?}"),
        _ => log::info!("request: {request:?}"),
    }
    match request {
        Request::Create { disk, size, from_snapshot } => create(daemon, &disk, size, from_snapshot).await,
        Request::Snapshot { disk, name } => snapshot(daemon, &disk, &name).await,
        Request::Resize { disk, size } => {
            if daemon.disks.lock().await.contains_key(&disk) {
                bail!("disk {disk} is attached here. Detach it first");
            }
            daemon.catalog().resize(&disk, size).await?;
            Ok(json!({ "disk": disk, "size": size }))
        }
        Request::Attach { disk, force, prefetch, prefetch_parallel } => {
            // Taking a disk from another node can lose its data, so only root may.
            ensure!(!force || caller_is_root, "--force needs root");
            daemon.attach(&disk, force, Prefetch { chunks: prefetch, parallel: prefetch_parallel }).await
        }
        Request::Detach { disk } => daemon.detach(&disk).await,
        Request::Mount { disk, path, filesystem, force, owner } => {
            ensure!(!force || caller_is_root, "--force needs root");
            // Otherwise a member of the mica group could give a disk to any user.
            ensure!(caller_is_root || caller_uid == Some(owner.uid), "only root can give a disk to another user");
            managed_mount::mount_with_unit(daemon, &disk, &path, &filesystem, force, owner).await
        }
        Request::Umount { target } => managed_mount::umount_with_unit(daemon, &target).await,
        Request::Status => Ok(daemon.status().await),
        Request::ListDisks => Ok(json!({ "disks": daemon.catalog().list_disks().await? })),
        Request::DescribeDisks { disks } => describe_disks(daemon, disks).await,
        Request::DeleteDisk { disk } => {
            if daemon.disks.lock().await.contains_key(&disk) {
                bail!("disk {disk} is attached here. Detach it first");
            }
            daemon.catalog().delete_disk(&disk).await?;
            Ok(json!({ "disk": disk }))
        }
        Request::ListSnapshots => Ok(json!({ "snapshots": daemon.catalog().list_snapshots().await? })),
        Request::DescribeSnapshots { names } => describe_snapshots(daemon, names).await,
        Request::Warm { disk, parallel } => daemon.warm(&disk, parallel).await,
        Request::CacheStatus => Ok(daemon.cache_usage().await),
        Request::CachePrune { delete } => daemon.prune_cache(delete).await,
        Request::Gc { delete, grace_secs } => daemon.collect_garbage(delete, grace_secs).await,
        Request::DeleteSnapshot { name } => {
            daemon.catalog().delete_snapshot(&name).await?;
            Ok(json!({ "snapshot": name }))
        }
    }
}

async fn describe_snapshots(daemon: &Daemon, names: Vec<String>) -> Result<Value> {
    let catalog = daemon.catalog();
    let summaries: Vec<_> = futures::stream::iter(names)
        .map(|name| {
            let catalog = &catalog;
            async move { catalog.snapshot_summary(&name).await }
        })
        .buffered(16)
        .try_collect()
        .await?;
    Ok(json!({ "snapshots": summaries }))
}

async fn describe_disks(daemon: &Daemon, disks: Vec<String>) -> Result<Value> {
    let catalog = daemon.catalog();
    let summaries: Vec<_> = futures::stream::iter(disks)
        .map(|disk| {
            let catalog = &catalog;
            async move { catalog.summary(&disk).await }
        })
        .buffered(16)
        .try_collect()
        .await?;
    Ok(json!({ "disks": summaries }))
}

async fn create(daemon: &Daemon, disk: &str, size: Option<u64>, from_snapshot: Option<String>) -> Result<Value> {
    let catalog = daemon.catalog();
    match (&from_snapshot, size) {
        (Some(snapshot), size) => catalog.clone_snapshot(snapshot, disk, size).await?,
        (None, Some(size)) => catalog.create(disk, size).await?,
        (None, None) => bail!("give --size, or --from-snapshot"),
    }
    let size = catalog.load(disk).await?.manifest.disk_size;
    Ok(json!({ "disk": disk, "size": size, "from_snapshot": from_snapshot }))
}

/// An attached disk gets a checkpoint first, so the snapshot has its latest data.
async fn snapshot(daemon: &Arc<Daemon>, disk_id: &str, name: &str) -> Result<Value> {
    let attached = daemon.disks.lock().await.get(disk_id).map(|a| (a.disk.clone(), a.device.is_some()));
    match attached {
        Some((disk, true)) => disk.checkpoint().await?,
        Some((_, false)) => bail!("disk {disk_id} is still uploading. Try again when the detach is done"),
        None => {
            if let Owner::OtherNode(marker) = ownership::owner_of(&daemon.bucket, disk_id, &daemon.node).await? {
                bail!("disk {disk_id} is attached on {}. Take the snapshot there", marker.node);
            }
        }
    }
    daemon.catalog().snapshot(disk_id, name).await?;
    Ok(json!({ "snapshot": name, "disk": disk_id }))
}

pub fn group_id(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    (!group.is_null()).then(|| unsafe { (*group).gr_gid })
}
