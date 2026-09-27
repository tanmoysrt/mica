use crate::blocking::blocking;
use crate::bucket::{Bucket, keys};
use crate::content_hash::ContentHash;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
}

/// More open cache files than this, and one handle is closed for each new one.
const OPEN_FILE_LIMIT: usize = 1024;

#[derive(Default)]
struct CacheIndex {
    entries: HashMap<ContentHash, CacheEntry>,
    total_bytes: u64,
    clock: u64,
}

struct CacheEntry {
    bytes: u64,
    last_used: u64,
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
        let result = self.download(hash).await;
        self.downloads.lock().unwrap().remove(hash);
        result?;
        self.open_local(hash).ok_or_else(|| anyhow!("chunk {hash} vanished from the cache"))
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

    /// Moves a local file that S3 now has into the cache.
    pub fn adopt(&self, file: &Path, hash: &ContentHash) -> Result<()> {
        let target = self.path(hash);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::rename(file, &target)?;
        let bytes = std::fs::metadata(&target)?.len();
        self.insert(*hash, bytes);
        Ok(())
    }

    async fn download(&self, hash: &ContentHash) -> Result<()> {
        let data = self
            .bucket
            .get(&keys::chunk(hash))
            .await?
            .ok_or_else(|| anyhow!("chunk {hash} is missing in S3"))?;
        hash.verify(&data)?;
        let target = self.path(hash);
        let bytes = data.len() as u64;
        blocking(move || {
            std::fs::create_dir_all(target.parent().unwrap())?;
            // Write to a temp name and sync first, so a crash never leaves a
            // half-written chunk under its final name.
            let temp = target.with_extension("tmp");
            let mut file = File::create(&temp)?;
            file.write_all(&data)?;
            file.sync_data()?;
            std::fs::rename(&temp, &target)?;
            Ok(())
        })
        .await?;
        self.insert(*hash, bytes);
        Ok(())
    }

    fn open_local(&self, hash: &ContentHash) -> Option<Arc<File>> {
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

    fn insert(&self, hash: ContentHash, bytes: u64) {
        let mut index = self.index.lock().unwrap();
        index.clock += 1;
        let last_used = index.clock;
        if let Some(old) = index.entries.insert(hash, CacheEntry { bytes, last_used }) {
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
                    Ok(hash) => self.insert(hash, std::fs::metadata(&path)?.len()),
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
