use crate::disk::{ChunkPart, ChunkSlot, Disk, ReadPass, Source};
use std::fs::File;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;

/// A read that the ublk queue thread can serve alone: every part is local.
/// The pass keeps the space of its local chunks until the read ends.
pub struct LocalRead<'a> {
    /// `None` is a zero chunk.
    pub parts: Vec<(ChunkPart, Source)>,
    _pass: ReadPass<'a>,
}

/// A write that the ublk queue thread can do alone: every part goes into a
/// chunk that has local data. The chunk locks stay held until `finish`, as
/// in the normal write path, so a checkpoint cannot stage a chunk mid-write.
pub struct LocalWrite<'a> {
    disk: &'a Disk,
    pub parts: Vec<LocalWritePart>,
}

pub struct LocalWritePart {
    pub part: ChunkPart,
    pub file: Arc<File>,
    /// The offset of the part in `file`.
    pub offset: u64,
    slot: OwnedMutexGuard<ChunkSlot>,
}

impl Disk {
    /// Plans a read without waiting and without S3. `None` means the normal
    /// path must serve it: a chunk is missing locally, or a lock is busy.
    pub fn plan_local_read(&self, offset: u64, len: usize) -> Option<LocalRead<'_>> {
        self.ensure_usable().ok()?;
        let pass = self.start_read();
        let mut parts = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.loaded_slot(part.index)?.try_lock_owned().ok()?;
            let source = match (slot.local, slot.base) {
                (true, _) => {
                    let (file, start) = self.data.location(part.index);
                    Some((file.clone(), start))
                }
                (false, Some(hash)) => Some((self.cache.open_cached(&hash)?, 0)),
                (false, None) => None,
            };
            parts.push((part, source));
        }
        for (part, _) in &parts {
            self.profile.record(part.index);
        }
        self.note_read(offset, len);
        Some(LocalRead { parts, _pass: pass })
    }

    /// Plans a write into chunks with local data. `None` means the normal
    /// path must do it: a chunk is not local, a lock is busy, unsaved data is
    /// at its limit, or a staged chunk needs its held copy.
    pub fn plan_local_write(&self, offset: u64, len: usize) -> Option<LocalWrite<'_>> {
        self.ensure_usable().ok()?;
        if !self.has_room() {
            return None;
        }
        let mut slots = Vec::new();
        for part in self.split(offset, len).ok()? {
            let slot = self.loaded_slot(part.index)?.try_lock_owned().ok()?;
            // A staged chunk needs a held copy first: the normal path makes it.
            if !slot.local || (slot.staged && !slot.held) {
                return None;
            }
            slots.push((part, slot));
        }
        let mut parts = Vec::with_capacity(slots.len());
        for (part, mut slot) in slots {
            self.mark_unsaved(&mut slot, part.index);
            let (file, start) = self.data.location(part.index);
            let (file, offset) = (file.clone(), start + part.offset);
            parts.push(LocalWritePart { part, file, offset, slot });
        }
        Some(LocalWrite { disk: self, parts })
    }
}

impl LocalWrite<'_> {
    /// Call after the data is in the files. Releases the chunk locks.
    /// An error here (the `recent` list) fails the request.
    pub fn finish(mut self) -> anyhow::Result<()> {
        for part in &mut self.parts {
            self.disk.finish_write(&mut part.slot, part.part.index, part.part.buffer.len() as u64)?;
        }
        self.disk.note_unsaved_write();
        Ok(())
    }
}
