use crate::blocking::blocking;
use crate::bucket::{Bucket, keys};
use crate::codec;
use crate::manifest::MAX_CHUNK_SIZE;
use crate::content_hash::ContentHash;
use anyhow::{Result, anyhow};
use std::collections::{BTreeSet, HashMap, HashSet};
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

/// Clean chunks on the local SSD, with an independent budget per disk.
/// Every chunk here is also in S3, so any of them can be evicted.
pub struct ChunkCache {
    root: PathBuf,
    limit_bytes: AtomicU64,
    bucket: Arc<Bucket>,
    index: Mutex<CacheIndex>,
    /// Serializes publication and deletion, without blocking cached reads.
    files: Mutex<()>,
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
const EVICTION_BATCH: usize = 64;
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
    by_age: BTreeSet<(u64, ContentHash)>,
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
            limit_bytes: AtomicU64::new(limit_bytes),
            bucket,
            index: Mutex::default(),
            files: Mutex::default(),
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
    pub async fn open(self: &Arc<Self>, hash: &ContentHash) -> Result<Arc<File>> {
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
        result
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
    pub async fn fetch(self: &Arc<Self>, hash: &ContentHash, kind: Fetch) -> Result<()> {
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
        let _background = self.bucket.background_slot().await?;
        let _slot = self.download_slots.acquire().await?;
        let _busy = (kind == Fetch::Likely).then(|| self.busy());
        let lock = self.download_lock(hash);
        let _guard = lock.lock().await;
        if self.has(hash) {
            return Ok(());
        }
        let result = self.download(hash).await;
        self.downloads.lock().unwrap().remove(hash);
        result.map(|_| ())
    }

    /// True when new fetches would push old chunks out of the cache.
    pub fn nearly_full(&self) -> bool {
        self.index.lock().unwrap().total_bytes >= self.limit_bytes() / 10 * 9
    }

    /// How full the cache is, and how much of it `in_use` covers.
    pub fn usage(&self, in_use: &HashSet<ContentHash>) -> CacheUsage {
        let index = self.index.lock().unwrap();
        let mut usage = CacheUsage { limit_bytes: self.limit_bytes(), ..CacheUsage::default() };
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

    pub fn set_limit(&self, bytes: u64) {
        self.limit_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> u64 {
        self.index.lock().unwrap().total_bytes
    }

    pub fn trim_to(&self, target: u64) {
        let _files = self.files.lock().unwrap();
        self.evict_to(target);
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes.load(Ordering::Relaxed)
    }

    /// Opens a chunk only if it is here. It never downloads, so the ublk
    /// queue thread can call it.
    pub fn open_cached(&self, hash: &ContentHash) -> Option<Arc<File>> {
        self.open_local(hash)
    }

    /// Deletes every chunk not in `keep`. S3 has all of them, so a chunk
    /// that is needed again is downloaded again. Returns the count and bytes.
    pub fn prune(&self, keep: &HashSet<ContentHash>) -> (usize, u64) {
        let _files = self.files.lock().unwrap();
        let unused: Vec<ContentHash> = self.index.lock().unwrap().entries.keys()
            .filter(|hash| !keep.contains(*hash))
            .copied()
            .collect();
        let mut bytes = 0;
        for batch in unused.chunks(EVICTION_BATCH) {
            {
                let mut index = self.index.lock().unwrap();
                for hash in batch {
                    bytes += index.remove(hash).unwrap().bytes;
                }
            }
            for hash in batch {
                self.delete_file(hash);
            }
        }
        (unused.len(), bytes)
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
        let _files = self.files.lock().unwrap();
        std::fs::rename(&temp, &target)?;
        self.insert(*hash, bytes, true);
        Ok(())
    }

    /// Checks the hash of a file that the cache found at start. A torn or
    /// corrupt file is deleted, so the caller downloads the chunk again.
    async fn verify_found_file(self: &Arc<Self>, hash: &ContentHash) -> Result<bool> {
        if !matches!(self.index.lock().unwrap().entries.get(hash), Some(entry) if !entry.verified) {
            return Ok(false);
        }
        let cache = self.clone();
        let hash = *hash;
        blocking(move || {
            let _files = cache.files.lock().unwrap();
            let found = matches!(cache.index.lock().unwrap().entries.get(&hash), Some(entry) if !entry.verified);
            if !found {
                return Ok(false);
            }
            let good = std::fs::read(cache.path(&hash)).map(|data| hash.verify(&data).is_ok()).unwrap_or(false);
            let mut index = cache.index.lock().unwrap();
            if good {
                index.entries.get_mut(&hash).unwrap().verified = true;
            } else {
                index.remove(&hash);
                drop(index);
                log::warn!("cache: chunk {hash} was torn or corrupt; it is downloaded again");
                cache.delete_file(&hash);
            }
            Ok(good)
        })
        .await
    }

    async fn download(self: &Arc<Self>, hash: &ContentHash) -> Result<Arc<File>> {
        let packed = {
            let _node_slot = self.bucket.download_slot().await?;
            self.bucket.get(&keys::chunk(hash)).await?
        }
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
        let cache = self.clone();
        let hash = *hash;
        blocking(move || {
            std::fs::create_dir_all(target.parent().unwrap())?;
            // No sync: a read must not wait for the SSD. S3 has the chunk,
            // and after a crash the file is checked before its first read.
            let temp = temp_path(&target);
            let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&temp)?;
            file.write_all(&data)?;
            let _files = cache.files.lock().unwrap();
            std::fs::rename(&temp, &target)?;
            cache.insert(hash, bytes, true);
            // Keep the inode readable even if another download evicts it now.
            Ok(Arc::new(file))
        })
        .await
    }

    /// Opens a cached chunk whose content is checked. Never waits for I/O
    /// other than the open.
    fn open_local(&self, hash: &ContentHash) -> Option<Arc<File>> {
        let mut index = self.index.lock().unwrap();
        if !index.entries.get(hash).is_some_and(|entry| entry.verified) {
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
        let last_used = index.entries[hash].last_used;
        index.by_age.remove(&(last_used, *hash));
        index.clock += 1;
        let now = index.clock;
        index.entries.get_mut(hash).unwrap().last_used = now;
        index.by_age.insert((now, *hash));
        Some(file)
    }

    fn download_lock(&self, hash: &ContentHash) -> Arc<tokio::sync::Mutex<()>> {
        self.downloads.lock().unwrap().entry(*hash).or_default().clone()
    }

    // The caller holds `files`, or is scanning before the cache is shared.
    fn insert(&self, hash: ContentHash, bytes: u64, verified: bool) {
        let over_limit = {
            let mut index = self.index.lock().unwrap();
            index.remove(&hash);
            index.clock += 1;
            let last_used = index.clock;
            index.entries.insert(hash, CacheEntry { bytes, last_used, verified });
            index.by_age.insert((last_used, hash));
            index.total_bytes += bytes;
            index.total_bytes > self.limit_bytes()
        };
        if over_limit {
            self.evict_to((self.limit_bytes() / 10 * 9).max(bytes.min(self.limit_bytes())));
        }
    }

    /// Deletes oldest chunks in batches, releasing the read-path lock before I/O.
    fn evict_to(&self, target: u64) {
        loop {
            let mut removed = Vec::with_capacity(EVICTION_BATCH);
            {
                let mut index = self.index.lock().unwrap();
                while index.total_bytes > target && removed.len() < EVICTION_BATCH {
                    let Some(&(_, hash)) = index.by_age.first() else { break };
                    index.remove(&hash);
                    removed.push(hash);
                }
            }
            if removed.is_empty() {
                return;
            }
            for hash in removed {
                self.delete_file(&hash);
            }
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

impl CacheIndex {
    fn remove(&mut self, hash: &ContentHash) -> Option<CacheEntry> {
        let entry = self.entries.remove(hash)?;
        self.by_age.remove(&(entry.last_used, *hash));
        self.total_bytes -= entry.bytes;
        Some(entry)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[test]
    fn eviction_preserves_recent_chunks_and_open_readers() {
        let root = tempfile::tempdir().unwrap();
        let cache = ChunkCache::load(root.path().into(), 30, Arc::new(Bucket::in_memory())).unwrap();
        let hashes: Vec<_> = (0..4).map(|value| ContentHash::of(&[value; 10])).collect();
        for (value, hash) in hashes.iter().take(3).enumerate() {
            cache.store(hash, |file| Ok(file.write_all_at(&[value as u8; 10], 0)?)).unwrap();
        }
        let old_reader = cache.open_cached(&hashes[0]).unwrap();
        cache.open_cached(&hashes[1]).unwrap();
        cache.open_cached(&hashes[2]).unwrap();
        cache.store(&hashes[3], |file| Ok(file.write_all_at(&[3; 10], 0)?)).unwrap();
        assert!(!cache.has(&hashes[0]));
        assert!(!cache.has(&hashes[1]));
        assert!(cache.has(&hashes[2]));
        assert!(cache.has(&hashes[3]));
        let mut data = [9; 10];
        old_reader.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(data, [0; 10]);
        cache.store(&hashes[0], |file| Ok(file.write_all_at(&[0; 10], 0)?)).unwrap();
        assert!(cache.open_cached(&hashes[0]).is_some());
        let index = cache.index.lock().unwrap();
        assert_eq!(index.entries.len(), index.by_age.len());
        assert_eq!(index.total_bytes, 30);
    }

    #[test]
    fn eviction_releases_index_before_deleting_files() {
        let root = tempfile::tempdir().unwrap();
        let cache = ChunkCache::load(root.path().into(), 10, Arc::new(Bucket::in_memory())).unwrap();
        let hash = ContentHash::of(&[1; 10]);
        cache.store(&hash, |file| Ok(file.write_all_at(&[1; 10], 0)?)).unwrap();
        let (started, writing) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Stall delete_file after the victim has been removed from the index.
            let handles = cache.open_files.lock().unwrap();
            let worker = scope.spawn(|| {
                let _files = cache.files.lock().unwrap();
                cache.evict_to(0);
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut released = false;
            while Instant::now() < deadline {
                if let Ok(index) = cache.index.try_lock()
                    && index.entries.is_empty()
                {
                    released = true;
                    break;
                }
                std::thread::yield_now();
            }
            let replacement = scope.spawn(|| {
                cache.store(&hash, |file| {
                    file.write_all_at(&[1; 10], 0)?;
                    started.send(()).unwrap();
                    Ok(())
                }).unwrap();
            });
            // Publication must wait until deletion of the old file has finished.
            if released {
                writing.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            drop(handles);
            worker.join().unwrap();
            replacement.join().unwrap();
            assert!(released, "eviction held the index while waiting to delete files");
        });
        assert_eq!(std::fs::read(cache.path(&hash)).unwrap(), [1; 10]);
        assert!(cache.open_cached(&hash).is_some());
    }
}
