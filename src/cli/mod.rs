//! The `mica` command line.
//!
//! Each command except `mica daemon` and `mica service` sends one request
//! to the daemon. Commands on disks need the bucket of the disk: give a
//! TOML file with `--store`, or set the variables `S3_ENDPOINT`,
//! `S3_BUCKET`, `S3_PREFIX`, `S3_REGION`, `S3_ACCESS_KEY_ID` and
//! `S3_SECRET_ACCESS_KEY`.

mod mount;
mod output;
pub mod service;

use crate::config::{Config, parse_size};
use crate::daemon::api::{Request, call};
use crate::store::StoreConfig;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "mica", version, about = "Block devices whose data lives in an S3 bucket")]
pub struct Cli {
    #[arg(long, global = true, default_value = crate::config::DEFAULT_PATH)]
    pub config: PathBuf,
    /// The bucket of the disk, as a TOML file.
    #[arg(long, global = true, env = "MICA_STORE")]
    pub store: Option<PathBuf>,
    /// Print the answer of the daemon as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon. systemd runs it as `mica.service`.
    Daemon,
    /// Show the disks attached on this node.
    Status,
    #[command(subcommand)]
    Disk(DiskCommand),
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    #[command(subcommand)]
    Service(service::ServiceCommand),
}

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// List the disks in the bucket.
    Ls,
    /// Show one disk and its snapshots.
    Info { disk: String },
    /// Make a new, empty disk. It uses no space until it is written.
    Create {
        disk: String,
        #[arg(long)]
        size: String,
        /// 64K to 4M. Small chunks make cold reads cheap, large chunks need fewer requests.
        #[arg(long)]
        chunk_size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Make /dev/mica/<disk> on this node.
    Attach {
        disk: String,
        /// Take the disk from another node. Only when that node is off.
        #[arg(long)]
        force: bool,
        /// Do not download the rest of the disk in the background.
        #[arg(long)]
        no_fill: bool,
    },
    /// Save all data in the bucket, and give the disk free for other nodes.
    Detach { disk: String },
    /// Attach, format a new disk, and mount it.
    Mount {
        disk: String,
        path: PathBuf,
        #[arg(long, default_value = "ext4")]
        fs: String,
        #[arg(long)]
        force: bool,
        /// Do not download the rest of the disk in the background.
        #[arg(long)]
        no_fill: bool,
    },
    /// Trim, unmount and detach. Takes a disk name or a mount path.
    Unmount { target: String },
    /// Upload the changes of an attached disk now.
    Commit { disk: String },
    /// Change the budgets of a disk. They follow the disk between nodes.
    SetLimits {
        disk: String,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Make a detached disk larger.
    Resize {
        disk: String,
        #[arg(long)]
        size: String,
    },
    /// Delete a disk and all its snapshots.
    Delete {
        disk: String,
        #[arg(long)]
        force: bool,
    },
    /// Make a new disk from a snapshot.
    Restore { disk: String, snapshot: String, new_disk: String },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// List the snapshots of a disk.
    Ls { disk: String },
    /// Keep the current version of a disk under a name.
    Create { disk: String, name: String },
    /// Delete a snapshot. Its objects that no other version uses go.
    Delete { disk: String, name: String },
}

#[derive(Debug, Args)]
pub struct LimitArgs {
    /// Clean chunks kept on the node for this disk.
    #[arg(long)]
    cache_size: Option<String>,
    /// Data not in the bucket yet. Writes wait at this limit.
    #[arg(long)]
    unsaved_size: Option<String>,
}

impl LimitArgs {
    fn request(&self) -> Result<crate::daemon::api::LimitsRequest> {
        let size = |value: &Option<String>| value.as_deref().map(parse_size).transpose();
        Ok(crate::daemon::api::LimitsRequest { cache_bytes: size(&self.cache_size)?, unsaved_bytes: size(&self.unsaved_size)? })
    }
}

pub async fn run(cli: Cli, config: Config) -> Result<()> {
    let store = || StoreConfig::load(cli.store.as_deref());
    let snapshots_only = matches!(cli.command, Command::Snapshot(_));
    let request = match cli.command {
        Command::Daemon => unreachable!("main runs the daemon"),
        Command::Service(command) => return service::run(command, &cli.config),
        Command::Status => Request::Status,
        Command::Disk(DiskCommand::Ls) => Request::List { store: store()? },
        Command::Disk(DiskCommand::Info { disk }) => Request::Info { store: store()?, disk },
        Command::Disk(DiskCommand::Create { disk, size, chunk_size, limits }) => Request::Create {
            store: store()?,
            disk,
            size: parse_size(&size)?,
            chunk_size: chunk_size.as_deref().map(parse_size).transpose()?,
            limits: limits.request()?,
        },
        Command::Disk(DiskCommand::Attach { disk, force, no_fill }) => Request::Attach { store: store()?, disk, force, fill: !no_fill },
        Command::Disk(DiskCommand::Detach { disk }) => Request::Detach { disk },
        Command::Disk(DiskCommand::Mount { disk, path, fs, force, no_fill }) => {
            let attach = Request::Attach { store: store()?, disk, force, fill: !no_fill };
            return mount::mount(&config, attach, &path, &fs).await;
        }
        Command::Disk(DiskCommand::Unmount { target }) => return mount::unmount(&config, &target).await,
        Command::Disk(DiskCommand::Commit { disk }) => Request::Commit { disk },
        Command::Disk(DiskCommand::SetLimits { disk, limits }) => Request::SetLimits { store: store()?, disk, limits: limits.request()? },
        Command::Disk(DiskCommand::Resize { disk, size }) => Request::Resize { store: store()?, disk, size: parse_size(&size)? },
        Command::Disk(DiskCommand::Delete { disk, force }) => Request::Delete { store: store()?, disk, force },
        Command::Disk(DiskCommand::Restore { disk, snapshot, new_disk }) => {
            Request::Restore { store: store()?, disk, snapshot, target: new_disk }
        }
        Command::Snapshot(SnapshotCommand::Ls { disk }) => Request::Info { store: store()?, disk },
        Command::Snapshot(SnapshotCommand::Create { disk, name }) => Request::SnapshotCreate { store: store()?, disk, snapshot: name },
        Command::Snapshot(SnapshotCommand::Delete { disk, name }) => Request::SnapshotDelete { store: store()?, disk, snapshot: name },
    };
    let result: Value = call(&config.socket, &request).await?;
    match cli.json {
        true => println!("{}", serde_json::to_string_pretty(&result)?),
        false => output::print(&request, &result, snapshots_only),
    }
    Ok(())
}
