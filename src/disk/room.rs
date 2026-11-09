//! The unsaved limit. Writes wait at it, and a transaction makes room.

use super::Disk;
use crate::store::Limits;
use anyhow::Result;
use std::time::Duration;

impl Disk {
    pub fn set_limits(&self, limits: Limits) {
        *self.limits.lock().unwrap() = limits;
        self.cache.set_limit(limits.cache_bytes);
        self.room.notify_waiters();
    }

    /// Behind: the oldest unsaved write missed three transactions.
    pub fn is_behind(&self) -> bool {
        self.oldest_unsaved.lock().unwrap().is_some_and(|since| since.elapsed() > 3 * self.interval())
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.limits.lock().unwrap().interval_seconds.max(1))
    }

    pub fn has_unsaved(&self) -> bool {
        !self.unsaved.lock().unwrap().is_empty()
    }

    pub(super) fn unsaved_limit(&self) -> usize {
        (self.limits.lock().unwrap().unsaved_bytes / self.chunk_size).max(2) as usize
    }

    /// Unsaved data is high or old: a background transaction takes all chunks.
    pub(super) fn under_pressure(&self) -> bool {
        self.unsaved.lock().unwrap().len() >= self.unsaved_limit() / 2 || self.is_behind()
    }

    /// A write into an unsaved chunk adds no unsaved data, so it always has room.
    pub(super) fn has_room_for(&self, index: usize) -> bool {
        let limit = self.unsaved_limit();
        let unsaved = self.unsaved.lock().unwrap();
        unsaved.len() < limit || unsaved.contains(&index)
    }

    /// Writes wait at the unsaved limit until a transaction makes room. The
    /// guest sees a slow disk, not an error.
    pub(super) async fn wait_for_room(&self, index: usize) -> Result<()> {
        loop {
            let room = self.room.notified();
            self.ensure_usable()?;
            if self.has_room_for(index) {
                return Ok(());
            }
            self.transaction_wanted.notify_one();
            room.await;
        }
    }
}
