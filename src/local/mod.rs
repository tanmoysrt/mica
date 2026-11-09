//! The node side of mica: the files on the local disk of the node.
//!
//! Each attached disk has a folder with its data files and its dirty log.
//! The node has one chunk cache, with a folder and a budget for each disk.

pub mod buckets;
pub mod cache;
pub mod data_files;
pub mod dirty_log;
pub mod folder;

pub use buckets::KnownBuckets;
pub use cache::{ChunkCache, NodeCache};
pub use data_files::DataFiles;
pub use dirty_log::DirtyLog;
pub use folder::{DiskFolder, LocalState};

use anyhow::{Context, Result};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Writes a small file so that a crash or a power loss leaves the old or
/// the new content, never a part: a new file, synced, renamed into place,
/// then the folder synced. `mode` is for a new file, such as 0o600.
pub fn write_durable(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let folder = path.parent().context("a file needs a folder")?;
    let temp = path.with_extension("new");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&temp)?;
    std::io::Write::write_all(&mut file, content)?;
    file.sync_all()?;
    std::fs::rename(&temp, path).with_context(|| format!("write {}", path.display()))?;
    std::fs::File::open(folder)?.sync_all()?;
    Ok(())
}

/// Makes a new, empty file NOCOW on btrfs, as for VM images. A
/// copy-on-write file gets new extents and checksums at each sync: in the
/// lab, a NOCOW data file gave 70 % more random writes and 40 % more synced
/// writes. The cache files are NOCOW too, because btrfs reflinks a chunk
/// between the data file and the cache only when both files are alike.
///
/// btrfs then keeps no checksums of these files, so it cannot find a bad
/// sector in data that is not in the bucket yet. A cache file has its hash.
/// Other filesystems refuse the flag, and the file stays as it is.
pub fn no_copy_on_write(file: &File) {
    const FS_NOCOW_FL: libc::c_int = 0x0080_0000;
    let mut flags: libc::c_int = 0;
    unsafe {
        if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) == 0 {
            flags |= FS_NOCOW_FL;
            libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags);
        }
    }
}
