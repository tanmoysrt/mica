use crate::blocking::blocking;
use crate::disk::{Committed, Disk};
use crate::disk_limits::DiskLimits;
use crate::records::Head;
use anyhow::{Result, ensure};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::OnceCell;

impl Disk {
    pub async fn grow_to(&self, size: u64) -> Result<u64> {
        let _checkpoint = self.checkpoint_running.lock().await;
        self.ensure_usable()?;
        let (mut manifest, parent, profile) = {
            let committed = self.committed.lock().unwrap();
            (committed.manifest.clone(), committed.manifest_hash, committed.profile.clone())
        };
        ensure!(size >= self.size(), "a disk cannot be made smaller");
        manifest.grow_to(size)?;
        let size = manifest.disk_size;
        if size == self.size() {
            return Ok(size);
        }
        let data = self.data.clone();
        blocking(move || data.grow_to(size)).await?;
        let manifest = manifest.next(&self.id, parent);
        let hash = manifest.save(&self.bucket).await?;
        self.confirm_ownership().await?;
        let limits = *self.limits.lock().unwrap();
        Head::new(&hash, profile.clone(), manifest.seq, &self.node.hostname)
            .with_limits(limits)
            .save(&self.bucket, &self.id)
            .await?;
        let page_count = manifest.pages.len();
        *self.committed.lock().unwrap() = Committed { manifest, manifest_hash: hash, profile };
        self.pages.write().unwrap().resize_with(page_count, || Arc::new(OnceCell::new()));
        self.size.store(size, Ordering::Release);
        Ok(size)
    }

    pub async fn set_limits(&self, update: DiskLimits, default_dirty: u64) -> Result<DiskLimits> {
        let _checkpoint = self.checkpoint_running.lock().await;
        self.ensure_usable()?;
        let mut limits = *self.limits.lock().unwrap();
        limits.cache_bytes = update.cache_bytes.or(limits.cache_bytes);
        limits.dirty_bytes = update.dirty_bytes.or(limits.dirty_bytes);
        limits.validate(self.chunk_size)?;
        let (hash, seq, profile) = {
            let committed = self.committed.lock().unwrap();
            (committed.manifest_hash, committed.manifest.seq, committed.profile.clone())
        };
        self.confirm_ownership().await?;
        Head::new(&hash, profile, seq, &self.node.hostname).with_limits(limits).save(&self.bucket, &self.id).await?;
        *self.limits.lock().unwrap() = limits;
        if let Some(bytes) = limits.cache_bytes {
            self.cache.set_limit(bytes);
        }
        self.local_chunk_limit
            .store((limits.dirty_bytes.unwrap_or(default_dirty) / self.chunk_size).max(2) as usize, Ordering::Relaxed);
        self.checkpoint_wanted.notify_one();
        self.space_freed.notify_waiters();
        Ok(limits)
    }
}
