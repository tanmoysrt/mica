use crate::local_io::{LocalRead, LocalWrite};
use anyhow::{Result, bail};
use io_uring::{opcode, squeue, types};
use libublk::UblkUringData;
use libublk::io::UblkIOCtx;
use libublk::uring_async::ublk_submit_sqe_async;
use std::fs::File;
use std::os::fd::AsRawFd;

/// The queue ring has the char device `/dev/ublkcN` at fixed file 0.
const CHAR_DEVICE: types::Fixed = types::Fixed(0);

/// The data pages of one ublk request, used in place. Before the request
/// reaches the daemon, the kernel registers its pages in the queue ring as
/// fixed buffer `tag` (`UBLK_F_AUTO_BUF_REG`). `ReadFixed` and `WriteFixed`
/// then move data between the pages and a file with no copy. Their address
/// is the offset in the request, not a pointer.
///
/// Data that tokio has in memory goes through `/dev/ublkcN` instead
/// (`UBLK_F_USER_COPY`): one copy. That also works when the kernel could
/// not register the pages.
///
/// Every operation must complete before the commit, because the commit
/// unregisters the buffer: each method returns only after its I/O ends.
pub struct RequestBuffer<'a> {
    pub queue_id: u16,
    pub tag: u16,
    /// `/dev/zero`, for the parts of a read that were never written.
    pub zero: &'a File,
}

impl RequestBuffer<'_> {
    /// Reads local chunks straight into the request. Returns bytes done, or
    /// a negative errno.
    pub async fn read_local(&self, plan: LocalRead<'_>, len: usize) -> i32 {
        let reads = plan.parts.iter().map(|(part, source)| {
            let (file, offset) = match source {
                Some((file, start)) => (file.as_raw_fd(), start + part.offset),
                None => (self.zero.as_raw_fd(), 0),
            };
            transfer(part.buffer.len(), move |done, left| {
                let target = (part.buffer.start + done) as *mut u8;
                opcode::ReadFixed::new(types::Fd(file), target, left, self.tag).offset(offset + done as u64).build()
            })
        });
        let done = futures::future::join_all(reads).await;
        // `plan` holds the files open until every read has completed.
        drop(plan);
        match done.iter().all(|ok| *ok) {
            true => len as i32,
            false => -libc::EIO,
        }
    }

    /// Writes the request straight into local chunks.
    pub async fn write_local(&self, plan: LocalWrite<'_>, len: usize) -> i32 {
        let writes = plan.parts.iter().map(|part| {
            let file = types::Fd(part.file.as_raw_fd());
            transfer(part.part.buffer.len(), move |done, left| {
                let source = (part.part.buffer.start + done) as *const u8;
                opcode::WriteFixed::new(file, source, left, self.tag).offset(part.offset + done as u64).build()
            })
        });
        if !futures::future::join_all(writes).await.iter().all(|ok| *ok) {
            return -libc::EIO;
        }
        if let Err(error) = plan.finish() {
            log::error!("local write could not be listed: {error:#}");
            return -libc::EIO;
        }
        len as i32
    }

    /// Copies data into the request, for a read that tokio served.
    pub async fn copy_in(&self, data: &[u8]) -> Result<()> {
        let copied = transfer(data.len(), |done, left| {
            opcode::Write::new(CHAR_DEVICE, data[done..].as_ptr(), left).offset(self.position(done)).build()
        });
        match copied.await {
            true => Ok(()),
            false => bail!("copy into the request failed"),
        }
    }

    /// Copies the request out, for a write that tokio does.
    pub async fn copy_out(&self, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0u8; len];
        let base = data.as_mut_ptr();
        let copied = transfer(len, |done, left| {
            // The reads fill separate ranges of `data`, one after another.
            let target = unsafe { base.add(done) };
            opcode::Read::new(CHAR_DEVICE, target, left).offset(self.position(done)).build()
        });
        match copied.await {
            true => Ok(data),
            false => bail!("copy out of the request failed"),
        }
    }

    fn position(&self, offset: usize) -> u64 {
        UblkIOCtx::ublk_user_copy_pos(self.queue_id, self.tag, offset as u32)
    }
}

/// Moves `len` bytes with operations on the queue's ring. `entry(done,
/// left)` makes the operation for the rest. A short result is not an error:
/// io_uring retries short reads only on files and block devices, so a
/// character device such as `/dev/zero` can stop early under load.
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
