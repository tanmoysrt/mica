//! The buckets that this node knows, so that a command needs only the name
//! of a disk. The daemon keeps each bucket that a command gave and that
//! worked, one root-only file for each place:
//!
//! ```text
//! <data folder>/buckets/<hash of the place>.toml
//! ```
//!
//! These files are the only copy of the keys on the node. The folder of an
//! attached disk names only the place of its bucket. New keys for a place
//! replace the old ones.

use crate::store::StoreConfig;
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub struct KnownBuckets {
    folder: PathBuf,
}

impl KnownBuckets {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self { folder: data_dir.join("buckets") }
    }

    /// Keeps the bucket. True when it replaced other keys for the same place.
    pub fn remember(&self, store: &StoreConfig) -> Result<bool> {
        let old = self.get(&store.place());
        if old.as_ref() == Some(store) {
            return Ok(false);
        }
        std::fs::create_dir_all(&self.folder)?;
        super::write_durable(&self.path(&store.place()), toml::to_string(store)?.as_bytes(), 0o600)?;
        Ok(old.is_some())
    }

    pub fn get(&self, place: &str) -> Option<StoreConfig> {
        StoreConfig::read(&self.path(place)).ok()
    }

    pub fn all(&self) -> Vec<StoreConfig> {
        let Ok(entries) = std::fs::read_dir(&self.folder) else { return Vec::new() };
        let entries = entries.flatten().filter(|entry| entry.path().extension().is_some_and(|extension| extension == "toml"));
        let mut stores: Vec<StoreConfig> = entries.filter_map(|entry| StoreConfig::read(&entry.path()).ok()).collect();
        stores.sort_by_key(StoreConfig::place);
        stores
    }

    /// The one bucket that `name` means: its place, its bucket name, or
    /// its bucket name and prefix, such as `my-disks/laptop`.
    pub fn find(&self, name: &str) -> Result<StoreConfig> {
        let name = name.trim_end_matches('/');
        let mut found: Vec<StoreConfig> = self
            .all()
            .into_iter()
            .filter(|store| {
                let short = format!("{}/{}", store.bucket, store.prefix.trim_matches('/'));
                store.place() == name || store.bucket == name || short.trim_end_matches('/') == name
            })
            .collect();
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => bail!("this machine knows no bucket {name} (see `sudo mica bucket ls`)"),
            _ => bail!("{name} names more than one bucket: give its full place, as `sudo mica bucket ls` shows it"),
        }
    }

    pub fn forget(&self, store: &StoreConfig) -> Result<()> {
        Ok(std::fs::remove_file(self.path(&store.place()))?)
    }

    fn path(&self, place: &str) -> PathBuf {
        let name = hex::encode(&Sha256::digest(place.as_bytes())[..8]);
        self.folder.join(format!("{name}.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(bucket: &str, prefix: &str, key: &str) -> StoreConfig {
        let endpoint = "https://s3.example.com".into();
        StoreConfig { endpoint, bucket: bucket.into(), prefix: prefix.into(), access_key_id: key.into(), ..StoreConfig::default() }
    }

    #[test]
    fn new_keys_replace_the_old_keys_of_a_place() {
        let dir = tempfile::tempdir().unwrap();
        let buckets = KnownBuckets::new(dir.path());
        assert!(!buckets.remember(&store("disks", "a", "key-1")).unwrap());
        assert!(!buckets.remember(&store("disks", "a", "key-1")).unwrap());
        assert!(buckets.remember(&store("disks", "a", "key-2")).unwrap());
        assert!(!buckets.remember(&store("disks", "b", "key-1")).unwrap());
        let all = buckets.all();
        assert_eq!(all.len(), 2);
        assert_eq!(buckets.get(&store("disks", "a", "").place()).unwrap().access_key_id, "key-2");
    }

    #[test]
    fn a_short_name_finds_one_bucket_only() {
        let dir = tempfile::tempdir().unwrap();
        let buckets = KnownBuckets::new(dir.path());
        buckets.remember(&store("disks", "a", "k")).unwrap();
        buckets.remember(&store("disks", "b", "k")).unwrap();
        buckets.remember(&store("photos", "", "k")).unwrap();
        assert_eq!(buckets.find("photos").unwrap().bucket, "photos");
        assert_eq!(buckets.find("disks/b").unwrap().prefix, "b");
        assert_eq!(buckets.find("https://s3.example.com/disks/a").unwrap().prefix, "a");
        assert!(buckets.find("disks").is_err());
        assert!(buckets.find("nothing").is_err());
        buckets.forget(&buckets.find("photos").unwrap()).unwrap();
        assert!(buckets.find("photos").is_err());
    }
}
