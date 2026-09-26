use crate::api::{Request, send};
use crate::commands::{ask_before_delete, ask_before_reads};
use crate::output;
use anyhow::Result;
use clap::Subcommand;
use std::path::Path;

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// List all snapshots
    Ls {
        /// Do not ask before reading the details of every snapshot
        #[arg(short, long)]
        yes: bool,
    },
    /// Save a disk as a snapshot
    Create { disk: String, name: String },
    /// Delete a snapshot
    Delete {
        name: String,
        /// Do not ask first
        #[arg(short, long)]
        yes: bool,
    },
}

pub async fn run(command: SnapshotCommand, socket: &Path) -> Result<()> {
    match command {
        SnapshotCommand::Ls { yes } => list(socket, yes).await?,
        SnapshotCommand::Create { disk, name } => {
            send(socket, &Request::Snapshot { disk: disk.clone(), name: name.clone() }).await?;
            println!("Saved disk {disk} as snapshot {name}");
        }
        SnapshotCommand::Delete { name, yes } => {
            if ask_before_delete(&format!("Delete snapshot {name}? Disks made from it stay."), yes)? {
                send(socket, &Request::DeleteSnapshot { name: name.clone() }).await?;
                println!("Deleted snapshot {name}");
            }
        }
    }
    Ok(())
}

/// Listing the names is one S3 request. The details cost two reads per snapshot.
async fn list(socket: &Path, yes: bool) -> Result<()> {
    let result = send(socket, &Request::ListSnapshots).await?;
    let names: Vec<String> = serde_json::from_value(result["snapshots"].clone())?;
    if names.is_empty() {
        output::print_snapshots(&[]);
        return Ok(());
    }
    if !ask_before_reads(names.len(), 2, "snapshots", yes)? {
        names.iter().for_each(|name| println!("{name}"));
        return Ok(());
    }
    let details = send(socket, &Request::DescribeSnapshots { names }).await?;
    output::print_snapshots(details["snapshots"].as_array().map(Vec::as_slice).unwrap_or_default());
    Ok(())
}
