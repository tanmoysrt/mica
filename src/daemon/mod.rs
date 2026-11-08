//! The mica daemon: one for each node.
//!
//! The daemon serves the disks that are attached on the node. The CLI and
//! any controller talk to it on a Unix socket (see `api.rs`). Each request
//! that needs the store carries the credentials of the bucket.
//!
//! Attach: claim the disk, open its local data, make the ublk device, and
//! start the background work: transactions, write-back and downloads.
//!
//! Detach: remove the device, run transactions until no chunk is unsaved,
//! release the claim, and delete the local folder. The cache stays.
//!
//! Restart: mica keeps each device paused when it exits. The next daemon
//! takes it over with the credentials in `/run`. The guest sees a pause.
//!
//! One daemon only on a data folder: the daemon locks it, and its run folder,
//! before it reads them. Two daemons on one folder would punch out the
//! chunks of each other. A stopped daemon can still run its last
//! transactions, so the next daemon must wait for its exit.

pub mod api;
mod operations;

use crate::config::Config;
use crate::disk::{Disk, TransactionKind};
use crate::local::{DiskFolder, LocalState, NodeCache};
use crate::owner::{Claim, NodeId};
use crate::store::{Bucket, DiskStore, StoreConfig, validate_name};
use crate::transfers::Transfers;
use crate::ublk::{UblkDevice, device};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DETACH_RETRY: Duration = Duration::from_secs(10);
const LAST_TRANSACTION_LIMIT: Duration = Duration::from_secs(60);

pub struct Daemon {
    config: Config,
    node: NodeId,
    cache: Arc<NodeCache>,
    transfers: Arc<Transfers>,
    disks: Mutex<HashMap<String, Attached>>,
    /// One attach or detach of each disk at a time.
    locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Held until the process exits.
    _folder_locks: [File; 2],
}

struct Attached {
    disk: Arc<Disk>,
    /// `None` while the disk detaches.
    device: Option<UblkDevice>,
    link: String,
}

impl Daemon {
    pub async fn start(config: Config) -> Result<Arc<Self>> {
        let folder_locks = [lock_folder(&config.data_dir)?, lock_folder(&config.run_dir)?];
        let daemon = Arc::new(Self {
            node: NodeId::load(&config.data_dir)?,
            cache: NodeCache::load(config.data_dir.join("cache"))?,
            transfers: Transfers::new(),
            config,
            disks: Mutex::default(),
            locks: Default::default(),
            _folder_locks: folder_locks,
        });
        log::info!("node {} ({})", daemon.node.hostname, daemon.node.id);
        daemon.recover().await;
        Ok(daemon)
    }

    /// Attaches a disk on this node, and returns its device. A disk that is
    /// attached here already gives its device again.
    pub async fn attach(&self, store: &StoreConfig, name: &str, force: bool, fill: bool) -> Result<Value> {
        validate_name(name)?;
        let lock = self.lock(name);
        let _one_at_a_time = lock.lock().await;
        if let Some(attached) = self.disks.lock().await.get(name) {
            return match &attached.device {
                Some(_) => Ok(json!({ "device": attached.link, "blank": attached.disk.is_blank() })),
                None => bail!("disk {name} is detaching"),
            };
        }
        let folder = self.folder(name);
        if folder.exists() && folder.state()? == LocalState::Orphaned {
            bail!("disk {name} has orphaned local data in {}: copy it out, then delete the folder", folder.path.display());
        }
        let store_handle = DiskStore::new(Arc::new(Bucket::connect(store)?), name);
        let mut claim = Claim::take(store_handle, &self.node, force).await?;
        if let Err(error) = check_folder(&folder, &claim) {
            let _ = claim.release().await;
            return Err(error);
        }
        folder.create(&claim.head().id)?;
        folder.save_store(store)?;
        let disk = self.open(claim, &folder).await?;
        let profile = disk.claim().await.head().profile.clone();
        let device = match self.start_device(&disk, &folder, None).await {
            Ok(device) => device,
            Err(error) => {
                disk.close();
                let _ = disk.claim().await.release().await;
                return Err(error);
            }
        };
        let blank = disk.is_blank();
        let link = self.activate(disk, device, profile, fill).await?;
        Ok(json!({ "device": link, "blank": blank }))
    }

    /// Removes the device, saves all local data in the store, and releases
    /// the disk. When it returns, any node can attach the disk.
    pub async fn detach(self: &Arc<Self>, name: &str) -> Result<Value> {
        let lock = self.lock(name);
        let _one_at_a_time = lock.lock().await;
        let (disk, device) = {
            let mut disks = self.disks.lock().await;
            let Some(attached) = disks.get_mut(name) else { bail!("disk {name} is not attached on this node") };
            if attached.device.as_ref().is_some_and(|device| device::in_use(&device.path)) {
                bail!("disk {name} is in use: unmount it, or stop the program that uses it");
            }
            (attached.disk.clone(), attached.device.take())
        };
        if let Some(device) = device {
            tokio::task::spawn_blocking(move || device.stop()).await??;
        }
        device::unlink(name);
        // A separate task, so the upload goes on when the client goes away.
        tokio::spawn(self.clone().drain(disk)).await??;
        Ok(json!({ "detached": name }))
    }

    /// Uploads all local data, then releases the disk.
    async fn drain(self: Arc<Self>, disk: Arc<Disk>) -> Result<()> {
        let folder = self.folder(&disk.name);
        disk.close();
        loop {
            match disk.transaction(TransactionKind::All, None).await {
                Ok(_) if !disk.has_unsaved() => break,
                Ok(_) => {}
                Err(error) if disk.is_stopped() => {
                    folder.set_state(LocalState::Orphaned)?;
                    self.disks.lock().await.remove(&disk.name);
                    return Err(error);
                }
                Err(error) => {
                    log::warn!("disk {}: upload at detach failed, retried: {error:#}", disk.name);
                    tokio::time::sleep(DETACH_RETRY).await;
                }
            }
        }
        disk.cache_saved().await;
        disk.claim().await.release().await?;
        folder.remove()?;
        self.disks.lock().await.remove(&disk.name);
        log::info!("disk {}: detached, the store has all its data", disk.name);
        Ok(())
    }

    /// At start: takes over the devices that the last daemon left paused.
    /// A disk whose credentials went away with a reboot waits for the next
    /// `mica disk attach`, which goes on from its local data.
    async fn recover(&self) {
        let folders = DiskFolder::list(&self.config.data_dir, &self.config.run_dir).unwrap_or_default();
        for folder in folders {
            if let Err(error) = self.recover_disk(&folder).await {
                log::error!("disk {}: not served after the restart: {error:#}", folder.disk);
            }
        }
    }

    async fn recover_disk(&self, folder: &DiskFolder) -> Result<()> {
        if !folder.exists() || folder.state()? == LocalState::Orphaned {
            return Ok(());
        }
        let paused = folder.ublk()?.filter(|&id| device::exists(id));
        let Some(store) = folder.saved_store() else {
            if let Some(id) = paused {
                tokio::task::spawn_blocking(move || device::remove_paused(id)).await??;
            }
            log::info!("disk {}: local data waits for `mica disk attach`", folder.disk);
            return Ok(());
        };
        let claim = match Claim::take(DiskStore::new(Arc::new(Bucket::connect(&store)?), &folder.disk), &self.node, false).await {
            Ok(claim) => claim,
            Err(error) => {
                if let Some(id) = paused {
                    tokio::task::spawn_blocking(move || device::remove_paused(id)).await??;
                }
                folder.set_state(LocalState::Orphaned)?;
                return Err(error.context("the local data is kept as orphaned"));
            }
        };
        if let Err(error) = check_folder(folder, &claim) {
            if let Some(id) = paused {
                tokio::task::spawn_blocking(move || device::remove_paused(id)).await??;
            }
            return Err(error);
        }
        let disk = self.open(claim, folder).await?;
        let device = self.start_device(&disk, folder, paused).await?;
        log::info!("disk {}: serves {} again after the restart", disk.name, device.path);
        self.activate(disk, device, Vec::new(), true).await?;
        Ok(())
    }

    /// At exit: each device stays paused for the next daemon, and each disk
    /// runs one last transaction.
    pub async fn shut_down(&self) {
        let disks = self.disks.lock().await;
        let mut last = Vec::new();
        for attached in disks.values() {
            if let Some(device) = &attached.device {
                device.hand_over();
            }
            last.push(attached.disk.clone());
        }
        let transactions = futures::future::join_all(last.iter().map(|disk| disk.transaction(TransactionKind::All, None)));
        match tokio::time::timeout(LAST_TRANSACTION_LIMIT, transactions).await {
            Ok(results) => results.iter().filter_map(|result| result.as_ref().err()).for_each(|error| log::warn!("last transaction failed: {error:#}")),
            Err(_) => log::warn!("the last transactions took too long; the local data stays"),
        }
    }

    pub async fn status(&self) -> Value {
        let disks = self.disks.lock().await;
        let mut list: Vec<Value> = disks
            .iter()
            .map(|(name, attached)| {
                json!({ "disk": name, "device": attached.link, "detaching": attached.device.is_none(), "status": attached.disk.status() })
            })
            .collect();
        list.sort_by(|a, b| a["disk"].as_str().cmp(&b["disk"].as_str()));
        json!({ "node": self.node.hostname, "disks": list })
    }

    async fn open(&self, claim: Claim, folder: &DiskFolder) -> Result<Arc<Disk>> {
        let limits = claim.head().limits;
        let cache = self.cache.for_disk(&folder.disk, limits.cache_bytes);
        Disk::open(claim, folder, cache, self.transfers.clone(), self.config.tuning()).await.context("open the local data")
    }

    async fn start_device(&self, disk: &Arc<Disk>, folder: &DiskFolder, paused: Option<i32>) -> Result<UblkDevice> {
        let (disk, runtime) = (disk.clone(), tokio::runtime::Handle::current());
        let device = tokio::task::spawn_blocking(move || match paused {
            Some(id) => UblkDevice::recover(disk, runtime, id),
            None => UblkDevice::start(disk, runtime),
        })
        .await??;
        folder.set_ublk(Some(device.id))?;
        Ok(device)
    }

    async fn activate(&self, disk: Arc<Disk>, device: UblkDevice, profile: Vec<u32>, fill: bool) -> Result<String> {
        let link = device::link(&disk.name, &device.path)?;
        tokio::spawn(disk.clone().run_transactions());
        tokio::spawn(disk.clone().run_writeback());
        tokio::spawn(disk.clone().run_prefetch(profile, fill));
        log::info!("disk {}: attached as {}", disk.name, device.path);
        let name = disk.name.clone();
        self.disks.lock().await.insert(name, Attached { disk, device: Some(device), link: link.clone() });
        Ok(link)
    }

    async fn attached(&self, name: &str) -> Option<Arc<Disk>> {
        self.disks.lock().await.get(name).map(|attached| attached.disk.clone())
    }

    fn folder(&self, name: &str) -> DiskFolder {
        DiskFolder::new(&self.config.data_dir, &self.config.run_dir, name)
    }

    fn lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.locks.lock().unwrap().entry(name.to_string()).or_default().clone()
    }
}

/// Local data of another disk with the same name: from another bucket, or
/// from a deleted disk. It is kept as orphaned, and never used.
fn check_folder(folder: &DiskFolder, claim: &Claim) -> Result<()> {
    if folder.exists() && folder.disk_id()? != claim.head().id {
        folder.set_state(LocalState::Orphaned)?;
        bail!("the local data in {} is of another disk {}: it is kept as orphaned", folder.path.display(), folder.disk);
    }
    Ok(())
}

/// Makes the folder and locks it for this process.
fn lock_folder(folder: &Path) -> Result<File> {
    std::fs::create_dir_all(folder).with_context(|| format!("create {}", folder.display()))?;
    let file = File::create(folder.join("daemon.lock"))?;
    file.try_lock().map_err(|_| anyhow::anyhow!("another mica daemon uses {}", folder.display()))?;
    Ok(file)
}
