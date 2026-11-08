//! The name of each device: `/dev/mica/<disk>`, a link to `/dev/ublkbN`.
//!
//! udev makes the link, so that systemd knows the device by that name and
//! an fstab mount of it waits for it at boot. The daemon writes the disk
//! name of each device into a file, and the rule of `mica service install`
//! reads it:
//!
//! ```text
//! <run folder>/udev/ublkb0    MICA_DISK=photos
//! ```
//!
//! Without the rule, such as in a container, mica makes the link itself.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub const RULE_PATH: &str = "/etc/udev/rules.d/90-mica.rules";
const LINKS: &str = "/dev/mica";
const UDEV_WAIT: Duration = Duration::from_secs(5);

/// The udev rule for the run folder `run_dir`.
pub fn rule(run_dir: &Path) -> String {
    format!(
        "# mica: /dev/mica/<disk> for each mica device. Written by `mica service install`.\n\
         KERNEL==\"ublkb*\", ACTION==\"add|change\", IMPORT{{file}}=\"{}/udev/%k\"\n\
         KERNEL==\"ublkb*\", ENV{{MICA_DISK}}==\"?*\", SYMLINK+=\"mica/$env{{MICA_DISK}}\"\n",
        run_dir.display()
    )
}

/// Names `device`, such as `/dev/ublkb0`, after `disk`, and returns the link.
pub fn link(disk: &str, device: &str, run_dir: &Path) -> Result<String> {
    let link = format!("{LINKS}/{disk}");
    let folder = run_dir.join("udev");
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join(kernel_name(device)), format!("MICA_DISK={disk}\n"))?;
    if Path::new(RULE_PATH).exists() && changed(device) && wait_for_link(&link, device) {
        return Ok(link);
    }
    std::fs::create_dir_all(LINKS)?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(device, &link).with_context(|| format!("create {link}"))?;
    Ok(link)
}

/// Removes the name. Call it after the device is gone, so that udev has
/// nothing to name again.
pub fn unlink(disk: &str, device: &str, run_dir: &Path) {
    let _ = std::fs::remove_file(run_dir.join("udev").join(kernel_name(device)));
    let _ = std::fs::remove_file(format!("{LINKS}/{disk}"));
}

fn kernel_name(device: &str) -> &str {
    device.trim_start_matches("/dev/")
}

/// Asks udev to read the rules for the device again, and waits for it.
fn changed(device: &str) -> bool {
    let status = Command::new("udevadm").args(["trigger", "--action=change", "--settle", device]).status();
    status.is_ok_and(|status| status.success())
}

fn wait_for_link(link: &str, device: &str) -> bool {
    let started = Instant::now();
    while started.elapsed() < UDEV_WAIT {
        if std::fs::canonicalize(link).is_ok_and(|target| target == Path::new(device)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log::warn!("udev did not make {link}: mica makes it");
    false
}
