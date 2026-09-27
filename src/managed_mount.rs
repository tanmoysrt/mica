use crate::api::{Request, send};
use crate::catalog::validate_name;
use crate::daemon::{Daemon, Prefetch};
use crate::mount_tools;
use crate::service::{self, mount_unit};
use crate::systemd;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// `<config dir>/mounts/<disk-id>.toml`: where `mica-mount@<disk-id>` mounts the disk.
#[derive(Debug, Serialize, Deserialize)]
pub struct MountSpec {
    pub path: PathBuf,
    pub filesystem: String,
    #[serde(default)]
    pub owner: Option<FolderOwner>,
}

/// The owner of the disk's top folder. Without `always`, it is set only when
/// mica formats the disk; an existing disk keeps its ownership.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FolderOwner {
    pub uid: u32,
    pub gid: u32,
    pub always: bool,
}

impl FolderOwner {
    /// Sets the owner of the mounted folder, not of the files in it.
    fn apply(&self, path: &Path, formatted: bool) -> Result<()> {
        if self.always || formatted {
            std::os::unix::fs::chown(path, Some(self.uid), Some(self.gid))
                .with_context(|| format!("set the owner of {}", path.display()))?;
        }
        Ok(())
    }
}

impl MountSpec {
    pub fn load(config_path: &Path, disk_id: &str) -> Result<Option<Self>> {
        match std::fs::read_to_string(spec_path(config_path, disk_id)) {
            Ok(text) => Ok(Some(toml::from_str(&text)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Disk IDs that have a mount spec.
    pub fn list(config_path: &Path) -> Result<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(mounts_dir(config_path)) else { return Ok(Vec::new()) };
        let mut disks = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                disks.push(path.file_stem().unwrap_or_default().to_string_lossy().to_string());
            }
        }
        Ok(disks)
    }

    fn save(&self, config_path: &Path, disk_id: &str) -> Result<()> {
        std::fs::create_dir_all(mounts_dir(config_path))?;
        std::fs::write(spec_path(config_path, disk_id), toml::to_string(self)?)?;
        Ok(())
    }

    fn remove(config_path: &Path, disk_id: &str) -> Result<()> {
        match std::fs::remove_file(spec_path(config_path, disk_id)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

// Daemon side. The daemon runs as root and starts `mica-mount@<disk>`, so
// members of the mica group can mount without root.

pub async fn mount_with_unit(
    daemon: &Arc<Daemon>,
    disk: &str,
    path: &Path,
    filesystem: &str,
    force: bool,
    owner: FolderOwner,
) -> Result<Value> {
    validate_name(disk)?;
    ensure!(matches!(filesystem, "ext4" | "xfs"), "the filesystem must be ext4 or xfs");
    check_mount_path(&daemon.config.mount_roots, path)?;
    if let Some(spec) = MountSpec::load(&daemon.config_path, disk)? {
        bail!("disk {disk} is already mounted at {}", spec.path.display());
    }
    if force {
        daemon.attach(disk, true, Prefetch::default()).await?;
    }
    let blank = daemon.is_blank(disk).await?;
    let spec = MountSpec { path: path.to_path_buf(), filesystem: filesystem.to_string(), owner: Some(owner) };
    spec.save(&daemon.config_path, disk)?;
    let unit = mount_unit(disk);
    if let Err(error) = run_systemctl(&["enable", "--now", "--quiet", &unit]).await {
        let _ = run_systemctl(&["disable", "--quiet", &unit]).await;
        let _ = daemon.detach_if_attached(disk).await;
        MountSpec::remove(&daemon.config_path, disk)?;
        return Err(error.context(format!("cannot mount disk {disk}. See: journalctl -u {unit}")));
    }
    Ok(json!({ "path": path, "formatted": blank.then_some(filesystem) }))
}

/// Returns `managed: false` when the disk was not mounted with the unit, so
/// the CLI can unmount it directly.
pub async fn umount_with_unit(daemon: &Arc<Daemon>, target: &str) -> Result<Value> {
    let Some(disk) = find_managed_disk(&daemon.config_path, target)? else {
        return Ok(json!({ "managed": false }));
    };
    let unit = mount_unit(&disk);
    // The unit's stop step unmounts and detaches, and waits for the upload.
    run_systemctl(&["disable", "--now", "--quiet", &unit]).await?;
    if daemon.disks.lock().await.contains_key(&disk) {
        bail!("cannot unmount disk {disk}. See: journalctl -u {unit}");
    }
    MountSpec::remove(&daemon.config_path, &disk)?;
    Ok(json!({ "managed": true, "disk": disk }))
}

/// Members of the mica group must not become root through a mount. So mounts
/// go only below the configured roots, and use nosuid and nodev.
fn check_mount_path(roots: &[PathBuf], path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "the mount path must be absolute");
    ensure!(
        path.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "the mount path must not contain '.' or '..'"
    );
    let resolved = resolve_existing_part(path)?;
    let allowed = roots.iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        resolved.starts_with(&root) && resolved != root
    });
    let list: Vec<String> = roots.iter().map(|root| root.display().to_string()).collect();
    ensure!(allowed, "mount paths must be inside {}", list.join(", "));
    Ok(())
}

/// Follows symlinks in the part of the path that exists, so a link cannot
/// point the mount outside the allowed roots.
fn resolve_existing_part(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::new();
    while !existing.exists() {
        rest.push(existing.file_name().ok_or_else(|| anyhow!("bad mount path"))?);
        existing = existing.parent().ok_or_else(|| anyhow!("bad mount path"))?;
    }
    let mut resolved = existing.canonicalize()?;
    resolved.extend(rest.into_iter().rev());
    Ok(resolved)
}

fn find_managed_disk(config_path: &Path, target: &str) -> Result<Option<String>> {
    if validate_name(target).is_ok() && MountSpec::load(config_path, target)?.is_some() {
        return Ok(Some(target.to_string()));
    }
    for disk in MountSpec::list(config_path)? {
        if MountSpec::load(config_path, &disk)?.is_some_and(|spec| spec.path == Path::new(target)) {
            return Ok(Some(disk));
        }
    }
    Ok(None)
}

async fn run_systemctl(args: &[&str]) -> Result<()> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        systemd::systemctl(&args)
    })
    .await?
}

// CLI side. These run as root: without the service, and inside the unit.

/// Without mica.service, the CLI mounts directly. The mount does not come
/// back after a reboot.
pub async fn mount_direct(
    socket: &Path,
    disk: &str,
    path: &Path,
    filesystem: &str,
    force: bool,
    owner: FolderOwner,
) -> Result<Value> {
    ensure!(path.is_absolute(), "the mount path must be absolute");
    ensure!(!service::is_installed(), "mica.service is installed; the daemon mounts through it");
    let result = mount_now(socket, disk, path, filesystem, force).await?;
    owner.apply(path, result["formatted"].is_string())?;
    Ok(result)
}

/// Accepts a disk ID or a mount path.
pub async fn umount_direct(socket: &Path, target: &str) -> Result<String> {
    let disk = resolve_attached_disk(socket, target).await?;
    unmount_and_detach(socket, &disk).await?;
    Ok(disk)
}

/// `ExecStart` of `mica-mount@<disk>`.
pub async fn unit_start(socket: &Path, config_path: &Path, disk: &str) -> Result<()> {
    let spec = MountSpec::load(config_path, disk)?.ok_or_else(|| anyhow!("no mount spec for disk {disk}"))?;
    let result = mount_now(socket, disk, &spec.path, &spec.filesystem, false).await?;
    if let Some(owner) = spec.owner {
        owner.apply(&spec.path, result["formatted"].is_string())?;
    }
    println!("mounted disk {disk} at {}", spec.path.display());
    if let Some(filesystem) = result["formatted"].as_str() {
        println!("formatted it with {filesystem}, because it had no data");
    }
    Ok(())
}

/// `ExecStop` of `mica-mount@<disk>`.
pub async fn unit_stop(socket: &Path, disk: &str) -> Result<()> {
    if attached_entry(socket, disk).await?.is_some() {
        unmount_and_detach(socket, disk).await?;
    }
    println!("unmounted and detached disk {disk}");
    Ok(())
}

/// Attaches, formats a disk that was never written, and mounts it.
async fn mount_now(socket: &Path, disk: &str, path: &Path, filesystem: &str, force: bool) -> Result<Value> {
    let request = Request::Attach { disk: disk.to_string(), force, prefetch: None, prefetch_parallel: None };
    let attached = send(socket, &request).await?;
    let device = attached["device"].as_str().ok_or_else(|| anyhow!("daemon gave no device"))?.to_string();
    if mount_tools::device_mounted_at(path).ok().flatten().is_some() {
        return Ok(json!({ "path": path, "formatted": null }));
    }
    let format_with = attached["blank"].as_bool().unwrap_or(false).then_some(filesystem);
    if let Err(error) = mount_tools::mount(&device, path, format_with) {
        let _ = send(socket, &Request::Detach { disk: disk.to_string() }).await;
        return Err(error);
    }
    Ok(json!({ "path": path, "formatted": format_with }))
}

async fn unmount_and_detach(socket: &Path, disk: &str) -> Result<()> {
    if let Some(ublk) = attached_entry(socket, disk).await?.and_then(|entry| entry["ublk"].as_str().map(String::from)) {
        for mount_point in mount_tools::mount_points_of(&ublk)? {
            // Space freed by deleted files becomes zero chunks, which are not uploaded.
            mount_tools::trim(&mount_point);
            mount_tools::unmount(&mount_point)?;
        }
    }
    send(socket, &Request::Detach { disk: disk.to_string() }).await?;
    Ok(())
}

async fn resolve_attached_disk(socket: &Path, target: &str) -> Result<String> {
    if attached_entry(socket, target).await?.is_some() {
        return Ok(target.to_string());
    }
    let device = mount_tools::device_mounted_at(Path::new(target))
        .ok()
        .flatten()
        .ok_or_else(|| anyhow!("{target} is not an attached disk or a mica mount path"))?;
    let status = send(socket, &Request::Status).await?;
    status["disks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["ublk"].as_str().map(PathBuf::from) == Some(device.clone()))
        .and_then(|entry| entry["disk"].as_str().map(String::from))
        .with_context(|| format!("{} is not a mica disk", device.display()))
}

async fn attached_entry(socket: &Path, disk: &str) -> Result<Option<Value>> {
    let status = send(socket, &Request::Status).await?;
    Ok(status["disks"].as_array().into_iter().flatten().find(|entry| entry["disk"] == disk).cloned())
}

fn mounts_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or(Path::new("/etc/mica")).join("mounts")
}

fn spec_path(config_path: &Path, disk_id: &str) -> PathBuf {
    mounts_dir(config_path).join(format!("{disk_id}.toml"))
}
