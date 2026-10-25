use crate::chunk_cache::Fetch;
use crate::disk::Disk;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedReceiver;

/// Downloads that run ahead of the guest, for one disk: read-ahead for
/// sequential reads, the startup profile at attach, then the background
/// fill. All of them are fetches: they start only while no read waits.
const DEFAULT_DISK_FETCHES: usize = 8;
/// Fill bytes in flight for one disk: enough to fill a 100 MiB/s link at a
/// round trip of 20 ms, and little enough that a new read waits at most for
/// these to arrive. Separate from the other slots, so a fill that waits for
/// an idle link never holds a slot that read-ahead needs.
const DISK_FILL_BYTES: u64 = 8 << 20;
/// Read-ahead is a hint: past this many waiting chunks, new ones are dropped.
const READ_AHEAD_PENDING: usize = 64;
const READ_AHEAD_FIRST_BYTES: u64 = 1 << 20;
const READ_AHEAD_MAX_BYTES: u64 = 8 << 20;
/// Two reads in a row that continue each other make a sequential reader.
const SEQUENTIAL_AFTER: u32 = 2;
/// A read that starts at most this far after the last one continues it.
/// Small files read in order sit close together, but not end to end.
const FORWARD_GAP: u64 = 1 << 20;
const QUEUE_POLL: Duration = Duration::from_secs(1);

pub struct PrefetchPlan {
    /// The chunks of the startup profile, in the order the last session read them.
    pub startup: Vec<u32>,
    /// Then every other chunk of the disk, in disk order.
    pub fill: bool,
    pub parallel: Option<usize>,
}

/// Read-ahead state of one disk. A sequential reader gets a window of
/// chunks ahead of it, which doubles up to `READ_AHEAD_MAX_BYTES`.
#[derive(Default)]
pub struct ReadAhead {
    next_offset: u64,
    streak: u32,
    window: usize,
    /// Chunks up to this index are fetched already.
    scheduled_until: usize,
}

impl ReadAhead {
    /// Records a read, and returns the chunks to fetch for it, if any.
    pub fn after_read(&mut self, offset: u64, len: usize, chunk_size: u64, chunk_count: usize) -> Option<Range<usize>> {
        let last = ((offset + len as u64).saturating_sub(1) / chunk_size) as usize;
        let continues = offset >= self.next_offset && offset - self.next_offset <= FORWARD_GAP;
        if continues && self.next_offset != 0 {
            self.streak += 1;
        } else {
            self.streak = 0;
            self.window = (READ_AHEAD_FIRST_BYTES / chunk_size).max(1) as usize;
            self.scheduled_until = last;
        }
        self.next_offset = offset + len as u64;
        if self.streak < SEQUENTIAL_AFTER {
            return None;
        }
        let target = (last + self.window).min(chunk_count.saturating_sub(1));
        if target <= self.scheduled_until {
            return None;
        }
        let start = self.scheduled_until.max(last) + 1;
        self.scheduled_until = target;
        self.window = (self.window * 2).min((READ_AHEAD_MAX_BYTES / chunk_size).max(1) as usize);
        (start <= target).then_some(start..target + 1)
    }
}

/// Runs until the disk closes.
pub async fn run(disk: Arc<Disk>, plan: PrefetchPlan, queue: UnboundedReceiver<Range<usize>>) {
    let slots = Arc::new(Semaphore::new(plan.parallel.unwrap_or(DEFAULT_DISK_FETCHES).max(1)));
    let ahead = tokio::spawn(serve_read_ahead(disk.clone(), slots.clone(), queue));
    let fill_slots = Arc::new(Semaphore::new((DISK_FILL_BYTES / disk.chunk_size).clamp(1, 32) as usize));
    let started = Instant::now();
    let startup: Vec<usize> = plan.startup.iter().map(|index| *index as usize).collect();
    if !startup.is_empty() {
        let count = startup.len();
        fetch_all(&disk, &slots, startup).await;
        log::info!("disk {}: prefetch of {count} startup chunks done in {} ms", disk.id, started.elapsed().as_millis());
    }
    if plan.fill && !disk.is_closed() {
        background_fill(&disk, &fill_slots).await;
    }
    let _ = ahead.await;
}

/// The queue lives as long as the disk, so a poll notices a close. Each
/// task checks for a close after it gets its slot.
async fn serve_read_ahead(disk: Arc<Disk>, slots: Arc<Semaphore>, mut queue: UnboundedReceiver<Range<usize>>) {
    let pending = Arc::new(AtomicUsize::new(0));
    loop {
        if disk.is_closed() {
            return;
        }
        match tokio::time::timeout(QUEUE_POLL, queue.recv()).await.ok() {
            Some(Some(range)) => {
                for index in range {
                    if disk.is_local(index) || pending.load(Ordering::SeqCst) >= READ_AHEAD_PENDING {
                        continue;
                    }
                    pending.fetch_add(1, Ordering::SeqCst);
                    let (disk, slots, pending) = (disk.clone(), slots.clone(), pending.clone());
                    tokio::spawn(async move {
                        if let Ok(_slot) = slots.acquire().await
                            && let Err(error) = disk.fetch_chunk(index).await
                        {
                            log::debug!("disk {}: read-ahead of chunk {index} failed: {error:#}", disk.id);
                        }
                        pending.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
            Some(None) => return,
            None => {}
        }
    }
}

async fn fetch_all(disk: &Arc<Disk>, slots: &Arc<Semaphore>, indexes: Vec<usize>) {
    let mut tasks = Vec::new();
    for index in indexes {
        if disk.is_closed() {
            break;
        }
        let Ok(slot) = slots.clone().acquire_owned().await else { break };
        let disk = disk.clone();
        tasks.push(tokio::spawn(async move {
            let _slot = slot;
            if disk.is_closed() {
                return;
            }
            if let Err(error) = disk.fetch_chunk(index).await {
                log::warn!("disk {}: prefetch of chunk {index} failed: {error:#}", disk.id);
            }
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// Downloads every chunk of the last commit that the cache does not have,
/// in disk order, until the cache is nearly full. The disk then serves all
/// reads locally, also after the link goes away.
async fn background_fill(disk: &Arc<Disk>, slots: &Arc<Semaphore>) {
    let started = Instant::now();
    let chunks = match disk.committed_chunks().await {
        Ok(chunks) => chunks,
        Err(error) => return log::warn!("disk {}: background fill cannot read pages: {error:#}", disk.id),
    };
    let missing: Vec<usize> = chunks
        .iter()
        .enumerate()
        .filter_map(|(index, hash)| hash.filter(|hash| !disk.cache.has(hash)).map(|_| index))
        .collect();
    if missing.is_empty() {
        return;
    }
    let count = missing.len();
    let mut tasks = Vec::new();
    for index in missing {
        if disk.is_closed() || disk.cache.nearly_full() {
            log::info!("disk {}: background fill stopped: the disk closed or the cache is nearly full", disk.id);
            break;
        }
        let Ok(slot) = slots.clone().acquire_owned().await else { break };
        let (disk, hash) = (disk.clone(), chunks[index]);
        tasks.push(tokio::spawn(async move {
            let _slot = slot;
            if disk.is_closed() {
                return;
            }
            if let Some(hash) = hash
                && let Err(error) = disk.cache.fetch(&hash, Fetch::Fill).await
            {
                log::debug!("disk {}: background fill of chunk {index} failed: {error:#}", disk.id);
            }
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
    log::info!("disk {}: background fill of {count} chunks done in {} s", disk.id, started.elapsed().as_secs());
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 256 << 10;

    #[test]
    fn random_reads_fetch_nothing() {
        let mut ahead = ReadAhead::default();
        for offset in [10 * CHUNK, 3 * CHUNK, 50 * CHUNK, 7 * CHUNK] {
            assert_eq!(ahead.after_read(offset, 4096, CHUNK, 1000), None);
        }
    }

    #[test]
    fn a_sequential_reader_gets_a_growing_window() {
        let mut ahead = ReadAhead::default();
        let len = 128 << 10;
        let mut offset = 100 * CHUNK;
        let mut fetched = Vec::new();
        for _ in 0..64 {
            if let Some(range) = ahead.after_read(offset, len, CHUNK, 10_000) {
                fetched.push(range);
            }
            offset += len as u64;
        }
        // After two reads in a row, the reader is in chunk 101. The first
        // window is the next 1 MiB (4 chunks).
        assert_eq!(fetched[0], 102..106);
        // Windows never overlap and never go back.
        for pair in fetched.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        // The window stays within 8 MiB (32 chunks) of the reader.
        let last_read_chunk = ((offset - 1) / CHUNK) as usize;
        assert!(fetched.last().unwrap().end <= last_read_chunk + 33);
    }

    #[test]
    fn small_files_read_in_order_count_as_sequential() {
        let mut ahead = ReadAhead::default();
        let mut offset = 500 * CHUNK;
        let mut fetched = 0;
        for _ in 0..20 {
            // A 16 KiB file, then a gap of 48 KiB to the next one.
            if let Some(range) = ahead.after_read(offset, 16 << 10, CHUNK, 100_000) {
                fetched += range.len();
            }
            offset += 64 << 10;
        }
        assert!(fetched > 0);
    }

    #[test]
    fn the_window_stops_at_the_end_of_the_disk() {
        let mut ahead = ReadAhead::default();
        let len = CHUNK as usize;
        let mut ranges = Vec::new();
        for index in 1..10 {
            if let Some(range) = ahead.after_read(index * CHUNK, len, CHUNK, 10) {
                ranges.push(range);
            }
        }
        assert!(ranges.iter().all(|range| range.end <= 10));
    }
}
