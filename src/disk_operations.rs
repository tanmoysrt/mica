use crate::blocking::blocking;
use crate::catalog::validate_name;
use crate::chunk_cache::ChunkCache;
use crate::daemon::Daemon;
use crate::disk_limits::DiskLimits;
use crate::ownership::{self, Owner};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub(crate) enum DiskOperation {
    Resize(u64),
    Limits(DiskLimits),
    Delete,
}

impl Daemon {
    pub(crate) async fn disk_cache(&self, id: &str, limit: Option<u64>) -> Result<Arc<ChunkCache>> {
        let (manager, id) = (self.cache.clone(), id.to_string());
        let limit = limit.unwrap_or(self.config.disk_cache_limit_mib << 20);
        blocking(move || manager.for_disk(&id, limit)).await
    }

    pub(crate) async fn reconcile_storage(self: Arc<Self>) {
        loop {
            let manager = self.cache.clone();
            if let Err(error) = blocking(move || manager.reconcile()).await {
                log::warn!("storage reconciliation failed: {error:#}");
                self.cache.pressure.store(true, std::sync::atomic::Ordering::Release);
            }
            let disks: Vec<_> = self.disks.lock().await.values().map(|attached| attached.disk.clone()).collect();
            for disk in disks {
                if let Ok(_checkpoint) = disk.checkpoint_running.try_lock()
                    && let Err(error) = disk.free_saved_chunks().await
                {
                    log::warn!("disk {}: reclaiming saved chunks failed: {error:#}", disk.id);
                }
                if self.cache.pressure.load(std::sync::atomic::Ordering::Acquire) || disk.under_pressure() {
                    disk.checkpoint_wanted.notify_one();
                }
                disk.space_freed.notify_waiters();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub(crate) async fn operate_disk(&self, id: &str, operation: DiskOperation) -> Result<Value> {
        validate_name(id)?;
        let lock = self.disk_lock(id);
        let _operation = lock.lock().await;
        ensure!(!self.recovering.lock().await.contains(id), "disk {id} is recovering");
        let attached = {
            let disks = self.disks.lock().await;
            match disks.get(id) {
                Some(attached) => {
                    let Some(device) = &attached.device else { bail!("disk {id} is detaching") };
                    let resize = match operation {
                        DiskOperation::Resize(_) => Some(device.resize_control()?),
                        DiskOperation::Delete => bail!("disk {id} is attached; detach it first"),
                        _ => None,
                    };
                    Some((attached.disk.clone(), resize))
                }
                None => None,
            }
        };
        if let Some((disk, control)) = attached {
            return match operation {
                DiskOperation::Resize(size) => {
                    let size = disk.grow_to(size).await?;
                    blocking(move || {
                        crate::ublk_device::ensure_control_ring()?;
                        let result = control.unwrap().update_size(size)?;
                        ensure!(
                            result >= 0,
                            "ublk resize failed: errno {}. Retry resize to finish applying the saved size",
                            -result
                        );
                        Ok(())
                    })
                    .await?;
                    Ok(json!({ "disk": id, "size": size }))
                }
                DiskOperation::Limits(limits) => {
                    let limits = disk.set_limits(limits, self.config.dirty_limit_bytes()).await?;
                    self.disk_cache(id, limits.cache_bytes).await?;
                    Ok(json!({ "disk": id, "limits": limits }))
                }
                DiskOperation::Delete => unreachable!(),
            };
        }
        if !matches!(ownership::owner_of(&self.bucket, id, &self.node).await?, Owner::Nobody) {
            bail!("disk {id} is attached on another session; operate on its owning node");
        }
        self.catalog().load(id).await?;
        let folder = crate::local_state::DiskFolder::new(&self.config.data_dir, id);
        ensure!(!folder.has_chunks()?, "disk {id} has local data awaiting recovery");
        folder.set_status(crate::local_state::LocalStatus::Attached)?;
        ownership::claim_then(&self.bucket, id, &self.node, false, async { Ok(()) }).await?;
        let result = self.operate_detached(id, operation).await;
        if let Err(error) = ownership::release(&self.bucket, id, &self.node).await {
            log::warn!("disk {id}: could not release operation marker: {error:#}");
            return match result {
                Ok(_) => Err(error),
                Err(original) => Err(original),
            };
        }
        folder.remove()?;
        result
    }

    async fn operate_detached(&self, id: &str, operation: DiskOperation) -> Result<Value> {
        match operation {
            DiskOperation::Resize(size) => {
                self.catalog().resize(id, size).await?;
                Ok(json!({ "disk": id, "size": self.catalog().load(id).await?.manifest.disk_size }))
            }
            DiskOperation::Limits(update) => {
                let loaded = self.catalog().load(id).await?;
                let mut head = loaded.head;
                head.limits.cache_bytes = update.cache_bytes.or(head.limits.cache_bytes);
                head.limits.dirty_bytes = update.dirty_bytes.or(head.limits.dirty_bytes);
                head.limits.validate(loaded.manifest.chunk_size)?;
                head.save(&self.bucket, id).await?;
                self.disk_cache(id, head.limits.cache_bytes).await?;
                Ok(json!({ "disk": id, "limits": head.limits }))
            }
            DiskOperation::Delete => {
                self.catalog().delete_disk(id).await?;
                let (manager, disk) = (self.cache.clone(), id.to_string());
                blocking(move || manager.remove_disk(&disk)).await?;
                Ok(json!({ "disk": id }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::Bucket;
    use crate::manifest::DEFAULT_CHUNK_SIZE;
    use crate::records::AttachedMarker;

    #[tokio::test]
    async fn concurrent_resizes_cannot_overwrite_a_larger_size() {
        let root = tempfile::tempdir().unwrap();
        let daemon = Daemon::test_daemon(root.path(), Arc::new(Bucket::in_memory()), "one");
        daemon.catalog().create("disk", 1 << 20, DEFAULT_CHUNK_SIZE, DiskLimits::default()).await.unwrap();
        let (small, large) = tokio::join!(
            daemon.operate_disk("disk", DiskOperation::Resize(4 << 20)),
            daemon.operate_disk("disk", DiskOperation::Resize(8 << 20)),
        );
        large.unwrap();
        let _ = small;
        assert_eq!(daemon.catalog().load("disk").await.unwrap().manifest.disk_size, 8 << 20);
        assert!(AttachedMarker::load(&daemon.bucket, "disk").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn detached_mutations_refuse_another_nodes_claim() {
        let root = tempfile::tempdir().unwrap();
        let daemon = Daemon::test_daemon(root.path(), Arc::new(Bucket::in_memory()), "one");
        daemon.catalog().create("disk", 1 << 20, DEFAULT_CHUNK_SIZE, DiskLimits::default()).await.unwrap();
        let other = crate::node::NodeIdentity { id: "two".into(), hostname: "two".into() };
        AttachedMarker::for_node(&other).create(&daemon.bucket, "disk").await.unwrap();
        assert!(daemon.operate_disk("disk", DiskOperation::Resize(8 << 20)).await.is_err());
        assert!(daemon.operate_disk("disk", DiskOperation::Delete).await.is_err());
        assert_eq!(daemon.catalog().load("disk").await.unwrap().manifest.disk_size, 1 << 20);
        assert_eq!(AttachedMarker::load(&daemon.bucket, "disk").await.unwrap().unwrap().node_id, "two");
    }
}
