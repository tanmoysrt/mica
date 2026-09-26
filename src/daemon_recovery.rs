use crate::daemon::Daemon;
use crate::local_state::{DiskFolder, LocalStatus};
use crate::ownership::{self, Owner};
use crate::ublk_device::{UblkDevice, device_exists, remove_paused_device};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const LAST_CHECKPOINT_LIMIT: Duration = Duration::from_secs(60);

impl Daemon {
    /// After a restart: takes back devices the last daemon left paused, and
    /// finishes the detach of disks whose device is gone (after a reboot).
    pub(crate) async fn recover_disks(self: &Arc<Self>) -> Result<()> {
        for folder in DiskFolder::list(&self.config.data_dir)? {
            if let Err(error) = self.recover_disk(&folder).await {
                log::error!("disk {}: recovery failed: {error:#}", folder.disk_id);
            }
        }
        Ok(())
    }

    /// On SIGTERM: one last checkpoint per disk, then leave the devices paused.
    /// The kernel holds their I/O until the next daemon takes them over.
    pub async fn shut_down(&self) {
        // Held until exit, so no attach or detach starts now.
        let disks = self.disks.lock().await;
        let mut paused = 0;
        for (disk_id, attached) in disks.iter() {
            let Some(device) = &attached.device else { continue };
            match tokio::time::timeout(LAST_CHECKPOINT_LIMIT, attached.disk.checkpoint()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("disk {disk_id}: last checkpoint failed: {error:#}"),
                Err(_) => log::warn!("disk {disk_id}: last checkpoint took too long; local data stays on the SSD"),
            }
            device.hand_over();
            paused += 1;
        }
        if paused > 0 {
            log::warn!("{paused} devices are paused until mica starts again");
        }
    }

    async fn recover_disk(self: &Arc<Self>, folder: &DiskFolder) -> Result<()> {
        let disk_id = &folder.disk_id;
        if folder.status()? == LocalStatus::Orphaned {
            log::warn!("disk {disk_id}: orphaned local data in {}", folder.path.display());
            return Ok(());
        }
        let owner = ownership::owner_of(&self.bucket, disk_id, &self.node).await?;
        if let Some(id) = folder.ublk_id()?.filter(|id| device_exists(*id)) {
            if matches!(owner, Owner::ThisNode) {
                match self.take_over(folder, id).await {
                    Ok(()) => return Ok(()),
                    Err(error) => log::error!("disk {disk_id}: cannot take over /dev/ublkb{id}: {error:#}"),
                }
            }
            // Nobody will serve this device. Remove it, so its users get
            // I/O errors instead of waiting forever.
            tokio::task::spawn_blocking(move || remove_paused_device(id)).await??;
            folder.set_ublk_id(None)?;
        }
        if !folder.has_chunks()? {
            if let Owner::ThisNode = owner {
                ownership::release(&self.bucket, disk_id, &self.node).await?;
            }
            return folder.remove();
        }
        if !matches!(owner, Owner::ThisNode) {
            folder.set_status(LocalStatus::Orphaned)?;
            log::error!("disk {disk_id}: another node owns it now. Local data kept in {}", folder.path.display());
            return Ok(());
        }
        log::info!("disk {disk_id}: uploading data left by the last run");
        let disk = Arc::new(self.open_disk(disk_id, folder).await?);
        self.disks.lock().await.insert(disk_id.clone(), crate::daemon::AttachedDisk {
            disk: disk.clone(),
            device: None,
            link: String::new(),
        });
        tokio::spawn(self.clone().drain(disk));
        Ok(())
    }

    /// The device kept its data in the page cache and the dirty folder, so the
    /// disk opens from the same local files and nothing is lost.
    async fn take_over(self: &Arc<Self>, folder: &DiskFolder, id: i32) -> Result<()> {
        let disk = Arc::new(self.open_disk(&folder.disk_id, folder).await?);
        let runtime = tokio::runtime::Handle::current();
        let device_disk = disk.clone();
        let device = tokio::task::spawn_blocking(move || UblkDevice::recover(device_disk, runtime, id)).await??;
        log::info!("disk {}: took over {} from the last run", folder.disk_id, device.path);
        let mut disks = self.disks.lock().await;
        self.activate(&mut disks, folder, disk, device)?;
        Ok(())
    }
}
