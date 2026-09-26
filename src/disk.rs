use crate::blocking::blocking;
use crate::bucket::Bucket;
use crate::chunk_cache::ChunkCache;
use crate::content_hash::ContentHash;
use crate::dirty_folder::DirtyFolder;
use crate::manifest::Manifest;
use crate::node::NodeIdentity;
use crate::read_profile::ReadProfile;
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;


/// One attached disk. It turns block reads and writes into chunk operations.
/// It knows nothing about ublk.
pub struct Disk {
    pub id: String,
    pub size: u64,
    pub(crate) chunk_size: u64,
    pub(crate) slots: Vec<tokio::sync::Mutex<ChunkSlot>>,
    pub(crate) folder: Arc<DirtyFolder>,
    pub(crate) cache: Arc<ChunkCache>,
    pub(crate) bucket: Arc<Bucket>,
    pub(crate) node: Arc<NodeIdentity>,
    pub(crate) committed: Mutex<Committed>,
    pub(crate) profile: ReadProfile,
    pub(crate) unsynced: Mutex<BTreeSet<usize>>,
    pub(crate) folder_changed: AtomicBool,
    pub(crate) local_chunks: AtomicUsize,
    local_chunk_limit: usize,
    pub(crate) space_freed: Notify,
    pub(crate) checkpoint_wanted: Notify,
    pub(crate) checkpoint_running: tokio::sync::Mutex<()>,
    pub(crate) ownership_lost: AtomicBool,
    /// Set when a local sync fails. Linux may have dropped the unsynced pages,
    /// so a later sync that succeeds proves nothing. The disk stops for good.
    pub(crate) sync_failed: AtomicBool,
    max_unsaved_age: Duration,
    wait_when_behind: bool,
    closed: AtomicBool,
    pub(crate) oldest_unsaved_write: Mutex<Option<Instant>>,
}

/// Where the current content of a chunk is. The first present one wins:
/// `working`, then `frozen`, then `base` (in S3), else zeros.
pub(crate) struct ChunkSlot {
    pub base: Option<ContentHash>,
    pub working: Option<Arc<File>>,
    pub frozen: Option<Arc<File>>,
}

pub(crate) struct Committed {
    pub manifest: Manifest,
    pub manifest_hash: ContentHash,
    pub profile: Vec<u32>,
}

pub struct DiskParts {
    pub disk_id: String,
    pub manifest: Manifest,
    pub manifest_hash: ContentHash,
    pub profile: Vec<u32>,
    pub folder: DirtyFolder,
    pub cache: Arc<ChunkCache>,
    pub bucket: Arc<Bucket>,
    pub node: Arc<NodeIdentity>,
    pub dirty_limit_bytes: u64,
    pub max_unsaved_age: Duration,
    pub wait_when_behind: bool,
}

#[derive(Debug, Serialize)]
pub struct DiskStatus {
    pub size: u64,
    pub seq: u64,
    pub local_chunks: usize,
    pub unsaved_bytes: u64,
    pub behind: bool,
    pub ownership_lost: bool,
    pub sync_failed: bool,
}

struct ChunkPart {
    index: usize,
    offset: u64,
    buffer: Range<usize>,
}

impl Disk {
    /// Builds the disk from its manifest, then adds the local chunks left by a crash.
    pub fn open(parts: DiskParts) -> Result<Self> {
        let manifest = parts.manifest;
        let chunk_size = manifest.chunk_size;
        let mut slots: Vec<ChunkSlot> = manifest
            .chunks
            .iter()
            .map(|base| ChunkSlot { base: *base, working: None, frozen: None })
            .collect();
        let recovered = parts.folder.recover()?;
        for &index in &recovered {
            let Some(slot) = slots.get_mut(index) else { bail!("local chunk {index} is outside the disk") };
            slot.working = Some(Arc::new(parts.folder.open_working(index)?));
        }
        Ok(Self {
            id: parts.disk_id,
            size: manifest.disk_size,
            chunk_size,
            slots: slots.into_iter().map(tokio::sync::Mutex::new).collect(),
            folder: Arc::new(parts.folder),
            cache: parts.cache,
            bucket: parts.bucket,
            node: parts.node,
            committed: Mutex::new(Committed {
                manifest,
                manifest_hash: parts.manifest_hash,
                profile: parts.profile,
            }),
            profile: ReadProfile::new(),
            unsynced: Mutex::default(),
            folder_changed: AtomicBool::new(false),
            local_chunks: AtomicUsize::new(recovered.len()),
            local_chunk_limit: (parts.dirty_limit_bytes / chunk_size).max(2) as usize,
            space_freed: Notify::new(),
            checkpoint_wanted: Notify::new(),
            checkpoint_running: tokio::sync::Mutex::new(()),
            ownership_lost: AtomicBool::new(false),
            sync_failed: AtomicBool::new(false),
            max_unsaved_age: parts.max_unsaved_age,
            wait_when_behind: parts.wait_when_behind,
            closed: AtomicBool::new(false),
            oldest_unsaved_write: Mutex::new((!recovered.is_empty()).then(Instant::now)),
        })
    }

    pub async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        let mut sources = Vec::new();
        for part in self.split(offset, len)? {
            self.profile.record(part.index);
            if let Some(file) = self.readable_file(part.index).await? {
                sources.push((part, file));
            }
        }
        blocking(move || {
            let mut data = vec![0u8; len];
            for (part, file) in sources {
                file.read_exact_at(&mut data[part.buffer], part.offset)?;
            }
            Ok(data)
        })
        .await
    }

    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        self.ensure_usable()?;
        self.wait_for_room().await;
        let data = Arc::new(data);
        for part in self.split(offset, data.len())? {
            self.write_part(part, data.clone()).await?;
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// A whole chunk becomes a sparse file of zeros, which the checkpoint
    /// stores as a zero chunk: nothing to upload. A discard is only a hint, so
    /// a part of a chunk is left alone. Zeroing it would fetch the chunk from
    /// S3 and upload it again, which costs more than it frees.
    pub async fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        for part in self.split(offset, len as usize)? {
            if part.buffer.len() as u64 == self.chunk_size {
                self.wait_for_room().await;
                self.zero_whole_chunk(part.index).await?;
                self.note_unsaved_write();
            }
        }
        Ok(())
    }

    /// Unlike a discard, write-zeroes must zero every byte.
    pub async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        self.wait_for_room().await;
        for part in self.split(offset, len as usize)? {
            if part.buffer.len() as u64 == self.chunk_size {
                self.zero_whole_chunk(part.index).await?;
            } else {
                let len = part.buffer.len();
                let part = ChunkPart { buffer: 0..len, ..part };
                self.write_part(part, Arc::new(vec![0u8; len])).await?;
            }
        }
        self.note_unsaved_write();
        Ok(())
    }

    /// Makes all written data durable on the local SSD. It never touches S3.
    pub async fn flush(&self) -> Result<()> {
        self.ensure_usable()?;
        // Writes during the sync add their chunk to the list again.
        let indexes = std::mem::take(&mut *self.unsynced.lock().unwrap());
        let synced = indexes.clone();
        let mut files = Vec::new();
        for index in indexes {
            if let Some(file) = self.slots[index].lock().await.working.clone() {
                files.push(file);
            }
        }
        let folder_changed = self.folder_changed.swap(false, Ordering::SeqCst);
        let folder = self.folder.clone();
        let result = blocking(move || {
            for file in files {
                file.sync_data()?;
            }
            if folder_changed {
                folder.sync()?;
            }
            Ok(())
        })
        .await;
        if let Err(error) = &result {
            // Keep them marked, and stop the disk: a retry could succeed
            // after Linux already dropped the data.
            self.unsynced.lock().unwrap().extend(synced);
            if folder_changed {
                self.folder_changed.store(true, Ordering::SeqCst);
            }
            self.fail_sync(error);
        }
        result
    }

    /// Gets a chunk into the cache before the guest asks for it.
    pub async fn prefetch_chunk(&self, index: usize) -> Result<()> {
        let base = {
            let Some(slot) = self.slots.get(index) else { return Ok(()) };
            let slot = slot.lock().await;
            if slot.working.is_some() || slot.frozen.is_some() {
                return Ok(());
            }
            slot.base
        };
        if let Some(hash) = base {
            self.cache.open(&hash).await?;
        }
        Ok(())
    }

    pub fn status(&self) -> DiskStatus {
        let behind = self.is_behind();
        DiskStatus {
            size: self.size,
            seq: self.committed.lock().unwrap().manifest.seq,
            local_chunks: self.local_chunks.load(Ordering::SeqCst),
            unsaved_bytes: self.local_chunks.load(Ordering::SeqCst) as u64 * self.chunk_size,
            behind,
            ownership_lost: self.ownership_lost.load(Ordering::SeqCst),
            sync_failed: self.sync_failed.load(Ordering::SeqCst),
        }
    }

    pub fn is_blank(&self) -> bool {
        self.committed.lock().unwrap().manifest.is_blank() && !self.has_local_chunks()
    }

    pub fn has_local_chunks(&self) -> bool {
        self.local_chunks.load(Ordering::SeqCst) > 0
    }

    pub fn committed_profile(&self) -> Vec<u32> {
        self.committed.lock().unwrap().profile.clone()
    }

    pub async fn checkpoint_requested(&self) {
        self.checkpoint_wanted.notified().await
    }

    /// Stops the checkpoint loop and the prefetch of this disk.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.checkpoint_wanted.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn ensure_usable(&self) -> Result<()> {
        if self.ownership_lost.load(Ordering::SeqCst) {
            bail!("disk {} is owned by another node now", self.id);
        }
        if self.sync_failed.load(Ordering::SeqCst) {
            bail!("disk {} stopped after a failed local sync; its local data is kept", self.id);
        }
        Ok(())
    }

    /// Stops the disk for good after a failed sync. See `sync_failed`.
    pub(crate) fn fail_sync(&self, error: &anyhow::Error) {
        self.sync_failed.store(true, Ordering::SeqCst);
        log::error!("disk {}: local sync failed, the disk is stopped: {error:#}", self.id);
    }

    /// True when the oldest write that is not in S3 is older than the limit.
    pub fn is_behind(&self) -> bool {
        let limit = self.max_unsaved_age;
        self.oldest_unsaved_write.lock().unwrap().is_some_and(|since| since.elapsed() > limit)
    }

    async fn readable_file(&self, index: usize) -> Result<Option<Arc<File>>> {
        let slot = self.slots[index].lock().await;
        if let Some(file) = slot.working.as_ref().or(slot.frozen.as_ref()) {
            return Ok(Some(file.clone()));
        }
        match slot.base {
            Some(hash) => Ok(Some(Arc::new(self.cache.open(&hash).await?))),
            None => Ok(None),
        }
    }

    /// Holds the slot lock during the write, so a checkpoint cannot freeze
    /// the file while the write is still going into it.
    async fn write_part(&self, part: ChunkPart, data: Arc<Vec<u8>>) -> Result<()> {
        let mut slot = self.slots[part.index].lock().await;
        let whole_chunk = part.buffer.len() as u64 == self.chunk_size;
        let file = self.working_file(&mut slot, part.index, whole_chunk).await?;
        blocking(move || Ok(file.write_all_at(&data[part.buffer], part.offset)?)).await?;
        self.unsynced.lock().unwrap().insert(part.index);
        Ok(())
    }

    async fn zero_whole_chunk(&self, index: usize) -> Result<()> {
        let mut slot = self.slots[index].lock().await;
        if slot.working.is_none() && slot.frozen.is_none() && slot.base.is_none() {
            return Ok(());
        }
        let file = self.working_file(&mut slot, index, true).await?;
        let chunk_size = self.chunk_size;
        blocking(move || {
            file.set_len(0)?;
            Ok(file.set_len(chunk_size)?)
        })
        .await?;
        self.unsynced.lock().unwrap().insert(index);
        Ok(())
    }

    /// Returns `<index>.chunk`, and makes it first if needed. When the write
    /// covers the whole chunk, the old content is not needed, so nothing is fetched.
    async fn working_file(&self, slot: &mut ChunkSlot, index: usize, whole_chunk: bool) -> Result<Arc<File>> {
        if let Some(file) = &slot.working {
            return Ok(file.clone());
        }
        let source = match (&slot.frozen, slot.base) {
            (Some(frozen), _) => Some(frozen.clone()),
            (None, _) if whole_chunk => None,
            (None, Some(hash)) => Some(Arc::new(self.cache.open(&hash).await?)),
            (None, None) => None,
        };
        let folder = self.folder.clone();
        let file = Arc::new(blocking(move || folder.create_working(index, source.as_deref())).await?);
        if slot.frozen.is_none() {
            self.local_chunk_added();
        }
        self.folder_changed.store(true, Ordering::SeqCst);
        slot.working = Some(file.clone());
        Ok(file)
    }

    fn local_chunk_added(&self) {
        let count = self.local_chunks.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= self.local_chunk_limit / 2 {
            self.checkpoint_wanted.notify_one();
        }
    }

    /// Writes wait for a checkpoint at the dirty limit, and, with
    /// `wait_when_behind`, while unsaved data is older than the limit. That
    /// makes the data-loss bound hold on a slow link. The guest sees a slow
    /// disk, not an I/O error.
    async fn wait_for_room(&self) {
        loop {
            let freed = self.space_freed.notified();
            let full = self.local_chunks.load(Ordering::SeqCst) >= self.local_chunk_limit;
            let too_old = self.wait_when_behind && self.is_behind();
            if !full && !too_old {
                return;
            }
            self.checkpoint_wanted.notify_one();
            freed.await;
        }
    }

    fn note_unsaved_write(&self) {
        self.oldest_unsaved_write.lock().unwrap().get_or_insert_with(Instant::now);
    }

    fn split(&self, offset: u64, len: usize) -> Result<Vec<ChunkPart>> {
        let end = offset + len as u64;
        if end > self.size {
            bail!("I/O at {offset}+{len} is past the end of the disk ({})", self.size);
        }
        let mut parts = Vec::new();
        let mut position = offset;
        while position < end {
            let index = (position / self.chunk_size) as usize;
            let offset_in_chunk = position % self.chunk_size;
            let part_len = (self.chunk_size - offset_in_chunk).min(end - position);
            let start = (position - offset) as usize;
            parts.push(ChunkPart { index, offset: offset_in_chunk, buffer: start..start + part_len as usize });
            position += part_len;
        }
        Ok(parts)
    }
}
