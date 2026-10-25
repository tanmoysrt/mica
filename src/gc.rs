use crate::bucket::{Bucket, ObjectInfo, keys};
use crate::content_hash::ContentHash;
use crate::manifest::{Manifest, Page};
use crate::records::{AttachedMarker, Head, Snapshot, now_text};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

const DELETE_PARALLELISM: usize = 16;
/// A checkpoint waits this long for a running GC to end before it checks its chunks.
const GC_WAIT: Duration = Duration::from_secs(600);
const GC_POLL: Duration = Duration::from_secs(2);
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
    pub kept_pages: usize,
    pub kept_chunks: usize,
    pub garbage_chunks: usize,
    pub garbage_chunk_bytes: u64,
    pub garbage_pages: usize,
    pub garbage_page_bytes: u64,
    pub garbage_manifests: usize,
    pub garbage_manifest_bytes: u64,
    pub deleted: bool,
}

/// `gc/last`: written at the end of each GC that deleted, before its lock goes.
#[derive(Serialize, Deserialize)]
struct GcLastRun {
    run: String,
    node: String,
    finished: String,
}

/// What a checkpoint sees of GC: a running GC (its lock), and the last
/// finished one. A GC can delete a chunk that a checkpoint uploads only if
/// it runs during the checkpoint. Then the lock is there at the end, or the
/// last run changed.
#[derive(Debug, PartialEq)]
pub struct GcActivity {
    running: bool,
    last_run: Option<String>,
}

impl GcActivity {
    /// The lock first, then the last run, never at the same time: a GC
    /// writes its last run before it deletes its lock. So a GC that ended
    /// between the two reads shows up in the last run.
    pub async fn read(bucket: &Bucket) -> Result<Self> {
        let running = bucket.exists(&keys::gc_lock()).await?;
        let last = bucket.get(&keys::gc_last()).await?;
        let last_run = last.and_then(|data| serde_json::from_slice::<GcLastRun>(&data).ok()).map(|last| last.run);
        Ok(Self { running, last_run })
    }

    /// Like `read`, but waits for a running GC to end first, up to a limit.
    /// A GC that runs longer gets the check while it runs, as in v0.1.
    pub async fn finished(bucket: &Bucket) -> Result<Self> {
        let started = std::time::Instant::now();
        loop {
            let activity = Self::read(bucket).await?;
            if !activity.running || started.elapsed() > GC_WAIT {
                return Ok(activity);
            }
            tokio::time::sleep(GC_POLL).await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn may_have_deleted_since(&self, before: &GcActivity) -> bool {
        self.running || self.last_run != before.last_run
    }
}

#[derive(Serialize, Deserialize)]
struct GcLock {
    node: String,
    since: String,
}

/// The roots, pages and chunks that must stay.
#[derive(Default)]
struct LiveSet {
    manifests: HashSet<ContentHash>,
    pages: HashSet<ContentHash>,
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
            // Checkpoints that ran meanwhile see the new run and check their chunks.
            let last = GcLastRun { run: random_id(), node: self.node_name.to_string(), finished: now_text() };
            let written = self.bucket.put(&keys::gc_last(), serde_json::to_vec(&last)?.into()).await;
            // Without the record, keep the lock: a checkpoint then still checks.
            if written.is_ok() {
                let _ = self.bucket.delete(&keys::gc_lock()).await;
            }
            written?;
        }
        result
    }

    async fn collect(&self, delete: bool) -> Result<GcReport> {
        let mut report = GcReport::default();
        let live = self.find_live(&mut report).await?;
        let garbage_manifests = self.find_garbage("manifests", &live.manifests).await?;
        let garbage_pages = self.find_garbage("pages", &live.pages).await?;
        let garbage_chunks = self.find_garbage("chunks", &live.chunks).await?;
        report.kept_manifests = live.manifests.len();
        report.kept_pages = live.pages.len();
        report.kept_chunks = live.chunks.len();
        report.garbage_manifests = garbage_manifests.len();
        report.garbage_manifest_bytes = garbage_manifests.iter().map(|object| object.size).sum();
        report.garbage_pages = garbage_pages.len();
        report.garbage_page_bytes = garbage_pages.iter().map(|object| object.size).sum();
        report.garbage_chunks = garbage_chunks.len();
        report.garbage_chunk_bytes = garbage_chunks.iter().map(|object| object.size).sum();
        if delete {
            // Chunks first, roots last: a root never points to a deleted page.
            self.delete_all(garbage_chunks).await?;
            self.delete_all(garbage_pages).await?;
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
        // Checkpoints share most pages, so each page is read only once.
        let new_pages: Vec<ContentHash> =
            manifest.pages.iter().flatten().copied().filter(|page| live.pages.insert(*page)).collect();
        let pages: Vec<Page> = futures::stream::iter(new_pages)
            .map(|page| async move { Page::load(self.bucket, &page).await })
            .buffer_unordered(DELETE_PARALLELISM)
            .try_collect()
            .await?;
        for page in pages {
            live.chunks.extend(page.chunks.iter().flatten().copied());
        }
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
    /// Every delete runs to its end before this returns, also after one
    /// fails: `gc/last` must come after the last delete.
    async fn delete_all(&self, garbage: Vec<ObjectInfo>) -> Result<()> {
        let results: Vec<Result<()>> = futures::stream::iter(garbage)
            .map(|object| async move {
                match self.bucket.info(&object.key).await? {
                    Some(current) if current.age_secs > self.grace.as_secs() => self.bucket.delete(&object.key).await,
                    _ => Ok(()),
                }
            })
            .buffer_unordered(DELETE_PARALLELISM)
            .collect()
            .await;
        results.into_iter().collect()
    }

    /// With conditional writes, only one GC can create the lock, so a
    /// checkpoint that sees no lock knows that no GC runs. Without them the
    /// lock is not atomic: two GCs at once still delete only garbage, but
    /// the first to end deletes the lock while the other may still delete.
    async fn take_lock(&self) -> Result<()> {
        let stale = match self.bucket.info(&keys::gc_lock()).await? {
            Some(object) if object.age_secs < STALE_LOCK.as_secs() => {
                let lock: Option<GcLock> =
                    self.bucket.get(&keys::gc_lock()).await?.and_then(|data| serde_json::from_slice(&data).ok());
                let holder = lock.map(|lock| format!("{} since {}", lock.node, lock.since)).unwrap_or_default();
                bail!("another GC is running ({holder}). Try again later");
            }
            Some(_) => true,
            None => false,
        };
        let lock = GcLock { node: self.node_name.to_string(), since: now_text() };
        let data: bytes::Bytes = serde_json::to_vec(&lock)?.into();
        if !self.bucket.conditional_writes() {
            return self.bucket.put(&keys::gc_lock(), data).await;
        }
        // Two GCs that both take over a stale lock could delete each other's
        // new lock. Only an operator knows that no GC runs.
        if stale {
            bail!(
                "a GC lock older than {} hours exists, from a GC that likely died. If no GC runs, delete {} and try again",
                STALE_LOCK.as_secs() / 3600,
                self.bucket.full_key(&keys::gc_lock())
            );
        }
        // Of GCs that start at the same moment, one wins the create.
        if !self.bucket.put_if_absent(&keys::gc_lock(), data).await? {
            bail!("another GC just started. Try again later");
        }
        Ok(())
    }
}

/// `chunks/ab/<hex>`, `pages/ab/<hex>` and `manifests/<hex>` end with the
/// hash. Other keys are not mica objects and are never deleted.
fn hash_of(key: &str) -> Option<ContentHash> {
    ContentHash::from_hex(key.rsplit('/').next()?).ok()
}

fn random_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().to_string()
}
