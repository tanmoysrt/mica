use anyhow::{Context, Result, bail};
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
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            // A torn ID would make this node a stranger to its own disks.
            Ok(_) => bail!("{} is empty. Restore it, or delete it if this node has no local data", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim().to_string();
                write_durable(&path, &id).with_context(|| format!("write {}", path.display()))?;
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

/// Synced and renamed into place: after a power loss the file is whole, or missing.
fn write_durable(path: &Path, text: &str) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temp)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}
