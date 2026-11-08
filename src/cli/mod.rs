//! The `mica` command line.
//!
//! Each command except `mica daemon` and `mica service` sends one request
//! to the daemon. A command on a disk can give the bucket of the disk: each
//! value comes from a flag, else from its `OBJECT_STORAGE_*` variable, else
//! from the TOML file of `--store`. The daemon keeps a bucket that works,
//! so later commands need only the name of the disk, or `--bucket <name>`
//! when the machine knows more than one bucket.

mod args;
mod output;
mod progress;
pub mod service;

pub use args::{Cli, Command};
use args::{BucketCommand, DiskCommand, SnapshotCommand};
use crate::config::{Config, parse_size};
use crate::daemon::api::{Command as Call, Request, call};
use anyhow::Result;

pub async fn run(cli: Cli, config: Config) -> Result<()> {
    let ((store, bucket), command) = match cli.command {
        Command::Daemon => unreachable!("main runs the daemon"),
        Command::Service(command) => return service::run(command, &cli.config, &config),
        Command::Status { watch: true } => return watch(&config).await,
        Command::Status { watch: false } => ((None, None), Call::Status),
        Command::Disk { store, command } => (store.resolve()?, disk_command(command)?),
        Command::Snapshot { store, command } => (store.resolve()?, snapshot_command(command)),
        Command::Bucket { store, command } => (store.resolve()?, bucket_command(command)),
    };
    let request = Request { store, bucket, command };
    let socket = &config.socket;
    let message = waiting(&request.command);
    let result = match &request.command {
        Call::Detach { disk } | Call::SnapshotCreate { disk, .. } => progress::call_with_upload(socket, &request, disk, message).await?,
        Call::Restore { target, .. } => progress::call_with_task(socket, &request, target, message).await?,
        _ => progress::call_with_spinner(socket, &request, message).await?,
    };
    match cli.json {
        true => println!("{}", serde_json::to_string_pretty(&result)?),
        false => output::print(&request.command, &result),
    }
    Ok(())
}

/// `mica status` again each second, on a cleared screen.
async fn watch(config: &Config) -> Result<()> {
    let terminal = console::Term::stdout();
    loop {
        let result = call(&config.socket, &Request::default()).await?;
        terminal.clear_screen()?;
        output::print(&Call::Status, &result);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// What the spinner says while the daemon works.
fn waiting(command: &Call) -> String {
    match command {
        Call::Status => "Asking mica".into(),
        Call::List => "Reading the buckets".into(),
        Call::Info { disk } => format!("Reading disk {disk}"),
        Call::Create { disk, .. } => format!("Making disk {disk}"),
        Call::Attach { disk } => format!("Attaching disk {disk}"),
        Call::Detach { disk } | Call::SnapshotCreate { disk, .. } => format!("Uploading {disk}"),
        Call::Set { disk, .. } => format!("Changing disk {disk}"),
        Call::Release { disk } => format!("Releasing disk {disk}"),
        Call::Delete { disk } => format!("Deleting disk {disk}"),
        Call::SnapshotDelete { snapshot, .. } => format!("Deleting snapshot {snapshot}"),
        Call::Restore { target, .. } => format!("Copying into disk {target}"),
        Call::BucketAdd => "Checking the bucket".into(),
        Call::BucketList => "Asking mica".into(),
        Call::BucketForget { name } => format!("Forgetting bucket {name}"),
    }
}

fn disk_command(command: DiskCommand) -> Result<Call> {
    let size = |value: Option<String>| value.as_deref().map(parse_size).transpose();
    Ok(match command {
        DiskCommand::Ls => Call::List,
        DiskCommand::Info { disk } => Call::Info { disk },
        DiskCommand::Create { disk, size: disk_size, chunk_size, limits } => {
            Call::Create { disk, size: parse_size(&disk_size)?, chunk_size: size(chunk_size)?, limits: limits.request()? }
        }
        DiskCommand::Attach { disk } => Call::Attach { disk },
        DiskCommand::Detach { disk } => Call::Detach { disk },
        DiskCommand::Set { disk, size: new_size, limits } => Call::Set { disk, size: size(new_size)?, limits: limits.request()? },
        DiskCommand::Release { disk } => Call::Release { disk },
        DiskCommand::Delete { disk } => Call::Delete { disk },
        DiskCommand::Restore { disk, snapshot, new_disk } => Call::Restore { disk, snapshot, target: new_disk },
    })
}

fn snapshot_command(command: SnapshotCommand) -> Call {
    match command {
        SnapshotCommand::Create { disk, name } => Call::SnapshotCreate { disk, snapshot: name },
        SnapshotCommand::Delete { disk, name } => Call::SnapshotDelete { disk, snapshot: name },
    }
}

fn bucket_command(command: BucketCommand) -> Call {
    match command {
        BucketCommand::Add => Call::BucketAdd,
        BucketCommand::Ls => Call::BucketList,
        BucketCommand::Forget { name } => Call::BucketForget { name },
    }
}
