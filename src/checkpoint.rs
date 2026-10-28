use crate::blocking::blocking;
use crate::bucket::keys;
use crate::codec;
use crate::content_hash::{ContentHash, is_all_zero};
use crate::disk::{Committed, Disk};
use crate::gc::GcActivity;
use crate::manifest::{CHUNKS_PER_PAGE, Manifest};
use crate::ownership::{self, Owner};
use crate::records::Head;
use anyhow::{Result, bail};
use futures::{StreamExt, TryStreamExt};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use std::time::Instant;

/// Uploads in flight, as bytes: enough to fill a link with a long round trip.
const UPLOAD_BYTES_IN_FLIGHT: u64 = 64 << 20;
const MIN_UPLOADS: usize = 16;
const MAX_UPLOADS: usize = 64;
const PAGE_UPLOADS: usize = 16;
const HOT_CHUNK: std::time::Duration = std::time::Duration::from_secs(1);
const REPAIR_ROUNDS: usize = 5;
/// How often an upload checks whether a GC runs. See `release_safe`.
const GC_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// A committed chunk: its index and new hash. `None` is a zero chunk.
type UploadedChunk = (usize, Option<ContentHash>);
type Uploaded = Vec<UploadedChunk>;

/// A chunk read for its upload.
struct Staged {
    /// The committed hash, if the upload may skip a chunk equal to it.
    base: Option<ContentHash>,
    /// Found in the dirty log after a restart: see `ChunkSlot::confirm`.
    confirm: bool,
    content: Vec<u8>,
}

impl Disk {
    /// Uploads the unsaved chunks and commits a new root. See "Checkpoint"
    /// in docs/design.md.
    ///
    /// The work is the set of unsaved chunks at the start. Chunks that the
    /// guest changes later go to the next checkpoint, so a busy writer cannot
    /// make a checkpoint last forever. Each chunk is staged, then uploaded,
    /// while other chunks are staged: the upload starts at once.
    pub async fn checkpoint(&self) -> Result<()> {
        self.run_checkpoint(false).await
    }

    /// A checkpoint from the timer or the dirty limit. It leaves chunks that
    /// were written in the last second for a later one, unless unsaved data is
    /// high or old. Detach, snapshot and `mica disk checkpoint` take all chunks.
    pub async fn background_checkpoint(&self) -> Result<()> {
        self.run_checkpoint(true).await
    }

    async fn run_checkpoint(&self, skip_hot: bool) -> Result<()> {
        let _running = self.checkpoint_running.lock().await;
        self.ensure_usable()?;
        let started = Instant::now();
        let mut pending: Vec<usize> = self.unsaved.lock().unwrap().iter().copied().collect();
        let all = pending.len();
        if skip_hot && !self.under_pressure() {
            pending = self.cold_chunks(pending).await?;
        }
        let skipped = pending.len() < all;
        if pending.is_empty() && !self.profile.has_unsaved_reads() {
            return Ok(());
        }
        let gc_before = GcActivity::read(&self.bucket).await?;
        let uploaded = match self.stage_and_upload(&pending, &gc_before).await {
            Ok(uploaded) => uploaded,
            Err(error) => {
                self.fail_checkpoint(&pending).await?;
                return Err(error);
            }
        };
        if let Err(error) = self.commit(&uploaded, gc_before, started).await {
            self.fail_checkpoint(&pending).await?;
            return Err(error);
        }
        self.finish(uploaded, started, skipped).await
    }

    async fn commit(&self, uploaded: &Uploaded, gc_before: GcActivity, started: Instant) -> Result<()> {
        let (manifest, manifest_hash, profile, new_pages) = self.commit_root(uploaded).await?;
        self.confirm_ownership().await?;
        let limits = *self.limits.lock().unwrap();
        Head::new(&manifest_hash, profile.clone(), manifest.seq, &self.node.hostname)
            .with_limits(limits)
            .save(&self.bucket, &self.id)
            .await?;
        log::info!(
            "disk {}: committed seq {} with {} chunks in {} ms",
            self.id,
            manifest.seq,
            uploaded.len(),
            started.elapsed().as_millis()
        );
        let saved_reads = profile.len();
        *self.committed.lock().unwrap() = Committed { manifest, manifest_hash, profile };
        self.profile.mark_saved(saved_reads);
        // GC can check a chunk's age before our upload and delete it after.
        // That needs a GC that ran during this checkpoint. Only then are the
        // committed chunks checked, after that GC ends, so that none of its
        // deletes comes later. The source is the data file, or the hold file
        // for a chunk the guest wrote since its stage.
        let gc_after = GcActivity::finished(&self.bucket).await?;
        // A GC that still runs could delete after the check. The chunks stay
        // unsaved, so the next checkpoint uploads them again.
        if gc_after.is_running() {
            bail!("disk {}: a GC still runs after the wait; the checkpoint is retried", self.id);
        }
        if gc_after.may_have_deleted_since(&gc_before) {
            self.repair_missing_pages(&new_pages).await?;
            self.repair_missing_chunks(uploaded).await?;
        }
        Ok(())
    }

    /// The chunks not written in the last `HOT_CHUNK` time. A chunk whose
    /// lock is busy is being written now, so it is hot too.
    async fn cold_chunks(&self, pending: Vec<usize>) -> Result<Vec<usize>> {
        let mut cold = Vec::with_capacity(pending.len());
        for index in pending {
            let Ok(slot) = self.slot(index).await?.try_lock_owned() else { continue };
            if slot.written.is_none_or(|written| written.elapsed() >= HOT_CHUNK) {
                cold.push(index);
            }
        }
        Ok(cold)
    }

    /// Stages and uploads each pending chunk. Returns what to commit.
    /// Meanwhile, uploaded chunks that no GC can delete leave the stage.
    async fn stage_and_upload(&self, pending: &[usize], gc_before: &GcActivity) -> Result<Uploaded> {
        let uploads = (UPLOAD_BYTES_IN_FLIGHT / self.chunk_size).clamp(MIN_UPLOADS as u64, MAX_UPLOADS as u64);
        let done_uploads = Mutex::new(Vec::new());
        let finished = Notify::new();
        // Every task runs to its end, also after one fails, so that the
        // stage of every chunk is known when it ends.
        let done_list = &done_uploads;
        let upload_all = async {
            let results: Vec<Result<Option<UploadedChunk>>> = futures::stream::iter(pending.iter().copied())
                .map(|index| async move {
                    let Some(staged) = self.stage(index).await? else { return Ok(None) };
                    let uploaded = self.upload(index, staged).await?;
                    done_list.lock().unwrap().push(index);
                    Ok(Some(uploaded))
                })
                .buffer_unordered(uploads as usize)
                .collect()
                .await;
            finished.notify_one();
            results
        };
        let release = async {
            loop {
                tokio::select! {
                    _ = finished.notified() => return,
                    _ = tokio::time::sleep(GC_CHECK) => {}
                }
                if !self.release_safe(&done_uploads, gc_before).await {
                    return;
                }
            }
        };
        let (results, ()) = tokio::join!(upload_all, release);
        let mut uploaded = Vec::with_capacity(results.len());
        for result in results {
            uploaded.extend(result?);
        }
        Ok(uploaded)
    }

    /// A GC deletes our upload only if it was running when the upload
    /// ended: it checks an object's age before it deletes, and the upload
    /// makes the age new. So when a GC check after some uploads finds no GC
    /// running, and no GC run since the checkpoint started, those uploads
    /// are safe: their chunks leave the stage, and need no held copy. Returns
    /// false once a GC shows up: then every chunk stays staged to the end.
    async fn release_safe(&self, done_uploads: &Mutex<Vec<usize>>, gc_before: &GcActivity) -> bool {
        // Taken before the check: these uploads ended before it.
        let done = std::mem::take(&mut *done_uploads.lock().unwrap());
        match GcActivity::read(&self.bucket).await {
            Ok(now) if now == *gc_before && !now.is_running() => {}
            Ok(_) => return false,
            Err(_) => {
                done_uploads.lock().unwrap().extend(done);
                return true;
            }
        }
        for index in done {
            if let Ok(slot) = self.slot(index).await {
                let mut slot = slot.lock_owned().await;
                slot.staged = false;
                slot.held = false;
                self.held.lock().unwrap().remove(&index);
            }
        }
        true
    }

    /// Reads the chunk for its upload. The guest goes on writing it in
    /// place; its first write keeps a copy of this content. Returns
    /// the committed hash and the content, or `None` when the chunk is saved.
    async fn stage(&self, index: usize) -> Result<Option<Staged>> {
        let mut slot = self.slot(index).await?.lock_owned().await;
        if !slot.unsaved {
            return Ok(None);
        }
        let data = self.data.clone();
        let content = blocking(move || data.read_chunk(index)).await?;
        slot.staged = true;
        slot.changed = false;
        // No base makes the upload PUT whatever the hash.
        let base = if slot.reupload { None } else { slot.base };
        Ok(Some(Staged { base, confirm: slot.confirm, content }))
    }

    async fn upload(&self, index: usize, staged: Staged) -> Result<(usize, Option<ContentHash>)> {
        let Staged { base, confirm, content } = staged;
        let (content, hash) = blocking(move || {
            if is_all_zero(&content) {
                return Ok((content, None));
            }
            let hash = ContentHash::of(&content);
            Ok((content, Some(hash)))
        })
        .await?;
        let Some(hash) = hash else { return Ok((index, None)) };
        // Skip the upload only for the chunk that the current root already
        // has here: GC keeps it. Any other chunk may be garbage that GC is
        // about to delete, so upload it again; the new write time protects it.
        // A chunk found after a restart is checked first: a checkpoint that
        // failed before the restart may have left that object to GC.
        let skip = match (Some(hash) == base, confirm) {
            (false, _) => false,
            (true, false) => true,
            (true, true) => self.bucket.exists(&keys::chunk(&hash)).await?,
        };
        if !skip {
            let packed = blocking(move || codec::pack(&content, codec::CHUNK_LEVEL)).await?;
            self.bucket.put(&keys::chunk(&hash), packed.into()).await?;
        }
        Ok((index, Some(hash)))
    }

    /// The content a staged chunk had at its stage.
    async fn read_staged(&self, index: usize) -> Result<Vec<u8>> {
        let slot = self.slot(index).await?.lock_owned().await;
        let (data, changed, held) = (self.data.clone(), slot.changed, slot.held);
        if let Some(content) = self.held.lock().unwrap().get(&index).filter(|_| changed) {
            return Ok(content.clone());
        }
        match (changed, held) {
            (false, _) => blocking(move || data.read_chunk(index)).await,
            (true, true) => blocking(move || data.read_held(index)).await,
            (true, false) => bail!("chunk {index} changed without a held copy"),
        }
    }

    /// The chunks stay unsaved, and the next checkpoint PUTs each of them:
    /// the head may already name these uploads, which GC may then delete.
    async fn fail_checkpoint(&self, pending: &[usize]) -> Result<()> {
        for &index in pending {
            let mut slot = self.slot(index).await?.lock_owned().await;
            if slot.unsaved {
                slot.reupload = true;
            }
        }
        self.unstage(pending).await
    }

    /// Ends the stage of each chunk, and drops the held copies.
    async fn unstage(&self, indexes: &[usize]) -> Result<()> {
        for &index in indexes {
            let mut slot = self.slot(index).await?.lock_owned().await;
            slot.staged = false;
            slot.held = false;
        }
        self.held.lock().unwrap().clear();
        if !self.held_on_file.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let data = self.data.clone();
        blocking(move || data.clear_held()).await
    }

    /// Writes the changed pages, then the new root. With no changed chunks,
    /// the root stays: only the head (with the new read profile) is written.
    /// Also returns the pages it wrote.
    async fn commit_root(&self, uploaded: &Uploaded) -> Result<(Manifest, ContentHash, Vec<u32>, Vec<ContentHash>)> {
        let (committed, committed_hash, profile) = {
            let committed = self.committed.lock().unwrap();
            (committed.manifest.clone(), committed.manifest_hash, self.profile.current_or(&committed.profile))
        };
        if uploaded.is_empty() {
            return Ok((committed, committed_hash, profile, Vec::new()));
        }
        let mut changes: BTreeMap<usize, Vec<(usize, Option<ContentHash>)>> = BTreeMap::new();
        for &(index, hash) in uploaded {
            changes.entry(index / CHUNKS_PER_PAGE).or_default().push((index % CHUNKS_PER_PAGE, hash));
        }
        let new_pages: Vec<(usize, Option<ContentHash>)> = futures::stream::iter(changes)
            .map(|(page, entries)| {
                let old = committed.pages[page];
                async move {
                    let mut chunks = (*self.page_store.get_or_zero(old.as_ref()).await?).clone();
                    for (offset, hash) in entries {
                        chunks.chunks[offset] = hash;
                    }
                    Ok::<_, anyhow::Error>((page, self.page_store.save(chunks).await?))
                }
            })
            .buffer_unordered(PAGE_UPLOADS)
            .try_collect()
            .await?;
        let mut manifest = committed.next(&self.id, committed_hash);
        for &(page, hash) in &new_pages {
            manifest.pages[page] = hash;
        }
        let manifest_hash = manifest.save(&self.bucket).await?;
        Ok((manifest, manifest_hash, profile, new_pages.into_iter().filter_map(|(_, hash)| hash).collect()))
    }

    /// A page can also be equal to an old garbage page, so a GC can delete it
    /// after our upload. The page store still has it; upload it again.
    async fn repair_missing_pages(&self, pages: &[ContentHash]) -> Result<()> {
        futures::stream::iter(pages.iter().copied())
            .map(|hash| async move {
                if self.bucket.exists(&keys::page(&hash)).await? {
                    return Ok(());
                }
                log::warn!("disk {}: page {hash} was deleted by GC during the commit; uploaded again", self.id);
                let page = (*self.page_store.get(&hash).await?).clone();
                self.page_store.save(page).await.map(|_| ())
            })
            .buffer_unordered(PAGE_UPLOADS)
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    /// Checks every committed chunk, and uploads a missing one again from
    /// its staged content. Without conditional writes GC and a checkpoint
    /// cannot be made atomic, but the staged content is still here.
    async fn repair_missing_chunks(&self, uploaded: &Uploaded) -> Result<()> {
        let committed: Vec<(usize, ContentHash)> =
            uploaded.iter().filter_map(|&(index, hash)| hash.map(|hash| (index, hash))).collect();
        let checks = (UPLOAD_BYTES_IN_FLIGHT / self.chunk_size).clamp(MIN_UPLOADS as u64, MAX_UPLOADS as u64);
        for _ in 0..REPAIR_ROUNDS {
            let repaired: usize = futures::stream::iter(committed.iter().copied())
                .map(|(index, hash)| self.repair_chunk(index, hash))
                .buffer_unordered(checks as usize)
                .try_fold(0, |total, repaired| async move { Ok(total + repaired as usize) })
                .await?;
            if repaired == 0 {
                return Ok(());
            }
            log::warn!("disk {}: {repaired} chunks were deleted by GC during the commit; uploaded again", self.id);
        }
        bail!("disk {}: committed chunks keep vanishing from S3", self.id)
    }

    /// A chunk that left the stage was safe when its PUT ended, but a
    /// checkpoint longer than the GC grace lets it age into garbage. So every
    /// chunk is checked. One that is missing and changed since has no copy:
    /// the checkpoint fails, and the next one uploads the chunk again.
    async fn repair_chunk(&self, index: usize, hash: ContentHash) -> Result<bool> {
        if self.bucket.exists(&keys::chunk(&hash)).await? {
            return Ok(false);
        }
        let data = self.read_staged(index).await?;
        hash.verify(&data)?;
        let packed = blocking(move || codec::pack(&data, codec::CHUNK_LEVEL)).await?;
        self.bucket.put(&keys::chunk(&hash), packed.into()).await?;
        Ok(true)
    }

    /// Only a network error is retried. A lost marker stops the disk for good.
    pub(crate) async fn confirm_ownership(&self) -> Result<()> {
        match ownership::owner_of(&self.bucket, &self.id, &self.node).await? {
            Owner::ThisNode => Ok(()),
            Owner::Nobody => self.lose_ownership("the attached marker is gone"),
            Owner::OtherNode(marker) => self.lose_ownership(&format!("it is attached on {} now", marker.node)),
        }
    }

    pub(crate) fn lose_ownership(&self, reason: &str) -> Result<()> {
        self.ownership_lost.store(true, Ordering::SeqCst);
        self.space_freed.notify_waiters();
        log::error!("disk {}: stopped, {reason}. Local data is kept", self.id);
        bail!("disk {}: {reason}", self.id)
    }

    /// S3 has the chunks now. A chunk the guest did not write since its
    /// stage is saved. It moves to the clean cache, and local data is freed.
    async fn finish(&self, uploaded: Uploaded, started: Instant, skipped: bool) -> Result<()> {
        for &(index, hash) in &uploaded {
            let mut slot = self.slot(index).await?.lock_owned().await;
            slot.base = hash;
            slot.reupload = false;
            slot.confirm = false;
            if !slot.changed {
                if !slot.verify {
                    self.unsaved_chunks.fetch_sub(1, Ordering::SeqCst);
                }
                slot.verify = false;
                slot.unsaved = false;
                self.unsaved.lock().unwrap().remove(&index);
                if !slot.queued_for_eviction {
                    self.saved_local.lock().unwrap().push_back(index);
                    slot.queued_for_eviction = true;
                }
            }
        }
        let indexes: Vec<usize> = uploaded.iter().map(|&(index, _)| index).collect();
        self.unstage(&indexes).await?;
        self.free_saved_chunks().await?;
        // Writes after the stage are not in S3 yet. They are at most this old,
        // unless the checkpoint left out hot chunks: their writes can be older,
        // so the old age stays, and the disk can become behind and take them.
        // Set before the wake-up: a writer that waits while the disk is
        // behind must see the new age when it wakes, or it waits for ever.
        {
            let mut oldest = self.oldest_unsaved_write.lock().unwrap();
            *oldest = match (self.has_unsaved_chunks(), skipped) {
                (false, _) => None,
                (true, true) => oldest.or(Some(started)),
                (true, false) => Some(started),
            };
        }
        self.space_freed.notify_waiters();
        Ok(())
    }
}
