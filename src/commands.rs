use crate::api::{Request, send};
use crate::config::Config;
use crate::disk_commands::{self, DiskCommand};
use crate::managed_mount;
use crate::output;
use crate::service::{self, ServiceCommand};
use crate::snapshot_commands::{self, SnapshotCommand};
use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use dialoguer::Confirm;
use std::io::IsTerminal;
use std::path::Path;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show attached disks
    Status,
    /// Create, attach, mount and delete disks
    Disk {
        #[command(subcommand)]
        command: DiskCommand,
    },
    /// Create and delete snapshots
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Delete data that no disk or snapshot needs
    Gc {
        /// Keep objects written within this time, such as 30m or 2h [default: 24h from the config]
        #[arg(long, value_name = "TIME")]
        grace: Option<String>,
        /// Do not ask first
        #[arg(short, long)]
        yes: bool,
    },
    /// Show and prune the local chunk cache
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Set up and manage the mica service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Run the daemon (systemd runs this)
    #[command(hide = true)]
    Daemon,
    /// Used by the mica-mount@ systemd unit
    #[command(hide = true)]
    MountUnit {
        #[command(subcommand)]
        action: MountUnitAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Show how much the cache holds
    Status,
    /// Delete cached chunks that no attached disk uses
    Prune {
        /// Do not ask first
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum MountUnitAction {
    Start { disk: String },
    Stop { disk: String },
}

pub async fn run(command: Command, config_path: &Path, config: &Config) -> Result<()> {
    let socket = config.socket.as_path();
    match command {
        Command::Daemon => unreachable!("the daemon runs from main"),
        Command::Disk { command } => disk_commands::run(command, socket).await,
        Command::Snapshot { command } => snapshot_commands::run(command, socket).await,
        Command::Gc { grace, yes } => collect_garbage(socket, grace.as_deref(), yes).await,
        Command::Cache { command: CacheCommand::Status } => {
            output::print_cache_usage(&send(socket, &Request::CacheStatus).await?);
            Ok(())
        }
        Command::Cache { command: CacheCommand::Prune { yes } } => prune_cache(socket, yes).await,
        Command::Status => {
            output::print_status(&send(socket, &Request::Status).await?);
            Ok(())
        }
        Command::Service { command } => service::run(command, config_path, config).await,
        Command::MountUnit { action: MountUnitAction::Start { disk } } => {
            managed_mount::unit_start(socket, config_path, &disk).await
        }
        Command::MountUnit { action: MountUnitAction::Stop { disk } } => managed_mount::unit_stop(socket, &disk).await,
    }
}

/// Counts the garbage first, then asks. With `-y` it deletes in one pass.
async fn collect_garbage(socket: &Path, grace: Option<&str>, yes: bool) -> Result<()> {
    let grace_secs = grace
        .map(|text| humantime::parse_duration(text).map_err(|_| anyhow!("bad time: {text}. Use 30s, 10m, 2h or 1d")))
        .transpose()?
        .map(|grace| grace.as_secs());
    if !yes {
        let preview = send(socket, &Request::Gc { delete: false, grace_secs }).await?;
        output::print_gc_preview(&preview);
        let report = &preview["report"];
        if report["garbage_chunks"] == 0 && report["garbage_manifests"] == 0 {
            return Ok(());
        }
        if !ask_before_delete("Delete them?", false)? {
            return Ok(());
        }
    }
    let result = send(socket, &Request::Gc { delete: true, grace_secs }).await?;
    output::print_gc_result(&result["report"]);
    Ok(())
}

/// Shows what prune would delete, then asks.
async fn prune_cache(socket: &Path, yes: bool) -> Result<()> {
    if !yes {
        let preview = send(socket, &Request::CachePrune { delete: false }).await?;
        let usage = &preview["usage"];
        let number = |key: &str| usage[key].as_u64().unwrap_or(0);
        let (unused_chunks, unused_bytes) =
            (number("chunks") - number("in_use_chunks"), number("bytes") - number("in_use_bytes"));
        println!("Attached disks use {} chunks ({}). They stay.", number("in_use_chunks"), output::bytes(number("in_use_bytes")));
        if unused_chunks == 0 {
            println!("Nothing to delete.");
            return Ok(());
        }
        println!("{unused_chunks} chunks ({}) are not used by any attached disk.", output::bytes(unused_bytes));
        if !ask_before_delete("Delete them?", false)? {
            return Ok(());
        }
    }
    let result = send(socket, &Request::CachePrune { delete: true }).await?;
    let deleted = result["deleted_chunks"].as_u64().unwrap_or(0);
    println!("Deleted {deleted} chunks and freed {}", output::bytes(result["deleted_bytes"].as_u64().unwrap_or(0)));
    Ok(())
}

/// Details cost S3 reads for every item, so a list asks first. Without a
/// terminal it does not ask, and the caller prints only the names.
pub fn ask_before_reads(count: usize, reads_per_item: usize, kind: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let question = format!("Found {count} {kind}. Their details need {} reads from S3. Continue?", count * reads_per_item);
    Ok(Confirm::new().with_prompt(question).default(true).interact()?)
}

/// Deleting asks first, and the answer defaults to no. Scripts pass `-y`.
pub fn ask_before_delete(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("add -y to delete without a question");
    }
    let confirmed = Confirm::new().with_prompt(question).default(false).interact()?;
    if !confirmed {
        println!("Nothing was deleted.");
    }
    Ok(confirmed)
}

/// Accepts plain bytes or a K, M, G or T suffix (powers of 1024), e.g. `20G`.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim().trim_end_matches(['B', 'b']).trim_end_matches(['i', 'I']);
    let (number, shift) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 10),
        Some('M' | 'm') => (&text[..text.len() - 1], 20),
        Some('G' | 'g') => (&text[..text.len() - 1], 30),
        Some('T' | 't') => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    let number: u64 = number.trim().parse().map_err(|_| anyhow!("bad size: {text}"))?;
    if number == 0 {
        bail!("the size must be more than zero");
    }
    number.checked_mul(1 << shift).ok_or_else(|| anyhow!("size is too large: {text}"))
}
