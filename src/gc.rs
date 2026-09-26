use crate::bucket::{Bucket, ObjectInfo, keys};
use crate::content_hash::ContentHash;
use crate::manifest::Manifest;
use crate::records::{AttachedMarker, Head, Snapshot, now_text};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

const DELETE_PARALLELISM: usize = 16;
/// A grace shorter than this could delete chunks that a running checkpoint
/// is about to commit, so it is only allowed while no disk is attached.
const SAFE_GRACE: Duration = Duration::from_secs(3600);
/// A lock older than this is from a GC that died, and is ignored.
const STALE_LOCK: Duration = Duration::from_secs(6 * 3600);

/// Deletes chunks and manifests that no disk, snapshot or kept checkpoint
/// needs. See "Garbage collection" in docs/design.md.
pub struct GarbageCollector<'a> {
    pub bucket: &'a Bucket,
    pub node_name: &'a str,
    pub keep_checkpoints: usize,
    pub grace: Duration,
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub disks: usize,
    pub snapshots: usize,
    pub kept_manifests: usize,
    pub kept_chunks: usize,
    pub garbage_chunks: usize,
    pub garbage_chunk_bytes: u64,
    pub garbage_manifests: usize,
    pub garbage_manifest_bytes: u64,
    pub deleted: bool,
}

#[derive(Serialize, Deserialize)]
struct GcLock {
    node: String,
    since: String,
}

/// The manifests and chunks that must stay.
#[derive(Default)]
struct LiveSet {
    manifests: HashSet<ContentHash>,
    chunks: HashSet<ContentHash>,
}

impl GarbageCollector<'_> {
    /// With `delete` false, it only counts the garbage.
    pub async fn run(&self, delete: bool) -> Result<GcReport> {
        if delete {
            self.take_lock().await?;
        }
        let result = self.collect(delete).await;
        if delete {
            let _ = self.bucket.delete(&keys::gc_lock()).await;
        }
        result
    }

    async fn collect(&self, delete: bool) -> Result<GcReport> {
        let mut report = GcReport::default();
        let live = self.find_live(&mut report).await?;
        let garbage_manifests = self.find_garbage("manifests", &live.manifests).await?;
        let garbage_chunks = self.find_garbage("chunks", &live.chunks).await?;
        report.kept_manifests = live.manifests.len();
        report.kept_chunks = live.chunks.len();
        report.garbage_manifests = garbage_manifests.len();
        report.garbage_manifest_bytes = garbage_manifests.iter().map(|object| object.size).sum();
        report.garbage_chunks = garbage_chunks.len();
        report.garbage_chunk_bytes = garbage_chunks.iter().map(|object| object.size).sum();
        if delete {
            self.delete_all(garbage_chunks).await?;
            self.delete_all(garbage_manifests).await?;
            report.deleted = true;
        }
        Ok(report)
    }

    /// Any read error stops GC: a root it cannot read must not lose its chunks.
    async fn find_live(&self, report: &mut GcReport) -> Result<LiveSet> {
        let mut live = LiveSet::default();
        for disk in self.bucket.list_folders("disks").await? {
            // A disk folder can hold only a marker, after its head was deleted.
            if self.grace < SAFE_GRACE
                && let Some(marker) = AttachedMarker::load(self.bucket, &disk).await?
            {
                bail!(
                    "a grace shorter than 1 hour needs every disk detached, but disk {disk} is attached on {}",
                    marker.node
                );
            }
            let Some(head) = Head::load(self.bucket, &disk).await? else { continue };
            report.disks += 1;
            self.keep_checkpoints_of(head.manifest_hash()?, &mut live)
                .await
                .with_context(|| format!("GC stopped at disk {disk}; nothing was deleted"))?;
        }
        for name in self.bucket.list_objects("snapshots").await? {
            let Some(snapshot) = Snapshot::load(self.bucket, &name).await? else { continue };
            report.snapshots += 1;
            self.keep_manifest(ContentHash::from_reference(&snapshot.manifest)?, &mut live)
                .await
                .with_context(|| format!("GC stopped at snapshot {name}; nothing was deleted"))?;
        }
        Ok(live)
    }

    /// Keeps the current manifest and the checkpoints before it, by `parent`.
    async fn keep_checkpoints_of(&self, head: ContentHash, live: &mut LiveSet) -> Result<()> {
        let mut next = Some(head);
        for depth in 0..self.keep_checkpoints.max(1) {
            let Some(hash) = next else { break };
            // Older checkpoints may be gone already; the current one must exist.
            let required = depth == 0;
            next = self.keep_manifest_if_present(hash, required, live).await?;
        }
        Ok(())
    }

    async fn keep_manifest(&self, hash: ContentHash, live: &mut LiveSet) -> Result<()> {
        self.keep_manifest_if_present(hash, true, live).await.map(|_| ())
    }

    /// Returns the parent of the manifest.
    async fn keep_manifest_if_present(
        &self,
        hash: ContentHash,
        required: bool,
        live: &mut LiveSet,
    ) -> Result<Option<ContentHash>> {
        if !live.manifests.insert(hash) {
            return Ok(None);
        }
        if !required && self.bucket.info(&keys::manifest(&hash)).await?.is_none() {
            return Ok(None);
        }
        let manifest = Manifest::load(self.bucket, &hash).await?;
        live.chunks.extend(manifest.chunks.iter().flatten().copied());
        Ok(manifest.parent)
    }

    /// Objects under `prefix` that are not live and older than the grace period.
    async fn find_garbage(&self, prefix: &str, live: &HashSet<ContentHash>) -> Result<Vec<ObjectInfo>> {
        let objects = self.bucket.list_all(prefix).await?;
        Ok(objects
            .into_iter()
            .filter(|object| object.age_secs > self.grace.as_secs())
            .filter(|object| match hash_of(&object.key) {
                Some(hash) => !live.contains(&hash),
                None => false,
            })
            .collect())
    }

    /// Checks each object's age again just before deleting it. A checkpoint
    /// may have written it again since the listing.
    async fn delete_all(&self, garbage: Vec<ObjectInfo>) -> Result<()> {
        futures::stream::iter(garbage)
            .map(|object| async move {
                match self.bucket.info(&object.key).await? {
                    Some(current) if current.age_secs > self.grace.as_secs() => self.bucket.delete(&object.key).await,
                    _ => Ok(()),
                }
            })
            .buffer_unordered(DELETE_PARALLELISM)
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    /// Without conditional writes this lock is not atomic. It only stops
    /// needless work: two GCs at once are still safe, as each deletes only garbage.
    async fn take_lock(&self) -> Result<()> {
        if let Some(object) = self.bucket.info(&keys::gc_lock()).await?
            && object.age_secs < STALE_LOCK.as_secs()
        {
            let lock: Option<GcLock> =
                self.bucket.get(&keys::gc_lock()).await?.and_then(|data| serde_json::from_slice(&data).ok());
            let holder = lock.map(|lock| format!("{} since {}", lock.node, lock.since)).unwrap_or_default();
            bail!("another GC is running ({holder}). Try again later");
        }
        let lock = GcLock { node: self.node_name.to_string(), since: now_text() };
        self.bucket.put(&keys::gc_lock(), serde_json::to_vec(&lock)?.into()).await
    }
}

/// `chunks/ab/<hex>` and `manifests/<hex>` end with the hash. Other keys are
/// not mica objects and are never deleted.
fn hash_of(key: &str) -> Option<ContentHash> {
    ContentHash::from_hex(key.rsplit('/').next()?).ok()
}
