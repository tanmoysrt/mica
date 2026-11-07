//! Deletes the objects that no version of a disk needs any more.
//!
//! mica has no garbage collection over all disks. The owner of a disk
//! deletes the garbage of that disk itself, with two rules.
//!
//! The rules depend on one fact. An object is in one continuous range of
//! versions: it appears in the transaction that wrote it, and it stays in
//! each later version until the guest writes that chunk again. It never
//! comes back, because mica never uses a key again and has no rollback.
//!
//! Thus a version V keeps an object of an older version O if and only if
//! the entry is the same in both. And snapshot S keeps an object of O with
//! birth B if and only if B <= S <= O.
//!
//! - Free rule: after transaction N replaces version O, delete each object
//!   of O that N does not have, unless a snapshot older than N keeps it.
//! - Merge rule: to delete snapshot S, delete each object of S that the
//!   snapshot before it (P) and the version after it (N) do not have.
//!
//! Both rules compare two roots and read only the pages that differ.

use super::disk_store::DiskStore;
use super::format::{Entry, Root};
use super::head::Head;
use anyhow::Result;

/// The keys of the objects of `old` that `newer` does not have and that
/// no snapshot keeps: their birth is after `kept`. The root of `old` is
/// not included.
pub async fn unshared(store: &DiskStore, old: &Root, newer: &Root, kept: u64) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for (index, old_page) in old.pages.iter().enumerate() {
        let new_page = newer.pages.get(index).copied().unwrap_or(Entry::ZERO);
        // The same entry is the same page object, so all its chunks are shared.
        if *old_page == new_page || old_page.is_zero() {
            continue;
        }
        if old_page.birth > kept {
            keys.push(store.page_key(old_page.birth, index));
        }
        let (before, after) = (store.page(old_page, index).await?, store.page(&new_page, index).await?);
        for (offset, (chunk, newer_chunk)) in before.0.iter().zip(&after.0).enumerate() {
            if chunk != newer_chunk && !chunk.is_zero() && chunk.birth > kept {
                keys.push(store.chunk_key(chunk.birth, index * super::format::CHUNKS_PER_PAGE + offset));
            }
        }
    }
    Ok(keys)
}

/// The newest snapshot that is older than `transaction`, or 0.
pub fn kept_before(head: &Head, transaction: u64) -> u64 {
    head.snapshots.values().map(|snapshot| snapshot.transaction).filter(|&t| t < transaction).max().unwrap_or(0)
}

/// Finishes the free rule of transactions whose deletes a crash stopped.
///
/// The free rule deletes the replaced root last. So each old root that
/// still exists, and that no snapshot keeps, marks unfinished deletes.
/// They form a chain back from the current root. The oldest goes first,
/// so a crash here leaves a shorter chain for the next try.
pub async fn finish_frees(store: &DiskStore, head: &Head) -> Result<usize> {
    if head.transaction == 0 {
        return Ok(0);
    }
    let mut chain = vec![store.root(head, head.transaction).await?];
    loop {
        let child = chain.last().unwrap();
        let parent = child.parent;
        if parent == 0 || parent <= kept_before(head, child.transaction) || !store.root_exists(parent).await? {
            break;
        }
        chain.push(store.root(head, parent).await?);
    }
    let finished = chain.len() - 1;
    while chain.len() > 1 {
        let old = chain.pop().unwrap();
        let newer = chain.last().unwrap();
        let mut keys = unshared(store, &old, newer, kept_before(head, newer.transaction)).await?;
        store.bucket.delete_all(std::mem::take(&mut keys)).await?;
        store.bucket.delete_all(vec![store.root_key(old.transaction)]).await?;
    }
    Ok(finished)
}

/// Deletes the objects that only snapshot `transaction` uses, by the merge
/// rule. The caller keeps the snapshot in the head, marked as deleting,
/// until this returns. A second run after a crash does no harm.
pub async fn merge_snapshot(store: &DiskStore, head: &Head, transaction: u64) -> Result<()> {
    let newer = head
        .snapshots
        .values()
        .map(|snapshot| snapshot.transaction)
        .filter(|&t| t > transaction)
        .min()
        .unwrap_or(head.transaction);
    // A snapshot of the current version: the current version keeps it all.
    if newer == transaction || transaction == 0 {
        return Ok(());
    }
    // Another snapshot of the same version keeps it all too.
    let others = head.snapshots.values().filter(|snapshot| snapshot.transaction == transaction).count();
    if others > 1 {
        return Ok(());
    }
    // The root is gone when an earlier run finished.
    if !store.root_exists(transaction).await? {
        return Ok(());
    }
    let old = store.root(head, transaction).await?;
    let newer = store.root(head, newer).await?;
    let keys = unshared(store, &old, &newer, kept_before(head, transaction)).await?;
    store.bucket.delete_all(keys).await?;
    store.bucket.delete_all(vec![store.root_key(transaction)]).await
}

/// Deletes the folders of transactions that never finished.
pub async fn delete_lost(store: &DiskStore, lost: &[u64]) -> Result<()> {
    for &transaction in lost {
        store.delete_folder(transaction).await?;
    }
    Ok(())
}
