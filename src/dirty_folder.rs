use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// The chunks of one disk that S3 does not have yet.
///
/// - `<index>.chunk`  the current content of a chunk the guest changed
/// - `<index>.frozen` a chunk that a checkpoint uploads now
/// - `<index>.tmp`    a chunk being made; never trusted after a crash
pub struct DirtyFolder {
    dir: PathBuf,
    chunk_size: u64,
}

impl DirtyFolder {
    pub fn open(dir: PathBuf, chunk_size: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self { dir, chunk_size })
    }

    /// Cleans up after a crash, and returns the chunks that have local data.
    /// A `.chunk` is always newer than a `.frozen` of the same index.
    pub fn recover(&self) -> Result<Vec<usize>> {
        let mut working = BTreeSet::new();
        let mut frozen = BTreeSet::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let Some((index, kind)) = parse_name(&path) else { continue };
            match kind {
                "chunk" => drop(working.insert(index)),
                "frozen" => drop(frozen.insert(index)),
                _ => std::fs::remove_file(&path)?,
            }
        }
        for index in frozen {
            if working.contains(&index) {
                std::fs::remove_file(self.frozen_path(index))?;
            } else {
                std::fs::rename(self.frozen_path(index), self.working_path(index))?;
                working.insert(index);
            }
        }
        self.sync()?;
        Ok(working.into_iter().collect())
    }

    pub fn open_working(&self, index: usize) -> Result<File> {
        Ok(OpenOptions::new().read(true).write(true).open(self.working_path(index))?)
    }

    /// Makes `<index>.chunk` as a copy of `source`, or as zeros when there is no source.
    /// The file is synced before it gets its final name, so a crash cannot
    /// leave a half-copied chunk that looks complete.
    pub fn create_working(&self, index: usize, source: Option<&File>) -> Result<File> {
        let temp = self.dir.join(format!("{index:08}.tmp"));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&temp)?;
        if let Some(source) = source {
            copy_whole_file(source, &file, self.chunk_size)?;
        }
        file.set_len(self.chunk_size)?;
        file.sync_data()?;
        std::fs::rename(&temp, self.working_path(index))?;
        Ok(file)
    }

    pub fn freeze(&self, index: usize) -> Result<()> {
        std::fs::rename(self.working_path(index), self.frozen_path(index))?;
        Ok(())
    }

    pub fn remove_frozen(&self, index: usize) -> Result<()> {
        match std::fs::remove_file(self.frozen_path(index)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Makes file creates, renames and deletes in the folder durable.
    pub fn sync(&self) -> Result<()> {
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    pub fn working_path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("{index:08}.chunk"))
    }

    pub fn frozen_path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("{index:08}.frozen"))
    }
}

/// Uses copy_file_range with explicit offsets. On XFS and btrfs this is a
/// reflink. Explicit offsets matter: the source may be shared with readers.
fn copy_whole_file(source: &File, target: &File, len: u64) -> Result<()> {
    let mut source_offset: i64 = 0;
    let mut target_offset: i64 = 0;
    while (source_offset as u64) < len {
        let remaining = len as usize - source_offset as usize;
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
            break;
        }
    }
    Ok(())
}

fn parse_name(path: &std::path::Path) -> Option<(usize, &str)> {
    let index = path.file_stem()?.to_str()?.parse().ok()?;
    let kind = path.extension()?.to_str()?;
    Some((index, kind))
}
