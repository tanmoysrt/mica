//! Which bucket a request means. A request can carry the bucket of its
//! disk. Without one, the daemon looks in the buckets that it knows, so a
//! user gives the credentials of a bucket only once.

use super::Daemon;
use crate::store::{Bucket, DiskStore, StoreConfig};
use anyhow::{Result, bail};
use std::sync::Arc;

impl Daemon {
    /// The disk in the given bucket. Without one: the attached disk, else
    /// the one disk of this name in the buckets that this node knows.
    pub(super) async fn locate(&self, given: Option<&StoreConfig>, name: &str) -> Result<DiskStore> {
        if let Some(store) = given {
            return Ok(DiskStore::new(Arc::new(Bucket::connect(store)?), name));
        }
        if let Some(disk) = self.attached(name).await {
            return Ok(disk.store.clone());
        }
        let (mut found, mut failed) = (Vec::new(), None);
        for store in self.buckets.all() {
            let disk = DiskStore::new(Arc::new(Bucket::connect(&store)?), name);
            match disk.head().await {
                Ok(Some(_)) => found.push(disk),
                Ok(None) => {}
                Err(error) => failed = Some(error.context(format!("read {}", store.place()))),
            }
        }
        match (found.len(), failed) {
            (1, _) => Ok(found.remove(0)),
            (0, Some(error)) => Err(error),
            (0, None) => bail!(
                "there is no disk {name} in the buckets of this machine (see `sudo mica disk ls`). A disk in another bucket needs its bucket once: --store <file> or the OBJECT_STORAGE_* variables"
            ),
            _ => bail!("disk {name} is in more than one bucket: give its bucket with --store or the OBJECT_STORAGE_* variables"),
        }
    }

    /// The bucket for a new disk: the given one, else the only one that this node knows.
    pub(super) fn bucket(&self, given: Option<&StoreConfig>) -> Result<StoreConfig> {
        if let Some(store) = given {
            return Ok(store.clone());
        }
        match self.buckets.all().as_slice() {
            [store] => Ok(store.clone()),
            [] => bail!("this machine knows no bucket yet: {}", self.no_bucket_hint()),
            _ => bail!("this machine knows more than one bucket: give one with --store or the OBJECT_STORAGE_* variables"),
        }
    }

    pub(super) fn no_bucket_hint(&self) -> &'static str {
        "give the bucket once with --store <file> or the OBJECT_STORAGE_* variables (see `mica disk --help`)"
    }
}
