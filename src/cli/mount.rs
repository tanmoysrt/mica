//! `mica disk mount` and `mica disk unmount`.
//!
//! The daemon runs in its own mount namespace, so that a mount of its own
//! devices can never wait for itself. So the CLI mounts and unmounts, in
//! the namespace of the caller. The daemon only attaches and detaches.

use crate::config::Config;
use crate::daemon::api::{Request, call};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Attaches the disk, formats it if it was never written, and mounts it.
pub async fn mount(config: &Config, attach: Request, path: &Path, fs: &str) -> Result<()> {
    let Request::Attach { disk, .. } = &attach else { bail!("mount needs an attach request") };
    let disk = disk.clone();
    let attached = call(&config.socket, &attach).await?;
    let device = attached["device"].as_str().context("the daemon gave no device")?.to_string();
    let result = (|| {
        wait_for(&device)?;
        if attached["blank"].as_bool() == Some(true) {
            run(&format!("mkfs.{fs}"), &["-q", &device])?;
        }
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        run("mount", &["-o", "nosuid,nodev", &device, &path.to_string_lossy()])
    })();
    if let Err(error) = result {
        let _ = call(&config.socket, &Request::Detach { disk: disk.clone() }).await;
        return Err(error);
    }
    println!("Mounted disk {disk} at {}", path.display());
    Ok(())
}

/// Trims, unmounts and detaches. `target` is a disk name or a mount path.
/// The trim first frees the chunks of deleted files, so they are not uploaded.
pub async fn unmount(config: &Config, target: &str) -> Result<()> {
    let disk = match Path::new(target).is_absolute() {
        true => disk_mounted_at(Path::new(target))?,
        false => target.to_string(),
    };
    let device = std::fs::canonicalize(format!("/dev/mica/{disk}")).with_context(|| format!("disk {disk} is not attached here"))?;
    for point in mount_points(&device)? {
        if let Err(error) = run("fstrim", &[&point.to_string_lossy()]) {
            eprintln!("warning: {error:#}");
        }
        run("umount", &[&point.to_string_lossy()])?;
    }
    call(&config.socket, &Request::Detach { disk: disk.clone() }).await?;
    println!("Detached disk {disk}. The bucket has all its data");
    Ok(())
}

fn disk_mounted_at(path: &Path) -> Result<String> {
    let path = std::fs::canonicalize(path)?;
    let (device, _) = mounts()?.into_iter().find(|(_, point)| *point == path).with_context(|| format!("nothing is mounted at {}", path.display()))?;
    for link in std::fs::read_dir("/dev/mica")? {
        let link = link?;
        if std::fs::canonicalize(link.path())? == device {
            return Ok(link.file_name().to_string_lossy().into_owned());
        }
    }
    bail!("{} is not a mica disk", path.display())
}

fn mount_points(device: &Path) -> Result<Vec<PathBuf>> {
    Ok(mounts()?.into_iter().filter(|(source, _)| source == device).map(|(_, point)| point).collect())
}

fn mounts() -> Result<Vec<(PathBuf, PathBuf)>> {
    let text = std::fs::read_to_string("/proc/self/mounts")?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = std::fs::canonicalize(fields.next()?).ok()?;
            Some((source, PathBuf::from(fields.next()?.replace("\\040", " "))))
        })
        .collect())
}

/// udev makes the device node shortly after the daemon makes the device.
fn wait_for(device: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !Path::new(device).exists() {
        if Instant::now() > deadline {
            bail!("{device} did not appear");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status().with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}
