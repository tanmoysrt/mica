//! The requests that a ublk queue thread serves alone.
//!
//! A read whose chunks are all local, cached or zero, and a write into
//! chunks that are all local, need no download and no wait. The queue
//! thread does them with io_uring, straight between the pages of the
//! request and the files. It never waits for a lock: if a lock is busy,
//! the request goes to the tokio side instead.

use super::Disk;
use super::Part;
use super::slots::ChunkSlot;
use std::fs::File;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard};

/// Where a part of a read comes from: a file and an offset, or zeros.
pub enum ReadSource {
    File(Arc<File>, u64),
    Zeros,
}

/// A planned read. The read locks of the local chunks stay held until it drops.
pub struct LocalRead {
    pub parts: Vec<(Part, ReadSource)>,
    _locks: Vec<OwnedRwLockReadGuard<ChunkSlot>>,
}

/// A planned write. The write locks stay held until `finish`.
pub struct LocalWrite<'a> {
    disk: &'a Disk,
    pub parts: Vec<(Part, Arc<File>, u64)>,
    locks: Vec<OwnedRwLockWriteGuard<ChunkSlot>>,
}

impl Disk {
    /// `None` means that the tokio side must serve the read.
    pub fn plan_local_read(&self, offset: u64, len: usize) -> Option<LocalRead> {
        if self.is_stopped() {
            return None;
        }
        let mut parts = Vec::new();
        let mut locks = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.slots.loaded(part.index)?.try_read_owned().ok()?;
            let source = if slot.local {
                let (file, start) = self.data.location(part.index);
                ReadSource::File(file.clone(), start + part.offset)
            } else if slot.base.is_zero() {
                ReadSource::Zeros
            } else {
                // A cache file never changes, so it needs no lock.
                ReadSource::File(Arc::new(self.cache.open_checked(&slot.base.hash)?), part.offset)
            };
            if slot.local {
                locks.push(slot);
            }
            parts.push((part, source));
        }
        for (part, _) in &parts {
            self.profile.record(part.index);
        }
        self.note_read(offset, len);
        Some(LocalRead { parts, _locks: locks })
    }

    /// `None` means that the tokio side must do the write.
    pub fn plan_local_write(&self, offset: u64, len: usize) -> Option<LocalWrite<'_>> {
        if self.is_stopped() {
            return None;
        }
        let mut parts = Vec::new();
        let mut locks = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.slots.loaded(part.index)?.try_write_owned().ok()?;
            if !slot.local || (!slot.unsaved && !self.has_room_for(part.index)) {
                return None;
            }
            let (file, start) = self.data.location(part.index);
            parts.push((part.clone(), file.clone(), start + part.offset));
            locks.push(slot);
        }
        Some(LocalWrite { disk: self, parts, locks })
    }
}

impl LocalWrite<'_> {
    /// Call after the data is in the files. Releases the locks.
    pub fn finish(mut self) {
        for ((part, _, _), slot) in self.parts.iter().zip(self.locks.iter_mut()) {
            self.disk.mark_unsaved(slot, part.index);
            self.disk.finish_write(slot, part.index, part.buffer.len() as u64);
        }
        self.disk.note_unsaved_write();
    }
}
