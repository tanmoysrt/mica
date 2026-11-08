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
use crate::owner::Claim;
use crate::store::{Entry, Head, HeadConflict, Root, Snapshot, validate_name};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// A timed transaction leaves out chunks written this recently.
const HOT: Duration = Duration::from_secs(1);
const OWNER_CHECK: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// From the timer or the unsaved limit.
    Background,
    /// Takes all unsaved chunks: detach, snapshot, and the last transaction at exit.
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

pub(super) struct Change {
    pub index: usize,
    pub old: Entry,
    pub new: Entry,
}

/// A transaction whose head write worked.
struct Commit {
    head: Head,
    changes: Vec<Change>,
    replaced_pages: Vec<(usize, Entry)>,
    root: Option<Root>,
}

impl Disk {
    /// Runs one transaction. Returns the current version after it.
    pub async fn transaction(&self, kind: Kind, snapshot: Option<&str>) -> Result<u64> {
        let mut memory = self.transaction_state.lock().await;
        let mut claim = self.claim.lock().await;
        self.ensure_usable()?;
        self.retry_frees(&mut memory).await?;
        let started = Instant::now();
        let old = claim.head().clone();
        if let Some(name) = snapshot {
            validate_name(name)?;
            if old.snapshots.contains_key(name) {
                bail!("disk {} has a snapshot {name} already", self.name);
            }
        }
        let (pending, skipped) = self.pending(kind);
        let saved_profile = self.saved_profile.lock().unwrap().clone();
        let profile = self.profile.current_or(&saved_profile);
        if pending.is_empty() && snapshot.is_none() && profile == saved_profile {
            return Ok(old.transaction);
        }

        let mut written = BTreeSet::new();
        let commit = match self.commit(&mut claim, &old, &pending, profile, snapshot, &mut written).await {
            Ok(commit) => commit,
            Err(error) => {
                memory.uploaded.extend(written);
                self.unstage(&pending).await;
                return Err(error);
            }
        };
        let changed = self.apply(&commit).await;
        let garbage = self.garbage(&old, &commit, std::mem::take(&mut memory.uploaded), &written);
        memory.unfreed.push(garbage);
        if let Err(error) = self.retry_frees(&mut memory).await {
            log::warn!("disk {}: deletes after transaction {} failed, retried later: {error:#}", self.name, old.next);
        }
        self.free_saved().await?;
        self.after_finish(started, skipped);
        let version = self.version.load(Ordering::SeqCst);
        log::info!("disk {}: transaction {version} with {changed} changed chunks in {} ms", self.name, started.elapsed().as_millis());
        Ok(version)
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
                _ = tokio::time::sleep(self.interval()) => {}
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
                    Ok(false) => self.stop("another machine took the disk"),
                    Ok(true) => {}
                    Err(error) => log::warn!("disk {}: cannot read the head: {error:#}", self.name),
                }
            }
        }
    }

    /// The unsaved chunks to take, and whether some were left out.
    fn pending(&self, kind: Kind) -> (Vec<usize>, bool) {
        let mut pending: Vec<usize> = self.unsaved.lock().unwrap().iter().copied().collect();
        let all = pending.len();
        if kind == Kind::Background && !self.under_pressure() {
            pending.retain(|&index| self.is_cold(index));
        }
        let skipped = pending.len() < all;
        (pending, skipped)
    }

    /// Uploads the chunks, the pages and the root into the folder `next`,
    /// and writes the head. `written` gets each key written, also on failure.
    async fn commit(
        &self,
        claim: &mut Claim,
        old: &Head,
        pending: &[usize],
        profile: Vec<u32>,
        snapshot: Option<&str>,
        written: &mut BTreeSet<String>,
    ) -> Result<Commit> {
        let next = old.next;
        let (changes, keys, failure) = self.upload_chunks(pending, next).await;
        written.extend(keys);
        if let Some(error) = failure {
            return Err(error);
        }
        let mut head = old.clone();
        head.profile = profile;
        let (mut root, mut replaced_pages) = (None, Vec::new());
        let changed: Vec<&Change> = changes.iter().filter(|change| change.old != change.new).collect();
        if !changed.is_empty() {
            root = Some(self.upload_root(&changed, next, old.transaction, written, &mut replaced_pages).await?);
            (head.transaction, head.next, head.time) = (next, next + 1, unix_now());
        }
        if let Some(name) = snapshot {
            let snapshot = Snapshot { transaction: head.transaction, time: unix_now(), deleting: false };
            head.snapshots.insert(name.to_string(), snapshot);
        }
        if head != *old
            && let Err(error) = claim.write(head.clone()).await
        {
            if error.is::<HeadConflict>() {
                self.stop("another machine took the disk");
            }
            return Err(error);
        }
        Ok(Commit { head, changes, replaced_pages, root })
    }

    /// Makes the committed version current. Returns the count of changed chunks.
    async fn apply(&self, commit: &Commit) -> usize {
        *self.saved_profile.lock().unwrap() = commit.head.profile.clone();
        if let Some(root) = &commit.root {
            *self.root.lock().unwrap() = root.clone();
            self.version.store(root.transaction, Ordering::SeqCst);
        }
        self.finish(&commit.changes).await;
        commit.changes.iter().filter(|change| change.old != change.new).count()
    }

    /// The keys that the new version made free, by the free rule. Keys that
    /// failed tries wrote and the new version does not use are free too.
    fn garbage(&self, old: &Head, commit: &Commit, uploaded: BTreeSet<String>, written: &BTreeSet<String>) -> Vec<String> {
        let mut garbage: Vec<String> = uploaded.difference(written).cloned().collect();
        if commit.root.is_none() {
            return garbage;
        }
        let kept = kept_before(&commit.head, old.next);
        for change in commit.changes.iter().filter(|change| change.old != change.new && !change.old.is_zero() && change.old.birth > kept) {
            garbage.push(self.store.chunk_key(change.old.birth, change.index));
        }
        for (page, replaced) in commit.replaced_pages.iter().filter(|(_, replaced)| !replaced.is_zero() && replaced.birth > kept) {
            garbage.push(self.store.page_key(replaced.birth, *page));
        }
        // The replaced root goes last: while it exists, a crash recovery finds this free.
        if old.transaction > kept {
            garbage.push(self.store.root_key(old.transaction));
        }
        garbage
    }

    fn is_cold(&self, index: usize) -> bool {
        let Some(slot) = self.slots.loaded(index) else { return true };
        let Ok(slot) = slot.try_read_owned() else { return false };
        slot.written.is_none_or(|written| written.elapsed() >= HOT)
    }

    /// Each staged chunk that the guest did not write again is saved now.
    async fn finish(&self, changes: &[Change]) {
        for change in changes {
            self.staged_bytes.fetch_sub(self.chunk_size, Ordering::Relaxed);
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
        self.staged_bytes.store(0, Ordering::Relaxed);
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
