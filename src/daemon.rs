use crate::bucket::Bucket;
use crate::catalog::{Catalog, LoadedDisk, validate_name};
use crate::chunk_cache::ChunkCache;
use crate::config::Config;
use crate::local_data::LocalData;
use crate::gc::GarbageCollector;
use crate::disk::{Disk, DiskParts};
use crate::local_state::{DiskFolder, LocalStatus};
use crate::node::NodeIdentity;
use crate::page_store::PageStore;
use crate::ownership;
use crate::prefetch::{self, PrefetchPlan};
use crate::ublk_device::{UblkDevice, link_device, unlink_device};
use anyhow::{Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use crate::content_hash::ContentHash;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const RETRY_DELAY: Duration = Duration::from_secs(10);
const BUSY_RETRIES: usize = 15;
const DRAIN_POLL: Duration = Duration::from_secs(1);
const DEFAULT_WARM_PARALLEL: usize = 8;

pub struct Daemon {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) bucket: Arc<Bucket>,
    cache: Arc<ChunkCache>,
    page_store: Arc<PageStore>,
    pub(crate) node: Arc<NodeIdentity>,
    pub(crate) disks: Mutex<HashMap<String, AttachedDisk>>,
    /// One attach or detach of a disk at a time. The `disks` registry is
    /// locked only for short updates, so other disks attach in parallel.
    disk_locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    gc_running: Mutex<()>,
}

/// Prefetch for one attach. `None` fields take the config defaults.
#[derive(Debug, Default, Clone, Copy)]
pub struct Prefetch {
    /// Chunks of the startup profile. Without it, the whole profile when
    /// `prefetch_on_attach` is set.
    pub chunks: Option<usize>,
    pub parallel: Option<usize>,
    pub fill: Option<bool>,
}

pub(crate) struct AttachedDisk {
    pub disk: Arc<Disk>,
    pub device: Option<UblkDevice>,
    pub link: String,
}

impl AttachedDisk {
    fn describe(&self) -> Value {
        json!({
            "device": self.link,
            "ublk": self.device.as_ref().map(|device| &device.path),
            "blank": self.disk.is_blank(),
        })
    }
}

impl Daemon {
    pub async fn start(config: Config, config_path: PathBuf) -> Result<Arc<Self>> {
        crate::local_data::create_dir_durable(&config.data_dir)?;
        let bucket = Arc::new(Bucket::connect(&config.s3)?);
        let cache = ChunkCache::load(config.data_dir.join("cache"), config.cache_limit_bytes(), bucket.clone())?;
        let page_store = Arc::new(PageStore::new(config.data_dir.join("pages"), bucket.clone())?);
        let node = Arc::new(NodeIdentity::load_or_create(&config.data_dir)?);
        log::info!("node {} (id {})", node.hostname, node.id);
        let daemon = Arc::new(Self {
            config,
            config_path,
            bucket,
            cache: Arc::new(cache),
            page_store,
            node,
            disks: Mutex::default(),
            disk_locks: std::sync::Mutex::default(),
            gc_running: Mutex::default(),
        });
        daemon.recover_disks().await?;
        Ok(daemon)
    }

    pub fn catalog(&self) -> Catalog<'_> {
        Catalog::new(&self.bucket, &self.node.hostname)
    }

    /// Attaching a disk that is already attached here returns its device.
    /// A disk that is still uploading here is waited for, then attached again.
    /// Prefetch runs only when the caller asks for it.
    pub async fn attach(&self, disk_id: &str, force: bool, prefetch: Prefetch) -> Result<Value> {
        validate_name(disk_id)?;
        let lock = self.disk_lock(disk_id);
        let _one_at_a_time = lock.lock().await;
        loop {
            match self.disks.lock().await.get(disk_id) {
                None => break,
                Some(attached) if attached.device.is_some() => return Ok(attached.describe()),
                Some(_) => {}
            }
            tokio::time::sleep(DRAIN_POLL).await;
        }
        let mut timings = Timings::start();
        // Claim first, then load: a head loaded before the claim could be
        // replaced by the final commit of the previous owner. The load runs
        // during the settle wait of the claim, once our marker is written.
        let loaded = ownership::claim_then(&self.bucket, disk_id, &self.node, force, self.catalog().load(disk_id)).await?;
        timings.mark("claim_and_load");
        let folder = DiskFolder::new(&self.config.data_dir, disk_id);
        let (disk, device) = match self.open_device(disk_id, &folder, loaded, &mut timings).await {
            Ok(opened) => opened,
            Err(error) => {
                self.forget_if_clean(disk_id, &folder).await;
                return Err(error);
            }
        };
        let mut result = self.activate(&mut *self.disks.lock().await, &folder, disk, device, prefetch)?;
        timings.mark("activate");
        log::info!("disk {disk_id}: attached as {} ({})", result["ublk"].as_str().unwrap_or(""), timings.describe());
        result["timings_ms"] = timings.to_json();
        Ok(result)
    }

    fn disk_lock(&self, disk_id: &str) -> Arc<Mutex<()>> {
        self.disk_locks.lock().unwrap().entry(disk_id.to_string()).or_default().clone()
    }

    /// Removes the device, then uploads all local data and releases the disk.
    /// It returns only when S3 has everything, so success means the disk is free.
    pub async fn detach(self: &Arc<Self>, disk_id: &str) -> Result<Value> {
        let lock = self.disk_lock(disk_id);
        let one_at_a_time = lock.lock().await;
        let device = {
            let mut disks = self.disks.lock().await;
            let Some(attached) = disks.get_mut(disk_id) else { bail!("disk {disk_id} is not attached on this node") };
            let Some(device) = attached.device.take() else { bail!("disk {disk_id} is already detaching") };
            device
        };
        // The registry is not locked during the slow steps, so other disks
        // attach and report status meanwhile.
        if let Err(error) = ensure_not_in_use(&device.path).await {
            if let Some(attached) = self.disks.lock().await.get_mut(disk_id) {
                attached.device = Some(device);
            }
            return Err(error);
        }
        tokio::task::spawn_blocking(move || device.stop()).await??;
        let disk = {
            let disks = self.disks.lock().await;
            let attached = disks.get(disk_id).ok_or_else(|| anyhow::anyhow!("disk {disk_id} vanished during detach"))?;
            unlink_device(&attached.link);
            // The checkpoint loop and the prefetch end on their own. Aborting
            // them in the middle of a freeze would break the chunk state.
            attached.disk.close();
            attached.disk.clone()
        };
        // The drain keeps its registry entry as "detaching". A new attach
        // waits for it, so the lock can go now.
        drop(one_at_a_time);
        // A separate task, so the upload goes on even if the client disconnects.
        tokio::spawn(self.clone().drain(disk)).await??;
        Ok(json!({ "detached": disk_id }))
    }

    /// Downloads every chunk of a disk that the cache does not have, so that
    /// its reads never wait for S3. Works on a detached disk too.
    pub async fn warm(&self, disk_id: &str, parallel: Option<usize>) -> Result<Value> {
        validate_name(disk_id)?;
        let attached = self.disks.lock().await.get(disk_id).map(|attached| attached.disk.clone());
        let (chunks, chunk_size) = match attached {
            Some(disk) => (disk.committed_chunks().await?, disk.chunk_size),
            None => {
                let manifest = self.catalog().load(disk_id).await?.manifest;
                (self.page_store.chunks_of(&manifest).await?, manifest.chunk_size)
            }
        };
        // In disk order, each chunk once: a boot reads the start of a disk first.
        let mut seen = HashSet::new();
        let hashes: Vec<ContentHash> = chunks.into_iter().flatten().filter(|hash| seen.insert(*hash)).collect();
        let missing: Vec<ContentHash> = hashes.iter().filter(|hash| !self.cache.has(hash)).copied().collect();
        let results: Vec<bool> = futures::stream::iter(missing)
            .map(|hash| {
                let cache = self.cache.clone();
                async move { cache.open(&hash).await.is_ok() }
            })
            .buffer_unordered(parallel.unwrap_or(DEFAULT_WARM_PARALLEL).max(1))
            .collect()
            .await;
        let downloaded = results.iter().filter(|ok| **ok).count();
        Ok(json!({
            "chunks": hashes.len(),
            "downloaded": downloaded,
            "failed": results.len() - downloaded,
            "downloaded_bytes": downloaded as u64 * chunk_size,
            "disk_bytes": hashes.len() as u64 * chunk_size,
            "cache_limit_bytes": self.cache.limit_bytes(),
        }))
    }

    /// One GC at a time on this node. The S3 lock covers other nodes.
    pub async fn collect_garbage(&self, delete: bool, grace_secs: Option<u64>) -> Result<Value> {
        let Ok(_running) = self.gc_running.try_lock() else { bail!("a GC is already running on this node") };
        let collector = GarbageCollector {
            bucket: &self.bucket,
            node_name: &self.node.hostname,
            keep_checkpoints: self.config.gc_keep_checkpoints,
            grace: Duration::from_secs(grace_secs.unwrap_or(self.config.gc_grace_hours * 3600)),
        };
        let report = collector.run(delete).await?;
        if delete {
            log::info!(
                "gc: deleted {} chunks, {} pages and {} manifests",
                report.garbage_chunks,
                report.garbage_pages,
                report.garbage_manifests
            );
        }
        let grace = short_duration(collector.grace);
        Ok(json!({ "report": report, "grace": grace }))
    }

    /// With `delete` false, it only reports what prune would delete.
    pub async fn prune_cache(&self, delete: bool) -> Result<Value> {
        let in_use = self.chunks_in_use().await?;
        let usage = self.cache.usage(&in_use);
        if !delete {
            return Ok(json!({ "usage": usage }));
        }
        let (chunks, bytes) = self.cache.prune(&in_use);
        log::info!("cache: pruned {chunks} chunks");
        Ok(json!({ "usage": usage, "deleted_chunks": chunks, "deleted_bytes": bytes }))
    }

    pub async fn cache_usage(&self) -> Result<Value> {
        Ok(json!(self.cache.usage(&self.chunks_in_use().await?)))
    }

    /// The chunks of the current state of every disk on this node. A disk
    /// whose pages cannot be read keeps the whole cache: nothing is pruned.
    async fn chunks_in_use(&self) -> Result<HashSet<ContentHash>> {
        let disks: Vec<Arc<Disk>> = self.disks.lock().await.values().map(|attached| attached.disk.clone()).collect();
        let mut in_use = HashSet::new();
        for disk in disks {
            in_use.extend(disk.committed_chunks().await?.into_iter().flatten());
        }
        Ok(in_use)
    }

    pub async fn detach_if_attached(self: &Arc<Self>, disk_id: &str) -> Result<()> {
        if self.disks.lock().await.contains_key(disk_id) {
            self.detach(disk_id).await?;
        }
        Ok(())
    }

    /// True when the disk has no data at all, so a mount formats it.
    pub async fn is_blank(&self, disk_id: &str) -> Result<bool> {
        if let Some(attached) = self.disks.lock().await.get(disk_id) {
            return Ok(attached.disk.is_blank());
        }
        Ok(self.catalog().load(disk_id).await?.manifest.is_blank())
    }

    pub async fn status(&self) -> Value {
        let disks = self.disks.lock().await;
        let list: Vec<Value> = disks
            .iter()
            .map(|(id, attached)| {
                json!({
                    "disk": id,
                    "state": if attached.device.is_some() { "attached" } else { "detaching" },
                    "device": attached.device.as_ref().map(|_| &attached.link),
                    "ublk": attached.device.as_ref().map(|device| &device.path),
                    "blank": attached.disk.is_blank(),
                    "recoverable": attached.device.is_some(),
                    "status": attached.disk.status(),
                })
            })
            .collect();
        json!({
            "node": self.node.hostname,
            "node_id": self.node.id,
            "disks": list,
            "s3": self.bucket.request_counts(),
        })
    }

    /// Retries until S3 has everything. Only a lost owner stops it.
    pub(crate) async fn drain(self: Arc<Self>, disk: Arc<Disk>) -> Result<()> {
        let folder = DiskFolder::new(&self.config.data_dir, &disk.id);
        loop {
            match disk.checkpoint().await {
                Ok(()) => break,
                // The local data is kept for an operator in both cases.
                Err(error) if disk.status().ownership_lost || disk.status().sync_failed => {
                    folder.set_status(LocalStatus::Orphaned)?;
                    self.disks.lock().await.remove(&disk.id);
                    return Err(error);
                }
                Err(error) => {
                    log::warn!("disk {}: upload failed, retrying: {error:#}", disk.id);
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
        // A later attach on this node reads them from the cache.
        disk.cache_local_chunks().await;
        // The local folder goes before the marker. A folder without chunks
        // is safe to find after a crash; chunks without our marker are not.
        folder.remove_chunks()?;
        ownership::release(&self.bucket, &disk.id, &self.node).await?;
        folder.remove()?;
        self.disks.lock().await.remove(&disk.id);
        log::info!("disk {}: detached, all data is in S3", disk.id);
        Ok(())
    }

    async fn open_device(
        &self,
        disk_id: &str,
        folder: &DiskFolder,
        loaded: LoadedDisk,
        timings: &mut Timings,
    ) -> Result<(Arc<Disk>, UblkDevice)> {
        folder.set_status(LocalStatus::Attached)?;
        let disk = Arc::new(self.disk_from(disk_id, folder, loaded).await?);
        timings.mark("open_disk");
        let device = start_device(disk.clone()).await?;
        timings.mark("start_device");
        Ok((disk, device))
    }

    pub(crate) async fn open_disk(&self, disk_id: &str, folder: &DiskFolder) -> Result<Disk> {
        let loaded = self.catalog().load(disk_id).await?;
        self.disk_from(disk_id, folder, loaded).await
    }

    async fn disk_from(&self, disk_id: &str, folder: &DiskFolder, loaded: LoadedDisk) -> Result<Disk> {
        let (data, listed) = LocalData::open(
            &folder.dirty_path(),
            &folder.recent_path(),
            loaded.manifest.disk_size,
            loaded.manifest.chunk_size,
        )?;
        Disk::open(DiskParts {
            disk_id: disk_id.to_string(),
            data,
            listed,
            manifest: loaded.manifest,
            manifest_hash: loaded.manifest_hash,
            profile: loaded.head.profile,
            cache: self.cache.clone(),
            page_store: self.page_store.clone(),
            bucket: self.bucket.clone(),
            node: self.node.clone(),
            dirty_limit_bytes: self.config.dirty_limit_bytes(),
            checkpoint_after_bytes: self.config.checkpoint_after_mib << 20,
            max_unsaved_age: Duration::from_secs(self.config.max_unsaved_minutes * 60),
            wait_when_behind: self.config.wait_when_behind,
        })
        .await
    }

    /// Registers a disk with a live device, and starts its checkpoint loop.
    pub(crate) fn activate(
        &self,
        disks: &mut HashMap<String, AttachedDisk>,
        folder: &DiskFolder,
        disk: Arc<Disk>,
        device: UblkDevice,
        prefetch: Prefetch,
    ) -> Result<Value> {
        folder.set_ublk_id(Some(device.id))?;
        let link = link_device(&folder.disk_id, &device.path)?;
        tokio::spawn(run_checkpoints(disk.clone(), self.checkpoint_interval()));
        tokio::spawn(ownership::watch(disk.clone()));
        if let Some(queue) = disk.take_fetch_queue() {
            let startup_chunks = match prefetch.chunks {
                Some(chunks) => chunks,
                None if self.config.prefetch_on_attach => usize::MAX,
                None => 0,
            };
            let plan = PrefetchPlan {
                startup: disk.committed_profile().into_iter().take(startup_chunks).collect(),
                fill: prefetch.fill.unwrap_or(self.config.background_fill),
                parallel: prefetch.parallel,
            };
            tokio::spawn(prefetch::run(disk.clone(), plan, queue));
        }
        let attached = AttachedDisk { disk, device: Some(device), link };
        let result = attached.describe();
        disks.insert(folder.disk_id.clone(), attached);
        Ok(result)
    }

    async fn forget_if_clean(&self, disk_id: &str, folder: &DiskFolder) {
        if folder.has_chunks().unwrap_or(true) {
            return;
        }
        let _ = ownership::release(&self.bucket, disk_id, &self.node).await;
        let _ = folder.remove();
    }

    fn checkpoint_interval(&self) -> Duration {
        Duration::from_secs(self.config.checkpoint_interval_secs)
    }
}

/// A checkpoint starts `checkpoint_interval_secs` after the last one started,
/// or early when `checkpoint_after_mib` of local data waits, or when local
/// space runs low. Only one runs at a time.
async fn run_checkpoints(disk: Arc<Disk>, interval: Duration) {
    let mut last_start = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until((last_start + interval).into()) => {}
            _ = disk.checkpoint_requested() => {}
        }
        if disk.is_closed() {
            return;
        }
        last_start = Instant::now();
        if !disk.has_unsaved_chunks() {
            continue;
        }
        if let Err(error) = disk.background_checkpoint().await {
            log::warn!("disk {}: checkpoint failed: {error:#}", disk.id);
            if disk.status().ownership_lost || disk.status().sync_failed {
                return;
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
        if disk.status().behind {
            log::warn!("disk {}: behind, unsaved data is older than the limit", disk.id);
        }
    }
}

async fn start_device(disk: Arc<Disk>) -> Result<UblkDevice> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || UblkDevice::start(disk, runtime)).await?
}

/// An exclusive open fails while the device is mounted or held by a VM.
/// Right after an unmount, the kernel or udev can hold the device for a
/// moment, so a busy device gets a few more tries.
async fn ensure_not_in_use(device: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    for _ in 0..BUSY_RETRIES {
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(device) {
            Ok(_) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("{device} is in use. Unmount it, or stop the VM or container first")
}

/// "24h", "10m" or "30s": the form that `mica gc --grace` accepts.
fn short_duration(duration: Duration) -> String {
    match duration.as_secs() {
        secs if secs % 3600 == 0 => format!("{}h", secs / 3600),
        secs if secs % 60 == 0 => format!("{}m", secs / 60),
        secs => format!("{secs}s"),
    }
}

/// Time spent in each stage of an attach, for the log and the result.
struct Timings {
    started: Instant,
    last: Instant,
    stages: Vec<(&'static str, Duration)>,
}

impl Timings {
    fn start() -> Self {
        let now = Instant::now();
        Self { started: now, last: now, stages: Vec::new() }
    }

    fn mark(&mut self, stage: &'static str) {
        let now = Instant::now();
        self.stages.push((stage, now - self.last));
        self.last = now;
    }

    fn describe(&self) -> String {
        let mut parts: Vec<String> =
            self.stages.iter().map(|(stage, time)| format!("{stage} {} ms", time.as_millis())).collect();
        parts.push(format!("total {} ms", self.started.elapsed().as_millis()));
        parts.join(", ")
    }

    fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (stage, time) in &self.stages {
            map.insert(stage.to_string(), json!(time.as_secs_f64() * 1000.0));
        }
        map.insert("total".to_string(), json!(self.started.elapsed().as_secs_f64() * 1000.0));
        Value::Object(map)
    }
}
