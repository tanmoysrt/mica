use crate::disk::Disk;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const PROFILE_LENGTH: usize = 512;

/// The order in which the guest first reads chunks in this session.
/// Prefetch reads are never recorded; if they were, the profile would never change.
pub struct ReadProfile {
    state: Mutex<ProfileState>,
}

#[derive(Default)]
struct ProfileState {
    seen: HashSet<usize>,
    order: Vec<u32>,
    saved_len: usize,
}

impl ReadProfile {
    pub fn new() -> Self {
        Self { state: Mutex::default() }
    }

    pub fn record(&self, index: usize) {
        let mut state = self.state.lock().unwrap();
        if state.order.len() < PROFILE_LENGTH && state.seen.insert(index) {
            state.order.push(index as u32);
        }
    }

    pub fn has_unsaved_reads(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.order.len() > state.saved_len
    }

    /// The profile of this session, or the old one if the guest read nothing yet.
    pub fn current_or(&self, previous: &[u32]) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        if state.order.is_empty() { previous.to_vec() } else { state.order.clone() }
    }

    pub fn mark_saved(&self, len: usize) {
        let mut state = self.state.lock().unwrap();
        state.saved_len = state.saved_len.max(len.min(state.order.len()));
    }
}

/// Fetches chunks from the last profile, in order. Guest reads do not wait
/// for this; they fetch their own chunks.
pub async fn prefetch(disk: Arc<Disk>, profile: Vec<u32>, parallel_downloads: usize) {
    let total = profile.len();
    futures::stream::iter(profile)
        .take_while(|_| std::future::ready(!disk.is_closed()))
        .for_each_concurrent(parallel_downloads.max(1), |index| {
            let disk = disk.clone();
            async move {
                if let Err(error) = disk.prefetch_chunk(index as usize).await {
                    log::warn!("disk {}: prefetch of chunk {index} failed: {error:#}", disk.id);
                }
            }
        })
        .await;
    log::info!("disk {}: prefetch of {total} chunks done", disk.id);
}
