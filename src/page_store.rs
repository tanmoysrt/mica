use crate::blocking::blocking;
use crate::bucket::Bucket;
use crate::content_hash::ContentHash;
use crate::manifest::{Manifest, Page};
use anyhow::Result;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

/// Pages kept decoded in memory. A page is about 33 KiB, so this is ~34 MiB.
const MEMORY_PAGES: usize = 1024;
const FETCH_PARALLELISM: usize = 16;

/// Metadata pages for all disks of the node. A page comes from memory, then
/// from a local file, then from S3, and its hash is checked every time.
/// Pages are immutable, so a cached page is never stale.
pub struct PageStore {
    root: PathBuf,
    bucket: Arc<Bucket>,
    memory: Mutex<MemoryPages>,
    fetches: Mutex<HashMap<ContentHash, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Default)]
struct MemoryPages {
    pages: HashMap<ContentHash, (Arc<Page>, u64)>,
    clock: u64,
}

impl PageStore {
    pub fn new(root: PathBuf, bucket: Arc<Bucket>) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, bucket, memory: Mutex::default(), fetches: Mutex::default() })
    }

    pub async fn get(&self, hash: &ContentHash) -> Result<Arc<Page>> {
        if let Some(page) = self.cached_page(hash) {
            return Ok(page);
        }
        let lock = self.fetches.lock().unwrap().entry(*hash).or_default().clone();
        let _one_fetch = lock.lock().await;
        if let Some(page) = self.cached_page(hash) {
            return Ok(page);
        }
        let result = self.load(hash).await;
        self.fetches.lock().unwrap().remove(hash);
        let page = Arc::new(result?);
        self.remember(*hash, page.clone());
        Ok(page)
    }

    /// The page, or a zero page for `None`.
    pub async fn get_or_zero(&self, hash: Option<&ContentHash>) -> Result<Arc<Page>> {
        match hash {
            Some(hash) => self.get(hash).await,
            None => Ok(Arc::new(Page::zero())),
        }
    }

    /// Every chunk hash of a root, in disk order, trimmed to the disk size.
    pub async fn chunks_of(&self, manifest: &Manifest) -> Result<Vec<Option<ContentHash>>> {
        let pages: Vec<Arc<Page>> = futures::stream::iter(manifest.pages.clone())
            .map(|hash| async move { self.get_or_zero(hash.as_ref()).await })
            .buffered(FETCH_PARALLELISM)
            .try_collect()
            .await?;
        let mut chunks: Vec<Option<ContentHash>> = pages.iter().flat_map(|page| page.chunks.iter().copied()).collect();
        chunks.truncate(manifest.chunk_count());
        Ok(chunks)
    }

    /// Writes a new page to S3, and keeps it here, so the disk that made it
    /// never downloads it.
    pub async fn save(&self, page: Page) -> Result<Option<ContentHash>> {
        let Some(hash) = page.save(&self.bucket).await? else { return Ok(None) };
        let data = page.encode();
        let path = self.path(&hash);
        blocking(move || write_file(&path, &data)).await?;
        self.remember(hash, Arc::new(page));
        Ok(Some(hash))
    }

    async fn load(&self, hash: &ContentHash) -> Result<Page> {
        let path = self.path(hash);
        let expected = *hash;
        let local = blocking(move || {
            Ok(std::fs::read(&path).ok().filter(|data| expected.verify(data).is_ok()).map(Bytes::from))
        })
        .await?;
        if let Some(data) = local {
            return Page::decode(data);
        }
        let page = Page::load(&self.bucket, hash).await?;
        let data = page.encode();
        let path = self.path(hash);
        // The local copy is only a cache; a failed write costs a download later.
        if let Err(error) = blocking(move || write_file(&path, &data)).await {
            log::warn!("pages: cannot cache page {hash}: {error:#}");
        }
        Ok(page)
    }

    fn cached_page(&self, hash: &ContentHash) -> Option<Arc<Page>> {
        let mut memory = self.memory.lock().unwrap();
        memory.clock += 1;
        let now = memory.clock;
        let (page, last_used) = memory.pages.get_mut(hash)?;
        *last_used = now;
        Some(page.clone())
    }

    fn remember(&self, hash: ContentHash, page: Arc<Page>) {
        let mut memory = self.memory.lock().unwrap();
        memory.clock += 1;
        let now = memory.clock;
        memory.pages.insert(hash, (page, now));
        if memory.pages.len() > MEMORY_PAGES {
            let mut by_age: Vec<(u64, ContentHash)> =
                memory.pages.iter().map(|(hash, (_, used))| (*used, *hash)).collect();
            by_age.sort_unstable();
            for (_, hash) in by_age.into_iter().take(MEMORY_PAGES / 4) {
                memory.pages.remove(&hash);
            }
        }
    }

    fn path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(hex)
    }
}

/// Without a sync: a torn file fails its hash check and is fetched again.
fn write_file(path: &std::path::Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_of_the_same_page_all_succeed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("page");
        let data = Page::zero().encode();
        let start = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    start.wait();
                    for _ in 0..20 {
                        write_file(&path, &data).unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read(path).unwrap(), data);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
