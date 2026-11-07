//! The dirty log: the list of the local chunks of a disk.
//!
//! After a power loss, only the chunks in this list are local. mica punches
//! out all other ranges of the data files, because they can hold torn or
//! old writes. A flush syncs the data files first and then appends to the
//! log, so the log never lists a chunk whose data a power loss can lose.
//!
//! The log has two files, `log-a` and `log-b`. A rewrite goes to the file
//! that is not in use, and is synced before it counts. So one of the two
//! files is always complete. Each record has a CRC, and a torn append ends
//! the log at that record.
//!
//! The recent bitmap covers the time between a write and the next flush.
//! It has one bit for each chunk, in a file in `/run` that is mapped into
//! memory. A write sets a bit with one memory store, with no system call.
//! The bitmap survives a crash of mica, as the page cache does, and goes
//! away at a reboot, as the page cache does.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"MICALOG2";
const HEADER: usize = 24;
const FILES: [&str; 2] = ["log-a", "log-b"];

/// Two locks: `durable` is held during syncs, by flushes and rewrites only.
/// `state` is held for short times, also by the queue threads.
pub struct DirtyLog {
    durable: Mutex<Durable>,
    state: Mutex<State>,
}

struct Durable {
    files: [File; 2],
    current: usize,
    generation: u64,
    end: u64,
}

struct State {
    /// The chunks that the log on disk lists.
    logged: BTreeSet<usize>,
    /// Chunks written since the last flush that the log does not list yet.
    unlogged: BTreeSet<usize>,
    /// Chunks that a running flush takes from `unlogged` into the log.
    logging: BTreeSet<usize>,
    recent: RecentMap,
}

impl DirtyLog {
    /// Opens the log and returns the chunks it lists. After a crash of mica
    /// in this boot, it also returns the chunks of the recent bitmap: their
    /// writes are still in the page cache.
    pub fn open(dir: &Path, recent: &Path, chunks: usize) -> Result<(Self, BTreeSet<usize>)> {
        let new_folder = FILES.iter().all(|name| !dir.join(name).exists());
        let (recent, recent_chunks) = RecentMap::open(recent, chunks, new_folder)?;
        let mut contents = Vec::new();
        let mut files = Vec::new();
        for name in FILES {
            let path = dir.join(name);
            contents.push(read_log(&path)?);
            files.push(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?);
        }
        let mut durable = Durable { files: files.try_into().unwrap(), current: 0, generation: 0, end: 0 };
        let newest = (0..2).filter_map(|i| contents[i].as_ref().map(|log| (log.generation, i))).max();
        let logged: BTreeSet<usize> = match newest {
            Some((_, current)) => {
                let log = contents[current].take().unwrap();
                (durable.current, durable.generation, durable.end) = (current, log.generation, log.end);
                log.entries.into_iter().filter(|&index| index < chunks).collect()
            }
            None => {
                durable.write_new(&BTreeSet::new())?;
                BTreeSet::new()
            }
        };
        // Chunks that only the bitmap lists stay unlogged: the next flush logs them.
        let unlogged: BTreeSet<usize> = recent_chunks.into_iter().filter(|index| !logged.contains(index)).collect();
        let listed = logged.union(&unlogged).copied().collect();
        let state = State { logged, unlogged, logging: BTreeSet::new(), recent };
        Ok((Self { durable: Mutex::new(durable), state: Mutex::new(state) }, listed))
    }

    /// Call after the data of the chunk is in its data file.
    pub fn note_write(&self, index: usize) {
        let mut state = self.state.lock().unwrap();
        if !state.logged.contains(&index) && state.unlogged.insert(index) {
            state.recent.set(index);
        }
    }

    /// True when no list has the chunk, so no crash can bring it back as
    /// local. Only then may its space in the data files be freed.
    pub fn is_unlisted(&self, index: usize) -> bool {
        let state = self.state.lock().unwrap();
        ![&state.logged, &state.unlogged, &state.logging].iter().any(|list| list.contains(&index))
            && !state.recent.contains(index)
    }

    /// The chunks that a flush must log: all, or only those in `only`.
    /// They count as listed until `append` or `give_back`.
    pub fn take_unlogged(&self, only: Option<&BTreeSet<usize>>) -> BTreeSet<usize> {
        let mut state = self.state.lock().unwrap();
        let taken = match only {
            None => std::mem::take(&mut state.unlogged),
            Some(only) => {
                let taken: BTreeSet<usize> = state.unlogged.intersection(only).copied().collect();
                state.unlogged.retain(|index| !taken.contains(index));
                taken
            }
        };
        state.logging.extend(&taken);
        taken
    }

    /// Gives back the chunks of a failed append.
    pub fn give_back(&self, chunks: &BTreeSet<usize>) {
        let mut state = self.state.lock().unwrap();
        for index in chunks {
            state.logging.remove(index);
            if !state.logged.contains(index) {
                state.unlogged.insert(*index);
            }
        }
    }

    /// Appends the chunks and syncs the log. Call after their data files are synced.
    pub fn append(&self, chunks: &BTreeSet<usize>) -> Result<()> {
        let mut durable = self.durable.lock().unwrap();
        let entries = entry_bytes(chunks);
        let count = (chunks.len() as u32).to_le_bytes();
        let mut record = Vec::with_capacity(8 + entries.len());
        record.extend_from_slice(&count);
        record.extend_from_slice(&crc(&[&count, &entries]).to_le_bytes());
        record.extend_from_slice(&entries);
        let file = &durable.files[durable.current];
        file.write_all_at(&record, durable.end)?;
        file.sync_data()?;
        durable.end += record.len() as u64;
        let mut state = self.state.lock().unwrap();
        state.logged.extend(chunks);
        chunks.iter().for_each(|index| drop(state.logging.remove(index)));
        Ok(())
    }

    /// Writes a new log with the logged chunks for which `keep` is true.
    /// The others leave the log, so their space can be freed.
    ///
    /// A write makes its chunk local before `note_write`, so `keep` sees
    /// every chunk that a write skipped because the log listed it.
    pub fn rewrite(&self, keep: impl Fn(usize) -> bool) -> Result<()> {
        let mut durable = self.durable.lock().unwrap();
        let kept = {
            let mut state = self.state.lock().unwrap();
            state.logged.retain(|&index| keep(index));
            state.logged.clone()
        };
        durable.write_new(&kept)?;
        let state = self.state.lock().unwrap();
        state.recent.replace(state.unlogged.union(&state.logging));
        Ok(())
    }
}

impl Durable {
    /// Writes `kept` to the file not in use, syncs it, and uses it from now on.
    fn write_new(&mut self, kept: &BTreeSet<usize>) -> Result<()> {
        let (next, generation) = (1 - self.current, self.generation + 1);
        let entries = entry_bytes(kept);
        let count = (kept.len() as u32).to_le_bytes();
        let mut data = Vec::with_capacity(HEADER + entries.len());
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&generation.to_le_bytes());
        data.extend_from_slice(&count);
        data.extend_from_slice(&crc(&[&generation.to_le_bytes(), &count, &entries]).to_le_bytes());
        data.extend_from_slice(&entries);
        let file = &self.files[next];
        file.set_len(0)?;
        file.write_all_at(&data, 0)?;
        file.sync_all()?;
        (self.current, self.generation, self.end) = (next, generation, data.len() as u64);
        Ok(())
    }
}

struct LogContent {
    generation: u64,
    entries: Vec<usize>,
    end: u64,
}

/// `None` for a missing, empty or torn log file.
fn read_log(path: &Path) -> Result<Option<LogContent>> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if data.len() < HEADER || &data[..8] != MAGIC {
        return Ok(None);
    }
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let generation = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let count = u32_at(16) as usize;
    let Some(listed) = data.get(HEADER..HEADER + count * 4) else { return Ok(None) };
    if crc(&[&data[8..16], &data[16..20], listed]) != u32_at(20) {
        return Ok(None);
    }
    let mut entries = parse_entries(listed);
    let mut position = HEADER + count * 4;
    while let Some(header) = data.get(position..position + 8) {
        let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let Some(listed) = data.get(position + 8..position + 8 + count * 4) else { break };
        if count == 0 || crc(&[&header[..4], listed]) != u32::from_le_bytes(header[4..].try_into().unwrap()) {
            break;
        }
        entries.extend(parse_entries(listed));
        position += 8 + count * 4;
    }
    Ok(Some(LogContent { generation, entries, end: position as u64 }))
}

fn entry_bytes(chunks: &BTreeSet<usize>) -> Vec<u8> {
    chunks.iter().flat_map(|&index| (index as u32).to_le_bytes()).collect()
}

fn parse_entries(bytes: &[u8]) -> Vec<usize> {
    bytes.chunks_exact(4).map(|entry| u32::from_le_bytes(entry.try_into().unwrap()) as usize).collect()
}

fn crc(parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    parts.iter().for_each(|part| hasher.update(part));
    hasher.finalize()
}

/// The page before the bits holds the boot identifier and the session.
const RECENT_HEADER: usize = 4096;

/// The recent bitmap, mapped into memory. A rewrite stores each word again,
/// so a crash in the middle leaves each bit old or new.
struct RecentMap {
    _file: File,
    map: *mut AtomicU64,
    words: usize,
}

// The map is only used through atomics.
unsafe impl Send for RecentMap {}
unsafe impl Sync for RecentMap {}

impl RecentMap {
    /// Also returns the chunks that the bitmap lists, if the file is from
    /// this boot and this data folder. Otherwise the bitmap starts empty.
    fn open(path: &Path, chunks: usize, new_folder: bool) -> Result<(Self, Vec<usize>)> {
        let words = chunks.div_ceil(64);
        let len = RECENT_HEADER + words * 8;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut header = vec![0u8; boot.len()];
        let same_boot = file.read_exact_at(&mut header, 0).is_ok() && header == boot.as_bytes() && !boot.is_empty();
        let trusted = same_boot && !new_folder && file.metadata()?.len() == len as u64;
        if !trusted {
            file.set_len(0)?;
            file.set_len(len as u64)?;
            file.write_all_at(boot.as_bytes(), 0)?;
        }
        let protection = libc::PROT_READ | libc::PROT_WRITE;
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, protection, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("map the recent bitmap");
        }
        let map = unsafe { map.cast::<u8>().add(RECENT_HEADER).cast::<AtomicU64>() };
        let recent = Self { _file: file, map, words };
        let found = match trusted {
            true => (0..chunks).filter(|&index| recent.contains(index)).collect(),
            false => Vec::new(),
        };
        Ok((recent, found))
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        assert!(index / 64 < self.words);
        unsafe { &*self.map.add(index / 64) }
    }

    fn set(&self, index: usize) {
        self.word(index).fetch_or(1 << (index % 64), Ordering::SeqCst);
    }

    fn contains(&self, index: usize) -> bool {
        self.word(index).load(Ordering::SeqCst) & (1 << (index % 64)) != 0
    }

    /// Makes the bits equal to `chunks`.
    fn replace<'a>(&self, chunks: impl Iterator<Item = &'a usize>) {
        let mut words = vec![0u64; self.words];
        chunks.for_each(|&index| words[index / 64] |= 1 << (index % 64));
        for (at, value) in words.into_iter().enumerate() {
            self.word(at * 64).store(value, Ordering::SeqCst);
        }
    }
}

impl Drop for RecentMap {
    fn drop(&mut self) {
        let start = unsafe { self.map.cast::<u8>().sub(RECENT_HEADER) };
        unsafe { libc::munmap(start.cast(), RECENT_HEADER + self.words * 8) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path) -> (DirtyLog, BTreeSet<usize>) {
        DirtyLog::open(dir, &dir.join("run/recent"), 256).unwrap()
    }

    #[test]
    fn appends_and_rewrites_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (log, listed) = open(dir.path());
        assert!(listed.is_empty());
        log.append(&BTreeSet::from([3, 5])).unwrap();
        log.rewrite(|index| index == 5).unwrap();
        log.append(&BTreeSet::from([7])).unwrap();
        drop(log);
        std::fs::remove_file(dir.path().join("run/recent")).unwrap();
        assert_eq!(open(dir.path()).1, BTreeSet::from([5, 7]));
    }

    #[test]
    fn a_torn_append_ends_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (log, _) = open(dir.path());
        log.append(&BTreeSet::from([1])).unwrap();
        log.append(&BTreeSet::from([2])).unwrap();
        let current = dir.path().join(FILES[log.durable.lock().unwrap().current]);
        drop(log);
        let mut bytes = std::fs::read(&current).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        std::fs::write(&current, &bytes).unwrap();
        std::fs::remove_file(dir.path().join("run/recent")).unwrap();
        assert_eq!(open(dir.path()).1, BTreeSet::from([1]));
    }

    #[test]
    fn recent_chunks_come_back_after_a_crash_but_not_after_a_reboot() {
        let dir = tempfile::tempdir().unwrap();
        let (log, _) = open(dir.path());
        log.note_write(4);
        drop(log);
        let (log, listed) = open(dir.path());
        assert_eq!(listed, BTreeSet::from([4]));
        assert!(!log.is_unlisted(4));
        drop(log);
        std::fs::remove_file(dir.path().join("run/recent")).unwrap();
        assert!(open(dir.path()).1.is_empty());
    }
}
