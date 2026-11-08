//! The folder of a disk on the node, and the files that say why it exists.
//!
//! ```text
//! <data folder>/disks/<disk>/state         attached or orphaned, and the ublk device number
//! <data folder>/disks/<disk>/store.toml    the bucket credentials of the disk, root only
//! <data folder>/disks/<disk>/data          the data file
//! <data folder>/disks/<disk>/log-a, log-b  the dirty log
//! /run/mica/disks/<disk>/recent            the recent bitmap
//! ```
//!
//! The credentials stay while the node holds the disk, so after a restart
//! or a reboot the daemon serves the disk again by itself. A detach
//! deletes the folder and the credentials with it.

use crate::store::StoreConfig;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The version of the files in a folder. A mica that reads files of
/// another layout would see holes where the data is, so it refuses them.
const LAYOUT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalState {
    /// The node owns the disk.
    Attached,
    /// Another node took the disk. The local data is kept for an operator.
    Orphaned,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    layout: u32,
    state: LocalState,
    ublk: Option<i32>,
    /// The `id` of the head. A disk with the same name from another bucket,
    /// or a new disk with the name of a deleted one, has another id.
    disk_id: String,
}

#[derive(Clone)]
pub struct DiskFolder {
    pub disk: String,
    pub path: PathBuf,
    run: PathBuf,
}

impl DiskFolder {
    pub fn new(data_dir: &Path, run_dir: &Path, disk: &str) -> Self {
        Self { disk: disk.to_string(), path: data_dir.join("disks").join(disk), run: run_dir.join("disks").join(disk) }
    }

    /// The folders of all disks with local data.
    pub fn list(data_dir: &Path, run_dir: &Path) -> Result<Vec<Self>> {
        let Ok(entries) = std::fs::read_dir(data_dir.join("disks")) else { return Ok(Vec::new()) };
        let mut folders = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            folders.push(Self::new(data_dir, run_dir, &name));
        }
        Ok(folders)
    }

    pub fn exists(&self) -> bool {
        self.path.join("state").exists()
    }

    pub fn recent_path(&self) -> PathBuf {
        self.run.join("recent")
    }

    /// Makes the folder and syncs each folder on its path, so that a power
    /// loss cannot lose a folder that holds data.
    pub fn create(&self, disk_id: &str) -> Result<()> {
        std::fs::create_dir_all(&self.path).with_context(|| format!("create {}", self.path.display()))?;
        for path in self.path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            File::open(path)?.sync_all()?;
        }
        if !self.exists() {
            self.write_state(LocalState::Attached, None, disk_id)?;
        }
        Ok(())
    }

    pub fn state(&self) -> Result<LocalState> {
        Ok(self.read_state()?.state)
    }

    pub fn disk_id(&self) -> Result<String> {
        Ok(self.read_state()?.disk_id)
    }

    pub fn ublk(&self) -> Result<Option<i32>> {
        Ok(self.read_state()?.ublk)
    }

    pub fn set_state(&self, state: LocalState) -> Result<()> {
        let old = self.read_state()?;
        self.write_state(state, old.ublk, &old.disk_id)
    }

    pub fn set_ublk(&self, ublk: Option<i32>) -> Result<()> {
        let old = self.read_state()?;
        self.write_state(old.state, ublk, &old.disk_id)
    }

    /// Keeps the credentials for a restart of mica or of the node.
    pub fn save_store(&self, store: &StoreConfig) -> Result<()> {
        let path = self.path.join("store.toml");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        std::io::Write::write_all(&mut options.open(&path)?, toml::to_string(store)?.as_bytes())?;
        Ok(())
    }

    pub fn saved_store(&self) -> Option<StoreConfig> {
        StoreConfig::read(&self.path.join("store.toml")).ok()
    }

    /// Deletes all local data of the disk. The cache stays.
    pub fn remove(&self) -> Result<()> {
        let _ = std::fs::remove_dir_all(&self.run);
        match std::fs::remove_dir_all(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    fn read_state(&self) -> Result<StateFile> {
        let path = self.path.join("state");
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let file: StateFile = serde_json::from_str(&text).with_context(|| format!("read {}: another mica made it", path.display()))?;
        ensure!(file.layout == LAYOUT, "{} has layout {}, and this mica reads only layout {LAYOUT}", path.display(), file.layout);
        Ok(file)
    }

    /// A new file, synced, then renamed into place: a crash leaves the old or the new state.
    fn write_state(&self, state: LocalState, ublk: Option<i32>, disk_id: &str) -> Result<()> {
        let temp = self.path.join("state.new");
        let file = File::create(&temp)?;
        serde_json::to_writer(&file, &StateFile { layout: LAYOUT, state, ublk, disk_id: disk_id.to_string() })?;
        file.sync_all()?;
        std::fs::rename(&temp, self.path.join("state"))?;
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}
