//! The disk engine: one attached disk, independent of the kernel interface.
//!
//! The guest writes go to the local disk of the node and never wait for the
//! object store. A transaction uploads the changes later, in the background.
//! A read takes the local data, else the cache, else it downloads the chunk.
//!
//! The words in this module:
//!
//! - A local chunk has its current content in the data files.
//! - An unsaved chunk is a local chunk whose content is maybe not in the store.
//! - A saved chunk is a local chunk whose content is in the store. Saved
//!   chunks stay local for a short time, because the guest often writes
//!   them again. Then they move into the cache.
//!
//! The code is in four files: `io.rs` (read, write, flush), `fast.rs` (the
//! requests that a queue thread serves alone), `transaction.rs` and
//! `prefetch.rs`.

mod fast;
mod io;
pub mod prefetch;
mod slots;
mod transaction;
#[cfg(test)]
mod tests;

pub use fast::{LocalRead, LocalWrite, ReadSource};
pub use transaction::Kind as TransactionKind;

use crate::local::{ChunkCache, DataFiles, DirtyLog, DiskFolder};
use crate::owner::Claim;
use crate::store::{DiskStore, Limits, Root};
use crate::transfers::Transfers;
use anyhow::{Result, bail};
use prefetch::{ReadAhead, ReadProfile};
use serde::Serialize;
use slots::{ChunkMap, Slot};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Settings of the node that apply to each disk.
#[derive(Clone, Debug)]
pub struct Tuning {
    pub transaction_interval: Duration,
    /// A transaction starts early when this much unsaved data waits.
    pub transaction_after: u64,
    /// A disk is behind when its oldest unsaved write is older than this.
    pub max_unsaved_age: Duration,
    /// Saved chunks over this size leave the data files for the cache.
    pub keep_saved_local: u64,
}

pub struct Disk {
    pub name: String,
    pub size: u64,
    pub chunk_size: u64,
    pub store: DiskStore,
    pub cache: ChunkCache,
    transfers: Arc<Transfers>,
    data: Arc<DataFiles>,
    log: Arc<DirtyLog>,
    slots: ChunkMap,
    /// The head lock of the disk. Held by each transaction and each change
    /// of the head, so they run one at a time.
    claim: tokio::sync::Mutex<Claim>,
    transaction_state: tokio::sync::Mutex<transaction::Memory>,
    /// The current version. Changed only by a transaction.
    root: Mutex<Root>,
    version: AtomicU64,
    saved_profile: Mutex<Vec<u32>>,
    pub(crate) profile: ReadProfile,
    read_ahead: Mutex<ReadAhead>,
    ahead: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Range<usize>>>>,

    unsaved: Mutex<BTreeSet<usize>>,
    /// All local chunks, saved or not: what the dirty log lists.
    local: Arc<Mutex<BTreeSet<usize>>>,
    /// Saved local chunks, oldest first.
    saved: Mutex<VecDeque<usize>>,
    /// Chunks that left the data files, whose space a flush kept listed.
    unpunched: Mutex<Vec<usize>>,
    downloads: Mutex<HashMap<crate::store::Hash, Arc<tokio::sync::Mutex<()>>>>,

    /// Data files written since the last flush.
    /// Writes went into the data file since the last sync.
    unsynced: AtomicBool,
    /// Writes done, and how many of them a flush made durable.
    writes_done: AtomicU64,
    writes_synced: AtomicU64,
    flushing: tokio::sync::Mutex<()>,
    written_bytes: AtomicU64,
    writeback_wanted: Notify,

    limits: Mutex<Limits>,
    tuning: Tuning,
    transaction_wanted: Notify,
    room: Notify,
    oldest_unsaved: Mutex<Option<Instant>>,
    /// Why the disk stopped: another node took it, or a local sync failed.
    /// A stopped disk fails all I/O and keeps its local data.
    stopped: Mutex<Option<String>>,
    closed: AtomicBool,
    pub(crate) stats: IoStats,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub size: u64,
    pub chunk_size: u64,
    pub transaction: u64,
    pub unsaved_bytes: u64,
    pub local_bytes: u64,
    pub cache_bytes: u64,
    pub limits: Limits,
    pub behind: bool,
    pub stopped: Option<String>,
    /// Requests and bytes to the bucket since the attach: they set the bill.
    pub requests: serde_json::Value,
    pub io: serde_json::Value,
}

/// Where the time of writes and flushes goes, for `mica status --json`.
#[derive(Debug, Default, Serialize)]
pub struct IoStats {
    /// Chunks made local for a write that covers only part of the chunk.
    pub chunk_copies: AtomicU64,
    pub chunk_copy_micros: AtomicU64,
    pub flushes: AtomicU64,
    pub data_sync_micros: AtomicU64,
    pub log_append_micros: AtomicU64,
}

impl IoStats {
    pub fn add(counter: &AtomicU64, started: std::time::Instant) {
        counter.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// The part of a request that falls into one chunk.
#[derive(Clone, Debug)]
pub struct Part {
    pub index: usize,
    /// The offset inside the chunk.
    pub offset: u64,
    /// The range inside the request.
    pub buffer: Range<usize>,
}

impl Disk {
    /// Opens a claimed disk with its local data. Chunks that the dirty log
    /// lists are local, and count as unsaved: the next transaction compares
    /// each one with its entry, and uploads only those that differ.
    pub async fn open(
        claim: Claim,
        folder: &DiskFolder,
        cache: ChunkCache,
        transfers: Arc<Transfers>,
        tuning: Tuning,
    ) -> Result<Arc<Self>> {
        let head = claim.head().clone();
        let store = claim.store.clone();
        let root = store.current_root(&head).await?;
        let chunks = root.chunk_count();
        let (path, recent) = (folder.path.clone(), folder.recent_path());
        let (size, chunk_size) = (root.disk_size, root.chunk_size);
        let (log, data, listed) = tokio::task::spawn_blocking(move || {
            let (log, listed) = DirtyLog::open(&path, &recent, chunks)?;
            let data = DataFiles::open(&path, size, chunk_size, &listed)?;
            Ok::<_, anyhow::Error>((log, data, listed))
        })
        .await??;
        let disk = Arc::new(Self {
            name: store.disk.clone(),
            size,
            chunk_size,
            store,
            cache,
            transfers,
            data: Arc::new(data),
            log: Arc::new(log),
            slots: ChunkMap::new(chunks),
            claim: tokio::sync::Mutex::new(claim),
            transaction_state: Default::default(),
            version: AtomicU64::new(head.transaction),
            root: Mutex::new(root),
            saved_profile: Mutex::new(head.profile.clone()),
            profile: ReadProfile::new(chunk_size),
            read_ahead: Mutex::default(),
            ahead: Mutex::new(None),
            unsaved: Mutex::default(),
            local: Arc::default(),
            saved: Mutex::default(),
            unpunched: Mutex::default(),
            downloads: Mutex::default(),
            // The first flush syncs what a crashed daemon left.
            unsynced: AtomicBool::new(true),
            // Above zero: the first flush syncs what a crashed daemon left.
            writes_done: AtomicU64::new(1),
            writes_synced: AtomicU64::new(0),
            flushing: Default::default(),
            written_bytes: AtomicU64::new(0),
            writeback_wanted: Notify::new(),
            limits: Mutex::new(head.limits),
            tuning,
            transaction_wanted: Notify::new(),
            room: Notify::new(),
            oldest_unsaved: Mutex::new(None),
            stopped: Mutex::new(None),
            closed: AtomicBool::new(false),
            stats: IoStats::default(),
        });
        for index in listed {
            let mut slot = disk.slot(index).await?.write_owned().await;
            slot.local = true;
            disk.local.lock().unwrap().insert(index);
            disk.mark_unsaved(&mut slot, index);
        }
        Ok(disk)
    }

    pub fn chunk_count(&self) -> usize {
        (self.size / self.chunk_size) as usize
    }

    /// True for a disk that was never written: `mica disk mount` formats it.
    pub fn is_blank(&self) -> bool {
        self.version.load(Ordering::SeqCst) == 0 && self.unsaved.lock().unwrap().is_empty()
    }

    pub fn status(&self) -> Status {
        Status {
            size: self.size,
            chunk_size: self.chunk_size,
            transaction: self.version.load(Ordering::SeqCst),
            unsaved_bytes: self.unsaved.lock().unwrap().len() as u64 * self.chunk_size,
            local_bytes: self.local.lock().unwrap().len() as u64 * self.chunk_size,
            cache_bytes: self.cache.usage().bytes,
            limits: *self.limits.lock().unwrap(),
            behind: self.is_behind(),
            stopped: self.stopped.lock().unwrap().clone(),
            requests: serde_json::to_value(&self.store.bucket.counts).unwrap_or_default(),
            io: serde_json::to_value(&self.stats).unwrap_or_default(),
        }
    }

    /// The claim, for a change of the head. It waits for a running transaction.
    pub async fn claim(&self) -> tokio::sync::MutexGuard<'_, Claim> {
        self.claim.lock().await
    }

    pub fn set_limits(&self, limits: Limits) {
        *self.limits.lock().unwrap() = limits;
        self.cache.set_limit(limits.cache_bytes);
        self.room.notify_waiters();
    }

    /// Stops the background work of the disk. The device is gone already.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.ahead.lock().unwrap().take();
        self.transaction_wanted.notify_one();
        self.writeback_wanted.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Stops the disk for good. All I/O fails, and the local data stays.
    pub fn stop(&self, reason: &str) {
        log::error!("disk {}: stopped, {reason}. The local data is kept", self.name);
        self.stopped.lock().unwrap().get_or_insert_with(|| reason.to_string());
        self.room.notify_waiters();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.lock().unwrap().is_some()
    }

    fn ensure_usable(&self) -> Result<()> {
        match &*self.stopped.lock().unwrap() {
            Some(reason) => bail!("disk {} is stopped: {reason}", self.name),
            None => Ok(()),
        }
    }

    pub fn is_behind(&self) -> bool {
        self.oldest_unsaved.lock().unwrap().is_some_and(|since| since.elapsed() > self.tuning.max_unsaved_age)
    }

    pub fn has_unsaved(&self) -> bool {
        !self.unsaved.lock().unwrap().is_empty()
    }

    /// The slot of a chunk. Loads its page from the current version.
    async fn slot(&self, index: usize) -> Result<Slot> {
        self.slots
            .get(index, |page| async move {
                let entry = self.root.lock().unwrap().pages[page];
                Ok(self.store.page(&entry, page).await?.0)
            })
            .await
    }

    /// Splits a request into its chunk parts.
    fn split(&self, offset: u64, len: usize) -> Result<Vec<Part>> {
        let end = offset + len as u64;
        if end > self.size {
            bail!("I/O at {offset}+{len} is past the end of the disk ({} bytes)", self.size);
        }
        let mut parts = Vec::new();
        let mut position = offset;
        while position < end {
            let index = (position / self.chunk_size) as usize;
            let in_chunk = position % self.chunk_size;
            let part_len = (self.chunk_size - in_chunk).min(end - position);
            let start = (position - offset) as usize;
            parts.push(Part { index, offset: in_chunk, buffer: start..start + part_len as usize });
            position += part_len;
        }
        Ok(parts)
    }

    fn unsaved_limit(&self) -> usize {
        (self.limits.lock().unwrap().unsaved_bytes / self.chunk_size).max(2) as usize
    }

    /// Unsaved data is high or old: a background transaction takes all chunks.
    fn under_pressure(&self) -> bool {
        self.unsaved.lock().unwrap().len() >= self.unsaved_limit() / 2 || self.is_behind()
    }

    fn has_room(&self) -> bool {
        self.unsaved.lock().unwrap().len() < self.unsaved_limit()
    }

    /// Writes wait at the unsaved limit until a transaction makes room. The
    /// guest sees a slow disk, not an error.
    async fn wait_for_room(&self) -> Result<()> {
        loop {
            let room = self.room.notified();
            self.ensure_usable()?;
            if self.has_room() {
                return Ok(());
            }
            self.transaction_wanted.notify_one();
            room.await;
        }
    }
}
