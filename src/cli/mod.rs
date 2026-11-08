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

#[derive(Debug, Parser)]
#[command(name = "mica", version, about = "Block devices whose data lives in an S3 bucket")]
pub struct Cli {
    #[arg(long, global = true, default_value = crate::config::DEFAULT_PATH)]
    pub config: PathBuf,
    /// Print the answer of the daemon as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(flatten)]
    pub store: StoreArgs,
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
    Attach { disk: String },
    /// Save all data in the bucket, and give the disk free for other nodes.
    Detach { disk: String },
    /// Change the size or the limits of a disk. They follow the disk between nodes.
    Set {
        disk: String,
        /// A larger size. Only for a detached disk.
        #[arg(long)]
        size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Give free a disk whose node is gone for good. The unsaved data of that node is lost.
    Release { disk: String },
    /// Delete a disk and all its snapshots.
    Delete { disk: String },
    /// Make a new disk from a snapshot.
    Restore { disk: String, snapshot: String, new_disk: String },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// Keep the current version of a disk under a name.
    Create { disk: String, name: String },
    /// Delete a snapshot. Its objects that no other version uses go.
    Delete { disk: String, name: String },
}

const DEFAULT_STORE: &str = "/etc/mica/store.toml";

#[derive(Debug, Args)]
#[command(next_help_heading = "Object storage")]
pub struct StoreArgs {
    /// A TOML file with the values below [default: /etc/mica/store.toml]
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[arg(long, global = true, env = "OBJECT_STORAGE_ENDPOINT")]
    endpoint: Option<String>,
    #[arg(long, global = true, env = "OBJECT_STORAGE_BUCKET")]
    bucket: Option<String>,
    /// The disks go under this path in the bucket.
    #[arg(long, global = true, env = "OBJECT_STORAGE_PREFIX")]
    prefix: Option<String>,
    #[arg(long, global = true, env = "OBJECT_STORAGE_REGION")]
    region: Option<String>,
    #[arg(long, global = true, env = "OBJECT_STORAGE_ACCESS_KEY_ID")]
    access_key_id: Option<String>,
    /// Other users see a flag in `ps`: prefer the variable or the file.
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
    /// Clean chunks kept on the node for this disk.
    #[arg(long)]
    cache_size: Option<String>,
    /// Data not in the bucket yet. Writes wait at this limit.
    #[arg(long)]
    unsaved_size: Option<String>,
    /// How often a transaction uploads the changes: 30s, 5m, 1h.
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
    let store = || cli.store.resolve();
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
        Command::Disk(DiskCommand::Attach { disk }) => Request::Attach { store: store()?, disk },
        Command::Disk(DiskCommand::Detach { disk }) => Request::Detach { disk },
        Command::Disk(DiskCommand::Set { disk, size, limits }) => {
            Request::Set { store: store()?, disk, size: size.as_deref().map(parse_size).transpose()?, limits: limits.request()? }
        }
        Command::Disk(DiskCommand::Release { disk }) => Request::Release { store: store()?, disk },
        Command::Disk(DiskCommand::Delete { disk }) => Request::Delete { store: store()?, disk },
        Command::Disk(DiskCommand::Restore { disk, snapshot, new_disk }) => {
            Request::Restore { store: store()?, disk, snapshot, target: new_disk }
        }
        Command::Snapshot(SnapshotCommand::Create { disk, name }) => Request::SnapshotCreate { store: store()?, disk, snapshot: name },
        Command::Snapshot(SnapshotCommand::Delete { disk, name }) => Request::SnapshotDelete { store: store()?, disk, snapshot: name },
    };
    let result: Value = call(&config.socket, &request).await?;
    match cli.json {
        true => println!("{}", serde_json::to_string_pretty(&result)?),
        false => output::print(&request, &result),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_beats_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("store.toml");
        std::fs::write(&file, "endpoint = \"http://file\"\nbucket = \"from-file\"\n").unwrap();
        let cli = Cli::try_parse_from(["mica", "--store", file.to_str().unwrap(), "--bucket", "from-flag", "status"]).unwrap();
        let store = cli.store.resolve().unwrap();
        assert_eq!((store.endpoint.as_str(), store.bucket.as_str(), store.region.as_str()), ("http://file", "from-flag", "auto"));
    }
}
