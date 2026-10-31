use crate::bucket::Bucket;
use crate::chunk_cache::{CacheUsage, ChunkCache};
use crate::content_hash::ContentHash;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct CacheManager {
    root: PathBuf,
    bucket: Arc<Bucket>,
    caches: Mutex<HashMap<String, Arc<ChunkCache>>>,
    limit: u64,
    reserve: u64,
    pub pressure: Arc<AtomicBool>,
}

impl CacheManager {
    pub fn load(root: PathBuf, bucket: Arc<Bucket>, limit: u64, reserve: u64) -> Result<Self> {
        let mut caches = HashMap::new();
        let directory = root.join("disk-cache");
        std::fs::create_dir_all(&directory)?;
        std::fs::create_dir_all(root.join("cache-limits"))?;
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let saved_limit = std::fs::read_to_string(root.join("cache-limits").join(&name))
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(limit);
                caches.insert(name, Arc::new(ChunkCache::load(entry.path(), saved_limit, bucket.clone())?));
            }
        }
        Ok(Self {
            root,
            bucket,
            caches: Mutex::new(caches),
            limit,
            reserve,
            pressure: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn for_disk(&self, disk: &str, limit: u64) -> Result<Arc<ChunkCache>> {
        let mut caches = self.caches.lock().unwrap();
        let path = self.root.join("cache-limits").join(disk);
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, limit.to_string())?;
        std::fs::rename(temp, path)?;
        if let Some(cache) = caches.get(disk) {
            cache.set_limit(limit);
            return Ok(cache.clone());
        }
        let cache = Arc::new(ChunkCache::load(self.root.join("disk-cache").join(disk), limit, self.bucket.clone())?);
        caches.insert(disk.to_owned(), cache.clone());
        Ok(cache)
    }

    pub fn usage(&self, keep: &HashSet<ContentHash>) -> CacheUsage {
        let mut total = CacheUsage { limit_bytes: self.limit, ..Default::default() };
        for cache in self.all() {
            let usage = cache.usage(keep);
            total.chunks += usage.chunks;
            total.bytes += usage.bytes;
            total.in_use_chunks += usage.in_use_chunks;
            total.in_use_bytes += usage.in_use_bytes;
        }
        total
    }

    pub fn prune(&self, keep: &HashSet<ContentHash>) -> (usize, u64) {
        self.all()
            .iter()
            .map(|cache| cache.prune(keep))
            .fold((0, 0), |(count, bytes), (n, size)| (count + n, bytes + size))
    }

    /// Drops the cache of a deleted disk. The caller holds the disk operation lock.
    pub fn remove_disk(&self, disk: &str) -> Result<()> {
        self.caches.lock().unwrap().remove(disk);
        for path in [self.root.join("disk-cache").join(disk), self.root.join("cache-limits").join(disk)] {
            let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            match removed {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn reconcile(&self) -> Result<()> {
        let caches = self.all();
        let total: u64 = caches.iter().map(|cache| cache.bytes()).sum();
        // Measure free space once: btrfs shows deleted files as free only
        // after its next commit, and each cache would then free the whole shortfall.
        let mut excess = total.saturating_sub(self.limit).max(self.reserve.saturating_sub(free_bytes(&self.root)?));
        for cache in caches {
            let before = cache.bytes();
            cache.trim_to(before.saturating_sub(excess).min(cache.limit_bytes()));
            excess = excess.saturating_sub(before.saturating_sub(cache.bytes()));
        }
        self.pressure.store(free_bytes(&self.root)? < self.reserve, Ordering::Release);
        Ok(())
    }

    fn all(&self) -> Vec<Arc<ChunkCache>> {
        self.caches.lock().unwrap().values().cloned().collect()
    }
}

fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[test]
    fn disk_limits_are_independent_and_host_reconciliation_caps_total() {
        let root = tempfile::tempdir().unwrap();
        let manager = CacheManager::load(root.path().into(), Arc::new(Bucket::in_memory()), 300, 0).unwrap();
        let small = manager.for_disk("small", 100).unwrap();
        let large = manager.for_disk("large", 300).unwrap();
        for byte in 0..3 {
            let data = [byte; 100];
            let hash = ContentHash::of(&data);
            small.store(&hash, |file| Ok(file.write_all_at(&data, 0)?)).unwrap();
            large.store(&hash, |file| Ok(file.write_all_at(&data, 0)?)).unwrap();
        }
        assert!(small.bytes() <= 100);
        assert_eq!(large.bytes(), 300);
        manager.reconcile().unwrap();
        assert!(manager.usage(&HashSet::new()).bytes <= 300);
        assert!(!manager.pressure.load(Ordering::Acquire));
        large.set_limit(100);
        manager.reconcile().unwrap();
        assert!(large.bytes() <= 100);
    }

    #[test]
    fn a_deleted_disk_leaves_no_cache_behind() {
        let root = tempfile::tempdir().unwrap();
        let manager = CacheManager::load(root.path().into(), Arc::new(Bucket::in_memory()), 300, 0).unwrap();
        let cache = manager.for_disk("gone", 100).unwrap();
        let data = [1; 100];
        cache.store(&ContentHash::of(&data), |file| Ok(file.write_all_at(&data, 0)?)).unwrap();
        manager.remove_disk("gone").unwrap();
        assert_eq!(manager.usage(&HashSet::new()).bytes, 0);
        let reloaded = CacheManager::load(root.path().into(), Arc::new(Bucket::in_memory()), 300, 0).unwrap();
        assert_eq!(reloaded.usage(&HashSet::new()).bytes, 0);
        assert!(!root.path().join("cache-limits/gone").exists());
    }
}
