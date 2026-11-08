//! The uploads of a transaction: each changed chunk, then each changed
//! page, then the new root, all into the folder `next`.

use super::Disk;
use super::transaction::Change;
use crate::owner::unix_now;
use crate::store::format::{CHUNKS_PER_PAGE, is_all_zero, pack_chunk};
use crate::store::{Entry, Hash, Page, Root};
use anyhow::Result;
use futures::StreamExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use tokio::task::spawn_blocking;

const UPLOAD_BYTES_IN_FLIGHT: u64 = 64 << 20;
const PAGE_UPLOADS: usize = 16;

impl Disk {
    /// Uploads the changed chunks, many at a time. Every task runs to its
    /// end, also when one fails, so the stage of each chunk is known after.
    /// Returns the keys it wrote also after a failure, so they get deleted.
    pub(super) async fn upload_chunks(&self, pending: &[usize], next: u64) -> (Vec<Change>, Vec<String>, Option<anyhow::Error>) {
        let in_flight = (UPLOAD_BYTES_IN_FLIGHT / self.chunk_size).clamp(16, 64) as usize;
        let results: Vec<_> = futures::stream::iter(pending.iter().copied())
            .map(|index| self.upload_chunk(index, next))
            .buffer_unordered(in_flight)
            .collect()
            .await;
        let (mut changes, mut keys, mut failure) = (Vec::new(), Vec::new(), None);
        for result in results {
            match result {
                Ok(Some((change, key))) => {
                    changes.push(change);
                    keys.extend(key);
                }
                Ok(None) => {}
                Err(error) => failure = Some(error),
            }
        }
        (changes, keys, failure)
    }

    async fn upload_chunk(&self, index: usize, next: u64) -> Result<Option<(Change, Option<String>)>> {
        let _slot = self.transfers.upload().await?;
        let (old, content) = {
            let mut slot = self.slot(index).await?.write_owned().await;
            if !slot.unsaved {
                return Ok(None);
            }
            let data = self.data.clone();
            let content = spawn_blocking(move || data.read_chunk(index)).await??;
            slot.staged = true;
            slot.changed = false;
            (slot.base, content)
        };
        let packed = spawn_blocking(move || {
            if is_all_zero(&content) {
                return Ok::<_, anyhow::Error>(None);
            }
            let hash = Hash::of(&content);
            match hash == old.hash && !old.is_zero() {
                true => Ok(Some((hash, None))),
                false => Ok(Some((hash, Some(pack_chunk(&content)?)))),
            }
        })
        .await??;
        let (new, key) = match packed {
            None => (Entry::ZERO, None),
            Some((_, None)) => (old, None),
            Some((hash, Some(packed))) => {
                let entry = self.store.put_chunk(next, index, packed, hash).await?;
                (entry, Some(self.store.chunk_key(next, index)))
            }
        };
        self.staged_bytes.fetch_add(self.chunk_size, Ordering::Relaxed);
        Ok(Some((Change { index, old, new }, key)))
    }

    /// Uploads the changed pages and the new root. A page is built from the
    /// slots, whose entries are those of the current version.
    pub(super) async fn upload_root(
        &self,
        changed: &[&Change],
        next: u64,
        parent: u64,
        written: &mut BTreeSet<String>,
        replaced: &mut Vec<(usize, Entry)>,
    ) -> Result<Root> {
        // Owned values: borrowed items in a stream make the future not Send.
        let mut by_page: BTreeMap<usize, Vec<(usize, Entry)>> = BTreeMap::new();
        for change in changed {
            by_page.entry(change.index / CHUNKS_PER_PAGE).or_default().push((change.index, change.new));
        }
        // Before the uploads: a PUT can fail after the bucket stored the object.
        written.extend(by_page.keys().map(|&page| self.store.page_key(next, page)));
        let pages: Vec<Result<(usize, Entry)>> = futures::stream::iter(by_page)
            .map(|(page, changes)| async move {
                let slots = self.slots.page(page).ok_or_else(|| self.missing(page * CHUNKS_PER_PAGE))?;
                let mut entries = Vec::with_capacity(CHUNKS_PER_PAGE);
                for slot in slots {
                    entries.push(slot.read().await.base);
                }
                for (index, new) in changes {
                    entries[index % CHUNKS_PER_PAGE] = new;
                }
                Ok::<_, anyhow::Error>((page, self.store.put_page(next, page, &Page(entries)).await?))
            })
            .buffer_unordered(PAGE_UPLOADS)
            .collect()
            .await;
        let mut root = self.root.lock().unwrap().clone();
        let mut failure = None;
        for result in pages {
            match result {
                Ok((page, entry)) => {
                    if entry.is_zero() {
                        written.remove(&self.store.page_key(next, page));
                    }
                    replaced.push((page, root.pages[page]));
                    root.pages[page] = entry;
                }
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        (root.transaction, root.parent, root.time) = (next, parent, unix_now());
        written.insert(self.store.root_key(next));
        self.store.put_root(&root).await?;
        Ok(root)
    }
}
