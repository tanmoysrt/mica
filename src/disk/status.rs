//! What `mica status` shows of an attached disk.

use super::Disk;
use crate::store::Limits;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Serialize)]
pub struct Status {
    pub size: u64,
    pub chunk_size: u64,
    pub transaction: u64,
    /// When the last transaction ended, in Unix seconds. 0 for none.
    pub saved_time: u64,
    pub unsaved_bytes: u64,
    /// The part of `unsaved_bytes` that the running transaction has uploaded already.
    pub staged_bytes: u64,
    pub local_bytes: u64,
    pub cache_bytes: u64,
    pub limits: Limits,
    pub behind: bool,
    pub stopped: Option<String>,
    /// Requests and bytes to the bucket since the attach: they set the bill.
    pub requests: serde_json::Value,
    pub io: serde_json::Value,
}

/// Where the time of writes and flushes goes, for `mica status --json`.
#[derive(Debug, Default, Serialize)]
pub struct IoStats {
    /// Chunks made local for a write that covers only part of the chunk.
    pub chunk_copies: AtomicU64,
    pub chunk_copy_micros: AtomicU64,
    pub flushes: AtomicU64,
    pub data_sync_micros: AtomicU64,
    pub log_append_micros: AtomicU64,
}

impl IoStats {
    pub fn add(counter: &AtomicU64, started: std::time::Instant) {
        counter.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

impl Disk {
    pub fn status(&self) -> Status {
        // Each lock alone: a guard in the struct expression would live to its end.
        let saved_time = self.root.lock().unwrap().time;
        let unsaved_bytes = self.unsaved.lock().unwrap().len() as u64 * self.chunk_size;
        let local_bytes = self.local.lock().unwrap().len() as u64 * self.chunk_size;
        let limits = *self.limits.lock().unwrap();
        let stopped = self.stopped.lock().unwrap().clone();
        Status {
            size: self.size,
            chunk_size: self.chunk_size,
            transaction: self.version.load(Ordering::SeqCst),
            saved_time,
            unsaved_bytes,
            staged_bytes: self.staged_bytes.load(Ordering::Relaxed).min(unsaved_bytes),
            local_bytes,
            cache_bytes: self.cache.usage().bytes,
            limits,
            behind: self.is_behind(),
            stopped,
            requests: serde_json::to_value(&self.store.bucket.counts).unwrap_or_default(),
            io: serde_json::to_value(&self.stats).unwrap_or_default(),
        }
    }
}
