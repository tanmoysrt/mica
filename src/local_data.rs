use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Each data file covers this much of the disk. A sync of one file does not
/// wait for writes to the others, and each file stays small enough for the
/// filesystem to sync quickly.
const SEGMENT_BYTES: u64 = 1 << 30;
const LOG_MAGIC: &[u8; 8] = b"MICADLG1";
const LOG_HEADER: usize = 24;
const LOG_FILES: [&str; 2] = ["log-a", "log-b"];

/// The local data of one disk, in `<disk folder>/dirty/`:
///
/// - `data-<n>`  sparse files with the disk's local chunks, at their disk
///   offset. The guest writes go here, in place.
/// - `log-a`, `log-b`  the dirty log: the chunks with local data. After a
///   power loss, only these chunks are local.
///
/// A bitmap of the chunks written since their last FLUSH lives in a tmpfs
/// such as /run, mapped into memory (see `RecentMap`). After a daemon crash
/// the page cache still has every write, so these chunks are local too. A
/// reboot clears it.
/// - `hold`  a sparse file with the old content of chunks that the guest
///   wrote during the checkpoint that uploads them, at their disk offset:
///   the source if a GC deleted an upload. Never trusted after a crash.
///
/// Only online growth adds data files after open; FLUSH never syncs the folder.
pub struct LocalData {
    chunk_size: u64,
    segments: RwLock<Vec<Arc<File>>>,
    dir: std::path::PathBuf,
    hold: File,
    log: DirtyLog,
    /// True when every range of a chunk without local data is a hole, so a
    /// zero chunk needs no zeroing before a part of it is written.
    holes: bool,
}

impl LocalData {
    /// Opens or makes the folder. Returns the chunks that the dirty log
    /// lists: they have local data, which may differ from the last commit.
    ///
    /// `recent` is a file in a tmpfs such as /run, for the bitmap of chunks
    /// not logged yet. It survives a daemon crash, but not a reboot.
    pub fn open(dir: &Path, recent: &Path, disk_size: u64, chunk_size: u64) -> Result<(Self, Vec<usize>)> {
        create_dir_durable(dir)?;
        let mut created = false;
        let hold_path = dir.join("hold");
        created |= !hold_path.exists();
        let hold = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&hold_path)?;
        hold.set_len(disk_size)?;
        let mut segments = Vec::new();
        let mut found = Vec::new();
        for segment in 0..disk_size.div_ceil(SEGMENT_BYTES) {
            let path = dir.join(format!("data-{segment:05}"));
            created |= !path.exists();
            found.push(path.exists());
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
            let len = SEGMENT_BYTES.min(disk_size - segment * SEGMENT_BYTES);
            if file.metadata()?.len() < len {
                file.set_len(len)?;
            }
            segments.push(Arc::new(file));
        }
        let chunks = (disk_size / chunk_size) as usize;
        let (log, entries) = DirtyLog::open(dir, recent, chunks, &mut created)?;
        if created {
            File::open(dir)?.sync_all()?;
        }
        let listed: BTreeSet<usize> = entries.into_iter().filter(|index| *index < chunks).collect();
        let mut data = Self { chunk_size, segments: RwLock::new(segments), dir: dir.to_path_buf(), hold, log, holes: true };
        data.holes = data.clear_unlisted(&found, &listed)?;
        Ok((data, listed.into_iter().collect()))
    }

    pub fn grow_to(&self, size: u64) -> Result<()> {
        let mut segments = self.segments.write().unwrap();
        for segment in 0..size.div_ceil(SEGMENT_BYTES) as usize {
            let len = SEGMENT_BYTES.min(size - segment as u64 * SEGMENT_BYTES);
            if segment == segments.len() {
                let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
                    .open(self.dir.join(format!("data-{segment:05}")))?;
                segments.push(Arc::new(file));
            }
            if segments[segment].metadata()?.len() < len {
                segments[segment].set_len(len)?;
                segments[segment].sync_all()?;
            }
        }
        self.hold.set_len(size)?;
        self.log.state.lock().unwrap().recent.grow_to((size / self.chunk_size) as usize)?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// Punches out the ranges of chunks that no log lists: after a crash
    /// they can hold writes that are not local data. A new data file is all
    /// holes already. False when the filesystem cannot punch holes.
    fn clear_unlisted(&self, found: &[bool], listed: &BTreeSet<usize>) -> Result<bool> {
        let chunks_per_segment = SEGMENT_BYTES / self.chunk_size;
        for (segment, file) in self.segments.read().unwrap().iter().enumerate() {
            if !found[segment] {
                continue;
            }
            let first = segment as u64 * chunks_per_segment;
            let len = file.metadata()?.len();
            let mut start = 0;
            let keep = listed.range(first as usize..(first + chunks_per_segment) as usize);
            for &index in keep.chain(std::iter::once(&usize::MAX)) {
                let end = if index == usize::MAX { len } else { (index as u64 - first) * self.chunk_size };
                if end > start && !punch(file, start, end - start)? {
                    return Ok(false);
                }
                start = end + self.chunk_size;
            }
        }
        Ok(true)
    }

    /// True when a chunk without local data reads as zeros in the data files.
    pub fn has_holes(&self) -> bool {
        self.holes
    }

    /// The data file of a chunk, and the chunk's offset in it.
    pub fn location(&self, index: usize) -> (Arc<File>, u64) {
        let offset = index as u64 * self.chunk_size;
        (self.segment((offset / SEGMENT_BYTES) as usize), offset % SEGMENT_BYTES)
    }

    pub fn segment_of(&self, index: usize) -> usize {
        (index as u64 * self.chunk_size / SEGMENT_BYTES) as usize
    }

    pub fn segment(&self, segment: usize) -> Arc<File> {
        self.segments.read().unwrap()[segment].clone()
    }

    /// Starts writeback of a data file's dirty pages, without waiting.
    /// It reports no writeback error: only the FLUSH sync checks those.
    pub fn start_writeback(&self, segment: usize) -> Result<()> {
        let file = self.segment(segment);
        let fd = file.as_raw_fd();
        if unsafe { libc::sync_file_range(fd, 0, 0, libc::SYNC_FILE_RANGE_WRITE) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn read_chunk(&self, index: usize) -> Result<Vec<u8>> {
        let (file, offset) = self.location(index);
        let mut data = vec![0u8; self.chunk_size as usize];
        file.read_exact_at(&mut data, offset)?;
        Ok(data)
    }

    /// Puts a whole chunk from `source` (a cache file) into place. A reflink
    /// on btrfs and XFS, a copy elsewhere.
    pub fn copy_in(&self, index: usize, source: &File) -> Result<()> {
        let (file, offset) = self.location(index);
        copy_range(source, 0, &file, offset, self.chunk_size)
    }

    /// Makes a chunk all zeros, and frees its space where the filesystem can.
    pub fn zero(&self, index: usize) -> Result<()> {
        let (file, offset) = self.location(index);
        if !punch(&file, offset, self.chunk_size)? {
            file.write_all_at(&vec![0u8; self.chunk_size as usize], offset)?;
        }
        Ok(())
    }

    /// Keeps the chunk's content in the hold file, before a write changes it.
    pub fn hold(&self, index: usize) -> Result<()> {
        let (file, offset) = self.location(index);
        let start = index as u64 * self.chunk_size;
        copy_range(&file, offset, &self.hold, start, self.chunk_size)
    }

    pub fn read_held(&self, index: usize) -> Result<Vec<u8>> {
        let mut data = vec![0u8; self.chunk_size as usize];
        self.hold.read_exact_at(&mut data, index as u64 * self.chunk_size)?;
        Ok(data)
    }

    /// Drops all held chunks at once.
    pub fn clear_held(&self) -> Result<()> {
        let len = self.hold.metadata()?.len();
        self.hold.set_len(0)?;
        self.hold.set_len(len)?;
        Ok(())
    }

    /// Copies a chunk into `target`, such as a new cache file.
    pub fn copy_out(&self, index: usize, target: &File) -> Result<()> {
        let (file, offset) = self.location(index);
        copy_range(&file, offset, target, 0, self.chunk_size)?;
        target.set_len(self.chunk_size)?;
        Ok(())
    }

    pub fn log(&self) -> &DirtyLog {
        &self.log
    }
}

/// Makes a folder and its missing parents, and syncs every folder on its
/// path: a power loss must not lose a folder that holds data. Every one,
/// because an earlier try may have made a folder and died before its sync.
pub fn create_dir_durable(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for path in dir.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

/// True when the dirty log lists a chunk, or the `recent` bitmap in /run has
/// a bit set: in any session, since this only decides whether to keep the folder.
pub fn has_entries(dir: &Path, recent: &Path) -> Result<bool> {
    let bits = std::fs::read(recent)
        .is_ok_and(|data| data.get(RECENT_HEADER..).is_some_and(|bits| bits.iter().any(|byte| *byte != 0)));
    if bits {
        return Ok(true);
    }
    let logged = LOG_FILES.iter().filter_map(|name| read_log(&dir.join(name)).ok().flatten()).any(|log| !log.entries.is_empty());
    Ok(logged)
}

/// The chunks with local data, in two files used in turn. A rewrite goes to the file not in use and is synced before any
/// append to it, so one of the two is always whole. Appends are records
/// with a CRC: a torn append ends the log there.
///
/// An entry says only that the chunk is local, not whether S3 has it. After
/// a crash, the next checkpoint compares each listed chunk with its
/// committed hash, and uploads only those that differ.
///
/// Chunks not logged yet also go to the `recent` bitmap at once, for a
/// daemon crash.
///
/// Two locks: `durable` is held during the log syncs, by FLUSH and
/// checkpoints only. `state` is held briefly, also by the ublk queue threads.
pub struct DirtyLog {
    durable: Mutex<DurableLog>,
    state: Mutex<LogState>,
}

struct DurableLog {
    files: [File; 2],
    current: usize,
    generation: u64,
    end: u64,
}

struct LogState {
    /// The chunks that the durable log lists.
    logged: BTreeSet<usize>,
    /// Chunks the guest wrote that the log does not list yet. A FLUSH logs them.
    unlogged: BTreeSet<usize>,
    /// Chunks that a FLUSH took from `unlogged` and has not appended yet.
    logging: BTreeSet<usize>,
    recent: RecentMap,
}

struct LogContent {
    generation: u64,
    entries: Vec<usize>,
    end: u64,
}

impl DirtyLog {
    fn open(dir: &Path, recent_path: &Path, chunks: usize, created: &mut bool) -> Result<(Self, Vec<usize>)> {
        // A new log means a new folder: a bitmap left from an older one is not its own.
        let new_folder = LOG_FILES.iter().all(|name| !dir.join(name).exists());
        let (recent, found_recent) = RecentMap::open(recent_path, chunks, new_folder)?;
        let mut files = Vec::new();
        let mut contents = Vec::new();
        for name in LOG_FILES {
            let path = dir.join(name);
            *created |= !path.exists();
            contents.push(read_log(&path)?);
            files.push(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?);
        }
        let files: [File; 2] = files.try_into().unwrap();
        let newest = (0..2).filter(|&i| contents[i].is_some()).max_by_key(|&i| contents[i].as_ref().unwrap().generation);
        let mut durable = DurableLog { files, current: 0, generation: 0, end: 0 };
        let mut state = LogState {
            logged: BTreeSet::new(),
            unlogged: BTreeSet::new(),
            logging: BTreeSet::new(),
            recent,
        };
        let mut entries = match newest {
            Some(current) => {
                let content = contents[current].take().unwrap();
                durable.current = current;
                durable.generation = content.generation;
                durable.end = content.end;
                state.logged = content.entries.iter().copied().collect();
                content.entries
            }
            None => {
                durable.write_new(&BTreeSet::new())?;
                Vec::new()
            }
        };
        // After a daemon crash in this boot, chunks that only `recent` lists
        // may hold writes the guest has not flushed: they stay unlogged, so
        // the next FLUSH logs them.
        if let Some(found) = found_recent {
            let only_recent: Vec<usize> = found.into_iter().filter(|index| !state.logged.contains(index)).collect();
            state.unlogged.extend(only_recent.iter().copied());
            entries.extend(only_recent);
        }
        Ok((Self { durable: Mutex::new(durable), state: Mutex::new(state) }, entries))
    }

    /// Call after the chunk's data is in its data file (in the page cache is
    /// enough), and after the chunk counts as local.
    pub fn note_write(&self, index: usize) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.logged.contains(&index) || state.unlogged.contains(&index) {
            return Ok(());
        }
        state.recent.set(index);
        state.unlogged.insert(index);
        Ok(())
    }

    /// True when no list has the chunk, and no FLUSH is adding it, so no
    /// crash can bring it back as local. Only then may its space be freed.
    pub fn is_unlisted(&self, index: usize) -> bool {
        let state = self.state.lock().unwrap();
        ![&state.logged, &state.unlogged, &state.logging].iter().any(|list| list.contains(&index))
            && !state.recent.contains(index)
    }

    /// For a FLUSH: the chunks to log, all or those of `only`. They count
    /// as listed until `append` or `restore_unlogged`.
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
        state.logging.extend(taken.iter().copied());
        taken
    }

    /// Gives back chunks whose append failed.
    pub fn restore_unlogged(&self, indexes: BTreeSet<usize>) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        for index in indexes {
            state.logging.remove(&index);
            if !state.logged.contains(&index) {
                state.unlogged.insert(index);
            }
        }
    }

    /// Appends the chunks and syncs the log. Call after their data files are synced.
    pub fn append(&self, indexes: &BTreeSet<usize>) -> Result<()> {
        let mut durable = self.durable.lock().unwrap();
        let entries = entry_bytes(indexes.iter().copied());
        let count = (indexes.len() as u32).to_le_bytes();
        let mut record = Vec::with_capacity(8 + entries.len());
        record.extend_from_slice(&count);
        record.extend_from_slice(&crc(&[&count, &entries]).to_le_bytes());
        record.extend_from_slice(&entries);
        let file = &durable.files[durable.current];
        file.write_all_at(&record, durable.end)?;
        file.sync_data()?;
        durable.end += record.len() as u64;
        let mut state = self.state.lock().unwrap();
        state.logged.extend(indexes.iter().copied());
        for index in indexes {
            state.logging.remove(index);
        }
        Ok(())
    }

    /// Writes a new log that lists only the logged chunks for which `keep`
    /// is true: chunks whose local data is freed drop out, also from `recent`.
    ///
    /// The dropped chunks leave the in-memory log first. So a write that
    /// makes one local again lists it as new. A write makes its chunk local before
    /// `note_write`, so `keep` sees every chunk that a write skipped.
    pub fn rewrite(&self, keep: impl Fn(usize) -> bool) -> Result<()> {
        let mut durable = self.durable.lock().unwrap();
        let kept: BTreeSet<usize> = {
            let mut state = self.state.lock().unwrap();
            state.logged.retain(|&index| keep(index));
            state.logged.clone()
        };
        durable.write_new(&kept)?;
        let state = self.state.lock().unwrap();
        state.recent.replace(&state.unlogged.union(&state.logging).copied().collect());
        Ok(())
    }
}

impl DurableLog {
    /// Writes `kept` to the file not in use, syncs it, and uses it from now on.
    fn write_new(&mut self, kept: &BTreeSet<usize>) -> Result<()> {
        let next = 1 - self.current;
        let generation = self.generation + 1;
        let entries = entry_bytes(kept.iter().copied());
        let count = (kept.len() as u32).to_le_bytes();
        let mut data = Vec::with_capacity(LOG_HEADER + entries.len());
        data.extend_from_slice(LOG_MAGIC);
        data.extend_from_slice(&generation.to_le_bytes());
        data.extend_from_slice(&count);
        data.extend_from_slice(&crc(&[&generation.to_le_bytes(), &count, &entries]).to_le_bytes());
        data.extend_from_slice(&entries);
        let file = &self.files[next];
        file.set_len(0)?;
        file.write_all_at(&data, 0)?;
        file.sync_all()?;
        self.current = next;
        self.generation = generation;
        self.end = data.len() as u64;
        Ok(())
    }
}

/// The session ID page before the bits of `recent`.
const RECENT_HEADER: usize = 4096;

/// `recent`: a page with the boot ID and a random ID, then one bit for
/// each chunk, in a tmpfs, mapped into memory. A bit is set with one store:
/// no system call, no writeback. A daemon crash keeps every store; a reboot
/// clears the tmpfs. A rewrite stores each word anew, so a crash in the
/// middle leaves each bit old or new: never fewer bits than both lists share.
struct RecentMap {
    _file: File,
    map: *mut u8,
    len: usize,
}

// The map is only read and written through atomics.
unsafe impl Send for RecentMap {}
unsafe impl Sync for RecentMap {}

impl RecentMap {
    /// Also returns the listed chunks, if the file is from this boot and
    /// from this data folder. Otherwise it starts again with no bits.
    fn open(path: &Path, chunks: usize, new_folder: bool) -> Result<(Self, Option<Vec<usize>>)> {
        let len = RECENT_HEADER + chunks.div_ceil(64) * 8;
        let from_this_boot = session_of(path).is_some_and(|id| id.starts_with(&boot_id()));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let trusted = from_this_boot && !new_folder && file.metadata()?.len() >= RECENT_HEADER as u64;
        if trusted {
            file.set_len(file.metadata()?.len().max(len as u64))?;
        } else {
            file.set_len(0)?;
            file.set_len(len as u64)?;
            file.write_all_at(format!("{}/{}", boot_id(), random_id()).as_bytes(), 0)?;
        }
        let flags = libc::PROT_READ | libc::PROT_WRITE;
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, flags, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap recent");
        }
        let recent = Self { _file: file, map: map.cast(), len };
        let found = trusted.then(|| (0..chunks).filter(|&index| recent.contains(index)).collect());
        Ok((recent, found))
    }
    fn grow_to(&mut self, chunks: usize) -> Result<()> {
        let len = RECENT_HEADER + chunks.div_ceil(64) * 8;
        if len <= self.len {
            return Ok(());
        }
        self._file.set_len(len as u64)?;
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, self._file.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("grow recent bitmap");
        }
        unsafe { libc::munmap(self.map.cast(), self.len) };
        self.map = map.cast();
        self.len = len;
        Ok(())
    }

    fn words(&self) -> &[AtomicU64] {
        let count = (self.len - RECENT_HEADER) / 8;
        unsafe { std::slice::from_raw_parts(self.map.add(RECENT_HEADER).cast::<AtomicU64>(), count) }
    }

    fn set(&self, index: usize) {
        self.words()[index / 64].fetch_or(1 << (index % 64), Ordering::SeqCst);
    }

    fn contains(&self, index: usize) -> bool {
        self.words()[index / 64].load(Ordering::SeqCst) & (1 << (index % 64)) != 0
    }

    /// Makes the bits equal to `listed`.
    fn replace(&self, listed: &BTreeSet<usize>) {
        let mut words = vec![0u64; self.words().len()];
        for &index in listed {
            words[index / 64] |= 1 << (index % 64);
        }
        for (word, value) in self.words().iter().zip(words) {
            word.store(value, Ordering::SeqCst);
        }
    }
}

impl Drop for RecentMap {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

/// The session ID at the start of a `recent` file.
fn session_of(path: &Path) -> Option<String> {
    let mut header = vec![0u8; 128];
    let read = File::open(path).ok()?.read_at(&mut header, 0).ok()?;
    let id = String::from_utf8_lossy(&header[..read]).trim_end_matches('\0').trim().to_string();
    (!id.is_empty()).then_some(id)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().to_string()
}

fn random_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().to_string()
}

fn entry_bytes(indexes: impl Iterator<Item = usize>) -> Vec<u8> {
    indexes.flat_map(|index| (index as u32).to_le_bytes()).collect()
}

fn crc(parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

fn parse_entries(bytes: &[u8]) -> Vec<usize> {
    bytes.chunks_exact(4).map(|entry| u32::from_le_bytes(entry.try_into().unwrap()) as usize).collect()
}

/// `None` for a missing, empty or torn log.
fn read_log(path: &Path) -> Result<Option<LogContent>> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if data.len() < LOG_HEADER || &data[..8] != LOG_MAGIC {
        return Ok(None);
    }
    let generation = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let count = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let stored = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let Some(listed) = data.get(LOG_HEADER..LOG_HEADER + count * 4) else { return Ok(None) };
    if crc(&[&data[8..16], &data[16..20], listed]) != stored {
        return Ok(None);
    }
    let mut entries = parse_entries(listed);
    let mut position = LOG_HEADER + count * 4;
    while let Some(header) = data.get(position..position + 8) {
        let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(header[4..].try_into().unwrap());
        let Some(listed) = data.get(position + 8..position + 8 + count * 4) else { break };
        if count == 0 || crc(&[&header[..4], listed]) != stored {
            break;
        }
        entries.extend(parse_entries(listed));
        position += 8 + count * 4;
    }
    Ok(Some(LogContent { generation, entries, end: position as u64 }))
}

/// Uses copy_file_range with explicit offsets: a reflink on btrfs and XFS.
/// Explicit offsets matter: the files may be shared with readers.
fn copy_range(source: &File, source_offset: u64, target: &File, target_offset: u64, len: u64) -> Result<()> {
    let mut source_offset = source_offset as i64;
    let mut target_offset = target_offset as i64;
    let mut remaining = len as usize;
    while remaining > 0 {
        let copied = unsafe {
            libc::copy_file_range(
                source.as_raw_fd(),
                &mut source_offset,
                target.as_raw_fd(),
                &mut target_offset,
                remaining,
                0,
            )
        };
        if copied < 0 {
            return Err(std::io::Error::last_os_error()).context("copy_file_range");
        }
        if copied == 0 {
            bail!("copy_file_range: the source is {remaining} bytes short");
        }
        remaining -= copied as usize;
    }
    Ok(())
}

/// False when the filesystem cannot punch holes.
fn punch(file: &File, offset: u64, len: u64) -> Result<bool> {
    let flags = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    if unsafe { libc::fallocate(file.as_raw_fd(), flags, offset as i64, len as i64) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
        return Ok(false);
    }
    Err(error).context("fallocate")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 4096;

    #[test]
    fn the_log_keeps_appends_and_rewrites_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let (data, listed) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        assert!(listed.is_empty());
        data.log().append(&BTreeSet::from([3, 5])).unwrap();
        data.log().rewrite(|index| index == 5).unwrap();
        data.log().append(&BTreeSet::from([7])).unwrap();
        drop(data);

        let (_, listed) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![5, 7]);
    }

    #[test]
    fn a_torn_append_ends_the_log_and_a_torn_rewrite_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let (data, _) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        data.log().append(&BTreeSet::from([1])).unwrap();
        data.log().append(&BTreeSet::from([2])).unwrap();
        let current = dir.path().join(LOG_FILES[data.log().durable.lock().unwrap().current]);
        drop(data);
        // A torn last record.
        let mut bytes = std::fs::read(&current).unwrap();
        let len = bytes.len();
        bytes[len - 1] ^= 0xff;
        std::fs::write(&current, &bytes).unwrap();
        let (data, listed) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![1]);
        // A rewrite torn before its sync: the other file still counts.
        let next = dir.path().join(LOG_FILES[1 - data.log().durable.lock().unwrap().current]);
        drop(data);
        std::fs::write(&next, b"MICADLG1garbage-garbage-garbage").unwrap();
        let (_, listed) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![1]);
    }

    #[test]
    fn chunks_copy_in_hold_and_zero() {
        let dir = tempfile::tempdir().unwrap();
        let (data, _) = LocalData::open(dir.path(), &dir.path().join("run/recent"), 64 * CHUNK, CHUNK).unwrap();
        let source_path = dir.path().join("source");
        std::fs::write(&source_path, vec![7u8; CHUNK as usize]).unwrap();
        data.copy_in(9, &File::open(&source_path).unwrap()).unwrap();
        assert_eq!(data.read_chunk(9).unwrap(), vec![7u8; CHUNK as usize]);
        data.hold(9).unwrap();
        data.zero(9).unwrap();
        assert_eq!(data.read_chunk(9).unwrap(), vec![0u8; CHUNK as usize]);
        assert_eq!(data.read_held(9).unwrap(), vec![7u8; CHUNK as usize]);
        data.clear_held().unwrap();
        assert_eq!(data.read_held(9).unwrap(), vec![0u8; CHUNK as usize]);
    }

    #[test]
    fn recent_chunks_come_back_in_the_same_session_only() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("run/recent");
        let (data, _) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        data.log().note_write(4).unwrap();
        drop(data);
        // A daemon crash: the session file stays.
        let (data, listed) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![4]);
        assert!(!data.log().is_unlisted(4));
        drop(data);
        // A reboot: /run is empty.
        std::fs::remove_file(&session).unwrap();
        let (_, listed) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn ranges_that_no_log_lists_are_cleared_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("run/recent");
        let (data, _) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        let (file, offset) = data.location(2);
        file.write_all_at(&[9u8; CHUNK as usize], offset).unwrap();
        let (file, offset) = data.location(3);
        file.write_all_at(&[8u8; CHUNK as usize], offset).unwrap();
        data.log().append(&BTreeSet::from([3])).unwrap();
        drop(data);
        std::fs::remove_file(&session).unwrap();
        let (data, listed) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![3]);
        assert_eq!(data.read_chunk(2).unwrap(), vec![0u8; CHUNK as usize]);
        assert_eq!(data.read_chunk(3).unwrap(), vec![8u8; CHUNK as usize]);
    }

    #[test]
    fn growing_recent_bitmap_preserves_unflushed_chunks_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("run/recent");
        let (data, _) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        data.location(3).0.write_all_at(&[7; CHUNK as usize], 3 * CHUNK).unwrap();
        data.log().note_write(3).unwrap();
        data.grow_to(256 * CHUNK).unwrap();
        data.location(200).0.write_all_at(&[8; CHUNK as usize], 200 * CHUNK).unwrap();
        data.log().note_write(200).unwrap();
        drop(data);
        let (data, listed) = LocalData::open(dir.path(), &session, 256 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![3, 200]);
        assert_eq!(data.read_chunk(3).unwrap(), vec![7; CHUNK as usize]);
        assert_eq!(data.read_chunk(200).unwrap(), vec![8; CHUNK as usize]);
    }

    #[test]
    fn interrupted_growth_keeps_recent_bits_when_root_is_still_small() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("run/recent");
        let (data, _) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        data.log().note_write(3).unwrap();
        data.grow_to(256 * CHUNK).unwrap();
        drop(data);
        let (_, listed) = LocalData::open(dir.path(), &session, 64 * CHUNK, CHUNK).unwrap();
        assert_eq!(listed, vec![3]);
    }
}
