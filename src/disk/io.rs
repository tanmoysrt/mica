//! Read, write, discard and flush on the tokio side.
//!
//! The queue threads send here each request that they cannot serve alone:
//! a chunk that is not local yet, a busy lock, a full disk, a flush.
//!
//! Write: lock the chunk. If it is not local, put its current content into
//! the data file first, unless the write covers the full chunk. Write the
//! data. Mark the chunk unsaved. Do not sync.
//!
//! Flush: sync the data file, then append the new local chunks to the
//! dirty log and sync it. A flush never uses the bucket.

use super::clean::Clean;
use super::slots::ChunkSlot;
use super::{Disk, IoStats, Part};
use crate::transfers::Priority;
use anyhow::Result;
use futures::{StreamExt, TryStreamExt};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::task::spawn_blocking;

/// After this many bytes of writes, the write-back of the data file starts.
/// Linux would wait for gigabytes, and the next flush would write them all.
const WRITEBACK_AFTER: u64 = 16 << 20;
const SLOW_FLUSH: Duration = Duration::from_millis(200);

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
        let data = Arc::new(data);
        for part in self.split(offset, data.len())? {
            self.wait_for_room(part.index).await?;
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
                self.wait_for_room(part.index).await?;
                self.zero_chunk(part.index).await?;
            }
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// Unlike a discard, a write of zeroes must zero every byte.
    pub async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        for part in self.split(offset, len as usize)? {
            self.wait_for_room(part.index).await?;
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
        // Taken before the sync. A write marks the file before the log learns
        // of its chunk, so the sync makes the data of each listed chunk durable.
        let mut unlogged = self.log.take_unlogged();
        let unsynced = self.unsynced.swap(false, Ordering::SeqCst);
        let (file, synced) = (self.data.file().clone(), Instant::now());
        let mut result = match unsynced {
            true => spawn_blocking(move || file.sync_data()).await?.map_err(anyhow::Error::from),
            false => Ok(()),
        };
        IoStats::add(&self.stats.data_sync_micros, synced);
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);
        if result.is_ok() && !unlogged.is_empty() {
            let (log, appended) = (self.log.clone(), Instant::now());
            (result, unlogged) = spawn_blocking(move || (log.append(&unlogged), unlogged)).await?;
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

    /// Starts the write-back of written data files, so that a flush finds
    /// little left to write. Runs until the disk closes.
    pub async fn run_writeback(self: Arc<Self>) {
        while !self.is_closed() {
            self.writeback_wanted.notified().await;
            let data = self.data.clone();
            let _ = spawn_blocking(move || data.start_writeback()).await;
        }
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
            // Half the limit starts a transaction early, so writes seldom wait.
            if count >= (self.unsaved_limit() / 2).max(1) {
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
}
