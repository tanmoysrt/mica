use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalStatus {
    Attached,
    /// Another node took the disk with --force. The local data is kept for an operator.
    Orphaned,
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    status: LocalStatus,
    /// The ublk device number, so a restarted daemon can take the device over.
    #[serde(default)]
    ublk_id: Option<i32>,
}

/// `<data_dir>/disks/<disk-id>/`: the local state and dirty chunks of one disk.
pub struct DiskFolder {
    pub disk_id: String,
    pub path: PathBuf,
}

impl DiskFolder {
    pub fn new(data_dir: &Path, disk_id: &str) -> Self {
        Self { disk_id: disk_id.to_string(), path: data_dir.join("disks").join(disk_id) }
    }

    pub fn list(data_dir: &Path) -> Result<Vec<Self>> {
        let root = data_dir.join("disks");
        let Ok(entries) = std::fs::read_dir(&root) else { return Ok(Vec::new()) };
        let mut folders = Vec::new();
        for entry in entries {
            let disk_id = entry?.file_name().to_string_lossy().to_string();
            folders.push(Self::new(data_dir, &disk_id));
        }
        Ok(folders)
    }

    pub fn status(&self) -> Result<LocalStatus> {
        Ok(self.read_state()?.status)
    }

    pub fn ublk_id(&self) -> Result<Option<i32>> {
        Ok(self.read_state()?.ublk_id)
    }

    pub fn set_status(&self, status: LocalStatus) -> Result<()> {
        let state = StateFile { status, ..self.read_state()? };
        self.write_state(&state)
    }

    pub fn set_ublk_id(&self, ublk_id: Option<i32>) -> Result<()> {
        let state = StateFile { ublk_id, ..self.read_state()? };
        self.write_state(&state)
    }

    pub fn dirty_path(&self) -> PathBuf {
        self.path.join("dirty")
    }

    pub fn has_chunks(&self) -> Result<bool> {
        match std::fs::read_dir(self.dirty_path()) {
            Ok(mut entries) => Ok(entries.next().is_some()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn remove_chunks(&self) -> Result<()> {
        remove_dir(&self.dirty_path())?;
        std::fs::File::open(&self.path)?.sync_all()?;
        Ok(())
    }

    pub fn remove(&self) -> Result<()> {
        remove_dir(&self.path)
    }

    fn read_state(&self) -> Result<StateFile> {
        match std::fs::read(self.state_path()) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StateFile { status: LocalStatus::Attached, ublk_id: None })
            }
            Err(error) => Err(error.into()),
        }
    }

    fn write_state(&self, state: &StateFile) -> Result<()> {
        std::fs::create_dir_all(&self.path).with_context(|| format!("create {}", self.path.display()))?;
        std::fs::write(self.state_path(), serde_json::to_vec(state)?)?;
        Ok(())
    }

    fn state_path(&self) -> PathBuf {
        self.path.join("state")
    }
}

fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}
