//! The API of the daemon: one JSON line for each request, on a Unix socket.
//!
//! ```text
//! $ echo '{"command":"status"}' | socat - UNIX-CONNECT:/run/mica.sock
//! {"ok":true,"result":{"node":"node-23","disks":[]}}
//! ```
//!
//! The socket belongs to root, with mode 0600, because a request can make
//! devices and carries bucket credentials.

use super::Daemon;
pub use super::operations::LimitsRequest;
use crate::store::StoreConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Request {
    Status,
    List { store: StoreConfig },
    Info { store: StoreConfig, disk: String },
    Create { store: StoreConfig, disk: String, size: u64, chunk_size: Option<u64>, limits: LimitsRequest },
    Attach { store: StoreConfig, disk: String },
    Detach { disk: String },
    Set { store: StoreConfig, disk: String, size: Option<u64>, limits: LimitsRequest },
    Release { store: StoreConfig, disk: String },
    Delete { store: StoreConfig, disk: String },
    SnapshotCreate { store: StoreConfig, disk: String, snapshot: String },
    SnapshotDelete { store: StoreConfig, disk: String, snapshot: String },
    Restore { store: StoreConfig, disk: String, snapshot: String, target: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Value,
}

pub async fn serve(daemon: Arc<Daemon>, socket: &Path) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).with_context(|| format!("listen on {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    log::info!("listening on {}", socket.display());
    notify_ready();
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(answer(daemon.clone(), stream));
    }
}

async fn answer(daemon: Arc<Daemon>, stream: UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(reader).read_line(&mut line).await.is_err() {
        return;
    }
    let result = match serde_json::from_str::<Request>(&line) {
        Ok(request) => run(&daemon, request).await,
        Err(error) => Err(anyhow::anyhow!("bad request: {error}")),
    };
    let response = match result {
        Ok(result) => Response { ok: true, error: None, result },
        Err(error) => Response { ok: false, error: Some(format!("{error:#}")), result: Value::Null },
    };
    let mut text = serde_json::to_string(&response).unwrap_or_default();
    text.push('\n');
    let _ = writer.write_all(text.as_bytes()).await;
}

async fn run(daemon: &Arc<Daemon>, request: Request) -> Result<Value> {
    match request {
        Request::Status => Ok(daemon.status().await),
        Request::List { store } => daemon.list(&store).await,
        Request::Info { store, disk } => daemon.info(&store, &disk).await,
        Request::Create { store, disk, size, chunk_size, limits } => daemon.create(&store, &disk, size, chunk_size, limits).await,
        Request::Attach { store, disk } => daemon.attach(&store, &disk).await,
        Request::Detach { disk } => daemon.detach(&disk).await,
        Request::Set { store, disk, size, limits } => daemon.set(&store, &disk, size, limits).await,
        Request::Release { store, disk } => daemon.release(&store, &disk).await,
        Request::Delete { store, disk } => daemon.delete(&store, &disk).await,
        Request::SnapshotCreate { store, disk, snapshot } => daemon.create_snapshot(&store, &disk, &snapshot).await,
        Request::SnapshotDelete { store, disk, snapshot } => daemon.delete_snapshot(&store, &disk, &snapshot).await,
        Request::Restore { store, disk, snapshot, target } => daemon.restore(&store, &disk, &snapshot, &target).await,
    }
}

/// Sends one request to the daemon and returns its result.
pub async fn call(socket: &Path, request: &Request) -> Result<Value> {
    let stream = UnixStream::connect(socket).await.map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!("only root can talk to mica: run the command with sudo"),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            anyhow::anyhow!("mica is not running: start it with `sudo mica service install`")
        }
        _ => anyhow::Error::new(error).context(format!("connect to {}", socket.display())),
    })?;
    let (reader, mut writer) = stream.into_split();
    let mut text = serde_json::to_string(request)?;
    text.push('\n');
    writer.write_all(text.as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line).context("bad answer from the daemon")?;
    match response.ok {
        true => Ok(response.result),
        false => anyhow::bail!(response.error.unwrap_or_default()),
    }
}

/// Tells systemd that the daemon answers on its socket (`Type=notify`). So
/// `systemctl start mica` returns only then, and units after mica.service,
/// such as fstab mounts of mica disks, find the devices ready.
fn notify_ready() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let address = match bytes.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(&path),
    };
    let sent = address.and_then(|address| UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address));
    if let Err(error) = sent {
        log::warn!("cannot tell systemd that mica is ready: {error}");
    }
}
