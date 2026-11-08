//! The data pages of one ublk request, used in place.
//!
//! Before a request reaches mica, the kernel registers its pages in the
//! io_uring of the queue as fixed buffer number `tag` (`UBLK_F_AUTO_BUF_REG`).
//! `ReadFixed` and `WriteFixed` then move data between those pages and a
//! file, with no copy. Their address is the offset in the request, not a
//! pointer.
//!
//! Data that tokio has in memory goes through `/dev/ublkcN` instead
//! (`UBLK_F_USER_COPY`). That is one copy. It also works when the kernel
//! could not register the pages.
//!
//! The commit of a request unregisters its pages. So each method returns
//! only after all its I/O is complete.

use crate::disk::{LocalRead, LocalWrite, ReadSource};
use anyhow::{Result, bail};
use io_uring::{opcode, squeue, types};
use libublk::UblkUringData;
use libublk::io::UblkIOCtx;
use libublk::uring_async::ublk_submit_sqe_async;
use std::fs::File;
use std::os::fd::AsRawFd;

/// The ring of each queue has the char device `/dev/ublkcN` as fixed file 0.
const CHAR_DEVICE: types::Fixed = types::Fixed(0);

pub struct RequestBuffer<'a> {
    pub queue: u16,
    pub tag: u16,
    /// `/dev/zero`, for the parts of a read that were never written.
    pub zero: &'a File,
}

impl RequestBuffer<'_> {
    /// Reads local chunks straight into the request. Returns the bytes
    /// done, or a negative errno.
    pub async fn read_local(&self, plan: LocalRead, len: usize) -> i32 {
        let reads = plan.parts.iter().map(|(part, source)| {
            let (fd, offset) = match source {
                ReadSource::File(file, offset) => (file.as_raw_fd(), *offset),
                ReadSource::Zeros => (self.zero.as_raw_fd(), 0),
            };
            transfer(part.buffer.len(), move |done, left| {
                let target = (part.buffer.start + done) as *mut u8;
                opcode::ReadFixed::new(types::Fd(fd), target, left, self.tag).offset(offset + done as u64).build()
            })
        });
        let done = futures::future::join_all(reads).await;
        // The plan keeps the files open and the chunks locked until here.
        drop(plan);
        if done.iter().all(|ok| *ok) { len as i32 } else { -libc::EIO }
    }

    /// Writes the request straight into local chunks.
    pub async fn write_local(&self, plan: LocalWrite<'_>, len: usize) -> i32 {
        let writes = plan.parts.iter().map(|(part, file, offset)| {
            let (fd, offset) = (types::Fd(file.as_raw_fd()), *offset);
            transfer(part.buffer.len(), move |done, left| {
                let source = (part.buffer.start + done) as *const u8;
                opcode::WriteFixed::new(fd, source, left, self.tag).offset(offset + done as u64).build()
            })
        });
        let done = futures::future::join_all(writes).await;
        // Also after a failed part: the parts that worked changed the chunks.
        plan.finish();
        if done.iter().all(|ok| *ok) { len as i32 } else { -libc::EIO }
    }

    /// Copies data into the request, for a read that tokio served.
    pub async fn copy_in(&self, data: &[u8]) -> Result<()> {
        let copied = transfer(data.len(), |done, left| {
            opcode::Write::new(CHAR_DEVICE, data[done..].as_ptr(), left).offset(self.position(done)).build()
        });
        if !copied.await {
            bail!("copy into the request failed");
        }
        Ok(())
    }

    /// Copies the request out, for a write that tokio does.
    pub async fn copy_out(&self, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0u8; len];
        let base = data.as_mut_ptr();
        let copied = transfer(len, |done, left| {
            // Each read fills the next part of `data`.
            opcode::Read::new(CHAR_DEVICE, unsafe { base.add(done) }, left).offset(self.position(done)).build()
        });
        if !copied.await {
            bail!("copy out of the request failed");
        }
        Ok(data)
    }

    fn position(&self, offset: usize) -> u64 {
        UblkIOCtx::ublk_user_copy_pos(self.queue, self.tag, offset as u32)
    }
}

/// Moves `len` bytes with operations on the ring of the queue. `entry(done,
/// left)` makes the operation for the rest. A short result is no error:
/// io_uring does not retry short reads of a character device such as `/dev/zero`.
async fn transfer(len: usize, entry: impl Fn(usize, u32) -> squeue::Entry) -> bool {
    let mut done = 0;
    while done < len {
        match ublk_submit_sqe_async(entry(done, (len - done) as u32), UblkUringData::Target as u64).await {
            Ok(moved) if moved > 0 => done += moved as usize,
            Ok(result) => {
                log::error!("queue I/O returned {result} after {done} of {len} bytes");
                return false;
            }
            Err(error) => {
                log::error!("queue I/O could not be submitted: {error}");
                return false;
            }
        }
    }
    true
}
