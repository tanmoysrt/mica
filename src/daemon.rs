use crate::bucket::Bucket;
use crate::catalog::{Catalog, validate_name};
use crate::chunk_cache::ChunkCache;
use crate::config::Config;
use crate::dirty_folder::DirtyFolder;
use crate::gc::GarbageCollector;
use crate::disk::{Disk, DiskParts};
use crate::local_state::{DiskFolder, LocalStatus};
use crate::node::NodeIdentity;
use crate::ownership;
use crate::read_profile::prefetch;
use crate::ublk_device::{UblkDevice, link_device, unlink_device};
use anyhow::{Result, bail};
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

pub struct Daemon {
    pub(crate) config: Config,
    pub(crate) config_path: PathBuf,
    pub(crate) bucket: Arc<Bucket>,
    cache: Arc<ChunkCache>,
    pub(crate) node: Arc<NodeIdentity>,
    pub(crate) disks: Mutex<HashMap<String, AttachedDisk>>,
    gc_running: Mutex<()>,
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
        std::fs::create_dir_all(&config.data_dir)?;
        let bucket = Arc::new(Bucket::connect(&config.s3)?);
        let cache = ChunkCache::load(config.data_dir.join("cache"), config.cache_limit_bytes(), bucket.clone())?;
        let node = Arc::new(NodeIdentity::load_or_create(&config.data_dir)?);
        log::info!("node {} (id {})", node.hostname, node.id);
        let daemon = Arc::new(Self {
            config,
            config_path,
            bucket,
            cache: Arc::new(cache),
            node,
            disks: Mutex::default(),
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
    pub async fn attach(&self, disk_id: &str, force: bool) -> Result<Value> {
        validate_name(disk_id)?;
        let mut disks = loop {
            let disks = self.disks.lock().await;
            match disks.get(disk_id) {
                None => break disks,
                Some(attached) if attached.device.is_some() => return Ok(attached.describe()),
                Some(_) => {}
            }
            drop(disks);
            tokio::time::sleep(DRAIN_POLL).await;
        };
        // Claim first, then load: a head loaded before the claim could be
        // replaced by the final commit of the previous owner.
        ownership::claim(&self.bucket, disk_id, &self.node, force).await?;
        let folder = DiskFolder::new(&self.config.data_dir, disk_id);
        let (disk, device) = match self.open_device(disk_id, &folder).await {
            Ok(opened) => opened,
            Err(error) => {
                self.forget_if_clean(disk_id, &folder).await;
                return Err(error);
            }
        };
        log::info!("disk {disk_id}: attached as {}", device.path);
        tokio::spawn(prefetch(disk.clone(), disk.committed_profile()));
        self.activate(&mut disks, &folder, disk, device)
    }

    /// Removes the device, then uploads all local data and releases the disk.
    /// It returns only when S3 has everything, so success means the disk is free.
    pub async fn detach(self: &Arc<Self>, disk_id: &str) -> Result<Value> {
        let disk = {
            let mut disks = self.disks.lock().await;
            let Some(attached) = disks.get_mut(disk_id) else { bail!("disk {disk_id} is not attached on this node") };
            let Some(device) = attached.device.take() else { bail!("disk {disk_id} is already detaching") };
            if let Err(error) = ensure_not_in_use(&device.path).await {
                attached.device = Some(device);
                return Err(error);
            }
            tokio::task::spawn_blocking(move || device.stop()).await??;
            unlink_device(&attached.link);
            // The checkpoint loop and the prefetch end on their own. Aborting
            // them in the middle of a freeze would break the chunk state.
            attached.disk.close();
            attached.disk.clone()
        };
        // A separate task, so the upload goes on even if the client disconnects.
        tokio::spawn(self.clone().drain(disk)).await??;
        Ok(json!({ "detached": disk_id }))
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
            log::info!("gc: deleted {} chunks and {} manifests", report.garbage_chunks, report.garbage_manifests);
        }
        let grace = short_duration(collector.grace);
        Ok(json!({ "report": report, "grace": grace }))
    }

    /// With `delete` false, it only reports what prune would delete.
    pub async fn prune_cache(&self, delete: bool) -> Result<Value> {
        let in_use = self.chunks_in_use().await;
        let usage = self.cache.usage(&in_use);
        if !delete {
            return Ok(json!({ "usage": usage }));
        }
        let (chunks, bytes) = self.cache.prune(&in_use);
        log::info!("cache: pruned {chunks} chunks");
        Ok(json!({ "usage": usage, "deleted_chunks": chunks, "deleted_bytes": bytes }))
    }

    pub async fn cache_usage(&self) -> Value {
        json!(self.cache.usage(&self.chunks_in_use().await))
    }

    /// The chunks of the current state of every disk on this node.
    async fn chunks_in_use(&self) -> HashSet<ContentHash> {
        let disks = self.disks.lock().await;
        disks.values().flat_map(|attached| attached.disk.committed_chunks()).collect()
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
        // The local folder goes before the marker. A folder without chunks
        // is safe to find after a crash; chunks without our marker are not.
        folder.remove_chunks()?;
        ownership::release(&self.bucket, &disk.id, &self.node).await?;
        folder.remove()?;
        self.disks.lock().await.remove(&disk.id);
        log::info!("disk {}: detached, all data is in S3", disk.id);
        Ok(())
    }

    async fn open_device(&self, disk_id: &str, folder: &DiskFolder) -> Result<(Arc<Disk>, UblkDevice)> {
        folder.set_status(LocalStatus::Attached)?;
        let disk = Arc::new(self.open_disk(disk_id, folder).await?);
        let device = start_device(disk.clone()).await?;
        Ok((disk, device))
    }

    pub(crate) async fn open_disk(&self, disk_id: &str, folder: &DiskFolder) -> Result<Disk> {
        let loaded = self.catalog().load(disk_id).await?;
        Disk::open(DiskParts {
            disk_id: disk_id.to_string(),
            folder: DirtyFolder::open(folder.dirty_path(), loaded.manifest.chunk_size)?,
            manifest: loaded.manifest,
            manifest_hash: loaded.manifest_hash,
            profile: loaded.head.profile,
            cache: self.cache.clone(),
            bucket: self.bucket.clone(),
            node: self.node.clone(),
            dirty_limit_bytes: self.config.dirty_limit_bytes(),
            max_unsaved_age: Duration::from_secs(self.config.max_unsaved_minutes * 60),
            wait_when_behind: self.config.wait_when_behind,
        })
    }

    /// Registers a disk with a live device, and starts its checkpoint loop.
    pub(crate) fn activate(
        &self,
        disks: &mut HashMap<String, AttachedDisk>,
        folder: &DiskFolder,
        disk: Arc<Disk>,
        device: UblkDevice,
    ) -> Result<Value> {
        folder.set_ublk_id(Some(device.id))?;
        let link = link_device(&folder.disk_id, &device.path)?;
        tokio::spawn(run_checkpoints(disk.clone(), self.checkpoint_interval()));
        tokio::spawn(ownership::watch(disk.clone()));
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

/// A checkpoint starts 3 minutes after the last one started, or early when
/// local space runs low. Only one runs at a time.
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
        if !disk.has_local_chunks() {
            continue;
        }
        if let Err(error) = disk.checkpoint().await {
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
