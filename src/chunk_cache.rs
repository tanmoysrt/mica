use crate::blocking::blocking;
use crate::bucket::{Bucket, keys};
use crate::codec;
use crate::manifest::MAX_CHUNK_SIZE;
use crate::content_hash::ContentHash;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

#[derive(Debug, Default, serde::Serialize)]
pub struct CacheUsage {
    pub chunks: usize,
    pub bytes: u64,
    pub limit_bytes: u64,
    pub in_use_chunks: usize,
    pub in_use_bytes: u64,
}

/// Clean chunks on the local SSD, shared by all disks of the node.
/// Every chunk here is also in S3, so any of them can be evicted.
pub struct ChunkCache {
    root: PathBuf,
    limit_bytes: u64,
    bucket: Arc<Bucket>,
    index: Mutex<CacheIndex>,
    downloads: Mutex<HashMap<ContentHash, Arc<tokio::sync::Mutex<()>>>>,
    /// Open handles of hot chunks, so a read does not open the file again.
    open_files: Mutex<HashMap<ContentHash, Arc<File>>>,
    /// Downloads in flight on the node, for reads and fetches together.
    download_slots: Semaphore,
    /// Fetches for a likely read: read-ahead and the startup profile.
    fetch_slots: Semaphore,
    /// Fetches for the background fill.
    fill_slots: Semaphore,
    /// Reads that wait for a download slot. While there are any, no new fetch starts.
    reads_waiting: AtomicUsize,
    reads_done: Notify,
    /// Downloads in flight for reads and likely reads. The fill starts new
    /// downloads only while this is zero, so it never slows a reader.
    busy: AtomicUsize,
    idle: Notify,
    /// Downloads in flight for reads. Past `READS_FOR_LIKELY`, the reads fill
    /// the link, and a likely fetch would only take bandwidth from them.
    reads_active: AtomicUsize,
    /// When the last read download started, in ms since `started`.
    last_read_ms: AtomicU64,
    started: Instant,
}

/// How much a download is worth to the guest.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// A read will likely need it soon: read-ahead, the startup profile.
    Likely,
    /// Only to make the whole disk local: the background fill.
    Fill,
}

/// More open cache files than this, and one handle is closed for each new one.
const OPEN_FILE_LIMIT: usize = 1024;
const DOWNLOADS: usize = 64;
const FETCHES: usize = 16;
/// Fill downloads on the node. Each disk also has a limit in bytes, see `prefetch.rs`.
const FILLS: usize = 32;
const READS_FOR_LIKELY: usize = 8;
/// The fill starts only after this long without a new read download.
const FILL_QUIET: Duration = Duration::from_millis(250);
const FETCH_POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct CacheIndex {
    entries: HashMap<ContentHash, CacheEntry>,
    total_bytes: u64,
    clock: u64,
}

struct CacheEntry {
    bytes: u64,
    last_used: u64,
    /// False for files found at start: a download is not synced, so a crash
    /// can leave a torn file. Its hash is checked before the first read.
    verified: bool,
}

impl ChunkCache {
    pub fn load(root: PathBuf, limit_bytes: u64, bucket: Arc<Bucket>) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let cache = Self {
            root,
            limit_bytes,
            bucket,
            index: Mutex::default(),
            downloads: Mutex::default(),
            open_files: Mutex::default(),
            download_slots: Semaphore::new(DOWNLOADS),
            fetch_slots: Semaphore::new(FETCHES),
            fill_slots: Semaphore::new(FILLS),
            reads_waiting: AtomicUsize::new(0),
            reads_done: Notify::new(),
            busy: AtomicUsize::new(0),
            idle: Notify::new(),
            reads_active: AtomicUsize::new(0),
            last_read_ms: AtomicU64::new(0),
            started: Instant::now(),
        };
        cache.scan()?;
        Ok(cache)
    }

    /// Opens a chunk, and gets it from S3 first if it is not here.
    pub async fn open(&self, hash: &ContentHash) -> Result<Arc<File>> {
        if let Some(file) = self.open_local(hash) {
            return Ok(file);
        }
        // One download per chunk. Other callers wait here, then find the file.
        let lock = self.download_lock(hash);
        let _guard = lock.lock().await;
        if let Some(file) = self.open_local(hash) {
            return Ok(file);
        }
        if self.verify_found_file(hash).await? {
            self.downloads.lock().unwrap().remove(hash);
            return self.open_local(hash).ok_or_else(|| anyhow!("chunk {hash} vanished from the cache"));
        }
        let waiting = Waiting::new(self);
        let slot = self.download_slots.acquire().await;
        drop(waiting);
        let _slot = slot?;
        let _busy = self.busy();
        let _reading = Reading::new(self);
        let result = self.download(hash).await;
        self.downloads.lock().unwrap().remove(hash);
        result?;
        self.open_local(hash).ok_or_else(|| anyhow!("chunk {hash} vanished from the cache"))
    }

    fn since_last_read(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_read_ms.load(Ordering::SeqCst)))
    }

    fn busy(&self) -> Busy<'_> {
        self.busy.fetch_add(1, Ordering::SeqCst);
        Busy(self)
    }

    /// Gets a chunk into the cache for later, behind every read. A likely
    /// fetch starts only while no read waits for a download slot. A fill
    /// starts only while no read and no likely fetch is downloading. The
    /// slots are taken before the chunk's download lock, so a read of the
    /// same chunk never waits behind a fetch that waits.
    pub async fn fetch(&self, hash: &ContentHash, kind: Fetch) -> Result<()> {
        if self.has(hash) {
            return Ok(());
        }
        let _fetch = match kind {
            Fetch::Likely => self.fetch_slots.acquire().await?,
            Fetch::Fill => self.fill_slots.acquire().await?,
        };
        loop {
            let (done, idle) = (self.reads_done.notified(), self.idle.notified());
            match kind {
                Fetch::Likely if self.reads_waiting.load(Ordering::SeqCst) > 0 => done.await,
                Fetch::Likely if self.reads_active.load(Ordering::SeqCst) >= READS_FOR_LIKELY => {
                    tokio::time::sleep(FETCH_POLL).await
                }
                Fetch::Fill if self.busy.load(Ordering::SeqCst) > 0 => idle.await,
                Fetch::Fill if self.since_last_read() < FILL_QUIET => tokio::time::sleep(FETCH_POLL).await,
                _ => break,
            }
        }
        let _slot = self.download_slots.acquire().await?;
        let _busy = (kind == Fetch::Likely).then(|| self.busy());
        let lock = self.download_lock(hash);
        let _guard = lock.lock().await;
        if self.has(hash) {
            return Ok(());
        }
        let result = self.download(hash).await;
        self.downloads.lock().unwrap().remove(hash);
        result
    }

    /// True when new fetches would push old chunks out of the cache.
    pub fn nearly_full(&self) -> bool {
        self.index.lock().unwrap().total_bytes >= self.limit_bytes / 10 * 9
    }

    /// How full the cache is, and how much of it `in_use` covers.
    pub fn usage(&self, in_use: &HashSet<ContentHash>) -> CacheUsage {
        let index = self.index.lock().unwrap();
        let mut usage = CacheUsage { limit_bytes: self.limit_bytes, ..CacheUsage::default() };
        for (hash, entry) in &index.entries {
            usage.chunks += 1;
            usage.bytes += entry.bytes;
            if in_use.contains(hash) {
                usage.in_use_chunks += 1;
                usage.in_use_bytes += entry.bytes;
            }
        }
        usage
    }

    pub fn has(&self, hash: &ContentHash) -> bool {
        self.index.lock().unwrap().entries.contains_key(hash)
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Opens a chunk only if it is here. It never downloads, so the ublk
    /// queue thread can call it.
    pub fn open_cached(&self, hash: &ContentHash) -> Option<Arc<File>> {
        self.open_local(hash)
    }

    /// Deletes every chunk not in `keep`. S3 has all of them, so a chunk
    /// that is needed again is downloaded again. Returns the count and bytes.
    pub fn prune(&self, keep: &HashSet<ContentHash>) -> (usize, u64) {
        let mut index = self.index.lock().unwrap();
        let unused: Vec<ContentHash> = index.entries.keys().filter(|hash| !keep.contains(*hash)).copied().collect();
        let (mut count, mut bytes) = (0, 0);
        for hash in unused {
            let entry = index.entries.remove(&hash).unwrap();
            index.total_bytes -= entry.bytes;
            self.delete_file(&hash);
            count += 1;
            bytes += entry.bytes;
        }
        (count, bytes)
    }

    /// Adds a chunk that S3 has, which `write` puts into a new file. Not
    /// synced: after a crash, the scan at start checks it again.
    pub fn store(&self, hash: &ContentHash, write: impl FnOnce(&File) -> Result<()>) -> Result<()> {
        if self.has(hash) {
            return Ok(());
        }
        let target = self.path(hash);
        std::fs::create_dir_all(target.parent().unwrap())?;
        let temp = temp_path(&target);
        let file = File::create(&temp)?;
        write(&file)?;
        let bytes = file.metadata()?.len();
        std::fs::rename(&temp, &target)?;
        self.insert(*hash, bytes, true);
        Ok(())
    }

    /// Checks the hash of a file that the cache found at start. A torn or
    /// corrupt file is deleted, so the caller downloads the chunk again.
    async fn verify_found_file(&self, hash: &ContentHash) -> Result<bool> {
        let found = matches!(self.index.lock().unwrap().entries.get(hash), Some(entry) if !entry.verified);
        if !found {
            return Ok(false);
        }
        let (path, expected) = (self.path(hash), *hash);
        let good = blocking(move || Ok(std::fs::read(&path).map(|data| expected.verify(&data).is_ok()).unwrap_or(false))).await?;
        let mut index = self.index.lock().unwrap();
        if good {
            if let Some(entry) = index.entries.get_mut(hash) {
                entry.verified = true;
            }
        } else if let Some(entry) = index.entries.remove(hash) {
            log::warn!("cache: chunk {hash} was torn or corrupt; it is downloaded again");
            index.total_bytes -= entry.bytes;
            self.delete_file(hash);
        }
        Ok(good)
    }

    async fn download(&self, hash: &ContentHash) -> Result<()> {
        let packed = self
            .bucket
            .get(&keys::chunk(hash))
            .await?
            .ok_or_else(|| anyhow!("chunk {hash} is missing in S3"))?;
        let expected = *hash;
        let data = blocking(move || {
            let data = codec::unpack(&packed, MAX_CHUNK_SIZE as usize)?;
            expected.verify(&data)?;
            Ok(data)
        })
        .await?;
        let target = self.path(hash);
        let bytes = data.len() as u64;
        blocking(move || {
            std::fs::create_dir_all(target.parent().unwrap())?;
            // No sync: a read must not wait for the SSD. S3 has the chunk,
            // and after a crash the file is checked before its first read.
            let temp = temp_path(&target);
            let mut file = File::create(&temp)?;
            file.write_all(&data)?;
            std::fs::rename(&temp, &target)?;
            Ok(())
        })
        .await?;
        self.insert(*hash, bytes, true);
        Ok(())
    }

    /// Opens a cached chunk whose content is checked. Never waits for I/O
    /// other than the open.
    fn open_local(&self, hash: &ContentHash) -> Option<Arc<File>> {
        if !self.index.lock().unwrap().entries.get(hash).is_some_and(|entry| entry.verified) {
            return None;
        }
        let known = self.open_files.lock().unwrap().get(hash).cloned();
        let file = match known {
            Some(file) => file,
            None => {
                let file = Arc::new(File::open(self.path(hash)).ok()?);
                let mut open_files = self.open_files.lock().unwrap();
                if open_files.len() >= OPEN_FILE_LIMIT
                    && let Some(old) = open_files.keys().next().copied()
                {
                    open_files.remove(&old);
                }
                open_files.insert(*hash, file.clone());
                file
            }
        };
        let mut index = self.index.lock().unwrap();
        index.clock += 1;
        let now = index.clock;
        if let Some(entry) = index.entries.get_mut(hash) {
            entry.last_used = now;
        }
        Some(file)
    }

    fn download_lock(&self, hash: &ContentHash) -> Arc<tokio::sync::Mutex<()>> {
        self.downloads.lock().unwrap().entry(*hash).or_default().clone()
    }

    fn insert(&self, hash: ContentHash, bytes: u64, verified: bool) {
        let mut index = self.index.lock().unwrap();
        index.clock += 1;
        let last_used = index.clock;
        if let Some(old) = index.entries.insert(hash, CacheEntry { bytes, last_used, verified }) {
            index.total_bytes -= old.bytes;
        }
        index.total_bytes += bytes;
        if index.total_bytes > self.limit_bytes {
            self.evict(&mut index);
        }
    }

    /// Deletes the least recently used chunks until the cache is at 90% of its limit.
    /// Open files stay readable after the delete, so readers are safe.
    fn evict(&self, index: &mut CacheIndex) {
        let target = self.limit_bytes / 10 * 9;
        let mut by_age: Vec<(u64, ContentHash)> =
            index.entries.iter().map(|(hash, entry)| (entry.last_used, *hash)).collect();
        by_age.sort_unstable();
        for (_, hash) in by_age {
            if index.total_bytes <= target {
                break;
            }
            let entry = index.entries.remove(&hash).unwrap();
            index.total_bytes -= entry.bytes;
            self.delete_file(&hash);
        }
    }

    /// Closes our handle too: an open handle keeps a deleted file's space in
    /// use. A reader that holds its own handle still reads it safely.
    fn delete_file(&self, hash: &ContentHash) {
        self.open_files.lock().unwrap().remove(hash);
        if let Err(error) = std::fs::remove_file(self.path(hash)) {
            log::warn!("cache: cannot delete chunk {hash}: {error}");
        }
    }

    fn scan(&self) -> Result<()> {
        for folder in std::fs::read_dir(&self.root)? {
            for file in std::fs::read_dir(folder?.path())? {
                let path = file?.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                match ContentHash::from_hex(&name) {
                    Ok(hash) => self.insert(hash, std::fs::metadata(&path)?.len(), false),
                    Err(_) => std::fs::remove_file(&path)?,
                }
            }
        }
        Ok(())
    }

    fn path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(hex)
    }
}

/// Counts a download in `busy` while it lives.
struct Busy<'a>(&'a ChunkCache);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        if self.0.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Counts a read in `reads_waiting` while it waits for a slot, also when
/// the read is dropped while it waits.
struct Waiting<'a>(&'a ChunkCache);

impl<'a> Waiting<'a> {
    fn new(cache: &'a ChunkCache) -> Self {
        cache.reads_waiting.fetch_add(1, Ordering::SeqCst);
        Self(cache)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.0.reads_waiting.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.reads_done.notify_waiters();
        }
    }
}

/// Counts a read download in `reads_active` while it lives, and notes its start.
struct Reading<'a>(&'a ChunkCache);

impl<'a> Reading<'a> {
    fn new(cache: &'a ChunkCache) -> Self {
        cache.reads_active.fetch_add(1, Ordering::SeqCst);
        cache.last_read_ms.store(cache.started.elapsed().as_millis() as u64, Ordering::SeqCst);
        Self(cache)
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.0.reads_active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A temporary name of its own for each writer of a cache file, so two
/// writers of the same chunk never share a file. The scan at start deletes it.
fn temp_path(target: &std::path::Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    target.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::SeqCst)))
}
