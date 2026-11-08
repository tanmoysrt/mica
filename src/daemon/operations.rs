//! The operations on disks in the store. Each takes the bucket from the
//! request, else finds it with `Daemon::locate`.
//!
//! Only the owner changes the head of a disk. So each operation runs with
//! the claim of the disk: the claim of the attached disk when this node
//! owns it, else a claim that this node takes for the operation and gives
//! back after. A disk that is attached on another node is refused, with
//! the name of that node, so that a controller can send the request there.
//!
//! A restore makes a new disk from a snapshot. It copies the objects of the
//! snapshot inside the store, with the same transaction folders. mica has
//! no rollback: a copy has new keys, so no object can come back into a
//! version that already dropped it.

use super::Daemon;
use crate::disk::TransactionKind;
use crate::owner::{Claim, unix_now};
use crate::store::format::{CHUNKS_PER_PAGE, DEFAULT_CHUNK_SIZE, validate_chunk_size};
use crate::store::{Bucket, DiskStore, Head, Limits, Snapshot, StoreConfig, disk_store, validate_name};
use anyhow::{Result, bail, ensure};
use futures::{StreamExt, TryStreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const PARALLEL: usize = 64;

pub(super) const NO_CONDITIONS: &str = "this bucket ignores conditional writes, so mica cannot stop two machines from using the disk at once. Attach it on one machine only";

impl Daemon {
    pub async fn create(&self, store: Option<&StoreConfig>, name: &str, size: u64, chunk_size: Option<u64>, limits: LimitsRequest) -> Result<Value> {
        validate_name(name)?;
        let chunk_size = chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);
        validate_chunk_size(chunk_size)?;
        ensure!(size >= chunk_size, "the size must be at least one chunk");
        let size = size.next_multiple_of(chunk_size);
        let limits = limits.apply(Limits::DEFAULT, chunk_size)?;
        let disk = DiskStore::new(Arc::new(Bucket::connect(&self.bucket(store)?)?), name);
        let conditions = disk.bucket.check_conditional_writes().await?;
        disk.create_head(&Head::new(size, chunk_size, limits, None)).await?;
        let warning = (!conditions).then_some(NO_CONDITIONS);
        Ok(json!({ "disk": name, "size": size, "chunk_size": chunk_size, "warning": warning }))
    }

    /// Lists the disks of the given bucket, else of each bucket that this
    /// node knows. A disk attached here shows its local state too.
    pub async fn list(&self, store: Option<&StoreConfig>) -> Result<Value> {
        let stores = match store {
            Some(store) => vec![store.clone()],
            None => self.buckets.all(),
        };
        ensure!(!stores.is_empty(), "this machine knows no bucket yet: {}", super::buckets::no_bucket_hint());
        let mut disks = Vec::new();
        for store in &stores {
            for mut disk in self.list_bucket(store).await? {
                disk["bucket"] = store.place().into();
                disk["local"] = self.local_state(&disk).await;
                disk["cache_bytes"] = self.cache.bytes(disk["disk"].as_str().unwrap_or_default()).into();
                disks.push(disk);
            }
        }
        Ok(json!({ "buckets": stores.iter().map(StoreConfig::place).collect::<Vec<_>>(), "disks": disks }))
    }

    /// One listing, then one read of each head.
    async fn list_bucket(&self, store: &StoreConfig) -> Result<Vec<Value>> {
        let bucket = Arc::new(Bucket::connect(store)?);
        let names = disk_store::disk_names(&bucket).await?;
        futures::stream::iter(names)
            .map(|name| {
                let store = DiskStore::new(bucket.clone(), &name);
                async move { Ok::<_, anyhow::Error>(store.head().await?.map(|head| describe(&name, &head.head))) }
            })
            .buffered(16)
            .try_filter_map(|value| async move { Ok(value) })
            .try_collect()
            .await
    }

    pub async fn info(&self, store: Option<&StoreConfig>, name: &str) -> Result<Value> {
        let disk = self.locate(store, name).await?;
        let mut info = describe(name, &disk.require_head().await?.head);
        info["bucket"] = disk.bucket.place.clone().into();
        info["local"] = self.local_state(&info).await;
        Ok(info)
    }

    /// The state on this node of a disk from the bucket, when it is attached here.
    async fn local_state(&self, disk: &Value) -> Value {
        let status = self.status().await;
        let here = status["disks"].as_array().into_iter().flatten().find(|attached| attached["disk"] == disk["disk"] && attached["bucket"] == disk["bucket"]);
        here.cloned().unwrap_or_default()
    }

    /// Takes a snapshot. On an attached disk, it is a transaction of all
    /// unsaved chunks whose head write also adds the name.
    pub async fn create_snapshot(&self, store: Option<&StoreConfig>, name: &str, snapshot: &str) -> Result<Value> {
        validate_name(snapshot)?;
        if let Some(disk) = self.attached(name).await {
            let transaction = disk.transaction(TransactionKind::All, Some(snapshot)).await?;
            return Ok(json!({ "disk": name, "snapshot": snapshot, "transaction": transaction }));
        }
        let transaction = self
            .with_claim(store, name, async |claim| {
                ensure!(!claim.head().snapshots.contains_key(snapshot), "disk {name} has a snapshot {snapshot} already");
                let transaction = claim.head().transaction;
                claim
                    .update(|head| {
                        head.snapshots.insert(snapshot.into(), Snapshot { transaction, time: unix_now(), deleting: false });
                    })
                    .await?;
                Ok(transaction)
            })
            .await?;
        Ok(json!({ "disk": name, "snapshot": snapshot, "transaction": transaction }))
    }

    pub async fn delete_snapshot(&self, store: Option<&StoreConfig>, name: &str, snapshot: &str) -> Result<Value> {
        self.with_claim(store, name, async |claim| claim.delete_snapshot(snapshot).await).await?;
        Ok(json!({ "disk": name, "deleted": snapshot }))
    }

    /// Changes the limits, and makes the disk larger. A new size needs a
    /// detached disk: grow its filesystem after the next attach.
    pub async fn set(&self, store: Option<&StoreConfig>, name: &str, size: Option<u64>, request: LimitsRequest) -> Result<Value> {
        if size.is_some() && self.attached(name).await.is_some() {
            bail!("a new size needs a detached disk: detach {name}, set the size, attach it again and grow its filesystem");
        }
        let head = self
            .with_claim(store, name, async |claim| {
                let old = claim.head().clone();
                let limits = request.apply(old.limits, old.chunk_size)?;
                let size = size.map_or(old.size, |size| size.next_multiple_of(old.chunk_size));
                ensure!(size >= old.size, "a disk cannot become smaller");
                claim
                    .update(|head| {
                        head.limits = limits;
                        head.size = size;
                    })
                    .await?;
                Ok(claim.head().clone())
            })
            .await?;
        if let Some(disk) = self.attached(name).await {
            disk.set_limits(head.limits);
        }
        Ok(describe(name, &head))
    }

    pub async fn release(&self, store: Option<&StoreConfig>, name: &str) -> Result<Value> {
        let disk = self.locate(store, name).await?;
        let node = Claim::release_from(&disk, &self.node).await?;
        Ok(json!({ "disk": name, "released_from": node }))
    }

    /// Makes the new disk `target` from a snapshot of `name`.
    pub async fn restore(&self, store: Option<&StoreConfig>, name: &str, snapshot: &str, target: &str) -> Result<Value> {
        validate_name(target)?;
        let copied = self.with_claim(store, name, async |claim| self.copy_snapshot(claim, snapshot, target).await).await?;
        Ok(json!({ "disk": target, "from": name, "snapshot": snapshot, "copied_objects": copied }))
    }

    async fn copy_snapshot(&self, source: &mut Claim, snapshot: &str, target: &str) -> Result<usize> {
        let head = source.head().clone();
        let Some(chosen) = head.snapshots.get(snapshot).filter(|chosen| !chosen.deleting) else {
            bail!("disk {} has no snapshot {snapshot}", source.store.disk)
        };
        let root = source.store.root(&head, chosen.transaction).await?;
        let new = DiskStore::new(source.store.bucket.clone(), target);
        let mut claim = Claim::create(new.clone(), &self.node, Head::new(root.disk_size, root.chunk_size, head.limits, None)).await?;
        // The total is known after the pages of the snapshot are read.
        let running = self.tasks.start(target, "copy");
        let mut keys = Vec::new();
        if chosen.transaction != 0 {
            keys.push(source.store.root_key(chosen.transaction));
            for (index, entry) in root.pages.iter().enumerate().filter(|(_, entry)| !entry.is_zero()) {
                keys.push(source.store.page_key(entry.birth, index));
                let page = source.store.page(entry, index).await?;
                for (offset, chunk) in page.0.iter().enumerate().filter(|(_, chunk)| !chunk.is_zero()) {
                    keys.push(source.store.chunk_key(chunk.birth, index * CHUNKS_PER_PAGE + offset));
                }
            }
        }
        running.task.total.store(keys.len() as u64, Ordering::Relaxed);
        let (from, to) = (source.store.prefix(), new.prefix());
        let copies: Vec<(String, String)> = keys.iter().map(|key| (key.clone(), key.replacen(&from, &to, 1))).collect();
        futures::stream::iter(copies)
            .map(|(key, target)| {
                let (bucket, task) = (new.bucket.clone(), running.task.clone());
                async move {
                    bucket.copy(&key, &target).await?;
                    task.done.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, anyhow::Error>(())
                }
            })
            .buffer_unordered(PARALLEL)
            .try_collect::<Vec<()>>()
            .await?;
        claim
            .update(|head| {
                // The time of the data, as at its last save.
                (head.transaction, head.next, head.time) = (chosen.transaction, chosen.transaction + 1, root.time);
                head.profile = source.head().profile.clone();
            })
            .await?;
        claim.release().await?;
        Ok(keys.len())
    }

    /// Deletes a disk and all its snapshots. A crash leaves the head marked
    /// as deleted, and the same command finishes the delete.
    pub async fn delete(&self, store: Option<&StoreConfig>, name: &str) -> Result<Value> {
        if self.attached(name).await.is_some() {
            bail!("disk {name} is attached on this machine: detach it first with `sudo mica disk detach {name}`");
        }
        let disk = self.locate(store, name).await?;
        let current = disk.require_head().await?;
        if let Some(owner) = current.head.owner.as_ref().filter(|owner| owner.id != self.node.id) {
            bail!("disk {name} is attached on {}: detach it there first", owner.node);
        }
        if !current.head.deleted {
            let mut head = current.head.clone();
            head.deleted = true;
            disk.replace_head(&current, head).await?;
        }
        let head_key = format!("{}head", disk.prefix());
        let keys = disk.bucket.list(&disk.prefix()).await?.into_iter().filter(|key| *key != head_key).collect();
        disk.bucket.delete_all(keys).await?;
        disk.delete_head().await?;
        self.cache.remove_disk(name);
        let _ = self.folder(name).remove();
        Ok(json!({ "deleted": name }))
    }

    /// Runs `operation` with the claim of the disk.
    async fn with_claim<T>(&self, store: Option<&StoreConfig>, name: &str, operation: impl AsyncFnOnce(&mut Claim) -> Result<T>) -> Result<T> {
        if let Some(disk) = self.attached(name).await {
            return operation(&mut *disk.claim().await).await;
        }
        let disk = self.locate(store, name).await?;
        let mut claim = Claim::take(disk, &self.node).await?;
        let result = operation(&mut claim).await;
        let released = claim.release().await;
        let value = result?;
        released?;
        Ok(value)
    }

}

/// New budgets for a disk. A missing field keeps the old value.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct LimitsRequest {
    pub cache_bytes: Option<u64>,
    pub unsaved_bytes: Option<u64>,
    pub interval_seconds: Option<u64>,
}

impl LimitsRequest {
    fn apply(self, old: Limits, chunk_size: u64) -> Result<Limits> {
        let limits = Limits {
            cache_bytes: self.cache_bytes.unwrap_or(old.cache_bytes),
            unsaved_bytes: self.unsaved_bytes.unwrap_or(old.unsaved_bytes),
            interval_seconds: self.interval_seconds.unwrap_or(old.interval_seconds),
        };
        ensure!(limits.cache_bytes >= chunk_size, "the cache must hold at least one chunk");
        ensure!(limits.unsaved_bytes >= 2 * chunk_size, "the unsaved data must allow at least two chunks");
        ensure!((1..=86_400).contains(&limits.interval_seconds), "the interval must be from 1 second to 24 hours");
        Ok(limits)
    }
}

fn describe(name: &str, head: &Head) -> Value {
    let snapshots: Vec<Value> = head
        .snapshots
        .iter()
        .map(|(snapshot, info)| json!({ "name": snapshot, "transaction": info.transaction, "time": info.time, "deleting": info.deleting }))
        .collect();
    json!({
        "disk": name,
        "size": head.size,
        "chunk_size": head.chunk_size,
        "transaction": head.transaction,
        "time": head.time,
        "owner": head.owner.as_ref().map(|owner| owner.node.clone()),
        "limits": head.limits,
        "snapshots": snapshots,
        "deleted": head.deleted,
    })
}
