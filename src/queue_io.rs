use crate::local_io::{LocalRead, LocalWrite};
use anyhow::anyhow;
use io_uring::{opcode, types};
use libublk::UblkUringData;
use libublk::uring_async::ublk_submit_sqe_async;
use std::os::fd::AsRawFd;

/// Serves a local read on the ublk queue thread: io_uring reads straight into
/// the request buffer. No tokio task, no thread pool, no copy.
pub async fn read_local(plan: LocalRead, buffer: &mut [u8]) -> i32 {
    let base = buffer.as_mut_ptr();
    let mut reads = Vec::new();
    for (part, file) in &plan.parts {
        let len = part.buffer.len();
        // The parts cover separate ranges of the buffer.
        let target = unsafe { base.add(part.buffer.start) };
        match file {
            Some(file) => {
                let read = opcode::Read::new(types::Fd(file.as_raw_fd()), target, len as u32).offset(part.offset);
                reads.push(complete(read.build(), len));
            }
            None => unsafe { std::ptr::write_bytes(target, 0, len) },
        }
    }
    let done = futures::future::join_all(reads).await;
    // `plan` holds the files open until every read has completed.
    drop(plan);
    match done.iter().all(|ok| *ok) {
        true => buffer.len() as i32,
        false => -libc::EIO,
    }
}

/// Does a local write on the ublk queue thread. A durable (FUA) write also
/// syncs the chunk files it wrote, through the same ring.
pub async fn write_local(plan: LocalWrite<'_>, buffer: &[u8], durable: bool) -> i32 {
    let base = buffer.as_ptr();
    let writes = plan.parts.iter().map(|part| {
        let len = part.part.buffer.len();
        let source = unsafe { base.add(part.part.buffer.start) };
        let write = opcode::Write::new(types::Fd(part.file.as_raw_fd()), source, len as u32).offset(part.part.offset);
        complete(write.build(), len)
    });
    if !futures::future::join_all(writes).await.iter().all(|ok| *ok) {
        return -libc::EIO;
    }
    if durable {
        let syncs = plan.parts.iter().map(|part| {
            let sync = opcode::Fsync::new(types::Fd(part.file.as_raw_fd())).flags(types::FsyncFlags::DATASYNC);
            complete(sync.build(), 0)
        });
        if !futures::future::join_all(syncs).await.iter().all(|ok| *ok) {
            plan.disk().fail_sync(&anyhow!("fdatasync of a FUA write failed"));
            return -libc::EIO;
        }
    }
    plan.finish();
    buffer.len() as i32
}

/// Runs one operation on the queue's ring. `expected` is the byte count that
/// counts as success; a short read or write is treated as an error.
async fn complete(entry: io_uring::squeue::Entry, expected: usize) -> bool {
    match ublk_submit_sqe_async(entry, UblkUringData::Target as u64).await {
        Ok(result) if result >= 0 && result as usize == expected => true,
        Ok(result) => {
            log::error!("queue I/O returned {result}, expected {expected}");
            false
        }
        Err(error) => {
            log::error!("queue I/O could not be submitted: {error}");
            false
        }
    }
}
