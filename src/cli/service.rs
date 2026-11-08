//! `mica service`: the systemd unit of the daemon.
//!
//! The unit runs `mica daemon` as root. At a stop, the daemon runs one last
//! transaction for each disk, and leaves each device paused for the next
//! start. So a restart costs the guests a pause of about one second.
//!
//! On a laptop or a desktop, `mica service add-disk` mounts a disk at each
//! boot. Its unit only unmounts at shutdown: the local data stays, so a
//! shutdown never waits for a full upload. The next boot goes on from it.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::Path;
use std::process::Command;

const UNIT: &str = "/etc/systemd/system/mica.service";
/// The bucket files of the disks that mount at boot.
const DISKS: &str = "/etc/mica/disks";

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Write the unit, then start mica and start it at boot.
    Install,
    /// Stop mica and remove the unit. Refused while disks are attached.
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Mount a disk at each boot, for the user who runs sudo. Needs --store.
    AddDisk { disk: String, path: std::path::PathBuf },
    /// Stop the mount at boot. The disk stays in its bucket.
    RemoveDisk { disk: String },
}

pub fn run(command: ServiceCommand, config: &Path, store: Option<&Path>) -> Result<()> {
    match command {
        ServiceCommand::AddDisk { disk, path } => add_disk(config, store, &disk, &path),
        ServiceCommand::RemoveDisk { disk } => {
            let _ = systemctl(&["disable", "--now", &format!("mica-disk-{disk}.service")]);
            let _ = std::fs::remove_file(format!("{DISKS}/{disk}.toml"));
            std::fs::remove_file(disk_unit(&disk)).with_context(|| format!("remove {}", disk_unit(&disk)))?;
            systemctl(&["daemon-reload"])
        }
        ServiceCommand::Install => install(config),
        ServiceCommand::Uninstall => {
            if std::fs::read_dir("/dev/mica").is_ok_and(|mut links| links.next().is_some()) {
                bail!("disks are attached: detach them first");
            }
            systemctl(&["disable", "--now", "mica.service"])?;
            std::fs::remove_file(UNIT).with_context(|| format!("remove {UNIT}"))?;
            systemctl(&["daemon-reload"])
        }
        ServiceCommand::Start => systemctl(&["start", "mica.service"]),
        ServiceCommand::Stop => systemctl(&["stop", "mica.service"]),
        ServiceCommand::Restart => systemctl(&["restart", "mica.service"]),
        ServiceCommand::Status => systemctl(&["status", "--no-pager", "mica.service"]),
        ServiceCommand::Logs { follow } => {
            let mut args = vec!["-u", "mica.service", "--no-pager"];
            if follow {
                args.push("-f");
            }
            check(Command::new("journalctl").args(args).status()?, "journalctl")
        }
    }
}

fn install(config: &Path) -> Result<()> {
    let binary = std::env::current_exe()?.canonicalize()?;
    let unit = format!(
        "[Unit]\n\
         Description=mica block devices\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         ExecStartPre=-/sbin/modprobe ublk_drv\n\
         ExecStart={} --config {} daemon\n\
         Restart=on-failure\n\
         KillMode=process\n\
         TimeoutStopSec=90\n\
         LimitNOFILE=1048576\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        binary.display(),
        config.display()
    );
    std::fs::write(UNIT, unit).with_context(|| format!("write {UNIT} (are you root?)"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "mica.service"])?;
    println!("mica runs as mica.service");
    Ok(())
}

/// Copies the bucket file to /etc/mica/disks (root only) and writes a unit
/// that mounts the disk after mica starts.
fn add_disk(config: &Path, store: Option<&Path>, disk: &str, path: &Path) -> Result<()> {
    crate::store::validate_name(disk)?;
    let Some(store) = store else { bail!("give the bucket of the disk with --store") };
    crate::store::StoreConfig::load(Some(store))?;
    std::fs::create_dir_all(DISKS)?;
    let saved = format!("{DISKS}/{disk}.toml");
    std::fs::copy(store, &saved).with_context(|| format!("copy {} (are you root?)", store.display()))?;
    std::fs::set_permissions(&saved, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let (binary, path) = (std::env::current_exe()?.canonicalize()?, std::path::absolute(path)?);
    let user = std::env::var("SUDO_USER").unwrap_or_else(|_| "root".into());
    let unit = format!(
        "[Unit]\n\
         Description=mica disk {disk} at {path}\n\
         Requires=mica.service\n\
         After=mica.service network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={binary} --config {config} --store {saved} disk mount {disk} {path}\n\
         ExecStartPost=/usr/bin/chown {user}: {path}\n\
         ExecStop=/usr/bin/umount {path}\n\
         Restart=on-failure\n\
         RestartSec=10\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        path = path.display(),
        binary = binary.display(),
        config = config.display(),
    );
    std::fs::write(disk_unit(disk), unit)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &format!("mica-disk-{disk}.service")])?;
    println!("Disk {disk} is mounted at {} for {user}, and at each boot", path.display());
    Ok(())
}

fn disk_unit(disk: &str) -> String {
    format!("/etc/systemd/system/mica-disk-{disk}.service")
}

fn systemctl(args: &[&str]) -> Result<()> {
    check(Command::new("systemctl").args(args).status()?, "systemctl")
}

fn check(status: std::process::ExitStatus, program: &str) -> Result<()> {
    if !status.success() {
        bail!("{program} failed: {status}");
    }
    Ok(())
}
