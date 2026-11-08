//! Which bucket a request means, and the buckets that the node keeps.
//!
//! A request can carry the keys of a bucket, or name a bucket that the
//! node knows. Without either, the daemon looks in all the buckets that it
//! knows, so a user gives the keys of a bucket only once.

use super::Daemon;
use crate::store::{Bucket, DiskStore, StoreConfig, disk_store};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::sync::Arc;

const GIVE_BUCKET: &str = "give it with --bucket <name> (see `sudo mica bucket ls`), or a new bucket with --store <file>";

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
        let mut candidates = Vec::new();
        for store in self.buckets.all() {
            candidates.push(DiskStore::new(Arc::new(Bucket::connect(&store)?), name));
        }
        find_disk(candidates, name).await
    }

    /// The bucket for a new disk: the given one, else the only one that this node knows.
    pub(super) fn bucket(&self, given: Option<&StoreConfig>) -> Result<StoreConfig> {
        if let Some(store) = given {
            return Ok(store.clone());
        }
        match self.buckets.all().as_slice() {
            [store] => Ok(store.clone()),
            [] => bail!("this machine knows no bucket yet: {}", no_bucket_hint()),
            _ => bail!("this machine knows more than one bucket: {GIVE_BUCKET}"),
        }
    }

    /// Keeps a bucket that worked. Attached disks of the same place take
    /// its new keys at once, so a key rotation needs no new attach.
    pub(super) async fn remember(&self, store: &StoreConfig) -> Result<()> {
        if self.buckets.remember(store)? {
            for attached in self.disks.lock().await.values() {
                if attached.disk.store.bucket.place == store.place() {
                    attached.disk.store.bucket.rekey(store)?;
                    log::info!("disk {}: uses the new keys of its bucket", attached.disk.name);
                }
            }
        }
        Ok(())
    }

    /// Checks the bucket: one listing, and the probe of conditional writes.
    pub async fn add_bucket(&self, store: Option<&StoreConfig>) -> Result<Value> {
        let Some(store) = store else { bail!("give the bucket with --store <file> or the OBJECT_STORAGE_* variables") };
        let bucket = Bucket::connect(store)?;
        let disks = disk_store::disk_names(&bucket).await?.len();
        let conditions = bucket.check_conditional_writes().await?;
        Ok(json!({ "bucket": store.place(), "disks": disks, "warning": (!conditions).then_some(super::operations::NO_CONDITIONS) }))
    }

    pub async fn list_buckets(&self) -> Value {
        let attached: Vec<String> = self.disks.lock().await.values().map(|attached| attached.disk.store.bucket.place.clone()).collect();
        let buckets: Vec<Value> = self
            .buckets
            .all()
            .iter()
            .map(|store| {
                let here = attached.iter().filter(|place| **place == store.place()).count();
                json!({
                    "bucket": store.place(),
                    "endpoint": store.endpoint,
                    "name": store.bucket,
                    "prefix": store.prefix,
                    "access_key_id": store.access_key_id,
                    "attached_here": here,
                })
            })
            .collect();
        json!({ "buckets": buckets })
    }

    /// Deletes the keys of a bucket. Refused while a disk of it is attached here.
    pub async fn forget_bucket(&self, name: &str) -> Result<Value> {
        let store = self.buckets.find(name)?;
        let place = store.place();
        if let Some(attached) = self.disks.lock().await.values().find(|attached| attached.disk.store.bucket.place == place) {
            bail!("disk {} of this bucket is attached here: detach it first", attached.disk.name);
        }
        self.buckets.forget(&store)?;
        Ok(json!({ "forgotten": place }))
    }
}

/// The one candidate that holds the disk. A bucket that cannot answer is
/// an error only when no other bucket holds the disk.
async fn find_disk(candidates: Vec<DiskStore>, name: &str) -> Result<DiskStore> {
    let (mut found, mut failed) = (Vec::new(), None);
    for disk in candidates {
        match disk.head().await {
            Ok(Some(_)) => found.push(disk),
            Ok(None) => {}
            Err(error) => failed = Some(error.context(format!("read {}", disk.bucket.place))),
        }
    }
    match (found.len(), failed) {
        (1, _) => Ok(found.remove(0)),
        (0, Some(error)) => Err(error),
        (0, None) => bail!(
            "there is no disk {name} in the buckets of this machine (see `sudo mica disk ls`). A disk in another bucket needs its bucket once: --store <file> or the OBJECT_STORAGE_* variables"
        ),
        _ => bail!("disk {name} is in more than one bucket: {GIVE_BUCKET}"),
    }
}

pub(super) fn no_bucket_hint() -> &'static str {
    "give the bucket once with --store <file> or the OBJECT_STORAGE_* variables (see `mica disk --help`)"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Head, Limits};

    async fn disk_in(bucket: &Arc<Bucket>, name: &str) -> DiskStore {
        let disk = DiskStore::new(bucket.clone(), name);
        disk.create_head(&Head::new(1 << 30, 1 << 20, Limits::DEFAULT, None)).await.unwrap();
        disk
    }

    #[tokio::test]
    async fn a_disk_name_finds_the_one_bucket_that_holds_it() {
        let (first, second) = (Arc::new(Bucket::in_memory()), Arc::new(Bucket::in_memory()));
        disk_in(&first, "web").await;
        disk_in(&second, "db").await;
        disk_in(&first, "both").await;
        disk_in(&second, "both").await;
        let candidates = |name: &str| vec![DiskStore::new(first.clone(), name), DiskStore::new(second.clone(), name)];
        let found = find_disk(candidates("db"), "db").await.unwrap();
        assert!(Arc::ptr_eq(&found.bucket, &second));
        assert!(find_disk(candidates("nothing"), "nothing").await.err().unwrap().to_string().contains("no disk nothing"));
        assert!(find_disk(candidates("both"), "both").await.err().unwrap().to_string().contains("more than one bucket"));
    }
}
