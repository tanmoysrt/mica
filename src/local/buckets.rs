//! The buckets that this node knows, so that a command needs only the name
//! of a disk. The daemon keeps each bucket that a command gave and that
//! worked, one root-only file for each place:
//!
//! ```text
//! <data folder>/buckets/<hash of the place>.toml
//! ```
//!
//! The newest credentials for a place replace the old ones.

use crate::store::StoreConfig;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub struct KnownBuckets {
    folder: PathBuf,
}

impl KnownBuckets {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self { folder: data_dir.join("buckets") }
    }

    pub fn remember(&self, store: &StoreConfig) -> Result<()> {
        if self.all().contains(store) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.folder)?;
        let name = hex::encode(&Sha256::digest(store.place().as_bytes())[..8]);
        store.save(&self.folder.join(format!("{name}.toml")))
    }

    pub fn all(&self) -> Vec<StoreConfig> {
        let Ok(entries) = std::fs::read_dir(&self.folder) else { return Vec::new() };
        let mut stores: Vec<StoreConfig> = entries.flatten().filter_map(|entry| StoreConfig::read(&entry.path()).ok()).collect();
        stores.sort_by_key(StoreConfig::place);
        stores
    }
}
