use crate::blocking::blocking;
use crate::bucket::Bucket;
use crate::chunk_cache::{ChunkCache, Fetch};
use crate::content_hash::ContentHash;
use crate::local_data::LocalData;
use crate::manifest::{CHUNKS_PER_PAGE, Manifest};
use crate::node::NodeIdentity;
use crate::page_store::PageStore;
use crate::prefetch::ReadAhead;
use crate::read_profile::ReadProfile;
use anyhow::{Result, bail};
use serde::Serialize;
use futures::{StreamExt, TryStreamExt};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Notify, OnceCell};

/// A flush slower than this is logged, with where its time went.
const SLOW_FLUSH: Duration = Duration::from_millis(200);
const READS_POLL: Duration = Duration::from_millis(1);
/// Held copies stay in memory up to this size per disk, then go to the hold file.
const HELD_IN_MEMORY: u64 = 64 << 20;
/// Writeback of the data files starts after this many bytes of writes.
/// Linux would wait until gigabytes are dirty, and the next FLUSH would
/// then write all of them.
const WRITEBACK_AFTER: u64 = 16 << 20;

type ChunkSlots = Box<[Arc<tokio::sync::Mutex<ChunkSlot>>]>;
type SlotPage = Arc<OnceCell<ChunkSlots>>;

/// One attached disk, independent of the kernel block-device interface.
pub struct Disk {
    pub id: String,
    pub(crate) size: AtomicU64,
    pub(crate) chunk_size: u64,
    /// The chunk slots of each metadata page, made on first use. A disk
    /// costs memory only for the parts that the guest uses.
    pub(crate) pages: RwLock<Vec<SlotPage>>,
    /// Chunks whose local content may not be in S3: the work of the next checkpoint.
    pub(crate) unsaved: Arc<Mutex<BTreeSet<usize>>>,
    pub(crate) unsaved_chunks: AtomicUsize,
    /// Chunks queued for eviction, oldest entry first. Each chunk has at
    /// most one entry, even if it is written and saved again.
    pub(crate) saved_local: Mutex<VecDeque<usize>>,
    /// Chunks that left the data files, whose space is not freed yet: a
    /// FLUSH was adding them to the log. The next free tries again.
    unpunched: Mutex<Vec<usize>>,
    /// All chunks with local data, saved or not: what the dirty log lists.
    pub(crate) local_set: Arc<Mutex<BTreeSet<usize>>>,
    pub(crate) local_chunks: AtomicUsize,
    pub(crate) data: Arc<LocalData>,
    pub(crate) cache: Arc<ChunkCache>,
    pub(crate) page_store: Arc<PageStore>,
    pub(crate) bucket: Arc<Bucket>,
    pub(crate) node: Arc<NodeIdentity>,
    pub(crate) committed: Mutex<Committed>,
    pub(crate) profile: ReadProfile,
    read_ahead: Mutex<ReadAhead>,
    /// Chunk ranges for the prefetch task to fetch ahead of a sequential reader.
    fetches: UnboundedSender<Range<usize>>,
    fetch_queue: Mutex<Option<UnboundedReceiver<Range<usize>>>>,
    /// Held copies of staged chunks, see `ChunkSlot::held`: in memory first.
    pub(crate) held: Mutex<HashMap<usize, Vec<u8>>>,
    pub(crate) held_on_file: AtomicBool,
    /// Data files written since the last FLUSH.
    pub(crate) unsynced_segments: Mutex<BTreeSet<usize>>,
    /// Bytes written since the last start of writeback.
    written_bytes: AtomicU64,
    writeback_wanted: Notify,
    /// Starts the background fill again: the cache budget changed.
    pub(crate) fill_wanted: Notify,
    /// Writes done, and how many of them a FLUSH made durable. A FLUSH
    /// that finds all writes before it durable has nothing to do.
    pub(crate) writes_done: AtomicU64,
    writes_synced: AtomicU64,
    flushing: tokio::sync::Mutex<()>,
    /// Reads of local data in flight, in two epochs. The space of a chunk
    /// that left the data files is freed after the reads that may use it end.
    read_epoch: AtomicU64,
    readers: [AtomicUsize; 2],
    pub(crate) local_chunk_limit: AtomicUsize,
    pub(crate) limits: Mutex<crate::disk_limits::DiskLimits>,
    host_pressure: Arc<AtomicBool>,
    /// A checkpoint starts early at this many unsaved chunks.
    checkpoint_after_chunks: usize,
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

/// Where the current content of a chunk is: the data files when `local`,
/// else `base` (in S3), else zeros.
pub(crate) struct ChunkSlot {
    pub base: Option<ContentHash>,
    pub queued_for_eviction: bool,
    /// The data files hold the chunk's current content.
    pub local: bool,
    /// The local content may differ from `base`.
    pub unsaved: bool,
    /// Read by the running checkpoint, which may still need that content.
    pub staged: bool,
    /// Written since the running checkpoint staged the chunk.
    pub changed: bool,
    /// A copy of the staged content is kept, in memory or in the hold file:
    /// the chunk changed since.
    pub held: bool,
    /// Its last checkpoint failed, maybe after its head named the upload:
    /// `base` may name an object that GC can delete. The next upload PUTs it
    /// even if its hash equals `base`.
    pub reupload: bool,
    /// Found in the dirty log after a restart. A checkpoint that failed
    /// before the restart may have left its committed object to GC, so the
    /// upload checks that S3 has it before it skips a chunk equal to `base`.
    /// Only a successful checkpoint clears it.
    pub confirm: bool,
    /// Found in the dirty log after a restart. It counts as unsaved until a
    /// checkpoint compares it with its committed hash, but not toward the
    /// limit of unsaved data: it is most likely saved.
    pub verify: bool,
    /// The last write. A background checkpoint leaves a chunk that was just
    /// written for later: the guest is likely to write it again.
    pub written: Option<Instant>,
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
    pub data: LocalData,
    /// The chunks that the dirty logs list: they may have local changes.
    pub listed: Vec<usize>,
    pub cache: Arc<ChunkCache>,
    pub page_store: Arc<PageStore>,
    pub bucket: Arc<Bucket>,
    pub node: Arc<NodeIdentity>,
    pub dirty_limit_bytes: u64,
    pub limits: crate::disk_limits::DiskLimits,
    pub host_pressure: Arc<AtomicBool>,
    pub checkpoint_after_bytes: u64,
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
    pub cache_bytes: u64,
    pub cache_limit_bytes: u64,
    pub dirty_limit_bytes: u64,
}

/// The part of a request that falls in one chunk.
pub(crate) struct ChunkPart {
    pub index: usize,
    /// Offset inside the chunk.
    pub offset: u64,
    /// Range inside the request's buffer.
    pub buffer: Range<usize>,
}

/// Where a read part comes from: a file, and the chunk's offset in it.
pub(crate) type Source = Option<(Arc<File>, u64)>;

/// Counts a read of local data in its epoch while it lives. See `Disk::readers`.
pub(crate) struct ReadPass<'a> {
    disk: &'a Disk,
    epoch: usize,
}

impl Drop for ReadPass<'_> {
    fn drop(&mut self) {
        self.disk.readers[self.epoch].fetch_sub(1, Ordering::SeqCst);
    }
}

impl Disk {
    /// Builds the disk from its root, then finds which listed chunks hold
    /// local changes: those that differ from their committed hash.
    /// Only the pages of listed chunks load now; the others load on first use.
    pub async fn open(parts: DiskParts) -> Result<Self> {
        let manifest = parts.manifest;
        let chunk_size = manifest.chunk_size;
        let pages = (0..manifest.pages.len()).map(|_| Arc::new(OnceCell::new())).collect();
        let (fetches, fetch_queue) = unbounded_channel();
        let disk = Self {
            id: parts.disk_id,
            size: AtomicU64::new(manifest.disk_size),
            chunk_size,
            pages: RwLock::new(pages),
            unsaved: Arc::default(),
            unsaved_chunks: AtomicUsize::new(0),
            saved_local: Mutex::default(),
            unpunched: Mutex::default(),
            local_set: Arc::default(),
            local_chunks: AtomicUsize::new(0),
            data: Arc::new(parts.data),
            cache: parts.cache,
            page_store: parts.page_store,
            bucket: parts.bucket,
            node: parts.node,
            committed: Mutex::new(Committed {
                manifest,
                manifest_hash: parts.manifest_hash,
                profile: parts.profile,
            }),
            profile: ReadProfile::new(chunk_size),
            read_ahead: Mutex::default(),
            fetches,
            fetch_queue: Mutex::new(Some(fetch_queue)),
            held: Mutex::default(),
            held_on_file: AtomicBool::new(false),
            unsynced_segments: Mutex::default(),
            written_bytes: AtomicU64::new(0),
            writeback_wanted: Notify::new(),
            fill_wanted: Notify::new(),
            // Above zero: the first FLUSH syncs what a crashed daemon left.
            writes_done: AtomicU64::new(1),
            writes_synced: AtomicU64::new(0),
            flushing: tokio::sync::Mutex::new(()),
            read_epoch: AtomicU64::new(0),
            readers: [AtomicUsize::new(0), AtomicUsize::new(0)],
            local_chunk_limit: AtomicUsize::new((parts.dirty_limit_bytes / chunk_size).max(2) as usize),
            limits: Mutex::new(parts.limits),
            host_pressure: parts.host_pressure,
            checkpoint_after_chunks: (parts.checkpoint_after_bytes / chunk_size).max(1) as usize,
            space_freed: Notify::new(),
            checkpoint_wanted: Notify::new(),
            checkpoint_running: tokio::sync::Mutex::new(()),
            ownership_lost: AtomicBool::new(false),
            sync_failed: AtomicBool::new(false),
            max_unsaved_age: parts.max_unsaved_age,
            wait_when_behind: parts.wait_when_behind,
            closed: AtomicBool::new(false),
            oldest_unsaved_write: Mutex::new(None),
        };
        for index in parts.listed {
            disk.recover_chunk(index).await?;
        }
        if disk.has_unsaved_chunks() {
            *disk.oldest_unsaved_write.lock().unwrap() = Some(Instant::now());
        }
        Ok(disk)
    }

    /// A listed chunk is local. The next checkpoint finds out whether S3
    /// has it. It may hold writes the guest has not flushed, so the next
    /// FLUSH syncs its data file.
    async fn recover_chunk(&self, index: usize) -> Result<()> {
        if index >= self.chunk_count() {
            bail!("local chunk {index} is outside the disk");
        }
        let mut slot = self.slot(index).await?.lock_owned().await;
        slot.local = true;
        slot.unsaved = true;
        slot.verify = true;
        slot.confirm = true;
        self.local_set.lock().unwrap().insert(index);
        self.local_chunks.fetch_add(1, Ordering::SeqCst);
        self.unsaved.lock().unwrap().insert(index);
        self.unsynced_segments.lock().unwrap().insert(self.data.segment_of(index));
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    pub fn chunk_count(&self) -> usize {
        (self.size() / self.chunk_size) as usize
    }

    pub(crate) async fn slot(&self, index: usize) -> Result<Arc<tokio::sync::Mutex<ChunkSlot>>> {
        let page = index / CHUNKS_PER_PAGE;
        let cell = self.pages.read().unwrap()[page].clone();
        let slots = cell.get_or_try_init(|| self.load_page(page)).await?;
        Ok(slots[index % CHUNKS_PER_PAGE].clone())
    }

    pub(crate) fn loaded_slot(&self, index: usize) -> Option<Arc<tokio::sync::Mutex<ChunkSlot>>> {
        let pages = self.pages.read().unwrap();
        let slots = pages.get(index / CHUNKS_PER_PAGE)?.get()?;
        Some(slots[index % CHUNKS_PER_PAGE].clone())
    }

    /// A page that is not loaded has no local chunks, so its slots come from
    /// the committed root alone.
    async fn load_page(&self, page: usize) -> Result<ChunkSlots> {
        let hash = self.committed.lock().unwrap().manifest.pages[page];
        let chunks = self.page_store.get_or_zero(hash.as_ref()).await?;
        Ok(chunks
            .chunks
            .iter()
            .map(|base| {
                Arc::new(tokio::sync::Mutex::new(ChunkSlot {
                    base: *base,
                    queued_for_eviction: false,
                    local: false,
                    unsaved: false,
                    staged: false,
                    changed: false,
                    held: false,
                    reupload: false,
                    confirm: false,
                    verify: false,
                    written: None,
                }))
            })
            .collect())
    }

    /// Local parts are read first, under a read pass. Parts from the cache
    /// come after, without it: their download can take long, and the pass
    /// would hold up the freeing of local space.
    pub async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        self.note_read(offset, len);
        let pass = self.start_read();
        let mut local = Vec::new();
        let mut cached = Vec::new();
        for part in self.split(offset, len)? {
            self.profile.record(part.index);
            let slot = self.slot(part.index).await?.lock_owned().await;
            if slot.local {
                let (file, start) = self.data.location(part.index);
                local.push((part, file.clone(), start));
            } else if let Some(hash) = slot.base {
                cached.push((part, hash));
            }
        }
        let mut data = blocking(move || {
            let mut data = vec![0u8; len];
            for (part, file, start) in local {
                file.read_exact_at(&mut data[part.buffer], start + part.offset)?;
            }
            Ok(data)
        })
        .await?;
        drop(pass);
        // A write meanwhile makes the chunk local, but the cache still has
        // the content from before it: a read that races a write may see either.
        let cached: Vec<_> = futures::stream::iter(cached)
            .map(|(part, hash)| async move { Ok::<_, anyhow::Error>((part, self.cache.open(&hash).await?)) })
            .buffer_unordered(16)
            .try_collect()
            .await?;
        if !cached.is_empty() {
            data = blocking(move || {
                for (part, file) in cached {
                    file.read_exact_at(&mut data[part.buffer], part.offset)?;
                }
                Ok(data)
            })
            .await?;
        }
        Ok(data)
    }

    /// Call before a read picks its sources, and keep it until the read ends.
    pub(crate) fn start_read(&self) -> ReadPass<'_> {
        loop {
            let epoch = (self.read_epoch.load(Ordering::SeqCst) & 1) as usize;
            self.readers[epoch].fetch_add(1, Ordering::SeqCst);
            if (self.read_epoch.load(Ordering::SeqCst) & 1) as usize == epoch {
                return ReadPass { disk: self, epoch };
            }
            self.readers[epoch].fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Waits for every read that started before now. A read that starts
    /// later sees the slots as they are now.
    pub(crate) async fn wait_for_reads(&self) {
        let old = (self.read_epoch.fetch_add(1, Ordering::SeqCst) & 1) as usize;
        while self.readers[old].load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(READS_POLL).await;
        }
    }

    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        self.write_parts(offset, data).await.map(|_| ())
    }

    /// A FUA write: durable when it returns.
    ///
    /// It syncs only the data files of its chunks, and logs only its chunks,
    /// but in the same one-at-a-time order as FLUSH: Linux reports a
    /// writeback error only once to each open file, so two syncs at the same
    /// time could each miss a failure that the other saw.
    pub async fn write_durable(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        let chunks = self.write_parts(offset, data).await?;
        self.sync_local(Some(&chunks)).await
    }

    /// Returns the chunks it wrote.
    async fn write_parts(&self, offset: u64, data: Vec<u8>) -> Result<BTreeSet<usize>> {
        self.ensure_usable()?;
        self.wait_for_room().await?;
        let data = Arc::new(data);
        let mut chunks = BTreeSet::new();
        for part in self.split(offset, data.len())? {
            chunks.insert(part.index);
            self.write_part(part, data.clone()).await?;
        }
        self.note_unsaved_write();
        Ok(chunks)
    }

    /// A whole chunk becomes zeros, which the checkpoint stores as a zero
    /// chunk: nothing to upload. A discard is only a hint, so a part of a
    /// chunk is left alone. Zeroing it would fetch the chunk from S3 and
    /// upload it again, which costs more than it frees.
    pub async fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        for part in self.split(offset, len as usize)? {
            if part.buffer.len() as u64 == self.chunk_size {
                self.wait_for_room().await?;
                self.zero_whole_chunk(part.index).await?;
                self.note_unsaved_write();
            }
        }
        Ok(())
    }

    /// Unlike a discard, write-zeroes must zero every byte.
    pub async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.ensure_usable()?;
        self.wait_for_room().await?;
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
    ///
    /// First the data files that changed, then the dirty log, so the log
    /// never lists a chunk whose data could be lost. FLUSHes run one at a
    /// time; one that finds its writes durable already returns at once.
    pub async fn flush(&self) -> Result<()> {
        self.sync_local(None).await
    }

    /// FLUSH, or with `chunks`, the sync of a FUA write: only their data
    /// files, and only them in the log.
    async fn sync_local(&self, chunks: Option<&BTreeSet<usize>>) -> Result<()> {
        self.ensure_usable()?;
        let target = self.writes_done.load(Ordering::SeqCst);
        let started = Instant::now();
        let _one_at_a_time = self.flushing.lock().await;
        // The sync before may have failed, and Linux may have dropped pages.
        self.ensure_usable()?;
        if self.writes_synced.load(Ordering::SeqCst) >= target {
            return Ok(());
        }
        let waited = started.elapsed();
        let upto = self.writes_done.load(Ordering::SeqCst);
        // Taken before the data files: a chunk in this list wrote its data
        // file, and marked it, before it was listed.
        let unlogged = self.data.log().take_unlogged(chunks);
        let segments: BTreeSet<usize> = {
            let mut unsynced = self.unsynced_segments.lock().unwrap();
            match chunks {
                None => std::mem::take(&mut *unsynced),
                Some(chunks) => {
                    let own: BTreeSet<usize> = chunks.iter().map(|&index| self.data.segment_of(index)).collect();
                    unsynced.retain(|segment| !own.contains(segment));
                    own
                }
            }
        };
        let syncs = segments.iter().map(|&segment| {
            let file = self.data.segment(segment);
            blocking(move || Ok(file.sync_data()?))
        });
        let mut result = futures::future::try_join_all(syncs).await.map(|_| ());
        let data_synced = started.elapsed();
        if result.is_ok() && !unlogged.is_empty() {
            let (data, list) = (self.data.clone(), unlogged.clone());
            result = blocking(move || data.log().append(&list)).await;
        }
        match &result {
            Ok(()) if chunks.is_none() => drop(self.writes_synced.fetch_max(upto, Ordering::SeqCst)),
            Ok(()) => {}
            Err(error) => {
                self.data.log().restore_unlogged(unlogged.clone());
                self.unsynced_segments.lock().unwrap().extend(segments.iter().copied());
                self.fail_sync(error);
            }
        }
        let elapsed = started.elapsed();
        if elapsed > SLOW_FLUSH {
            log::warn!(
                "disk {}: slow flush, {} ms: {} ms waiting, {} data files in {} ms, {} chunks logged",
                self.id,
                elapsed.as_millis(),
                waited.as_millis(),
                segments.len(),
                (data_synced - waited).as_millis(),
                unlogged.len()
            );
        }
        result
    }

    /// Feeds read-ahead. Cheap, and never waits, so the queue thread calls it too.
    pub(crate) fn note_read(&self, offset: u64, len: usize) {
        let range = self.read_ahead.lock().unwrap().after_read(offset, len, self.chunk_size, self.chunk_count());
        if let Some(range) = range {
            let _ = self.fetches.send(range);
        }
    }

    /// True when the chunk needs no download: local data, a zero chunk, or
    /// in the cache. False also when its page is not loaded yet.
    pub(crate) fn is_local(&self, index: usize) -> bool {
        let Some(slot) = self.loaded_slot(index) else { return false };
        let Ok(slot) = slot.try_lock_owned() else { return false };
        slot.local || slot.base.is_none_or(|hash| self.cache.has(&hash))
    }

    /// The read-ahead queue, for the prefetch task. There is only one.
    pub fn take_fetch_queue(&self) -> Option<UnboundedReceiver<Range<usize>>> {
        self.fetch_queue.lock().unwrap().take()
    }

    /// Gets a chunk into the cache before the guest asks for it, behind all reads.
    pub async fn fetch_chunk(&self, index: usize) -> Result<()> {
        let base = {
            if index >= self.chunk_count() || self.is_closed() {
                return Ok(());
            }
            let slot = self.slot(index).await?.lock_owned().await;
            if slot.local {
                return Ok(());
            }
            slot.base
        };
        if let Some(hash) = base {
            self.cache.fetch(&hash, Fetch::Likely).await?;
        }
        Ok(())
    }

    pub fn status(&self) -> DiskStatus {
        let behind = self.is_behind();
        DiskStatus {
            size: self.size(),
            cache_bytes: self.cache.bytes(),
            cache_limit_bytes: self.cache.limit_bytes(),
            dirty_limit_bytes: self.local_chunk_limit.load(Ordering::Relaxed) as u64 * self.chunk_size,
            seq: self.committed.lock().unwrap().manifest.seq,
            local_chunks: self.local_chunks.load(Ordering::SeqCst),
            unsaved_bytes: self.unsaved_chunks.load(Ordering::SeqCst) as u64 * self.chunk_size,
            behind,
            ownership_lost: self.ownership_lost.load(Ordering::SeqCst),
            sync_failed: self.sync_failed.load(Ordering::SeqCst),
        }
    }

    pub fn is_blank(&self) -> bool {
        self.committed.lock().unwrap().manifest.is_blank() && !self.has_unsaved_chunks()
    }

    /// Every chunk of the last commit, in disk order. Loads all pages.
    pub async fn committed_chunks(&self) -> Result<Vec<Option<ContentHash>>> {
        let manifest = self.committed.lock().unwrap().manifest.clone();
        self.page_store.chunks_of(&manifest).await
    }

    pub fn has_unsaved_chunks(&self) -> bool {
        !self.unsaved.lock().unwrap().is_empty()
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
        self.fill_wanted.notify_one();
        self.checkpoint_wanted.notify_one();
        self.writeback_wanted.notify_one();
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
        self.space_freed.notify_waiters();
        log::error!("disk {}: local sync failed, the disk is stopped: {error:#}", self.id);
    }

    /// True when the oldest write that is not in S3 is older than the limit.
    pub fn is_behind(&self) -> bool {
        let limit = self.max_unsaved_age;
        self.oldest_unsaved_write.lock().unwrap().is_some_and(|since| since.elapsed() > limit)
    }

    /// Holds the slot lock during the write, so a checkpoint cannot stage
    /// the chunk while the write is still going into it.
    async fn write_part(&self, part: ChunkPart, data: Arc<Vec<u8>>) -> Result<()> {
        let index = part.index;
        let mut slot = self.slot(index).await?.lock_owned().await;
        let whole_chunk = part.buffer.len() as u64 == self.chunk_size;
        if !slot.local {
            // A whole-chunk write replaces all of it: nothing to put in first.
            if !whole_chunk {
                self.make_local(index, slot.base).await?;
            }
            self.became_local(&mut slot, index);
        }
        self.hold_if_staged(&mut slot, index).await?;
        self.mark_unsaved(&mut slot, index);
        let (file, start) = self.data.location(index);
        let (file, offset) = (file.clone(), start + part.offset);
        let bytes = part.buffer.len() as u64;
        blocking(move || Ok(file.write_all_at(&data[part.buffer], offset)?)).await?;
        self.finish_write(&mut slot, index, bytes)
    }

    /// Puts the chunk's current content into the data files, before a part
    /// of it is written: from the cache, or zeros. A zero chunk is a hole
    /// already, unless the filesystem cannot punch holes.
    async fn make_local(&self, index: usize, base: Option<ContentHash>) -> Result<()> {
        let source = match base {
            Some(hash) => Some(self.cache.open(&hash).await?),
            None if self.data.has_holes() => return Ok(()),
            None => None,
        };
        let data = self.data.clone();
        blocking(move || match source {
            Some(source) => data.copy_in(index, &source),
            None => data.zero(index),
        })
        .await
    }

    /// Before `note_write`: a log rewrite keeps local chunks.
    fn became_local(&self, slot: &mut ChunkSlot, index: usize) {
        slot.local = true;
        self.local_set.lock().unwrap().insert(index);
        self.local_chunks.fetch_add(1, Ordering::SeqCst);
    }

    /// The first write to a chunk that the running checkpoint staged keeps
    /// the staged content, for a repair after a GC.
    async fn hold_if_staged(&self, slot: &mut ChunkSlot, index: usize) -> Result<()> {
        if !slot.staged || slot.held {
            return Ok(());
        }
        let data = self.data.clone();
        let in_memory = (self.held.lock().unwrap().len() as u64 + 1) * self.chunk_size <= HELD_IN_MEMORY;
        if in_memory {
            let content = blocking(move || data.read_chunk(index)).await?;
            self.held.lock().unwrap().insert(index, content);
        } else {
            self.held_on_file.store(true, Ordering::SeqCst);
            blocking(move || data.hold(index)).await?;
        }
        slot.held = true;
        Ok(())
    }

    pub(crate) fn mark_unsaved(&self, slot: &mut ChunkSlot, index: usize) {
        slot.changed = true;
        if slot.verify {
            // Changed now, so it counts toward the limit after all.
            slot.verify = false;
            self.unsaved_chunk_added();
        } else if !slot.unsaved {
            slot.unsaved = true;
            self.unsaved.lock().unwrap().insert(index);
            self.unsaved_chunk_added();
        }
    }

    /// After the data is in its file: the data file is marked for FLUSH
    /// before the chunk is listed, so a FLUSH that logs the chunk syncs it.
    pub(crate) fn finish_write(&self, slot: &mut ChunkSlot, index: usize, bytes: u64) -> Result<()> {
        self.unsynced_segments.lock().unwrap().insert(self.data.segment_of(index));
        self.data.log().note_write(index)?;
        self.writes_done.fetch_add(1, Ordering::SeqCst);
        slot.written = Some(Instant::now());
        if self.written_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes >= WRITEBACK_AFTER {
            self.written_bytes.store(0, Ordering::Relaxed);
            self.writeback_wanted.notify_one();
        }
        Ok(())
    }

    /// Starts writeback of the data files written since the last FLUSH, so
    /// that a FLUSH finds little left to write. It does not wait, and it
    /// makes nothing durable: FLUSH still syncs every data file.
    pub async fn run_writeback(self: Arc<Self>) {
        loop {
            self.writeback_wanted.notified().await;
            if self.is_closed() {
                return;
            }
            let segments: Vec<usize> = self.unsynced_segments.lock().unwrap().iter().copied().collect();
            let data = self.data.clone();
            let started = blocking(move || segments.iter().try_for_each(|&segment| data.start_writeback(segment)));
            if let Err(error) = started.await {
                log::warn!("disk {}: could not start writeback: {error:#}", self.id);
            }
        }
    }

    async fn zero_whole_chunk(&self, index: usize) -> Result<()> {
        let mut slot = self.slot(index).await?.lock_owned().await;
        if !slot.local && slot.base.is_none() {
            return Ok(());
        }
        if !slot.local {
            self.became_local(&mut slot, index);
        }
        self.hold_if_staged(&mut slot, index).await?;
        self.mark_unsaved(&mut slot, index);
        let data = self.data.clone();
        blocking(move || data.zero(index)).await?;
        self.finish_write(&mut slot, index, 0)
    }

    fn unsaved_chunk_added(&self) {
        let count = self.unsaved_chunks.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= self.checkpoint_after_chunks.min(self.local_chunk_limit.load(Ordering::Relaxed) / 2) {
            self.checkpoint_wanted.notify_one();
        }
    }

    /// Saved chunks stay in the data files up to the disk's cache budget, so
    /// a rewrite needs no copy and no download. Over it, or when the host is
    /// short of space, the oldest go to the clean cache. The dirty log drops
    /// them: a crash must not bring them back as local. Their space is freed
    /// after the reads that may still use it end.
    pub(crate) async fn free_saved_chunks(&self) -> Result<()> {
        let mut freed = std::mem::take(&mut *self.unpunched.lock().unwrap());
        while self.saved_over_budget() {
            let Some(index) = self.saved_local.lock().unwrap().pop_front() else { break };
            let mut slot = self.slot(index).await?.lock_owned().await;
            slot.queued_for_eviction = false;
            if !slot.local || slot.unsaved {
                continue;
            }
            if let Some(hash) = slot.base {
                let (data, cache) = (self.data.clone(), self.cache.clone());
                let stored = blocking(move || cache.store(&hash, |file| data.copy_out(index, file))).await;
                if let Err(error) = stored {
                    log::warn!("disk {}: chunk {index} not cached before its local data goes: {error:#}", self.id);
                }
            }
            slot.local = false;
            self.local_set.lock().unwrap().remove(&index);
            self.local_chunks.fetch_sub(1, Ordering::SeqCst);
            freed.push(index);
        }
        if freed.is_empty() {
            return Ok(());
        }
        let (data, local) = (self.data.clone(), self.local_set.clone());
        if let Err(error) = blocking(move || data.log().rewrite(|index| local.lock().unwrap().contains(&index))).await {
            self.fail_sync(&error);
            return Err(error);
        }
        self.wait_for_reads().await;
        for index in freed {
            let slot = self.slot(index).await?.lock_owned().await;
            if slot.local {
                continue;
            }
            if !self.data.log().is_unlisted(index) {
                self.unpunched.lock().unwrap().push(index);
                continue;
            }
            let data = self.data.clone();
            blocking(move || data.zero(index)).await?;
            drop(slot);
        }
        Ok(())
    }

    fn saved_over_budget(&self) -> bool {
        let local = self.local_chunks.load(Ordering::SeqCst);
        let saved = local.saturating_sub(self.unsaved_chunks.load(Ordering::SeqCst)) as u64;
        saved * self.chunk_size > self.cache.limit_bytes() || (saved > 0 && self.host_pressure.load(Ordering::Acquire))
    }

    /// Copies the newest saved local chunks into the cache, up to its budget,
    /// before a detach deletes the data files. Older ones would only push
    /// these out. A reflink on btrfs and XFS. A chunk that fails to copy is
    /// only read from S3 later.
    pub(crate) async fn cache_local_chunks(&self) {
        let budget = (self.cache.limit_bytes() / self.chunk_size) as usize;
        let saved: Vec<usize> = self.saved_local.lock().unwrap().iter().copied().collect();
        for index in saved.iter().skip(saved.len().saturating_sub(budget)).copied() {
            let Ok(slot) = self.slot(index).await else { continue };
            let slot = slot.lock_owned().await;
            let (true, Some(hash), false) = (slot.local, slot.base, slot.unsaved) else { continue };
            let (data, cache) = (self.data.clone(), self.cache.clone());
            if let Err(error) = blocking(move || cache.store(&hash, |file| data.copy_out(index, file))).await {
                log::warn!("disk {}: chunk {index} not cached: {error:#}", self.id);
            }
        }
    }

    /// Writes wait for a checkpoint at the dirty limit, and, with
    /// `wait_when_behind`, while unsaved data is older than the limit. That
    /// makes the data-loss bound hold on a slow link. The guest sees a slow
    /// disk, not an I/O error.
    async fn wait_for_room(&self) -> Result<()> {
        loop {
            let freed = self.space_freed.notified();
            self.ensure_usable()?;
            if self.has_room() {
                return Ok(());
            }
            self.checkpoint_wanted.notify_one();
            freed.await;
        }
    }

    /// Unsaved data is high, or old: a background checkpoint must take all
    /// chunks, also the ones that were just written.
    pub(crate) fn under_pressure(&self) -> bool {
        self.unsaved_chunks.load(Ordering::SeqCst) >= self.local_chunk_limit.load(Ordering::Relaxed) / 2 || self.is_behind()
    }

    pub(crate) fn has_room(&self) -> bool {
        let full = self.unsaved_chunks.load(Ordering::SeqCst) >= self.local_chunk_limit.load(Ordering::Relaxed);
        let too_old = self.wait_when_behind && self.is_behind();
        !full && !too_old && !self.host_pressure.load(Ordering::Acquire)
    }

    pub(crate) fn note_unsaved_write(&self) {
        self.oldest_unsaved_write.lock().unwrap().get_or_insert_with(Instant::now);
    }

    pub(crate) fn split(&self, offset: u64, len: usize) -> Result<Vec<ChunkPart>> {
        let end = offset + len as u64;
        if end > self.size() {
            bail!("I/O at {offset}+{len} is past the end of the disk ({})", self.size());
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

#[cfg(test)]
#[path = "disk_tests.rs"]
mod tests;
