use crate::blocking::blocking;
use crate::bucket::keys;
use crate::content_hash::{ContentHash, is_all_zero};
use crate::disk::{Committed, Disk};
use crate::manifest::Manifest;
use crate::ownership::{self, Owner};
use crate::records::Head;
use anyhow::{Result, bail};
use futures::{StreamExt, TryStreamExt};
use std::collections::HashSet;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::time::Instant;

const UPLOAD_PARALLELISM: usize = 16;

type Uploaded = Vec<(usize, Option<ContentHash>)>;

impl Disk {
    /// Uploads the dirty chunks and commits a new manifest. See "Checkpoint" in docs/design.md.
    pub async fn checkpoint(&self) -> Result<()> {
        let _running = self.checkpoint_running.lock().await;
        self.ensure_usable()?;
        let started = Instant::now();
        let frozen = self.freeze_all().await?;
        if frozen.is_empty() && !self.profile.has_unsaved_reads() {
            return Ok(());
        }
        let committed_chunks = self.committed_chunks();
        let uploaded = self.upload_all(frozen, &committed_chunks).await?;
        let (manifest, profile) = self.next_manifest(&uploaded);
        let manifest_hash = manifest.save(&self.bucket).await?;
        self.confirm_ownership().await?;
        Head::new(&manifest_hash, profile.clone(), manifest.seq, &self.node.hostname)
            .save(&self.bucket, &self.id)
            .await?;
        log::info!("disk {}: committed seq {} with {} chunks", self.id, manifest.seq, uploaded.len());
        let saved_reads = profile.len();
        *self.committed.lock().unwrap() = Committed { manifest, manifest_hash, profile };
        self.profile.mark_saved(saved_reads);
        self.clean_up_frozen(uploaded, started).await
    }

    /// Renames every `.chunk` to `.frozen`. Writes after this go to a new `.chunk`.
    async fn freeze_all(&self) -> Result<Vec<usize>> {
        let mut frozen = Vec::new();
        for index in 0..self.slots.len() {
            let mut slot = self.slots[index].lock().await;
            if let Some(working) = slot.working.clone() {
                let folder = self.folder.clone();
                // A leftover `.frozen` from a failed checkpoint is older than `.chunk`.
                let has_old_frozen = slot.frozen.is_some();
                let file = working.clone();
                blocking(move || {
                    file.sync_data()?;
                    if has_old_frozen {
                        folder.remove_frozen(index)?;
                    }
                    folder.freeze(index)
                })
                .await?;
                slot.frozen = Some(working);
                slot.working = None;
                self.unsynced.lock().unwrap().remove(&index);
            }
            if slot.frozen.is_some() {
                frozen.push(index);
            }
        }
        if !frozen.is_empty() {
            let folder = self.folder.clone();
            blocking(move || folder.sync()).await?;
        }
        Ok(frozen)
    }

    async fn upload_all(&self, frozen: Vec<usize>, committed_chunks: &HashSet<ContentHash>) -> Result<Uploaded> {
        futures::stream::iter(frozen)
            .map(|index| self.upload_one(index, committed_chunks))
            .buffer_unordered(UPLOAD_PARALLELISM)
            .try_collect()
            .await
    }

    async fn upload_one(&self, index: usize, committed_chunks: &HashSet<ContentHash>) -> Result<(usize, Option<ContentHash>)> {
        let Some(file) = self.slots[index].lock().await.frozen.clone() else {
            bail!("chunk {index} is not frozen");
        };
        let chunk_size = self.chunk_size as usize;
        let (data, hash) = blocking(move || {
            let mut data = vec![0u8; chunk_size];
            file.read_exact_at(&mut data, 0)?;
            let hash = (!is_all_zero(&data)).then(|| ContentHash::of(&data));
            Ok((data, hash))
        })
        .await?;
        // Skip the upload only for a chunk the current manifest already has:
        // GC keeps it. Any other chunk may be garbage that GC is about to
        // delete, so upload it again; the new write time protects it.
        if let Some(hash) = hash
            && !committed_chunks.contains(&hash)
        {
            self.bucket.put(&keys::chunk(&hash), data.into()).await?;
        }
        Ok((index, hash))
    }

    pub(crate) fn committed_chunks(&self) -> HashSet<ContentHash> {
        self.committed.lock().unwrap().manifest.chunks.iter().flatten().copied().collect()
    }

    fn next_manifest(&self, uploaded: &Uploaded) -> (Manifest, Vec<u32>) {
        let committed = self.committed.lock().unwrap();
        let mut manifest = committed.manifest.next(&self.id, committed.manifest_hash);
        for &(index, hash) in uploaded {
            manifest.chunks[index] = hash;
        }
        let profile = self.profile.current_or(&committed.profile);
        (manifest, profile)
    }

    /// Only a network error is retried. A lost marker stops the disk for good.
    async fn confirm_ownership(&self) -> Result<()> {
        match ownership::owner_of(&self.bucket, &self.id, &self.node).await? {
            Owner::ThisNode => Ok(()),
            Owner::Nobody => self.lose_ownership("the attached marker is gone"),
            Owner::OtherNode(marker) => self.lose_ownership(&format!("it is attached on {} now", marker.node)),
        }
    }

    fn lose_ownership(&self, reason: &str) -> Result<()> {
        self.ownership_lost.store(true, Ordering::SeqCst);
        log::error!("disk {}: stopped, {reason}. Local data is kept", self.id);
        bail!("disk {}: {reason}", self.id)
    }

    /// S3 has the frozen chunks now. Move them into the cache, or delete them
    /// when the guest already wrote a newer `.chunk`.
    async fn clean_up_frozen(&self, uploaded: Uploaded, started: Instant) -> Result<()> {
        for (index, hash) in uploaded {
            let mut slot = self.slots[index].lock().await;
            slot.base = hash;
            slot.frozen = None;
            let has_newer = slot.working.is_some();
            let (folder, cache) = (self.folder.clone(), self.cache.clone());
            let result = blocking(move || match hash {
                Some(hash) if !has_newer => cache.adopt(&folder.frozen_path(index), &hash),
                _ => folder.remove_frozen(index),
            })
            .await;
            if let Err(error) = result {
                log::warn!("disk {}: cannot clean up frozen chunk {index}: {error:#}", self.id);
            }
            if !has_newer {
                self.local_chunks.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let folder = self.folder.clone();
        blocking(move || folder.sync()).await?;
        self.space_freed.notify_waiters();
        // Writes after the freeze are not in S3 yet. They are at most this old.
        *self.oldest_unsaved_write.lock().unwrap() = self.has_local_chunks().then_some(started);
        Ok(())
    }
}
