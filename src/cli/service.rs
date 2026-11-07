//! `mica service`: the systemd unit of the daemon.
//!
//! The unit runs `mica daemon` as root. At a stop, the daemon runs one last
//! transaction for each disk, and leaves each device paused for the next
//! start. So a restart costs the guests a pause of about one second.
//! Unmount and detach all disks before the node shuts down.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::Path;
use std::process::Command;

const UNIT: &str = "/etc/systemd/system/mica.service";

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
}

pub fn run(command: ServiceCommand, config: &Path) -> Result<()> {
    match command {
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

fn systemctl(args: &[&str]) -> Result<()> {
    check(Command::new("systemctl").args(args).status()?, "systemctl")
}

fn check(status: std::process::ExitStatus, program: &str) -> Result<()> {
    if !status.success() {
        bail!("{program} failed: {status}");
    }
    Ok(())
}
