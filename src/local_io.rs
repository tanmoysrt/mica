use crate::disk::{ChunkPart, ChunkSlot, Disk};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::MutexGuard;

/// A read that the ublk queue thread can serve alone: every part is local.
pub struct LocalRead {
    /// `None` is a zero chunk.
    pub parts: Vec<(ChunkPart, Option<Arc<File>>)>,
}

/// A write that the ublk queue thread can do alone: every part goes into an
/// existing `.chunk` file. The chunk locks stay held until `finish`, as in
/// the normal write path, so a checkpoint cannot freeze a file mid-write.
pub struct LocalWrite<'a> {
    disk: &'a Disk,
    pub parts: Vec<LocalWritePart<'a>>,
}

pub struct LocalWritePart<'a> {
    pub part: ChunkPart,
    pub file: Arc<File>,
    _slot: MutexGuard<'a, ChunkSlot>,
}

impl Disk {
    /// Plans a read without waiting and without S3. `None` means the normal
    /// path must serve it: a chunk is missing locally, or a lock is busy.
    pub fn plan_local_read(&self, offset: u64, len: usize) -> Option<LocalRead> {
        self.ensure_usable().ok()?;
        let mut parts = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.slots[part.index].try_lock().ok()?;
            let file = match (&slot.working, &slot.frozen, slot.base) {
                (Some(file), _, _) | (None, Some(file), _) => Some(file.clone()),
                (None, None, Some(hash)) => Some(self.cache.open_cached(&hash)?),
                (None, None, None) => None,
            };
            parts.push((part, file));
        }
        for (part, _) in &parts {
            self.profile.record(part.index);
        }
        Some(LocalRead { parts })
    }

    /// Plans a write into existing `.chunk` files. `None` means the normal
    /// path must do it: a chunk has no `.chunk` yet, a lock is busy, local
    /// space is short, or a durable write needs a folder sync first.
    pub fn plan_local_write(&self, offset: u64, len: usize, durable: bool) -> Option<LocalWrite<'_>> {
        self.ensure_usable().ok()?;
        if !self.has_room() || (durable && self.folder_changed.load(Ordering::SeqCst)) {
            return None;
        }
        let mut parts = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.slots[part.index].try_lock().ok()?;
            let file = slot.working.clone()?;
            parts.push(LocalWritePart { part, file, _slot: slot });
        }
        Some(LocalWrite { disk: self, parts })
    }
}

impl LocalWrite<'_> {
    /// Call after the data is in the files. Releases the chunk locks.
    pub fn finish(self) {
        let mut unsynced = self.disk.unsynced.lock().unwrap();
        for part in &self.parts {
            unsynced.insert(part.part.index);
        }
        drop(unsynced);
        self.disk.note_unsaved_write();
    }

    pub fn disk(&self) -> &Disk {
        self.disk
    }
}
