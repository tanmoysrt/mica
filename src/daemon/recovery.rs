//! At start, the daemon serves again each disk that the node holds, and
//! takes over the devices that the last daemon left paused. A disk that the
//! bucket cannot confirm yet, such as at a boot before the network, is
//! tried again in the background.

use super::{Daemon, check_folder};
use crate::local::{DiskFolder, LocalState};
use crate::owner::{Claim, NodeId};
use crate::store::{Bucket, DiskStore};
use crate::ublk::device;
use anyhow::{Result, bail};
use std::sync::Arc;
use std::time::Duration;

const RECOVER_RETRY: Duration = Duration::from_secs(10);

impl Daemon {
    pub(super) async fn recover(self: &Arc<Self>) {
        for folder in DiskFolder::list(&self.config.data_dir, &self.config.run_dir).unwrap_or_default() {
            if let Err(error) = self.recover_disk(&folder).await {
                log::warn!("disk {}: not served yet, tried again: {error:#}", folder.disk);
                tokio::spawn(self.clone().retry_recover(folder));
            }
        }
    }

    async fn retry_recover(self: Arc<Self>, folder: DiskFolder) {
        loop {
            tokio::time::sleep(RECOVER_RETRY).await;
            match self.recover_disk(&folder).await {
                Ok(()) => return,
                Err(error) => log::warn!("disk {}: not served yet, tried again: {error:#}", folder.disk),
            }
        }
    }

    /// An error means: try again later. The local data is orphaned only
    /// when the bucket says that the node does not hold the disk any more.
    async fn recover_disk(&self, folder: &DiskFolder) -> Result<()> {
        if !folder.exists() || folder.state()? == LocalState::Orphaned {
            return Ok(());
        }
        let paused = folder.ublk()?.filter(|&id| device::exists(id));
        let Some(place) = folder.bucket() else {
            log::error!("disk {}: the folder names no bucket: attach the disk again", folder.disk);
            return remove_paused(paused).await;
        };
        let Some(store) = self.buckets.get(&place) else {
            bail!("this machine has no keys for the bucket {place} any more: give them with `sudo mica bucket add`");
        };
        let store = DiskStore::new(Arc::new(Bucket::connect(&store)?), &folder.disk);
        if let Some(reason) = lost(&store, &self.node).await? {
            folder.set_state(LocalState::Orphaned)?;
            log::error!("disk {}: {reason}: the local data in {} is kept as orphaned", folder.disk, folder.path.display());
            return remove_paused(paused).await;
        }
        let mut claim = Claim::take(store, &self.node).await?;
        if let Err(error) = check_folder(folder, &claim) {
            log::error!("disk {}: {error:#}", folder.disk);
            let _ = claim.release().await;
            return remove_paused(paused).await;
        }
        let disk = self.open(claim, folder).await?;
        let device = self.start_device(&disk, folder, paused).await?;
        log::info!("disk {}: serves {} again", disk.name, device.path);
        self.activate(disk, device, Vec::new()).await?;
        Ok(())
    }
}

/// Why the bucket says that this node does not hold the disk any more.
/// The node gives a disk free only at detach, which deletes the folder.
async fn lost(store: &DiskStore, node: &NodeId) -> Result<Option<String>> {
    let Some(current) = store.head().await? else { return Ok(Some("the disk is gone from the bucket".into())) };
    if current.head.deleted {
        return Ok(Some("the disk is being deleted".into()));
    }
    Ok(match current.head.owner {
        Some(owner) if owner.id == node.id => None,
        Some(owner) => Some(format!("{} took the disk", owner.node)),
        None => Some("another machine took the disk and gave it free".into()),
    })
}

async fn remove_paused(paused: Option<i32>) -> Result<()> {
    if let Some(id) = paused {
        tokio::task::spawn_blocking(move || device::remove_paused(id)).await??;
    }
    Ok(())
}
