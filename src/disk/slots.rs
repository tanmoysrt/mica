//! The state of each chunk of an attached disk.
//!
//! mica keeps one slot for each chunk of each page that the guest uses. A
//! page of slots loads on first use, from the page of the current version.
//! Thus a large disk costs memory only for the parts that the guest uses.
//!
//! Each slot has a read and write lock. A read of local data holds a read
//! lock until its I/O ends. A write, a transaction and the eviction of a
//! chunk hold the write lock. Thus the space of a chunk is never freed
//! while a read still uses it.

use crate::store::Entry;
use crate::store::format::CHUNKS_PER_PAGE;
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OnceCell, RwLock};

#[derive(Debug)]
pub struct ChunkSlot {
    /// The chunk in the current version. Zero for a chunk of zeros.
    pub base: Entry,
    /// The data files hold the current content of the chunk.
    pub local: bool,
    /// The local content can differ from `base`.
    pub unsaved: bool,
    /// The running transaction read the chunk.
    pub staged: bool,
    /// The guest wrote the chunk after the running transaction read it.
    pub changed: bool,
    /// In the queue of saved local chunks, see `Disk::free_saved`.
    pub queued: bool,
    pub written: Option<Instant>,
}

impl ChunkSlot {
    fn new(base: Entry) -> Self {
        Self { base, local: false, unsaved: false, staged: false, changed: false, queued: false, written: None }
    }
}

pub type Slot = Arc<RwLock<ChunkSlot>>;

pub struct ChunkMap {
    pages: Vec<OnceCell<Box<[Slot]>>>,
}

impl ChunkMap {
    pub fn new(chunks: usize) -> Self {
        Self { pages: (0..chunks.div_ceil(CHUNKS_PER_PAGE)).map(|_| OnceCell::new()).collect() }
    }

    /// The slot of a chunk. `load` gives the entries of its page when the
    /// page is not loaded yet.
    pub async fn get<F, Fut>(&self, index: usize, load: F) -> Result<Slot>
    where
        F: FnOnce(usize) -> Fut,
        Fut: Future<Output = Result<Vec<Entry>>>,
    {
        let page = index / CHUNKS_PER_PAGE;
        let slots = self.pages[page]
            .get_or_try_init(|| async {
                let entries = load(page).await?;
                Ok::<_, anyhow::Error>(entries.into_iter().map(|entry| Arc::new(RwLock::new(ChunkSlot::new(entry)))).collect())
            })
            .await?;
        Ok(slots[index % CHUNKS_PER_PAGE].clone())
    }

    /// The slot of a chunk whose page is loaded. Never waits.
    pub fn loaded(&self, index: usize) -> Option<Slot> {
        Some(self.pages.get(index / CHUNKS_PER_PAGE)?.get()?[index % CHUNKS_PER_PAGE].clone())
    }

    /// The slots of a loaded page, for a transaction that changed it.
    pub fn page(&self, page: usize) -> Option<&[Slot]> {
        self.pages.get(page)?.get().map(|slots| &slots[..])
    }
}
