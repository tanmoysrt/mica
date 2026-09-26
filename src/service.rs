use crate::api::{Request, send};
use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::local_state::DiskFolder;
use crate::managed_mount::MountSpec;
use crate::systemd;
use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

pub const BINARY_PATH: &str = "/usr/local/bin/mica";
pub const SERVICE_UNIT: &str = "mica.service";
pub const MOUNT_TEMPLATE_UNIT: &str = "mica-mount@.service";

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Configure mica and start the service
    Setup {
        /// Use the config file as it is, without questions
        #[arg(long)]
        use_config_file: bool,
    },
    /// Start mica
    Start,
    /// Stop mica
    Stop {
        /// Stop even with disks attached
        #[arg(long)]
        force: bool,
    },
    /// Restart mica; attached disks keep working
    Restart {
        /// Restart even if a disk cannot keep its device
        #[arg(long)]
        force: bool,
    },
    /// Show the service and the attached disks
    Status,
    /// Show the logs
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Start mica at boot
    Enable,
    /// Do not start mica at boot
    Disable,
    /// Write the systemd units
    Install,
    /// Remove the systemd units
    Uninstall {
        /// Also delete the config and the local cache
        #[arg(long)]
        purge: bool,
    },
}

pub fn mount_unit(disk_id: &str) -> String {
    format!("mica-mount@{disk_id}.service")
}

pub fn is_installed() -> bool {
    systemd::unit_exists(SERVICE_UNIT)
}

pub async fn run(command: ServiceCommand, config_path: &Path, config: &Config) -> Result<()> {
    match command {
        ServiceCommand::Setup { use_config_file } => crate::setup::run(config_path, use_config_file).await?,
        ServiceCommand::Install => install(config_path, config)?,
        ServiceCommand::Uninstall { purge } => uninstall(config_path, config, purge).await?,
        ServiceCommand::Enable => systemd::systemctl(&["enable", SERVICE_UNIT])?,
        ServiceCommand::Disable => systemd::systemctl(&["disable", SERVICE_UNIT])?,
        ServiceCommand::Start => systemd::systemctl(&["start", SERVICE_UNIT])?,
        ServiceCommand::Stop { force } => {
            if !force {
                refuse_if_disks_attached(config, "stop").await?;
            }
            systemd::systemctl(&["stop", SERVICE_UNIT])?;
        }
        ServiceCommand::Restart { force } => {
            if !force {
                refuse_if_not_recoverable(config).await?;
            }
            systemd::systemctl(&["restart", SERVICE_UNIT])?;
        }
        ServiceCommand::Status => status(config).await,
        ServiceCommand::Logs { follow } => logs(follow)?,
    }
    Ok(())
}

/// Writes both units. The mount units only want mica, so that a restart of
/// mica does not restart (and so unmount) every mounted disk.
pub fn install(config_path: &Path, config: &Config) -> Result<()> {
    config.validate_memory_high()?;
    let config_arg = if config_path == Path::new(DEFAULT_CONFIG_PATH) {
        String::new()
    } else {
        format!(" --config {}", config_path.display())
    };
    systemd::write_unit(SERVICE_UNIT, &service_unit_text(&config_arg, config.memory_high.trim()))?;
    systemd::write_unit(MOUNT_TEMPLATE_UNIT, &mount_unit_text(&config_arg))?;
    systemd::systemctl(&["daemon-reload"])?;
    println!("  ok  installed {SERVICE_UNIT} and {MOUNT_TEMPLATE_UNIT}");
    Ok(())
}

async fn uninstall(config_path: &Path, config: &Config, purge: bool) -> Result<()> {
    refuse_if_disks_attached(config, "uninstall").await?;
    if !MountSpec::list(config_path)?.is_empty() {
        bail!("some disks are mounted by mica. Run `mica disk unmount` for them first");
    }
    let local = DiskFolder::list(&config.data_dir)?;
    if !local.is_empty() {
        let names: Vec<&str> = local.iter().map(|folder| folder.disk_id.as_str()).collect();
        bail!("local data that may not be in S3 exists for: {}. Start mica to upload it first", names.join(", "));
    }
    systemd::systemctl(&["disable", "--now", SERVICE_UNIT])?;
    systemd::remove_unit(SERVICE_UNIT)?;
    systemd::remove_unit(MOUNT_TEMPLATE_UNIT)?;
    systemd::systemctl(&["daemon-reload"])?;
    println!("Removed the mica units");
    if purge {
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(&config.data_dir);
        println!("Removed {} and {}", config_path.display(), config.data_dir.display());
    }
    Ok(())
}

async fn status(config: &Config) {
    let _ = Command::new("systemctl").args(["status", SERVICE_UNIT, "--no-pager", "--lines=0"]).status();
    println!();
    match send(&config.socket, &Request::Status).await {
        Ok(status) => crate::output::print_status(&status),
        Err(error) => println!("{error:#}"),
    }
}

fn logs(follow: bool) -> Result<()> {
    let mut command = Command::new("journalctl");
    command.args(["-u", SERVICE_UNIT, "-u", "mica-mount@*", "--no-pager"]);
    if follow {
        command.arg("-f");
    }
    command.status()?;
    Ok(())
}

async fn attached_disks(config: &Config) -> Vec<Value> {
    match send(&config.socket, &Request::Status).await {
        Ok(status) => status["disks"].as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn refuse_if_disks_attached(config: &Config, action: &str) -> Result<()> {
    let disks = attached_disks(config).await;
    if disks.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = disks.iter().map(|disk| disk["disk"].as_str().unwrap_or("?").to_string()).collect();
    let hint = if action == "stop" { ", or use --force" } else { "" };
    bail!(
        "cannot {action}: disks are attached here: {}. Detach or unmount them first{hint}",
        names.join(", ")
    )
}

/// Devices made by an older mica cannot be taken over, and would die in a restart.
pub async fn refuse_if_not_recoverable(config: &Config) -> Result<()> {
    let disks = attached_disks(config).await;
    let blocked: Vec<String> = disks
        .iter()
        .filter(|disk| disk["recoverable"] != true)
        .map(|disk| disk["disk"].as_str().unwrap_or("?").to_string())
        .collect();
    if !blocked.is_empty() {
        bail!(
            "cannot restart: these disks would lose their device: {}. Detach them first, or use --force",
            blocked.join(", ")
        );
    }
    Ok(())
}

fn service_unit_text(config_arg: &str, memory_high: &str) -> String {
    let memory_line = match memory_high {
        "" => String::new(),
        limit => format!("MemoryHigh={limit}\n"),
    };
    format!(
        "# Managed by mica. `mica setup` rewrites this file.
[Unit]
Description=mica: disks that live in S3
Wants=network-online.target
After=network-online.target

[Service]
Type=notify
ExecStartPre=-modprobe ublk_drv
ExecStart={BINARY_PATH}{config_arg} daemon
Restart=always
RestartSec=1
LimitNOFILE=1048576
{memory_line}TimeoutStopSec=5min

[Install]
WantedBy=multi-user.target
"
    )
}

fn mount_unit_text(config_arg: &str) -> String {
    format!(
        "# Managed by mica. `mica disk mount` and `mica disk unmount` use this template.
[Unit]
Description=mica disk %i, attached and mounted
Wants=mica.service
After=mica.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={BINARY_PATH}{config_arg} mount-unit start %i
ExecStop={BINARY_PATH}{config_arg} mount-unit stop %i
TimeoutStartSec=infinity
TimeoutStopSec=infinity

[Install]
WantedBy=multi-user.target
"
    )
}
