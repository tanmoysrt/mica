use anyhow::{Context, Result};
use std::path::Path;

/// Who this node is. The ID lives on the local SSD, so a wiped SSD gives a new
/// ID. That is correct: the node no longer has the local data of any disk.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    pub id: String,
    pub hostname: String,
}

impl NodeIdentity {
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("node-id");
        let id = match std::fs::read_to_string(&path) {
            Ok(text) => text.trim().to_string(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim().to_string();
                std::fs::write(&path, &id).with_context(|| format!("write {}", path.display()))?;
                id
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        Ok(Self { id, hostname })
    }
}
