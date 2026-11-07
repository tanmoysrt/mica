//! The transaction: one upload of the changes of a disk, and one write of
//! its head. It makes a new version of the disk.
//!
//! 1. Take the unsaved chunks. A timed transaction leaves out chunks written
//!    in the last second, because the guest will probably write them again.
//! 2. For each chunk: lock it, read it, mark it staged, and unlock it. The
//!    guest can write it again at once. If the data is zeros, or has the
//!    hash of the current entry, nothing is uploaded. Else the chunk goes to
//!    `d/<disk>/<next>/c/<index>`.
//! 3. Upload the changed pages and the new root into the same folder.
//! 4. Commit: write the head with `transaction = next` and `next + 1`, on
//!    the condition that nobody changed it. This one write makes the new
//!    version current.
//! 5. Each staged chunk that the guest did not write again is now saved.
//! 6. Free: delete the replaced objects that no snapshot keeps.
//!
//! A snapshot is a transaction whose head write also adds the name. If no
//! chunk changed, the snapshot names the current version, and only the head
//! is written.

use super::Disk;
use crate::owner::unix_now;
use crate::store::cleanup::kept_before;
use crate::store::format::{CHUNKS_PER_PAGE, is_all_zero, pack_chunk};
use crate::store::{Entry, Hash, HeadConflict, Page, Snapshot, validate_name};
use anyhow::{Result, bail};
use futures::{StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::spawn_blocking;

const UPLOAD_BYTES_IN_FLIGHT: u64 = 64 << 20;
const PAGE_UPLOADS: usize = 16;
/// A timed transaction leaves out chunks written this recently.
const HOT: Duration = Duration::from_secs(1);
const OWNER_CHECK: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// From the timer or the unsaved limit.
    Background,
    /// Takes all unsaved chunks: detach, snapshot, `mica disk commit`.
    All,
}

/// What a transaction leaves for the next one.
#[derive(Default)]
pub(super) struct Memory {
    /// Keys that failed tries wrote into the folder `next`. After the
    /// commit, the keys that the new version does not use are deleted.
    uploaded: BTreeSet<String>,
    /// Deletes of the free rule that failed. They run again before the next
    /// transaction, oldest first: the next free can delete the root that
    /// a crash recovery needs to find these.
    unfreed: Vec<Vec<String>>,
}

struct Change {
    index: usize,
    old: Entry,
    new: Entry,
}

impl Disk {
    /// Runs one transaction. Returns the current version after it.
    pub async fn transaction(&self, kind: Kind, snapshot: Option<&str>) -> Result<u64> {
        let mut memory = self.transaction_state.lock().await;
        let mut claim = self.claim.lock().await;
        self.ensure_usable()?;
        self.retry_frees(&mut memory).await?;
        let started = Instant::now();
        let head = claim.head().clone();
        if let Some(name) = snapshot {
            validate_name(name)?;
            if head.snapshots.contains_key(name) {
                bail!("disk {} has a snapshot {name} already", self.name);
            }
        }
        let mut pending: Vec<usize> = self.unsaved.lock().unwrap().iter().copied().collect();
        let all = pending.len();
        if kind == Kind::Background && !self.under_pressure() {
            pending.retain(|&index| self.is_cold(index));
        }
        let skipped = pending.len() < all;
        let saved_profile = self.saved_profile.lock().unwrap().clone();
        let profile = self.profile.current_or(&saved_profile);
        if pending.is_empty() && snapshot.is_none() && profile == saved_profile {
            return Ok(head.transaction);
        }

        let next = head.next;
        let mut written = BTreeSet::new();
        let (changes, keys, failure) = self.upload_chunks(&pending, next).await;
        memory.uploaded.extend(keys.iter().cloned());
        written.extend(keys);
        if let Some(error) = failure {
            self.unstage(&pending).await;
            return Err(error);
        }
        let changed: Vec<&Change> = changes.iter().filter(|change| change.old != change.new).collect();
        let mut new_head = head.clone();
        new_head.profile = profile.clone();
        let mut replaced_pages = Vec::new();
        let mut new_root = None;
        if !changed.is_empty() {
            let built = self.upload_root(&changed, next, head.transaction, &mut written, &mut replaced_pages).await;
            match built {
                Ok(root) => new_root = Some(root),
                Err(error) => {
                    memory.uploaded.extend(written);
                    self.unstage(&pending).await;
                    return Err(error);
                }
            }
            (new_head.transaction, new_head.next, new_head.time) = (next, next + 1, unix_now());
        }
        if let Some(name) = snapshot {
            let snapshot = Snapshot { transaction: new_head.transaction, time: unix_now(), deleting: false };
            new_head.snapshots.insert(name.to_string(), snapshot);
        }
        if new_head != head {
            if let Err(error) = claim.write(new_head.clone()).await {
                memory.uploaded.extend(written);
                self.unstage(&pending).await;
                if error.is::<HeadConflict>() {
                    self.stop("another node took the disk");
                }
                return Err(error);
            }
        }

        // The new version is current.
        *self.saved_profile.lock().unwrap() = profile;
        if let Some(root) = new_root {
            *self.root.lock().unwrap() = root;
            self.version.store(next, Ordering::SeqCst);
        }
        self.finish(&changes).await;
        let mut garbage: Vec<String> = memory.uploaded.difference(&written).cloned().collect();
        memory.uploaded.clear();
        if !changed.is_empty() {
            let kept = kept_before(&new_head, next);
            for change in changed.iter().filter(|change| !change.old.is_zero() && change.old.birth > kept) {
                garbage.push(self.store.chunk_key(change.old.birth, change.index));
            }
            for (page, old) in replaced_pages.iter().filter(|(_, old)| !old.is_zero() && old.birth > kept) {
                garbage.push(self.store.page_key(old.birth, *page));
            }
            // The replaced root goes last: while it exists, a crash recovery finds this free.
            if head.transaction > kept {
                garbage.push(self.store.root_key(head.transaction));
            }
        }
        memory.unfreed.push(garbage);
        if let Err(error) = self.retry_frees(&mut memory).await {
            log::warn!("disk {}: deletes after transaction {next} failed, retried later: {error:#}", self.name);
        }
        self.free_saved().await?;
        self.after_finish(started, skipped);
        log::info!(
            "disk {}: transaction {} with {} changed chunks in {} ms",
            self.name,
            self.version.load(Ordering::SeqCst),
            changed.len(),
            started.elapsed().as_millis()
        );
        Ok(self.version.load(Ordering::SeqCst))
    }

    /// Runs transactions on a timer and when much data waits, and checks
    /// once a minute that this node still owns the disk. Runs until the disk closes.
    pub async fn run_transactions(self: Arc<Self>) {
        if let Err(error) = self.claim.lock().await.repair().await {
            log::warn!("disk {}: repair after the claim failed: {error:#}", self.name);
        }
        let mut checked = Instant::now();
        while !self.is_closed() && !self.is_stopped() {
            tokio::select! {
                _ = tokio::time::sleep(self.tuning.transaction_interval) => {}
                _ = self.transaction_wanted.notified() => {}
            }
            if self.is_closed() {
                break;
            }
            if let Err(error) = self.transaction(Kind::Background, None).await {
                log::warn!("disk {}: transaction failed: {error:#}", self.name);
                tokio::time::sleep(RETRY).await;
            }
            if checked.elapsed() >= OWNER_CHECK {
                checked = Instant::now();
                match self.claim.lock().await.still_owned().await {
                    Ok(false) => self.stop("another node took the disk"),
                    Ok(true) => {}
                    Err(error) => log::warn!("disk {}: cannot read the head: {error:#}", self.name),
                }
            }
        }
    }

    fn is_cold(&self, index: usize) -> bool {
        let Some(slot) = self.slots.loaded(index) else { return true };
        let Ok(slot) = slot.try_read_owned() else { return false };
        slot.written.is_none_or(|written| written.elapsed() >= HOT)
    }

    /// Uploads the changed chunks, many at a time. Every task runs to its
    /// end, also when one fails, so the stage of each chunk is known after.
    /// Returns the keys it wrote also after a failure, so they get deleted.
    async fn upload_chunks(&self, pending: &[usize], next: u64) -> (Vec<Change>, Vec<String>, Option<anyhow::Error>) {
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
        Ok(Some((Change { index, old, new }, key)))
    }

    /// Uploads the changed pages and the new root. A page is built from the
    /// slots, whose entries are those of the current version.
    async fn upload_root(
        &self,
        changed: &[&Change],
        next: u64,
        parent: u64,
        written: &mut BTreeSet<String>,
        replaced: &mut Vec<(usize, Entry)>,
    ) -> Result<crate::store::Root> {
        let mut by_page: BTreeMap<usize, Vec<&Change>> = BTreeMap::new();
        for change in changed {
            by_page.entry(change.index / CHUNKS_PER_PAGE).or_default().push(change);
        }
        let pages: Vec<(usize, Entry)> = futures::stream::iter(by_page)
            .map(|(page, changes)| async move {
                let slots = self.slots.page(page).ok_or_else(|| self.missing(page * CHUNKS_PER_PAGE))?;
                let mut entries = Vec::with_capacity(CHUNKS_PER_PAGE);
                for slot in slots {
                    entries.push(slot.read().await.base);
                }
                for change in changes {
                    entries[change.index % CHUNKS_PER_PAGE] = change.new;
                }
                Ok::<_, anyhow::Error>((page, self.store.put_page(next, page, &Page(entries)).await?))
            })
            .buffer_unordered(PAGE_UPLOADS)
            .try_collect()
            .await?;
        let mut root = self.root.lock().unwrap().clone();
        for (page, entry) in pages {
            if !entry.is_zero() {
                written.insert(self.store.page_key(next, page));
            }
            replaced.push((page, root.pages[page]));
            root.pages[page] = entry;
        }
        (root.transaction, root.parent, root.time) = (next, parent, unix_now());
        self.store.put_root(&root).await?;
        written.insert(self.store.root_key(next));
        Ok(root)
    }

    /// Each staged chunk that the guest did not write again is saved now.
    async fn finish(&self, changes: &[Change]) {
        for change in changes {
            let Some(slot) = self.slots.loaded(change.index) else { continue };
            let mut slot = slot.write_owned().await;
            slot.base = change.new;
            slot.staged = false;
            if !slot.changed {
                slot.unsaved = false;
                self.unsaved.lock().unwrap().remove(&change.index);
                self.queue_saved(&mut slot, change.index);
            }
        }
    }

    async fn unstage(&self, pending: &[usize]) {
        for &index in pending {
            if let Some(slot) = self.slots.loaded(index) {
                slot.write_owned().await.staged = false;
            }
        }
    }

    /// Writes that came after the stage are not in the store yet, so they
    /// are at most as old as this transaction. If the transaction left out
    /// hot chunks, their writes can be older, so the old age stays.
    fn after_finish(&self, started: Instant, skipped: bool) {
        let mut oldest = self.oldest_unsaved.lock().unwrap();
        *oldest = match (self.has_unsaved(), skipped) {
            (false, _) => None,
            (true, true) => oldest.or(Some(started)),
            (true, false) => Some(started),
        };
        drop(oldest);
        self.room.notify_waiters();
    }

    async fn retry_frees(&self, memory: &mut Memory) -> Result<()> {
        while let Some(mut keys) = memory.unfreed.first().cloned() {
            let root = keys.pop();
            self.store.bucket.delete_all(keys).await?;
            if let Some(root) = root {
                self.store.bucket.delete_all(vec![root]).await?;
            }
            memory.unfreed.remove(0);
        }
        Ok(())
    }
}
