//! The data files of a disk: its local chunks, at their offset on the disk.
//!
//! A local chunk is a chunk whose current content is on the node. Guest
//! writes go into the data files in place. A range that is not local is a
//! hole, so a data file uses space only for local chunks.
//!
//! Each data file covers 1 GiB of the disk. A sync of one file does not wait
//! for the writes into the others, and a small file syncs fast.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

const SEGMENT_BYTES: u64 = 1 << 30;

pub struct DataFiles {
    chunk_size: u64,
    segments: Vec<Arc<File>>,
    /// False when the filesystem cannot punch holes. Then a chunk that is
    /// not local can hold old data, and must be zeroed before a partial write.
    holes: bool,
}

impl DataFiles {
    /// Opens or makes the files. A crash can leave writes in ranges that no
    /// log lists, so these ranges are punched out: only `listed` chunks stay.
    pub fn open(dir: &Path, disk_size: u64, chunk_size: u64, listed: &BTreeSet<usize>) -> Result<Self> {
        let mut segments = Vec::new();
        let mut found = Vec::new();
        for segment in 0..disk_size.div_ceil(SEGMENT_BYTES) {
            let path = dir.join(format!("data-{segment:05}"));
            found.push(path.exists());
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
            let len = SEGMENT_BYTES.min(disk_size - segment * SEGMENT_BYTES);
            if file.metadata()?.len() < len {
                file.set_len(len)?;
            }
            segments.push(Arc::new(file));
        }
        let mut files = Self { chunk_size, segments, holes: true };
        files.holes = files.punch_unlisted(&found, listed)?;
        File::open(dir)?.sync_all()?;
        Ok(files)
    }

    fn punch_unlisted(&self, found: &[bool], listed: &BTreeSet<usize>) -> Result<bool> {
        let per_segment = (SEGMENT_BYTES / self.chunk_size) as usize;
        for (segment, file) in self.segments.iter().enumerate().filter(|(segment, _)| found[*segment]) {
            let first = segment * per_segment;
            let len = file.metadata()?.len();
            let mut start = 0;
            for &index in listed.range(first..first + per_segment) {
                let end = (index - first) as u64 * self.chunk_size;
                if end > start && !punch(file, start, end - start)? {
                    return Ok(false);
                }
                start = end + self.chunk_size;
            }
            if len > start && !punch(file, start, len - start)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// True when a chunk that is not local reads as zeros.
    pub fn has_holes(&self) -> bool {
        self.holes
    }

    /// The data file of a chunk, and the offset of the chunk in it.
    pub fn location(&self, index: usize) -> (&Arc<File>, u64) {
        let offset = index as u64 * self.chunk_size;
        (&self.segments[(offset / SEGMENT_BYTES) as usize], offset % SEGMENT_BYTES)
    }

    pub fn segment_of(&self, index: usize) -> usize {
        (index as u64 * self.chunk_size / SEGMENT_BYTES) as usize
    }

    pub fn segment(&self, segment: usize) -> &Arc<File> {
        &self.segments[segment]
    }

    pub fn read_chunk(&self, index: usize) -> Result<Vec<u8>> {
        let (file, offset) = self.location(index);
        let mut data = vec![0u8; self.chunk_size as usize];
        file.read_exact_at(&mut data, offset)?;
        Ok(data)
    }

    pub fn write_at(&self, index: usize, offset_in_chunk: u64, data: &[u8]) -> Result<()> {
        let (file, offset) = self.location(index);
        Ok(file.write_all_at(data, offset + offset_in_chunk)?)
    }

    /// Puts a whole chunk from a cache file into place. A reflink on btrfs
    /// and XFS, a copy on other filesystems.
    pub fn copy_in(&self, index: usize, source: &File) -> Result<()> {
        let (file, offset) = self.location(index);
        copy_range(source, 0, file, offset, self.chunk_size)
    }

    /// Copies a chunk into a new cache file.
    pub fn copy_out(&self, index: usize, target: &File) -> Result<()> {
        let (file, offset) = self.location(index);
        copy_range(file, offset, target, 0, self.chunk_size)
    }

    /// Makes a chunk zeros, and frees its space where the filesystem can.
    pub fn zero(&self, index: usize) -> Result<()> {
        let (file, offset) = self.location(index);
        if !punch(file, offset, self.chunk_size)? {
            file.write_all_at(&vec![0u8; self.chunk_size as usize], offset)?;
        }
        Ok(())
    }

    /// Starts the write-back of a file without a wait, so that the next
    /// flush has less to write. Only the flush checks for write errors.
    pub fn start_writeback(&self, segment: usize) {
        let fd = self.segments[segment].as_raw_fd();
        unsafe { libc::sync_file_range(fd, 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
    }
}

/// Uses `copy_file_range` with explicit offsets, because other threads
/// read the same files.
fn copy_range(source: &File, from: u64, target: &File, to: u64, len: u64) -> Result<()> {
    let (mut from, mut to, mut left) = (from as i64, to as i64, len as usize);
    while left > 0 {
        let copied = unsafe { libc::copy_file_range(source.as_raw_fd(), &mut from, target.as_raw_fd(), &mut to, left, 0) };
        if copied < 0 {
            return Err(std::io::Error::last_os_error()).context("copy_file_range");
        }
        if copied == 0 {
            bail!("copy_file_range: the source is {left} bytes short");
        }
        left -= copied as usize;
    }
    Ok(())
}

/// False when the filesystem cannot punch holes.
fn punch(file: &File, offset: u64, len: u64) -> Result<bool> {
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    if unsafe { libc::fallocate(file.as_raw_fd(), mode, offset as i64, len as i64) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() == Some(libc::EOPNOTSUPP) {
        true => Ok(false),
        false => Err(error).context("fallocate"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 4096;

    #[test]
    fn unlisted_ranges_are_cleared_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let files = DataFiles::open(dir.path(), 64 * CHUNK, CHUNK, &BTreeSet::new()).unwrap();
        files.write_at(2, 0, &[9; CHUNK as usize]).unwrap();
        files.write_at(3, 0, &[8; CHUNK as usize]).unwrap();
        drop(files);
        let files = DataFiles::open(dir.path(), 64 * CHUNK, CHUNK, &BTreeSet::from([3])).unwrap();
        assert_eq!(files.read_chunk(2).unwrap(), vec![0; CHUNK as usize]);
        assert_eq!(files.read_chunk(3).unwrap(), vec![8; CHUNK as usize]);
    }

    #[test]
    fn chunks_copy_in_and_out() {
        let dir = tempfile::tempdir().unwrap();
        let files = DataFiles::open(dir.path(), 64 * CHUNK, CHUNK, &BTreeSet::new()).unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, vec![7u8; CHUNK as usize]).unwrap();
        files.copy_in(9, &File::open(&source).unwrap()).unwrap();
        let target = File::create(dir.path().join("target")).unwrap();
        files.copy_out(9, &target).unwrap();
        assert_eq!(std::fs::read(dir.path().join("target")).unwrap(), vec![7u8; CHUNK as usize]);
        files.zero(9).unwrap();
        assert_eq!(files.read_chunk(9).unwrap(), vec![0; CHUNK as usize]);
    }
}
