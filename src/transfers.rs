//! Limits on the transfers of the whole node, over all disks.
//!
//! Without limits, many busy disks overload the store, and the small
//! requests, such as the head writes, time out. Downloads have three
//! priorities, so that work ahead of the guest never slows the guest:
//!
//! 1. Read: a guest waits for the chunk.
//! 2. Ahead: read-ahead and the startup profile. These start only while no
//!    read waits for a slot, and while fewer than 8 reads download.
//! 3. Fill: the rest of the disk. These start only when no read and no
//!    ahead download ran in the last 250 ms.

use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const DOWNLOADS: usize = 128;
const AHEAD: usize = 16;
const FILL: usize = 32;
const UPLOADS: usize = 64;
const BUSY_READS: usize = 8;
const QUIET: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    Read,
    Ahead,
    Fill,
}

pub struct Transfers {
    downloads: Arc<Semaphore>,
    ahead: Arc<Semaphore>,
    fill: Arc<Semaphore>,
    uploads: Arc<Semaphore>,
    reads_waiting: AtomicUsize,
    reads_active: AtomicUsize,
    ahead_active: AtomicUsize,
    last_busy: Mutex<Instant>,
}

/// A permit for one transfer. It frees its slot when it drops.
pub struct Slot {
    transfers: Arc<Transfers>,
    priority: Option<Priority>,
    _permits: Vec<OwnedSemaphorePermit>,
}

impl Transfers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            downloads: Arc::new(Semaphore::new(DOWNLOADS)),
            ahead: Arc::new(Semaphore::new(AHEAD)),
            fill: Arc::new(Semaphore::new(FILL)),
            uploads: Arc::new(Semaphore::new(UPLOADS)),
            reads_waiting: AtomicUsize::new(0),
            reads_active: AtomicUsize::new(0),
            ahead_active: AtomicUsize::new(0),
            last_busy: Mutex::new(Instant::now()),
        })
    }

    pub async fn upload(self: &Arc<Self>) -> Result<Slot> {
        let permit = self.uploads.clone().acquire_owned().await?;
        Ok(Slot { transfers: self.clone(), priority: None, _permits: vec![permit] })
    }

    pub async fn download(self: &Arc<Self>, priority: Priority) -> Result<Slot> {
        let mut permits = Vec::new();
        match priority {
            Priority::Read => {
                self.reads_waiting.fetch_add(1, Ordering::SeqCst);
                let permit = self.downloads.clone().acquire_owned().await;
                self.reads_waiting.fetch_sub(1, Ordering::SeqCst);
                permits.push(permit?);
            }
            Priority::Ahead => {
                permits.push(self.ahead.clone().acquire_owned().await?);
                while self.reads_waiting.load(Ordering::SeqCst) > 0 || self.reads_active.load(Ordering::SeqCst) >= BUSY_READS {
                    tokio::time::sleep(POLL).await;
                }
                permits.push(self.downloads.clone().acquire_owned().await?);
            }
            Priority::Fill => {
                permits.push(self.fill.clone().acquire_owned().await?);
                while !self.quiet() {
                    tokio::time::sleep(POLL).await;
                }
                permits.push(self.downloads.clone().acquire_owned().await?);
            }
        }
        match priority {
            Priority::Read => drop(self.reads_active.fetch_add(1, Ordering::SeqCst)),
            Priority::Ahead => drop(self.ahead_active.fetch_add(1, Ordering::SeqCst)),
            Priority::Fill => {}
        }
        if priority != Priority::Fill {
            *self.last_busy.lock().unwrap() = Instant::now();
        }
        Ok(Slot { transfers: self.clone(), priority: Some(priority), _permits: permits })
    }

    fn quiet(&self) -> bool {
        let idle = self.reads_active.load(Ordering::SeqCst) + self.ahead_active.load(Ordering::SeqCst) == 0
            && self.reads_waiting.load(Ordering::SeqCst) == 0;
        idle && self.last_busy.lock().unwrap().elapsed() >= QUIET
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let transfers = &self.transfers;
        match self.priority {
            Some(Priority::Read) => drop(transfers.reads_active.fetch_sub(1, Ordering::SeqCst)),
            Some(Priority::Ahead) => drop(transfers.ahead_active.fetch_sub(1, Ordering::SeqCst)),
            _ => {}
        }
        if matches!(self.priority, Some(Priority::Read | Priority::Ahead)) {
            *transfers.last_busy.lock().unwrap() = Instant::now();
        }
    }
}
