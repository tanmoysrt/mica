use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskLimits {
    pub cache_bytes: Option<u64>,
    pub dirty_bytes: Option<u64>,
}

impl DiskLimits {
    pub fn validate(&self, chunk_size: u64) -> Result<()> {
        ensure!(
            self.cache_bytes.is_none_or(|bytes| bytes >= chunk_size),
            "cache must hold at least one chunk ({chunk_size} bytes)"
        );
        ensure!(
            self.dirty_bytes.is_none_or(|bytes| bytes >= 2 * chunk_size),
            "pending-write budget must hold at least two chunks ({} bytes)",
            2 * chunk_size
        );
        Ok(())
    }
}
