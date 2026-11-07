//! The chunk cache of the node: clean chunks, by hash.
//!
//! A clean chunk is a chunk whose content is in the store. The cache keeps
//! clean chunks on the local disk, so that a read does not download them
//! again. Each disk has its own folder and its own budget:
//!
//! ```text
//! cache/<disk>/<first 2 hexadecimal digits>/<hash>
//! ```
//!
//! Before a download, mica looks for the hash in the caches of all disks of
//! the node. A file that another disk has is linked into the folder of this
//! disk: no download, and no copy. Thus many disks from one image download
//! each chunk one time for the node.
//!
//! A download goes into the cache without a sync. A crash can tear such a
//! file, so mica checks the hash of each file that it finds at start before
//! the first read from it.

use crate::store::Hash;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct NodeCache {
    root: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    disks: HashMap<String, DiskIndex>,
    /// One disk that has each hash, for the lookup over all disks.
    owners: HashMap<Hash, String>,
    clock: u64,
}

#[derive(Default)]
struct DiskIndex {
    files: HashMap<Hash, Item>,
    by_age: BTreeSet<(u64, Hash)>,
    bytes: u64,
    limit: u64,
}

struct Item {
    bytes: u64,
    used: u64,
    /// False for a file found at start: it can be torn.
    verified: bool,
}

/// The cache of one disk.
#[derive(Clone)]
pub struct ChunkCache {
    node: Arc<NodeCache>,
    disk: String,
}

#[derive(Debug, serde::Serialize)]
pub struct Usage {
    pub chunks: usize,
    pub bytes: u64,
    pub limit: u64,
}

impl NodeCache {
    /// Finds the cache files of all disks. Their hashes are checked later.
    pub fn load(root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        let mut state = State::default();
        for disk in std::fs::read_dir(&root)? {
            let disk = disk?;
            let name = disk.file_name().to_string_lossy().into_owned();
            for prefix in std::fs::read_dir(disk.path())? {
                for file in std::fs::read_dir(prefix?.path())? {
                    let file = file?;
                    let file_name = file.file_name().to_string_lossy().into_owned();
                    match Hash::from_hex(&file_name) {
                        Some(hash) => state.add(&name, hash, file.metadata()?.len(), false),
                        // A file that a crash left half written.
                        None => drop(std::fs::remove_file(file.path())),
                    }
                }
            }
        }
        Ok(Arc::new(Self { root, state: Mutex::new(state) }))
    }

    pub fn for_disk(self: &Arc<Self>, disk: &str, limit: u64) -> ChunkCache {
        self.state().disks.entry(disk.to_string()).or_default().limit = limit;
        ChunkCache { node: self.clone(), disk: disk.to_string() }
    }

    /// Deletes the cache of a deleted disk.
    pub fn remove_disk(&self, disk: &str) {
        let mut state = self.state();
        if let Some(index) = state.disks.remove(disk) {
            for hash in index.files.keys() {
                state.forget_owner(hash, disk);
            }
        }
        drop(state);
        let _ = std::fs::remove_dir_all(self.root.join(disk));
    }

    /// The index stays usable after a panic in another thread.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn path(&self, disk: &str, hash: &Hash) -> PathBuf {
        let hex = hash.hex();
        self.root.join(disk).join(&hex[..2]).join(hex)
    }
}

impl State {
    fn add(&mut self, disk: &str, hash: Hash, bytes: u64, verified: bool) {
        self.clock += 1;
        let used = self.clock;
        let index = self.disks.entry(disk.to_string()).or_default();
        // A file added again replaces its old entry in both indexes.
        if let Some(old) = index.files.insert(hash, Item { bytes, used, verified }) {
            index.by_age.remove(&(old.used, hash));
            index.bytes -= old.bytes;
        }
        index.bytes += bytes;
        index.by_age.insert((used, hash));
        self.owners.entry(hash).or_insert_with(|| disk.to_string());
    }

    fn forget_owner(&mut self, hash: &Hash, disk: &str) {
        if self.owners.get(hash).is_some_and(|owner| owner == disk) {
            self.owners.remove(hash);
        }
    }

    /// Marks a file as used now. Returns whether it is verified.
    fn touch(&mut self, disk: &str, hash: &Hash) -> Option<bool> {
        self.clock += 1;
        let clock = self.clock;
        let index = self.disks.get_mut(disk)?;
        let item = index.files.get_mut(hash)?;
        index.by_age.remove(&(item.used, *hash));
        item.used = clock;
        index.by_age.insert((clock, *hash));
        Some(item.verified)
    }

    fn remove(&mut self, disk: &str, hash: &Hash) {
        let Some(index) = self.disks.get_mut(disk) else { return };
        if let Some(item) = index.files.remove(hash) {
            index.by_age.remove(&(item.used, *hash));
            index.bytes -= item.bytes;
        }
        self.forget_owner(hash, disk);
    }

    /// The oldest files of a disk over its budget.
    fn over_budget(&mut self, disk: &str) -> Vec<Hash> {
        let Some(index) = self.disks.get(disk) else { return Vec::new() };
        let (mut bytes, mut out) = (index.bytes, Vec::new());
        for (_, hash) in &index.by_age {
            if bytes <= index.limit {
                break;
            }
            bytes -= index.files.get(hash).map_or(0, |item| item.bytes);
            out.push(*hash);
        }
        out
    }
}

impl ChunkCache {
    /// Opens a cached chunk. It checks the hash of a file found at start,
    /// and links a file of another disk on the node. Blocking.
    pub fn open(&self, hash: &Hash) -> Result<Option<File>> {
        let verified = self.node.state().touch(&self.disk, hash);
        match verified {
            Some(true) => return Ok(Some(File::open(self.path(hash))?)),
            Some(false) => {
                if let Some(file) = self.check(&self.path(hash), hash)? {
                    self.add(hash, true);
                    return Ok(Some(file));
                }
                self.node.state().remove(&self.disk, hash);
                let _ = std::fs::remove_file(self.path(hash));
            }
            None => {}
        }
        self.link_from_other_disk(hash)
    }

    /// Opens a cached chunk only if its hash is checked already. It never
    /// blocks for long, so the queue threads use it.
    pub fn open_checked(&self, hash: &Hash) -> Option<File> {
        let verified = self.node.state().touch(&self.disk, hash)?;
        verified.then(|| File::open(self.path(hash)).ok()).flatten()
    }

    pub fn has(&self, hash: &Hash) -> bool {
        let state = self.node.state();
        state.disks.get(&self.disk).is_some_and(|index| index.files.contains_key(hash))
    }

    /// Adds a chunk that the store has. `write` fills a new file. Blocking.
    pub fn insert(&self, hash: &Hash, write: impl FnOnce(&File) -> Result<()>) -> Result<()> {
        if self.has(hash) {
            return Ok(());
        }
        let target = self.path(hash);
        let temp = temp_path(&target);
        std::fs::create_dir_all(target.parent().unwrap())?;
        let file = File::create(&temp)?;
        if let Err(error) = write(&file) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        std::fs::rename(&temp, &target)?;
        self.add(hash, true);
        Ok(())
    }

    pub fn set_limit(&self, limit: u64) {
        self.node.state().disks.entry(self.disk.clone()).or_default().limit = limit;
        self.evict();
    }

    pub fn usage(&self) -> Usage {
        let state = self.node.state();
        let index = state.disks.get(&self.disk);
        Usage {
            chunks: index.map_or(0, |index| index.files.len()),
            bytes: index.map_or(0, |index| index.bytes),
            limit: index.map_or(0, |index| index.limit),
        }
    }

    pub fn nearly_full(&self) -> bool {
        let usage = self.usage();
        usage.bytes >= usage.limit / 10 * 9
    }

    fn add(&self, hash: &Hash, verified: bool) {
        let bytes = std::fs::metadata(self.path(hash)).map(|meta| meta.len()).unwrap_or(0);
        self.node.state().add(&self.disk, *hash, bytes, verified);
        self.evict();
    }

    /// Deletes the oldest files over the budget. A reader with the file
    /// open keeps its data until it closes the file.
    fn evict(&self) {
        let mut state = self.node.state();
        for hash in state.over_budget(&self.disk) {
            state.remove(&self.disk, &hash);
            let _ = std::fs::remove_file(self.path(&hash));
        }
    }

    fn link_from_other_disk(&self, hash: &Hash) -> Result<Option<File>> {
        let Some(owner) = self.node.state().owners.get(hash).cloned() else { return Ok(None) };
        if owner == self.disk {
            return Ok(None);
        }
        let (source, target) = (self.node.path(&owner, hash), self.path(hash));
        std::fs::create_dir_all(target.parent().unwrap())?;
        let temp = temp_path(&target);
        if std::fs::hard_link(&source, &temp).is_err() {
            return Ok(None);
        }
        let Some(file) = self.check(&temp, hash)? else {
            let _ = std::fs::remove_file(&temp);
            return Ok(None);
        };
        std::fs::rename(&temp, &target)?;
        self.add(hash, true);
        Ok(Some(file))
    }

    /// Opens a file if its content has the hash.
    fn check(&self, path: &Path, hash: &Hash) -> Result<Option<File>> {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        if Hash::of(&data) != *hash {
            log::warn!("cache: {} is torn or corrupt, it is downloaded again", path.display());
            return Ok(None);
        }
        Ok(Some(File::open(path)?))
    }

    fn path(&self, hash: &Hash) -> PathBuf {
        self.node.path(&self.disk, hash)
    }
}

fn temp_path(target: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    target.with_extension(format!("tmp{}-{id}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn put(cache: &ChunkCache, data: &[u8]) -> Hash {
        let hash = Hash::of(data);
        cache.insert(&hash, |mut file| Ok(file.write_all(data)?)).unwrap();
        hash
    }

    #[test]
    fn a_chunk_of_another_disk_is_linked_not_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeCache::load(dir.path().to_path_buf()).unwrap();
        let (a, b) = (node.for_disk("a", 1 << 20), node.for_disk("b", 1 << 20));
        let hash = put(&a, b"block");
        assert!(!b.has(&hash));
        assert!(b.open(&hash).unwrap().is_some());
        assert!(b.has(&hash));
    }

    #[test]
    fn the_oldest_files_leave_at_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeCache::load(dir.path().to_path_buf()).unwrap();
        let cache = node.for_disk("a", 10);
        let first = put(&cache, b"12345");
        let second = put(&cache, b"67890");
        cache.open(&first).unwrap();
        let third = put(&cache, b"abcde");
        assert!(cache.has(&first) && !cache.has(&second) && cache.has(&third));
    }

    #[test]
    fn a_file_added_again_is_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeCache::load(dir.path().to_path_buf()).unwrap();
        let cache = node.for_disk("a", 10);
        let first = put(&cache, b"12345");
        cache.add(&first, true);
        assert_eq!(cache.usage().bytes, 5);
        put(&cache, b"67890");
        put(&cache, b"abcde");
        assert_eq!(cache.usage().bytes, 10);
    }

    #[test]
    fn torn_files_found_at_start_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeCache::load(dir.path().to_path_buf()).unwrap();
        let hash = put(&node.for_disk("a", 1 << 20), b"good");
        std::fs::write(node.path("a", &hash), b"torn").unwrap();
        let node = NodeCache::load(dir.path().to_path_buf()).unwrap();
        let cache = node.for_disk("a", 1 << 20);
        assert!(cache.open_checked(&hash).is_none());
        assert!(cache.open(&hash).unwrap().is_none());
        assert!(!cache.has(&hash));
    }
}
