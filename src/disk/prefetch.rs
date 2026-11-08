//! Downloads before the guest asks for the data.
//!
//! - Profile: each transaction saves in the head the chunks that the guest
//!   read first in its session, in order. At attach, mica downloads these
//!   chunks first, 8 at a time. So a boot finds its data ready.
//! - Read-ahead: when reads follow each other, mica downloads the next
//!   chunks. The window starts at 1 MiB and doubles up to 8 MiB.
//! - Fill: after the profile, mica downloads the other chunks of the
//!   current version in disk order, until the cache is nearly full.
//!
//! Guest reads always go first, see `transfers.rs`.

use super::Disk;
use crate::transfers::Priority;
use futures::StreamExt;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::unbounded_channel;

const PROFILE_BYTES: u64 = 64 << 20;
const AHEAD_FIRST: u64 = 1 << 20;
const AHEAD_MAX: u64 = 8 << 20;
/// Two reads in a row that continue each other make a sequential reader.
const SEQUENTIAL_AFTER: u32 = 2;
/// A read that starts at most this far after the last one continues it.
const FORWARD_GAP: u64 = 1 << 20;
const PARALLEL: usize = 8;
const FILL_BYTES_IN_FLIGHT: u64 = 8 << 20;

/// The order in which the guest first reads chunks in this session.
pub struct ReadProfile {
    state: Mutex<(HashSet<usize>, Vec<u32>)>,
    length: usize,
}

impl ReadProfile {
    pub fn new(chunk_size: u64) -> Self {
        Self { state: Mutex::default(), length: (PROFILE_BYTES / chunk_size).max(16) as usize }
    }

    pub fn record(&self, index: usize) {
        let mut state = self.state.lock().unwrap();
        let (seen, order) = &mut *state;
        if order.len() < self.length && seen.insert(index) {
            order.push(index as u32);
        }
    }

    /// The profile of this session, or the old one if the guest read nothing yet.
    pub fn current_or(&self, previous: &[u32]) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        if state.1.is_empty() { previous.to_vec() } else { state.1.clone() }
    }
}

/// Finds sequential readers and gives each one a window ahead of it.
#[derive(Default)]
pub struct ReadAhead {
    next_offset: u64,
    streak: u32,
    window: usize,
    /// Chunks up to this one are asked for already.
    until: usize,
}

impl ReadAhead {
    /// Records a read. Returns the chunks to download for it, if any.
    pub fn after_read(&mut self, offset: u64, len: usize, chunk_size: u64, chunks: usize) -> Option<Range<usize>> {
        let last = ((offset + len as u64).saturating_sub(1) / chunk_size) as usize;
        let continues = self.next_offset != 0 && offset >= self.next_offset && offset - self.next_offset <= FORWARD_GAP;
        if continues {
            self.streak += 1;
        } else {
            (self.streak, self.window, self.until) = (0, (AHEAD_FIRST / chunk_size).max(1) as usize, last);
        }
        self.next_offset = offset + len as u64;
        if self.streak < SEQUENTIAL_AFTER {
            return None;
        }
        let target = (last + self.window).min(chunks.saturating_sub(1));
        if target <= self.until {
            return None;
        }
        let start = self.until.max(last) + 1;
        self.until = target;
        self.window = (self.window * 2).min((AHEAD_MAX / chunk_size).max(1) as usize);
        (start <= target).then_some(start..target + 1)
    }
}

impl Disk {
    /// Feeds read-ahead. Cheap and never waits, so the queue threads call it too.
    pub(super) fn note_read(&self, offset: u64, len: usize) {
        let range = self.read_ahead.lock().unwrap().after_read(offset, len, self.chunk_size, self.chunk_count());
        if let (Some(range), Some(sender)) = (range, &*self.ahead.lock().unwrap()) {
            let _ = sender.send(range);
        }
    }

    /// Runs read-ahead, then the profile, then the fill, until the disk closes.
    pub async fn run_prefetch(self: Arc<Self>, profile: Vec<u32>) {
        let (sender, mut queue) = unbounded_channel::<Range<usize>>();
        *self.ahead.lock().unwrap() = Some(sender);
        let disk = self.clone();
        let ahead = tokio::spawn(async move {
            while let Some(range) = queue.recv().await {
                for index in range {
                    let disk = disk.clone();
                    tokio::spawn(async move { drop(disk.fetch(index, Priority::Ahead).await) });
                }
            }
        });
        let started = Instant::now();
        let count = profile.len();
        futures::stream::iter(profile)
            .for_each_concurrent(PARALLEL, |index| {
                let disk = self.clone();
                async move {
                    if let Err(error) = disk.fetch(index as usize, Priority::Ahead).await {
                        log::debug!("disk {}: profile download of chunk {index} failed: {error:#}", disk.name);
                    }
                }
            })
            .await;
        if count > 0 {
            log::info!("disk {}: profile of {count} chunks ready in {} ms", self.name, started.elapsed().as_millis());
        }
        self.fill().await;
        let _ = ahead.await;
    }

    /// Downloads every chunk of the current version that the cache does
    /// not have, in disk order, until the cache is nearly full.
    async fn fill(&self) {
        let started = Instant::now();
        let root = self.root.lock().unwrap().clone();
        let in_flight = (FILL_BYTES_IN_FLIGHT / self.chunk_size).clamp(1, 32) as usize;
        let mut fetched = 0usize;
        for (page, entry) in root.pages.iter().enumerate() {
            if self.is_closed() || self.cache.nearly_full() {
                break;
            }
            if entry.is_zero() {
                continue;
            }
            let Ok(chunks) = self.store.page(entry, page).await else { break };
            let wanted: Vec<usize> =
                chunks.0.iter().enumerate().filter(|(_, chunk)| !chunk.is_zero() && !self.cache.has(&chunk.hash)).map(|(offset, _)| offset).collect();
            fetched += futures::stream::iter(wanted)
                .map(|offset| async move {
                    if self.is_closed() || self.cache.nearly_full() {
                        return 0;
                    }
                    let index = page * crate::store::format::CHUNKS_PER_PAGE + offset;
                    usize::from(self.fetch(index, Priority::Fill).await.is_ok())
                })
                .buffer_unordered(in_flight)
                .fold(0, |total, done| async move { total + done })
                .await;
        }
        if fetched > 0 {
            log::info!("disk {}: fill downloaded {fetched} chunks in {} s", self.name, started.elapsed().as_secs());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 256 << 10;

    #[test]
    fn random_reads_download_nothing_ahead() {
        let mut ahead = ReadAhead::default();
        for offset in [10 * CHUNK, 3 * CHUNK, 50 * CHUNK, 7 * CHUNK] {
            assert_eq!(ahead.after_read(offset, 4096, CHUNK, 1000), None);
        }
    }

    #[test]
    fn a_sequential_reader_gets_a_growing_window() {
        let mut ahead = ReadAhead::default();
        let (len, mut offset, mut ranges) = (128 << 10, 100 * CHUNK, Vec::new());
        for _ in 0..64 {
            ranges.extend(ahead.after_read(offset, len, CHUNK, 10_000));
            offset += len as u64;
        }
        assert_eq!(ranges[0], 102..106);
        assert!(ranges.windows(2).all(|pair| pair[0].end == pair[1].start));
        assert!(ranges.last().unwrap().end <= ((offset - 1) / CHUNK) as usize + 33);
    }
}
