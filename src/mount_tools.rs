use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Formats the device when asked, then mounts it at `target`.
pub fn mount(device: &str, target: &Path, format_with: Option<&str>) -> Result<()> {
    wait_for_device(device)?;
    if let Some(filesystem) = format_with {
        run(&format!("mkfs.{filesystem}"), &["-q", device])?;
    }
    std::fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
    run("mount", &["-o", "nosuid,nodev", device, &target.to_string_lossy()])
}

/// Best effort: a filesystem without trim support is still unmounted.
pub fn trim(target: &Path) {
    if let Err(error) = run("fstrim", &[&target.to_string_lossy()]) {
        eprintln!("warning: {error:#}");
    }
}

pub fn unmount(target: &Path) -> Result<()> {
    run("umount", &[&target.to_string_lossy()])
}

/// Mount points of a block device, from /proc/self/mounts.
pub fn mount_points_of(device: &str) -> Result<Vec<PathBuf>> {
    let device = std::fs::canonicalize(device).unwrap_or_else(|_| PathBuf::from(device));
    Ok(read_mounts()?.into_iter().filter(|(source, _)| *source == device).map(|(_, target)| target).collect())
}

/// The block device mounted at `target`, if any.
pub fn device_mounted_at(target: &Path) -> Result<Option<PathBuf>> {
    let target = std::fs::canonicalize(target)?;
    Ok(read_mounts()?.into_iter().find(|(_, mounted)| *mounted == target).map(|(source, _)| source))
}

/// udev makes the device node shortly after the daemon creates the device.
fn wait_for_device(device: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !Path::new(device).exists() {
        if Instant::now() > deadline {
            bail!("{device} did not appear");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn read_mounts() -> Result<Vec<(PathBuf, PathBuf)>> {
    let text = std::fs::read_to_string("/proc/self/mounts")?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?.replace("\\040", " ");
            Some((PathBuf::from(source), PathBuf::from(target)))
        })
        .collect())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status().with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}
