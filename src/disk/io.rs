//! Read, write, discard and flush on the tokio side.
//!
//! The queue threads send here each request that they cannot serve alone:
//! a chunk that is not local yet, a busy lock, a full disk, a flush.
//!
//! Write: lock the chunk. If it is not local, put its current content into
//! the data file first, unless the write covers the full chunk. Write the
//! data. Mark the chunk unsaved. Do not sync.
//!
//! Flush: sync the data files that changed, then append the new local
//! chunks to the dirty log and sync it. A flush never uses the object store.

use super::slots::ChunkSlot;
use super::{Disk, IoStats, Part};
use crate::store::{Entry, Hash};
use crate::transfers::Priority;
use anyhow::{Result, anyhow};
use futures::{StreamExt, TryStreamExt};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::task::spawn_blocking;

/// After this many bytes of writes, the write-back of the data files starts.
/// Linux would wait for gigabytes, and the next flush would write them all.
const WRITEBACK_AFTER: u64 = 16 << 20;
const SLOW_FLUSH: Duration = Duration::from_millis(200);

/// The content of a clean chunk: a cache file, or a download in memory.
pub(super) enum Clean {
    File(File),
    Data(Arc<Vec<u8>>),
}

impl Clean {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<()> {
        match self {
            Clean::File(file) => Ok(file.read_exact_at(buffer, offset)?),
            Clean::Data(data) => {
                buffer.copy_from_slice(&data[offset as usize..offset as usize + buffer.len()]);
                Ok(())
            }
        }
    }
}

impl Disk {
    pub async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        self.note_read(offset, len);
        let mut local = Vec::new();
        let mut clean = Vec::new();
        for part in self.split(offset, len)? {
            self.profile.record(part.index);
            let slot = self.slot(part.index).await?.read_owned().await;
            match (slot.local, slot.base.is_zero()) {
                (true, _) => local.push((part, slot)),
                (false, false) => clean.push((part, slot.base)),
                (false, true) => {}
            }
        }
        // Local parts are read under their read locks.
        let data = self.data.clone();
        let mut buffer = spawn_blocking(move || {
            let mut buffer = vec![0u8; len];
            for (part, _lock) in local {
                let (file, start) = data.location(part.index);
                file.read_exact_at(&mut buffer[part.buffer.clone()], start + part.offset)?;
            }
            Ok::<_, anyhow::Error>(buffer)
        })
        .await??;
        // A write can make a clean chunk local meanwhile. The read then
        // returns the content from before the write, which is correct for
        // a read that overlaps a write.
        let sources: Vec<(Part, Clean)> = futures::stream::iter(clean)
            .map(|(part, base)| async move { Ok::<_, anyhow::Error>((part.clone(), self.clean(part.index, base, Priority::Read).await?)) })
            .buffered(16)
            .try_collect()
            .await?;
        if !sources.is_empty() {
            buffer = spawn_blocking(move || {
                for (part, source) in sources {
                    source.read_at(&mut buffer[part.buffer.clone()], part.offset)?;
                }
                Ok::<_, anyhow::Error>(buffer)
            })
            .await??;
        }
        Ok(buffer)
    }

    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        self.ensure_usable()?;
        self.wait_for_room().await?;
        let data = Arc::new(data);
        for part in self.split(offset, data.len())? {
            let slot = self.make_local(part.index, part.buffer.len() as u64 == self.chunk_size).await?;
            self.write_into(slot, part, data.clone()).await?;
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// A write with forced unit access: durable when it returns. The flush
    /// after it can share one sync with the flushes of other writers.
    pub async fn write_durable(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        self.write(offset, data).await?;
        self.flush().await
    }

    /// A discard of a full chunk makes it zeros. The next transaction
    /// stores a zero entry, so nothing is uploaded. A discard of a part of a
    /// chunk is only a hint, and mica ignores it: honoring it would download
    /// and upload the chunk again.
    pub async fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        for part in self.split(offset, len as usize)? {
            if part.buffer.len() as u64 == self.chunk_size {
                self.wait_for_room().await?;
                self.zero_chunk(part.index).await?;
            }
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// Unlike a discard, a write of zeroes must zero every byte.
    pub async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        self.wait_for_room().await?;
        for part in self.split(offset, len as usize)? {
            match part.buffer.len() as u64 == self.chunk_size {
                true => self.zero_chunk(part.index).await?,
                false => {
                    let zeros = Arc::new(vec![0u8; part.buffer.len()]);
                    let part = Part { buffer: 0..zeros.len(), ..part };
                    let slot = self.make_local(part.index, false).await?;
                    self.write_into(slot, part, zeros).await?;
                }
            }
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// Syncs the data file, then logs the new local chunks. Flushes run
    /// one at a time: Linux reports a write-back error only once for each
    /// file, so two syncs at the same time could each miss an error. A
    /// flush that finds all earlier writes durable returns at once: so the
    /// writers that wait for one sync share the next.
    pub async fn flush(&self) -> Result<()> {
        self.ensure_usable()?;
        let target = self.writes_done.load(Ordering::SeqCst);
        let started = Instant::now();
        let _one_at_a_time = self.flushing.lock().await;
        self.ensure_usable()?;
        if self.writes_synced.load(Ordering::SeqCst) >= target {
            return Ok(());
        }
        let upto = self.writes_done.load(Ordering::SeqCst);
        // Taken before the syncs: each chunk in it wrote its data before it was listed.
        let unlogged = self.log.take_unlogged();
        // A write marks the file before the log learns of its chunk, so the
        // file holds the data of each chunk in `unlogged`.
        let unsynced = self.unsynced.swap(false, Ordering::SeqCst);
        let (file, synced) = (self.data.file().clone(), Instant::now());
        let mut result = match unsynced {
            true => spawn_blocking(move || file.sync_data()).await?.map_err(anyhow::Error::from),
            false => Ok(()),
        };
        IoStats::add(&self.stats.data_sync_micros, synced);
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);
        if result.is_ok() && !unlogged.is_empty() {
            let (log, chunks, appended) = (self.log.clone(), unlogged.clone(), Instant::now());
            result = spawn_blocking(move || log.append(&chunks)).await?;
            IoStats::add(&self.stats.log_append_micros, appended);
        }
        match &result {
            Ok(()) => drop(self.writes_synced.fetch_max(upto, Ordering::SeqCst)),
            // Linux may have dropped the pages that failed, so a later sync
            // that works proves nothing. The disk stops for good.
            Err(error) => {
                self.log.give_back(&unlogged);
                self.unsynced.store(true, Ordering::SeqCst);
                self.stop(&format!("a local sync failed: {error:#}"));
            }
        }
        if started.elapsed() > SLOW_FLUSH {
            log::warn!("disk {}: slow flush, {} ms", self.name, started.elapsed().as_millis());
        }
        result
    }

    /// Locks a chunk and makes it local. Unless `whole` (the write covers
    /// the full chunk), its current content goes into the data file first.
    async fn make_local(&self, index: usize, whole: bool) -> Result<OwnedRwLockWriteGuard<ChunkSlot>> {
        let mut slot = self.slot(index).await?.write_owned().await;
        if slot.local {
            return Ok(slot);
        }
        if !whole {
            let (data, copied) = (self.data.clone(), Instant::now());
            self.stats.chunk_copies.fetch_add(1, Ordering::Relaxed);
            match slot.base.is_zero() {
                true if data.has_holes() => {}
                true => spawn_blocking(move || data.zero(index)).await??,
                false => match self.clean(index, slot.base, Priority::Read).await? {
                    Clean::File(file) => spawn_blocking(move || data.copy_in(index, &file)).await??,
                    Clean::Data(chunk) => spawn_blocking(move || data.write_at(index, 0, &chunk)).await??,
                },
            }
            IoStats::add(&self.stats.chunk_copy_micros, copied);
        }
        slot.local = true;
        self.local.lock().unwrap().insert(index);
        Ok(slot)
    }

    async fn write_into(&self, mut slot: OwnedRwLockWriteGuard<ChunkSlot>, part: Part, data: Arc<Vec<u8>>) -> Result<()> {
        self.mark_unsaved(&mut slot, part.index);
        let files = self.data.clone();
        let (index, offset, len) = (part.index, part.offset, part.buffer.len() as u64);
        spawn_blocking(move || files.write_at(index, offset, &data[part.buffer])).await??;
        self.finish_write(&mut slot, index, len);
        Ok(())
    }

    async fn zero_chunk(&self, index: usize) -> Result<()> {
        let mut slot = self.slot(index).await?.write_owned().await;
        if !slot.local && slot.base.is_zero() {
            return Ok(());
        }
        if !slot.local {
            slot.local = true;
            self.local.lock().unwrap().insert(index);
        }
        self.mark_unsaved(&mut slot, index);
        let data = self.data.clone();
        spawn_blocking(move || data.zero(index)).await??;
        self.finish_write(&mut slot, index, 0);
        Ok(())
    }

    pub(super) fn mark_unsaved(&self, slot: &mut ChunkSlot, index: usize) {
        slot.changed = true;
        if !slot.unsaved {
            slot.unsaved = true;
            let count = {
                let mut unsaved = self.unsaved.lock().unwrap();
                unsaved.insert(index);
                unsaved.len()
            };
            let early = (self.tuning.transaction_after / self.chunk_size) as usize;
            if count >= early.min(self.unsaved_limit() / 2).max(1) {
                self.transaction_wanted.notify_one();
            }
        }
    }

    /// Call after the data is in the file. The data file is marked for the
    /// next flush before the log learns of the chunk, so a flush that logs
    /// the chunk also syncs its data.
    pub(super) fn finish_write(&self, slot: &mut ChunkSlot, index: usize, bytes: u64) {
        self.unsynced.store(true, Ordering::SeqCst);
        self.log.note_write(index);
        self.writes_done.fetch_add(1, Ordering::SeqCst);
        slot.written = Some(Instant::now());
        if self.written_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes >= WRITEBACK_AFTER {
            self.written_bytes.store(0, Ordering::Relaxed);
            self.writeback_wanted.notify_one();
        }
    }

    pub(super) fn note_unsaved_write(&self) {
        self.oldest_unsaved.lock().unwrap().get_or_insert_with(Instant::now);
    }

    /// Starts the write-back of written data files, so that a flush finds
    /// little left to write. Runs until the disk closes.
    pub async fn run_writeback(self: Arc<Self>) {
        while !self.is_closed() {
            self.writeback_wanted.notified().await;
            let data = self.data.clone();
            let _ = spawn_blocking(move || data.start_writeback()).await;
        }
    }

    /// The content of a clean chunk: from the cache of this disk, from the
    /// cache of another disk on the node, or else from the store. Many
    /// reads of the same chunk share one download.
    pub(super) async fn clean(&self, index: usize, entry: Entry, priority: Priority) -> Result<Clean> {
        let hash = entry.hash;
        if let Some(file) = self.open_cached(hash).await? {
            return Ok(Clean::File(file));
        }
        let lock = self.downloads.lock().unwrap().entry(hash).or_default().clone();
        let _one_download = lock.lock().await;
        let result = self.download(index, entry, priority).await;
        self.downloads.lock().unwrap().remove(&hash);
        result
    }

    async fn download(&self, index: usize, entry: Entry, priority: Priority) -> Result<Clean> {
        if let Some(file) = self.open_cached(entry.hash).await? {
            return Ok(Clean::File(file));
        }
        let data = {
            let _slot = self.transfers.download(priority).await?;
            Arc::new(self.store.chunk(&entry, index, self.chunk_size).await?)
        };
        let (cache, chunk) = (self.cache.clone(), data.clone());
        let stored = spawn_blocking(move || cache.insert(&entry.hash, |mut file| Ok(std::io::Write::write_all(&mut file, &chunk)?))).await?;
        if let Err(error) = stored {
            log::warn!("disk {}: chunk {index} not cached: {error:#}", self.name);
        }
        Ok(Clean::Data(data))
    }

    async fn open_cached(&self, hash: Hash) -> Result<Option<File>> {
        let cache = self.cache.clone();
        spawn_blocking(move || cache.open(&hash)).await?
    }

    /// Gets a chunk into the cache before the guest reads it.
    pub async fn fetch(&self, index: usize, priority: Priority) -> Result<()> {
        if index >= self.chunk_count() || self.is_closed() {
            return Ok(());
        }
        let base = {
            let slot = self.slot(index).await?.read_owned().await;
            if slot.local || slot.base.is_zero() || self.cache.has(&slot.base.hash) {
                return Ok(());
            }
            slot.base
        };
        self.clean(index, base, priority).await.map(drop)
    }

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

    pub(super) fn missing(&self, index: usize) -> anyhow::Error {
        anyhow!("disk {}: chunk {index} is not loaded", self.name)
    }
}
