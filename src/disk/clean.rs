//! Clean chunks: chunks whose content is in the bucket. A read or a copy
//! takes them from the cache of this disk, from the cache of another disk
//! on the node, or else downloads them. Many reads of one chunk share one
//! download.

use super::Disk;
use crate::store::{Entry, Hash};
use crate::transfers::Priority;
use anyhow::Result;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use tokio::task::spawn_blocking;

/// The content of a clean chunk: a cache file, or a download in memory.
pub(super) enum Clean {
    File(File),
    Data(Arc<Vec<u8>>),
}

impl Clean {
    pub(super) fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<()> {
        match self {
            Clean::File(file) => Ok(file.read_exact_at(buffer, offset)?),
            Clean::Data(data) => {
                buffer.copy_from_slice(&data[offset as usize..offset as usize + buffer.len()]);
                Ok(())
            }
        }
    }
}

impl Disk {
    /// Gets a chunk into the cache before the guest reads it.
    pub async fn fetch(&self, index: usize, priority: Priority) -> Result<()> {
        if index >= self.chunk_count() || self.is_closed() {
            return Ok(());
        }
        let base = {
            let slot = self.slot(index).await?.read_owned().await;
            if slot.local || slot.base.is_zero() || self.cache.has(&slot.base.hash) {
                return Ok(());
            }
            slot.base
        };
        self.clean(index, base, priority).await.map(drop)
    }

    /// The content of a clean chunk: from the cache of this disk, from the
    /// cache of another disk on the node, or else from the store. Many
    /// reads of the same chunk share one download.
    pub(super) async fn clean(&self, index: usize, entry: Entry, priority: Priority) -> Result<Clean> {
        let hash = entry.hash;
        if let Some(file) = self.open_cached(hash).await? {
            return Ok(Clean::File(file));
        }
        let lock = self.downloads.lock().unwrap().entry(hash).or_default().clone();
        let _one_download = lock.lock().await;
        let result = self.download(index, entry, priority).await;
        self.downloads.lock().unwrap().remove(&hash);
        result
    }

    async fn download(&self, index: usize, entry: Entry, priority: Priority) -> Result<Clean> {
        if let Some(file) = self.open_cached(entry.hash).await? {
            return Ok(Clean::File(file));
        }
        let data = {
            let _slot = self.transfers.download(priority).await?;
            Arc::new(self.store.chunk(&entry, index, self.chunk_size).await?)
        };
        let (cache, chunk) = (self.cache.clone(), data.clone());
        let stored = spawn_blocking(move || cache.insert(&entry.hash, |mut file| Ok(std::io::Write::write_all(&mut file, &chunk)?))).await?;
        if let Err(error) = stored {
            log::warn!("disk {}: chunk {index} not cached: {error:#}", self.name);
        }
        Ok(Clean::Data(data))
    }

    async fn open_cached(&self, hash: Hash) -> Result<Option<File>> {
        let cache = self.cache.clone();
        spawn_blocking(move || cache.open(&hash)).await?
    }
}
