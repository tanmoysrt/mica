//! `mica service`: the systemd unit of the daemon.
//!
//! The unit runs `mica daemon` as root. At a stop, the daemon runs one last
//! transaction for each disk, and leaves each device paused for the next
//! start. So a restart costs the guests a pause of about one second.
//!
//! After a reboot, the daemon attaches each disk that the node held again,
//! before it tells systemd that it is ready. The udev rule of the install
//! names each device `/dev/mica/<disk>` (see `ublk::names`), so systemd
//! knows it, and an fstab entry with `x-systemd.requires=mica.service`
//! mounts it at boot.

use crate::config::Config;
use crate::ublk::names;
use anyhow::{Context, Result, bail};
use console::style;
use clap::Subcommand;
use std::path::Path;
use std::process::Command;

const UNIT: &str = "/etc/systemd/system/mica.service";

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Start mica now and at each boot. After an upgrade, it restarts mica with the new binary
    Install,
    /// Stop mica, remove it from the boot, and delete the keys of its buckets. Refused while disks are attached
    Uninstall,
    /// Show whether mica runs, and its last log lines
    Status,
}

pub fn run(command: ServiceCommand, config: &Path, settings: &Config) -> Result<()> {
    if !matches!(command, ServiceCommand::Status) && unsafe { libc::geteuid() } != 0 {
        bail!("this needs root: run it with sudo");
    }
    match command {
        ServiceCommand::Install => install(config, &settings.run_dir),
        ServiceCommand::Uninstall => uninstall(&settings.data_dir),
        ServiceCommand::Status => systemctl(&["status", "--no-pager", "mica.service"]),
    }
}

/// The keys of the buckets go too: nothing on the machine uses them after.
/// The cache stays, for a later install.
fn uninstall(data_dir: &Path) -> Result<()> {
    if std::fs::read_dir("/dev/mica").is_ok_and(|mut links| links.next().is_some()) {
        bail!("disks are attached: detach them first (see `sudo mica status`)");
    }
    systemctl(&["disable", "--now", "mica.service"])?;
    std::fs::remove_file(UNIT).with_context(|| format!("remove {UNIT}"))?;
    systemctl(&["daemon-reload"])?;
    let _ = std::fs::remove_file(names::RULE_PATH);
    let buckets = data_dir.join("buckets");
    if buckets.exists() {
        std::fs::remove_dir_all(&buckets).with_context(|| format!("remove {}", buckets.display()))?;
    }
    println!("{} mica is stopped, and does not start at boot any more", style("✔").green().bold());
    println!("  The keys of its buckets are deleted. The cache stays in {}.", data_dir.display());
    Ok(())
}

fn install(config: &Path, run_dir: &Path) -> Result<()> {
    std::fs::write(names::RULE_PATH, names::rule(run_dir)).with_context(|| format!("write {}", names::RULE_PATH))?;
    execute("udevadm", &["control", "--reload"])?;
    let binary = std::env::current_exe()?.canonicalize()?;
    let unit = format!(
        "[Unit]\n\
         Description=mica block devices\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=notify\n\
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
    std::fs::write(UNIT, unit).with_context(|| format!("write {UNIT}"))?;
    systemctl(&["daemon-reload"])?;
    // A restart, so that an install after an upgrade runs the new binary.
    systemctl(&["enable", "mica.service"])?;
    systemctl(&["restart", "mica.service"])?;
    println!("{} mica runs now, and starts at each boot", style("✔").green().bold());
    println!("  Make a disk: {}", style("sudo mica disk create data --size 50G --store s3.toml").cyan());
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    execute("systemctl", args)
}

fn execute(program: &str, args: &[&str]) -> Result<()> {
    check(Command::new(program).args(args).status().with_context(|| format!("run {program}"))?, program)
}

fn check(status: std::process::ExitStatus, program: &str) -> Result<()> {
    if !status.success() {
        bail!("{program} failed: {status}");
    }
    Ok(())
}
