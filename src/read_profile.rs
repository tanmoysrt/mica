use std::collections::HashSet;
use std::sync::Mutex;

/// The profile covers the first reads of a session, up to this many bytes.
const PROFILE_BYTES: u64 = 64 << 20;

/// The order in which the guest first reads chunks in this session.
/// Prefetch reads are never recorded; if they were, the profile would never change.
pub struct ReadProfile {
    state: Mutex<ProfileState>,
    length: usize,
}

#[derive(Default)]
struct ProfileState {
    seen: HashSet<usize>,
    order: Vec<u32>,
    saved_len: usize,
}

impl ReadProfile {
    pub fn new(chunk_size: u64) -> Self {
        Self { state: Mutex::default(), length: (PROFILE_BYTES / chunk_size).max(16) as usize }
    }

    pub fn record(&self, index: usize) {
        let mut state = self.state.lock().unwrap();
        if state.order.len() < self.length && state.seen.insert(index) {
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
