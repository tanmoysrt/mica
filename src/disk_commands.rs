use crate::api::{Request, send};
use crate::commands::{ask_before_delete, ask_before_reads, parse_size};
use crate::managed_mount::{self, FolderOwner};
use crate::output::{self, bytes};
use crate::service;
use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// List all disks
    Ls {
        /// Do not ask before reading the details of every disk
        #[arg(short, long)]
        yes: bool,
    },
    /// Create a disk
    Create {
        disk: String,
        /// Size, such as 20G
        #[arg(long)]
        size: Option<String>,
        /// Start from this snapshot
        #[arg(long, value_name = "SNAPSHOT")]
        from_snapshot: Option<String>,
    },
    /// Download a disk's chunks into the local cache
    Warm {
        disk: String,
        /// Downloads at the same time [default: 8]
        #[arg(long, value_name = "N")]
        parallel: Option<usize>,
    },
    /// Attach a disk as /dev/mica/<disk>
    Attach {
        disk: String,
        /// Chunks (4 MiB) of the read profile to download in the background [default: none]
        #[arg(long, value_name = "CHUNKS")]
        prefetch: Option<usize>,
        /// Downloads at the same time for this disk's prefetch [default: 8]
        #[arg(long, value_name = "N")]
        prefetch_parallel: Option<usize>,
        /// Take the disk from another node (root only)
        #[arg(long)]
        force: bool,
    },
    /// Attach and mount a disk
    Mount {
        disk: String,
        path: PathBuf,
        /// Filesystem for a new disk
        #[arg(long, default_value = "ext4")]
        fs: String,
        /// FolderOwner of the disk's top folder [default: you, for a new disk]
        #[arg(long, value_name = "USER[:GROUP]")]
        owner: Option<String>,
        /// Take the disk from another node (root only)
        #[arg(long)]
        force: bool,
    },
    /// Unmount and release a disk
    Unmount {
        /// Disk or mount path
        target: String,
    },
    /// Upload and release a disk
    Detach { disk: String },
    /// Grow a detached disk
    Resize {
        disk: String,
        /// New size, such as 40G
        #[arg(long)]
        size: String,
    },
    /// Delete a detached disk
    Delete {
        disk: String,
        /// Do not ask first
        #[arg(short, long)]
        yes: bool,
    },
}

pub async fn run(command: DiskCommand, socket: &Path) -> Result<()> {
    match command {
        DiskCommand::Ls { yes } => list(socket, yes).await?,
        DiskCommand::Create { disk, size, from_snapshot } => {
            let size = size.as_deref().map(parse_size).transpose()?;
            let request = Request::Create { disk: disk.clone(), size, from_snapshot: from_snapshot.clone() };
            let result = send(socket, &request).await?;
            let size = bytes(result["size"].as_u64().unwrap_or(0));
            match from_snapshot {
                Some(snapshot) => println!("Created disk {disk} ({size}) from snapshot {snapshot}"),
                None => println!("Created disk {disk} ({size})"),
            }
        }
        DiskCommand::Warm { disk, parallel } => warm(socket, &disk, parallel).await?,
        DiskCommand::Attach { disk, prefetch, prefetch_parallel, force } => {
            let request = Request::Attach { disk: disk.clone(), force, prefetch, prefetch_parallel };
            let result = send(socket, &request).await?;
            println!("Attached disk {disk} at {}", result["device"].as_str().unwrap_or("?"));
        }
        DiskCommand::Mount { disk, path, fs, owner, force } => {
            let owner = mount_owner(owner.as_deref())?;
            mount(socket, &disk, &path, &fs, force, owner).await?
        }
        DiskCommand::Unmount { target } => unmount(socket, &target).await?,
        DiskCommand::Detach { disk } => {
            send(socket, &Request::Detach { disk: disk.clone() }).await?;
            println!("Detached disk {disk}. All its data is in S3");
        }
        DiskCommand::Resize { disk, size } => {
            let size = parse_size(&size)?;
            send(socket, &Request::Resize { disk: disk.clone(), size }).await?;
            println!("Resized disk {disk} to {}. Grow its filesystem to use the space", bytes(size));
        }
        DiskCommand::Delete { disk, yes } => {
            if ask_before_delete(&format!("Delete disk {disk}? Its snapshots stay."), yes)? {
                send(socket, &Request::DeleteDisk { disk: disk.clone() }).await?;
                println!("Deleted disk {disk}");
            }
        }
    }
    Ok(())
}

async fn warm(socket: &Path, disk: &str, parallel: Option<usize>) -> Result<()> {
    let result = send(socket, &Request::Warm { disk: disk.to_string(), parallel }).await?;
    let number = |key: &str| result[key].as_u64().unwrap_or(0);
    println!(
        "Warmed disk {disk}: downloaded {} chunks ({}); {} of {} were cached already",
        number("downloaded"),
        bytes(number("downloaded_bytes")),
        number("chunks") - number("downloaded") - number("failed"),
        number("chunks")
    );
    if number("failed") > 0 {
        println!("{} chunks failed to download. Run it again to retry them", number("failed"));
    }
    if number("disk_bytes") > number("cache_limit_bytes") {
        println!("The disk's data is larger than the cache limit, so only part of it stays cached");
    }
    Ok(())
}

/// Listing the names is one S3 request. The details cost three reads per disk.
async fn list(socket: &Path, yes: bool) -> Result<()> {
    let result = send(socket, &Request::ListDisks).await?;
    let names: Vec<String> = serde_json::from_value(result["disks"].clone())?;
    if names.is_empty() {
        output::print_disks(&[]);
        return Ok(());
    }
    if !ask_before_reads(names.len(), 3, "disks", yes)? {
        names.iter().for_each(|name| println!("{name}"));
        return Ok(());
    }
    let details = send(socket, &Request::DescribeDisks { disks: names }).await?;
    output::print_disks(details["disks"].as_array().map(Vec::as_slice).unwrap_or_default());
    Ok(())
}

/// With the service, the daemon mounts through a systemd unit, so the mount
/// comes back after a reboot. Without it, the CLI mounts directly (needs root).
async fn mount(socket: &Path, disk: &str, path: &Path, filesystem: &str, force: bool, owner: FolderOwner) -> Result<()> {
    let path = std::path::absolute(path)?;
    let result = if service::is_installed() {
        let request = Request::Mount {
            disk: disk.to_string(),
            path: path.clone(),
            filesystem: filesystem.to_string(),
            force,
            owner,
        };
        send(socket, &request).await?
    } else {
        eprintln!("warning: mica.service is not installed, so this mount does not come back after a reboot");
        managed_mount::mount_direct(socket, disk, &path, filesystem, force, owner).await?
    };
    if let Some(filesystem) = result["formatted"].as_str() {
        println!("Formatted disk {disk} with {filesystem}");
    }
    println!("Mounted disk {disk} at {}", path.display());
    Ok(())
}

async fn unmount(socket: &Path, target: &str) -> Result<()> {
    let target_arg = match Path::new(target).exists() {
        true => std::path::absolute(target)?.display().to_string(),
        false => target.to_string(),
    };
    if service::is_installed() {
        let result = send(socket, &Request::Umount { target: target_arg.clone() }).await?;
        if result["managed"] == true {
            println!("Unmounted disk {}. All its data is in S3", result["disk"].as_str().unwrap_or(target));
            return Ok(());
        }
    }
    let disk = managed_mount::umount_direct(socket, &target_arg).await?;
    println!("Unmounted disk {disk}. All its data is in S3");
    Ok(())
}

/// With `--owner`, that user owns the top folder on every mount. Without it,
/// a new disk is owned by the caller: the sudo user when run with sudo.
fn mount_owner(text: Option<&str>) -> Result<FolderOwner> {
    let Some(text) = text else {
        let uid = env_id("SUDO_UID").unwrap_or_else(|| unsafe { libc::getuid() });
        let gid = env_id("SUDO_GID").unwrap_or_else(|| unsafe { libc::getgid() });
        return Ok(FolderOwner { uid, gid, always: false });
    };
    let (user, group) = match text.split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (text, None),
    };
    let (uid, primary_gid) = find_user(user)?;
    let gid = match group {
        Some(group) => find_group(group)?,
        None => primary_gid,
    };
    Ok(FolderOwner { uid, gid, always: true })
}

fn env_id(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.parse().ok()
}

/// A user name or a numeric uid. Returns the uid and the user's primary group.
fn find_user(user: &str) -> Result<(u32, u32)> {
    let name = std::ffi::CString::new(user)?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if !entry.is_null() {
        return Ok(unsafe { ((*entry).pw_uid, (*entry).pw_gid) });
    }
    let uid: u32 = user.parse().map_err(|_| anyhow!("no user named {user}"))?;
    let entry = unsafe { libc::getpwuid(uid) };
    let gid = if entry.is_null() { uid } else { unsafe { (*entry).pw_gid } };
    Ok((uid, gid))
}

/// A group name or a numeric gid.
fn find_group(group: &str) -> Result<u32> {
    if let Some(gid) = crate::api::group_id(group) {
        return Ok(gid);
    }
    group.parse().with_context(|| format!("no group named {group}"))
}
