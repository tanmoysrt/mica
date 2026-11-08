//! The `mica` command line.
//!
//! Each command except `mica daemon` and `mica service` sends one request
//! to the daemon. Commands on disks need the bucket of the disk. Each of
//! its values comes from a flag, else from its `OBJECT_STORAGE_*` variable,
//! else from the TOML file of `--store` or `/etc/mica/store.toml`.

mod output;
pub mod service;

use crate::config::{Config, parse_duration, parse_size};
use crate::daemon::api::{Request, call};
use crate::store::StoreConfig;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

const EXAMPLES: &str = "Examples:
  sudo mica service install                  run mica now and at each boot
  sudo mica disk create data --size 50G      a new disk in the bucket of /etc/mica/store.toml
  sudo mica disk attach data                 make /dev/mica/data on this machine
  sudo mica status                           the disks on this machine
  sudo mica disk detach data                 upload all, so another machine can attach it";

#[derive(Debug, Parser)]
#[command(
    name = "mica",
    version,
    about = "Block devices whose data lives in an S3 bucket",
    long_about = "Block devices whose data lives in an S3 bucket.\n\nThe machine keeps the newest writes and a cache on its local disk, so reads\nand writes run at local speed. The bucket holds the whole disk, so the disk\ncan move to another machine.",
    after_help = EXAMPLES,
    arg_required_else_help = true
)]
pub struct Cli {
    /// The settings of this machine. The file is optional.
    #[arg(long, global = true, default_value = crate::config::DEFAULT_PATH)]
    pub config: PathBuf,
    /// Print the answer of the daemon as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the disks attached on this machine.
    Status,
    /// Make, attach, change and delete disks.
    #[command(arg_required_else_help = true)]
    Disk {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: DiskCommand,
    },
    /// Keep a version of a disk under a name, or delete one.
    #[command(arg_required_else_help = true)]
    Snapshot {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Run mica as a systemd service.
    #[command(subcommand, arg_required_else_help = true)]
    Service(service::ServiceCommand),
    /// Run the daemon in the foreground. systemd runs it as `mica.service`.
    Daemon,
}

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// List the disks in the bucket.
    Ls,
    /// Show one disk: its size, its machine, its limits and its snapshots.
    Info { disk: String },
    /// Make a new, empty disk. It uses space in the bucket only for written data.
    #[command(arg_required_else_help = true)]
    Create {
        disk: String,
        /// The size of the disk, such as 50G. `disk set` can make it larger later.
        #[arg(long)]
        size: String,
        /// 64K to 4M [default: 1M]. Large chunks need fewer requests, small chunks make cold reads cheaper.
        #[arg(long)]
        chunk_size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Make /dev/mica/<disk> on this machine.
    Attach { disk: String },
    /// Upload all data, remove the device, and give the disk free for other machines.
    Detach { disk: String },
    /// Change the limits of a disk, or make it larger. They follow the disk between machines.
    #[command(arg_required_else_help = true)]
    Set {
        disk: String,
        /// A larger size, such as 100G. Only for a detached disk.
        #[arg(long)]
        size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Give free a disk whose machine is gone for good. The data that it did not upload is lost.
    Release { disk: String },
    /// Delete a disk and all its snapshots.
    Delete { disk: String },
    /// Make a new disk from a snapshot. The copy stays inside the bucket.
    #[command(arg_required_else_help = true)]
    Restore { disk: String, snapshot: String, new_disk: String },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// Upload all changes of a disk, and keep this version under a name.
    #[command(arg_required_else_help = true)]
    Create { disk: String, name: String },
    /// Delete a snapshot, and the data that only it uses.
    #[command(arg_required_else_help = true)]
    Delete { disk: String, name: String },
}

const DEFAULT_STORE: &str = "/etc/mica/store.toml";

#[derive(Debug, Args)]
#[command(next_help_heading = "Object storage")]
pub struct StoreArgs {
    /// A TOML file with the values below [default: /etc/mica/store.toml]
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// The URL of the S3 service, such as https://<account>.r2.cloudflarestorage.com
    #[arg(long, global = true, env = "OBJECT_STORAGE_ENDPOINT")]
    endpoint: Option<String>,
    /// The name of the bucket.
    #[arg(long, global = true, env = "OBJECT_STORAGE_BUCKET")]
    bucket: Option<String>,
    /// A folder in the bucket for the disks.
    #[arg(long, global = true, env = "OBJECT_STORAGE_PREFIX")]
    prefix: Option<String>,
    /// The region of the bucket [default: auto].
    #[arg(long, global = true, env = "OBJECT_STORAGE_REGION")]
    region: Option<String>,
    /// The access key ID.
    #[arg(long, global = true, env = "OBJECT_STORAGE_ACCESS_KEY_ID")]
    access_key_id: Option<String>,
    /// The secret key. Other users see a flag in `ps`: prefer the variable or the file.
    #[arg(long, global = true, env = "OBJECT_STORAGE_SECRET_ACCESS_KEY", hide_env_values = true)]
    secret_access_key: Option<String>,
}

impl StoreArgs {
    /// Each value from the flag or the variable, else from the file.
    pub fn resolve(&self) -> Result<StoreConfig> {
        let default = PathBuf::from(DEFAULT_STORE);
        let file = self.store.as_ref().or(default.exists().then_some(&default));
        let mut config = file.map(|path| StoreConfig::read(path)).transpose()?.unwrap_or_default();
        let values = [
            (&self.endpoint, &mut config.endpoint),
            (&self.bucket, &mut config.bucket),
            (&self.prefix, &mut config.prefix),
            (&self.region, &mut config.region),
            (&self.access_key_id, &mut config.access_key_id),
            (&self.secret_access_key, &mut config.secret_access_key),
        ];
        for (given, value) in values {
            if let Some(given) = given {
                value.clone_from(given);
            }
        }
        config.checked()
    }
}

#[derive(Debug, Args)]
pub struct LimitArgs {
    /// Space on this machine for data that is already in the bucket [new disk: 512M].
    #[arg(long)]
    cache_size: Option<String>,
    /// Space for data that is not in the bucket yet. Writes wait at this limit [new disk: 4G].
    #[arg(long)]
    unsaved_size: Option<String>,
    /// How often the changes go to the bucket, such as 30s, 5m or 1h [new disk: 5m].
    #[arg(long)]
    interval: Option<String>,
}

impl LimitArgs {
    fn request(&self) -> Result<crate::daemon::api::LimitsRequest> {
        let size = |value: &Option<String>| value.as_deref().map(parse_size).transpose();
        Ok(crate::daemon::api::LimitsRequest {
            cache_bytes: size(&self.cache_size)?,
            unsaved_bytes: size(&self.unsaved_size)?,
            interval_seconds: self.interval.as_deref().map(parse_duration).transpose()?,
        })
    }
}

pub async fn run(cli: Cli, config: Config) -> Result<()> {
    let request = match cli.command {
        Command::Daemon => unreachable!("main runs the daemon"),
        Command::Service(command) => return service::run(command, &cli.config),
        Command::Status => Request::Status,
        Command::Disk { store, command } => disk_request(command, &store)?,
        Command::Snapshot { store, command } => snapshot_request(command, &store)?,
    };
    let result: Value = call(&config.socket, &request).await?;
    match cli.json {
        true => println!("{}", serde_json::to_string_pretty(&result)?),
        false => output::print(&request, &result),
    }
    Ok(())
}

fn disk_request(command: DiskCommand, store: &StoreArgs) -> Result<Request> {
    let store = || store.resolve();
    Ok(match command {
        DiskCommand::Ls => Request::List { store: store()? },
        DiskCommand::Info { disk } => Request::Info { store: store()?, disk },
        DiskCommand::Create { disk, size, chunk_size, limits } => Request::Create {
            store: store()?,
            disk,
            size: parse_size(&size)?,
            chunk_size: chunk_size.as_deref().map(parse_size).transpose()?,
            limits: limits.request()?,
        },
        DiskCommand::Attach { disk } => Request::Attach { store: store()?, disk },
        DiskCommand::Detach { disk } => Request::Detach { disk },
        DiskCommand::Set { disk, size, limits } => {
            Request::Set { store: store()?, disk, size: size.as_deref().map(parse_size).transpose()?, limits: limits.request()? }
        }
        DiskCommand::Release { disk } => Request::Release { store: store()?, disk },
        DiskCommand::Delete { disk } => Request::Delete { store: store()?, disk },
        DiskCommand::Restore { disk, snapshot, new_disk } => Request::Restore { store: store()?, disk, snapshot, target: new_disk },
    })
}

fn snapshot_request(command: SnapshotCommand, store: &StoreArgs) -> Result<Request> {
    Ok(match command {
        SnapshotCommand::Create { disk, name } => Request::SnapshotCreate { store: store.resolve()?, disk, snapshot: name },
        SnapshotCommand::Delete { disk, name } => Request::SnapshotDelete { store: store.resolve()?, disk, snapshot: name },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_beats_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("store.toml");
        std::fs::write(&file, "endpoint = \"http://file\"\nbucket = \"from-file\"\n").unwrap();
        let cli = Cli::try_parse_from(["mica", "disk", "ls", "--store", file.to_str().unwrap(), "--bucket", "from-flag"]).unwrap();
        let Command::Disk { store, .. } = cli.command else { panic!("not a disk command") };
        let store = store.resolve().unwrap();
        assert_eq!((store.endpoint.as_str(), store.bucket.as_str(), store.region.as_str()), ("http://file", "from-flag", "auto"));
    }
}
