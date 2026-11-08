//! Saved chunks: local chunks whose content is in the bucket. They stay in
//! the data file up to the cache budget, because the guest often writes
//! them again. Then the oldest ones move into the cache.

use super::Disk;
use super::slots::ChunkSlot;
use anyhow::Result;
use tokio::task::spawn_blocking;

impl Disk {
    /// Saved chunks over the cache budget leave the data file, oldest first,
    /// and go into the cache. Until then they stay: a write into a local
    /// chunk needs no copy, and on ext4 or XFS it overwrites blocks that
    /// exist, so a flush syncs less. The dirty log drops each chunk before
    /// its space is freed: a crash must not bring back a chunk that is a hole.
    pub(super) async fn free_saved(&self) -> Result<()> {
        let keep = (self.cache.usage().limit / self.chunk_size) as usize;
        let mut freed = std::mem::take(&mut *self.unpunched.lock().unwrap());
        loop {
            let next = {
                let mut saved = self.saved.lock().unwrap();
                if saved.len() <= keep {
                    break;
                }
                saved.pop_front().unwrap()
            };
            let mut slot = self.slot(next).await?.write_owned().await;
            slot.queued = false;
            if !slot.local || slot.unsaved {
                continue;
            }
            if !slot.base.is_zero() {
                let (data, cache, hash) = (self.data.clone(), self.cache.clone(), slot.base.hash);
                let stored = spawn_blocking(move || cache.insert(&hash, |file| data.copy_out(next, file))).await?;
                if let Err(error) = stored {
                    log::warn!("disk {}: chunk {next} not cached: {error:#}", self.name);
                }
            }
            slot.local = false;
            self.local.lock().unwrap().remove(&next);
            freed.push(next);
        }
        if freed.is_empty() {
            return Ok(());
        }
        let (log, local) = (self.log.clone(), self.local.clone());
        if let Err(error) = spawn_blocking(move || log.rewrite(|index| local.lock().unwrap().contains(&index))).await? {
            self.stop(&format!("a rewrite of the dirty log failed: {error:#}"));
            return Err(error);
        }
        for index in freed {
            let slot = self.slot(index).await?.write_owned().await;
            if slot.local {
                continue;
            }
            // A flush that started before the rewrite still lists it.
            if !self.log.is_unlisted(index) {
                self.unpunched.lock().unwrap().push(index);
                continue;
            }
            let data = self.data.clone();
            spawn_blocking(move || data.zero(index)).await??;
            drop(slot);
        }
        Ok(())
    }

    /// Before a detach deletes the data files: the saved local chunks go
    /// into the cache, so a new attach on this node reads them locally.
    pub async fn cache_saved(&self) {
        let saved: Vec<usize> = self.saved.lock().unwrap().iter().copied().collect();
        for index in saved {
            let Ok(slot) = self.slot(index).await else { continue };
            let slot = slot.read_owned().await;
            if !slot.local || slot.unsaved || slot.base.is_zero() {
                continue;
            }
            let (data, cache, hash) = (self.data.clone(), self.cache.clone(), slot.base.hash);
            if let Ok(Err(error)) = spawn_blocking(move || cache.insert(&hash, |file| data.copy_out(index, file))).await {
                log::warn!("disk {}: chunk {index} not cached: {error:#}", self.name);
            }
        }
    }

    pub(super) fn queue_saved(&self, slot: &mut ChunkSlot, index: usize) {
        if !slot.queued {
            slot.queued = true;
            self.saved.lock().unwrap().push_back(index);
        }
    }
}
