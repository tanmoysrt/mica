//! The commands, options and help texts of `mica`.

use super::service;
use crate::config::{parse_duration, parse_size};
use crate::store::StoreConfig;
use anyhow::Result;
use clap::builder::styling::{AnsiColor, Styles};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

const EXAMPLES: &str = "\x1b[1;32mGet started:\x1b[0m
  sudo mica service install                              Start mica, now and at each boot
  sudo mica disk create data --size 50G --store s3.toml  Make a disk. mica keeps the bucket
  sudo mica disk attach data                             Make the device /dev/mica/data
  sudo mkfs.ext4 /dev/mica/data                          Make a filesystem, once
  sudo mica status                                       See your disks at work";

const BUCKET_FILE: &str = "\x1b[1;32mA bucket file (--store) has these lines:\x1b[0m
  endpoint = \"https://<account>.r2.cloudflarestorage.com\"
  bucket = \"my-disks\"
  access_key_id = \"<id>\"
  secret_access_key = \"<key>\"
Give the secret key in the file or its variable: other users see flags in `ps`.";

const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().bold())
    .valid(AnsiColor::Cyan.on_default().bold())
    .invalid(AnsiColor::Yellow.on_default().bold());

#[derive(Debug, Parser)]
#[command(
    name = "mica",
    version,
    about = "Block devices whose data lives in an S3 bucket",
    long_about = "Block devices whose data lives in an S3 bucket.\n\nThe machine keeps the newest writes and a cache on its local disk, so reads\nand writes run at local speed. The bucket holds the whole disk, so the disk\ncan move to another machine.",
    after_help = EXAMPLES,
    arg_required_else_help = true,
    disable_help_subcommand = true,
    max_term_width = 100,
    styles = STYLES
)]
pub struct Cli {
    /// The settings file of this machine
    #[arg(long, global = true, value_name = "FILE", default_value = crate::config::DEFAULT_PATH, help_heading = "Global options")]
    pub config: PathBuf,
    /// Print the answer as JSON
    #[arg(long, global = true, help_heading = "Global options")]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the disks attached on this machine, and how fast they work
    Status {
        /// Show it again each second, until Ctrl-C
        #[arg(short, long)]
        watch: bool,
    },
    /// Make, attach and delete disks
    #[command(arg_required_else_help = true, after_help = BUCKET_FILE)]
    Disk {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: DiskCommand,
    },
    /// Keep a version of a disk under a name
    #[command(arg_required_else_help = true)]
    Snapshot {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Add, list and forget the buckets that this machine keeps keys for
    #[command(arg_required_else_help = true, after_help = BUCKET_FILE)]
    Bucket {
        #[command(flatten)]
        store: StoreArgs,
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// Run mica as a systemd service
    #[command(subcommand, arg_required_else_help = true)]
    Service(service::ServiceCommand),
    /// Run the daemon in the foreground. `mica service install` runs it with systemd
    #[command(hide = true)]
    Daemon,
}

#[derive(Debug, Subcommand)]
pub enum DiskCommand {
    /// List the disks in the buckets that this machine knows, and where each is attached
    Ls,
    /// Show a disk: where it is, its limits and its snapshots
    #[command(arg_required_else_help = true)]
    Info {
        /// The disk
        disk: String,
    },
    /// Make a new disk. The bucket stores only the data that you write
    #[command(arg_required_else_help = true, after_help = BUCKET_FILE)]
    Create {
        /// A name of letters, digits, '-', '_' and '.'
        disk: String,
        /// The size, such as 50G. `mica disk set` can make it larger later
        #[arg(long, value_name = "SIZE")]
        size: String,
        /// The upload unit, 64K to 4M [default: 1M]
        #[arg(long, value_name = "SIZE")]
        chunk_size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Make /dev/mica/<DISK> on this machine
    #[command(arg_required_else_help = true)]
    Attach {
        /// The disk
        disk: String,
    },
    /// Upload all changes and remove the device, so another machine can attach the disk
    #[command(arg_required_else_help = true)]
    Detach {
        /// The disk
        disk: String,
    },
    /// Change the limits or the size of a disk. The limits follow the disk to other machines
    #[command(arg_required_else_help = true)]
    Set {
        /// The disk
        disk: String,
        /// A larger size, such as 100G. The disk must be detached
        #[arg(long, value_name = "SIZE")]
        size: Option<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Make a new disk from a snapshot. The bucket copies the data, so nothing downloads
    #[command(arg_required_else_help = true)]
    Restore {
        /// The disk of the snapshot
        disk: String,
        /// The snapshot
        snapshot: String,
        /// The name of the new disk
        new_disk: String,
    },
    /// Free a disk whose machine is gone for good. Changes not in the bucket are lost
    #[command(arg_required_else_help = true)]
    Release {
        /// The disk
        disk: String,
    },
    /// Delete a disk and all its snapshots
    #[command(arg_required_else_help = true)]
    Delete {
        /// The disk
        disk: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BucketCommand {
    /// Check a bucket and keep its keys. Then its disks need only their names
    Add,
    /// List the buckets that this machine knows
    Ls,
    /// Delete the keys of a bucket on this machine. Its disks stay in the bucket
    #[command(arg_required_else_help = true)]
    Forget {
        /// The bucket: its name, its name and prefix (my-disks/laptop), or its full place
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// Upload all changes of a disk, and keep this version under a name
    #[command(arg_required_else_help = true)]
    Create {
        /// The disk
        disk: String,
        /// A name for the snapshot
        name: String,
    },
    /// Delete a snapshot, and the data that only it uses
    #[command(arg_required_else_help = true)]
    Delete {
        /// The disk
        disk: String,
        /// The snapshot
        name: String,
    },
}

/// The bucket of a disk. A flag wins over its variable, and a variable wins over the file.
#[derive(Debug, Args)]
#[command(next_help_heading = "Bucket (give it once: mica keeps it)")]
pub struct StoreArgs {
    /// A TOML file with the values below
    #[arg(long, global = true, value_name = "FILE")]
    store: Option<PathBuf>,
    /// The URL of the S3 service
    #[arg(long, global = true, value_name = "URL", env = "OBJECT_STORAGE_ENDPOINT", hide_env_values = true)]
    endpoint: Option<String>,
    /// The bucket. Alone, it picks a bucket that mica knows
    #[arg(long, global = true, value_name = "NAME", env = "OBJECT_STORAGE_BUCKET", hide_env_values = true)]
    bucket: Option<String>,
    /// A folder in the bucket for the disks
    #[arg(long, global = true, value_name = "FOLDER", env = "OBJECT_STORAGE_PREFIX", hide_env_values = true)]
    prefix: Option<String>,
    /// The region [default: auto]
    #[arg(long, global = true, value_name = "NAME", env = "OBJECT_STORAGE_REGION", hide_env_values = true)]
    region: Option<String>,
    /// The access key
    #[arg(long, global = true, value_name = "ID", env = "OBJECT_STORAGE_ACCESS_KEY_ID", hide_env_values = true)]
    access_key_id: Option<String>,
    /// The secret key
    #[arg(long, global = true, value_name = "KEY", env = "OBJECT_STORAGE_SECRET_ACCESS_KEY", hide_env_values = true)]
    secret_access_key: Option<String>,
}

impl StoreArgs {
    /// The bucket: its keys, or the name of a bucket that the daemon knows.
    /// Each value comes from the flag or the variable, else from the file.
    /// A bucket name alone, maybe with a prefix, names a known bucket.
    /// Nothing: the daemon looks in all the buckets that it knows.
    pub fn resolve(&self) -> Result<(Option<StoreConfig>, Option<String>)> {
        let keys = [&self.endpoint, &self.region, &self.access_key_id, &self.secret_access_key];
        if self.store.is_none() && keys.iter().all(|value| value.is_none()) {
            let name = self.bucket.as_ref().map(|bucket| match &self.prefix {
                Some(prefix) => format!("{bucket}/{prefix}"),
                None => bucket.clone(),
            });
            return Ok((None, name));
        }
        let mut config = self.store.as_deref().map(StoreConfig::read).transpose()?.unwrap_or_default();
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
        Ok((Some(config.checked()?), None))
    }
}

#[derive(Debug, Args)]
pub struct LimitArgs {
    /// Local space for a copy of data that is in the bucket [new disk: 512M]
    #[arg(long, value_name = "SIZE")]
    cache_size: Option<String>,
    /// Local space for changes not in the bucket yet [new disk: 4G]
    #[arg(long, value_name = "SIZE")]
    unsaved_size: Option<String>,
    /// How often changes go to the bucket: 30s, 5m, 1h... [new disk: 5m]
    #[arg(long, value_name = "TIME")]
    interval: Option<String>,
}

impl LimitArgs {
    pub fn request(&self) -> Result<crate::daemon::api::LimitsRequest> {
        let size = |value: &Option<String>| value.as_deref().map(parse_size).transpose();
        Ok(crate::daemon::api::LimitsRequest {
            cache_bytes: size(&self.cache_size)?,
            unsaved_bytes: size(&self.unsaved_size)?,
            interval_seconds: self.interval.as_deref().map(parse_duration).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_name_alone_names_a_known_bucket() {
        let cli = Cli::try_parse_from(["mica", "disk", "ls", "--bucket", "photos", "--prefix", "laptop"]).unwrap();
        let Command::Disk { store, .. } = cli.command else { panic!("not a disk command") };
        if std::env::var_os("OBJECT_STORAGE_ENDPOINT").is_none() {
            assert!(matches!(store.resolve().unwrap(), (None, Some(name)) if name == "photos/laptop"));
        }
    }

    #[test]
    fn no_bucket_values_give_no_bucket() {
        let cli = Cli::try_parse_from(["mica", "disk", "info", "data"]).unwrap();
        let Command::Disk { store, .. } = cli.command else { panic!("not a disk command") };
        let none = matches!(store.resolve().unwrap(), (None, None));
        assert!(std::env::var_os("OBJECT_STORAGE_BUCKET").is_some() || none);
    }

    #[test]
    fn a_flag_beats_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("store.toml");
        std::fs::write(&file, "endpoint = \"http://file\"\nbucket = \"from-file\"\n").unwrap();
        let cli = Cli::try_parse_from(["mica", "disk", "ls", "--store", file.to_str().unwrap(), "--bucket", "from-flag"]).unwrap();
        let Command::Disk { store, .. } = cli.command else { panic!("not a disk command") };
        let store = store.resolve().unwrap().0.unwrap();
        assert_eq!((store.endpoint.as_str(), store.bucket.as_str(), store.region.as_str()), ("http://file", "from-flag", "auto"));
    }
}
