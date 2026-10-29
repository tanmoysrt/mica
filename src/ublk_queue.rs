use crate::disk::Disk;
use crate::queue_io::RequestBuffer;
use anyhow::{Result, anyhow};
use io_uring::{opcode, types};
use libublk::io::{UblkDev, UblkQueue};
use libublk::uring_async::ublk_submit_sqe_async;
use libublk::{BufDesc, UblkError, UblkUringData, sys};
use std::fs::File;
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::runtime::Handle;

/// Requests whose pages the kernel could not register, since start.
static UNREGISTERED: AtomicU64 = AtomicU64::new(0);

/// Runs on the queue thread. libublk drives one smol executor per queue,
/// with one task per request tag.
pub fn serve_queue(queue_id: u16, dev: &UblkDev, disk: &Arc<Disk>, runtime: &Handle) {
    // Declared before the queue, so it outlives the io_uring read that points to it.
    let mut wake_buffer = Box::new([0u8; 8]);
    let waker = match QueueWaker::new() {
        Ok(waker) => waker,
        Err(error) => return log::error!("ublk queue {queue_id}: eventfd failed: {error}"),
    };
    let zero = match File::open("/dev/zero") {
        Ok(zero) => Rc::new(zero),
        Err(error) => return log::error!("ublk queue {queue_id}: open /dev/zero: {error}"),
    };
    let queue = match UblkQueue::new(queue_id, dev) {
        Ok(queue) => Rc::new(queue),
        Err(error) => return log::error!("ublk queue {queue_id} failed: {error}"),
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let mut tasks = Vec::new();
    for tag in queue.tags() {
        let (queue, zero, disk, runtime, waker) = (queue.clone(), zero.clone(), disk.clone(), runtime.clone(), waker.clone());
        tasks.push(executor.spawn(async move {
            let buffer = RequestBuffer { queue_id, tag, zero: &zero };
            match serve_tag(&queue, &buffer, &disk, &runtime, &waker).await {
                Ok(()) | Err(UblkError::QueueIsDown) => {}
                Err(error) => log::error!("ublk tag {tag} failed: {error}"),
            }
        }));
    }
    let buffer = wake_buffer.as_mut_ptr();
    executor.spawn(async move { waker.keep_reading(buffer).await }).detach();
    let ticker = executor.clone();
    smol::block_on(executor.run(async move {
        let run_ops = || while ticker.try_tick() {};
        let done = || tasks.iter().all(|task| task.is_finished());
        if let Err(error) = libublk::wait_and_handle_io_events(&queue, Some(20), run_ops, done).await {
            log::error!("ublk queue {queue_id} stopped: {error}");
        }
    }));
}

async fn serve_tag(
    queue: &UblkQueue<'_>,
    buffer: &RequestBuffer<'_>,
    disk: &Arc<Disk>,
    runtime: &Handle,
    waker: &QueueWaker,
) -> Result<(), UblkError> {
    // With the fallback flag, a failed registration still delivers the
    // request, marked with UBLK_IO_F_NEED_REG_BUF, instead of failing it.
    let pages = sys::ublk_auto_buf_reg {
        index: buffer.tag,
        flags: sys::UBLK_AUTO_BUF_REG_FALLBACK as u8,
        ..Default::default()
    };
    queue.submit_io_prep_cmd(buffer.tag, BufDesc::AutoReg(pages), 0, None).await?;
    loop {
        let request = *queue.get_iod(buffer.tag);
        let result = handle_request(request, buffer, disk, runtime, waker).await;
        queue.submit_io_commit_cmd(buffer.tag, BufDesc::AutoReg(pages), result).await?;
    }
}

/// Returns bytes done, or a negative errno.
async fn handle_request(
    request: sys::ublksrv_io_desc,
    buffer: &RequestBuffer<'_>,
    disk: &Arc<Disk>,
    runtime: &Handle,
    waker: &QueueWaker,
) -> i32 {
    let offset = request.start_sector << 9;
    let len = (request.nr_sectors as usize) << 9;
    let op = request.op_flags & 0xff;
    let durable = request.op_flags & sys::UBLK_IO_F_FUA != 0;
    let registered = request.op_flags & sys::UBLK_IO_F_NEED_REG_BUF == 0;
    if !registered {
        note_unregistered();
    }
    // Local data is served on this thread. Anything that needs S3, a chunk
    // that is not local yet, a FUA write, a busy lock, or pages that are not
    // registered go to tokio below.
    if registered
        && op == sys::UBLK_IO_OP_READ
        && let Some(plan) = disk.plan_local_read(offset, len)
    {
        return buffer.read_local(plan, len).await;
    }
    // A FUA write goes to the engine. Its sync must take the one FLUSH path:
    // Linux reports a writeback error only once to each open file, so two
    // syncs at the same time could otherwise hide a failure from each other.
    if registered
        && op == sys::UBLK_IO_OP_WRITE
        && !durable
        && let Some(plan) = disk.plan_local_write(offset, len)
    {
        return buffer.write_local(plan, len).await;
    }
    let disk = disk.clone();
    let outcome = match op {
        sys::UBLK_IO_OP_READ => match on_tokio(runtime, waker, async move { disk.read(offset, len).await }).await {
            Ok(data) => buffer.copy_in(&data).await.map(|()| len as i32),
            Err(error) => Err(error),
        },
        sys::UBLK_IO_OP_WRITE => match buffer.copy_out(len).await {
            Ok(data) => {
                on_tokio(runtime, waker, async move {
                    match durable {
                        true => disk.write_durable(offset, data).await?,
                        false => disk.write(offset, data).await?,
                    }
                    Ok(len as i32)
                })
                .await
            }
            Err(error) => Err(error),
        },
        sys::UBLK_IO_OP_FLUSH => on_tokio(runtime, waker, async move { disk.flush().await.map(|_| 0) }).await,
        sys::UBLK_IO_OP_DISCARD => {
            on_tokio(runtime, waker, async move { disk.discard(offset, len as u64).await.map(|_| 0) }).await
        }
        sys::UBLK_IO_OP_WRITE_ZEROES => {
            on_tokio(runtime, waker, async move { disk.write_zeroes(offset, len as u64).await.map(|_| 0) }).await
        }
        _ => Ok(-libc::EINVAL),
    };
    outcome.unwrap_or_else(|error| {
        log::error!("I/O at {offset}+{len} failed: {error:#}");
        -libc::EIO
    })
}

/// Rare: the kernel registers request pages unless it runs out of memory.
/// Logged at 1, 2, 4, 8... so a steady stream does not flood the log.
fn note_unregistered() {
    let count = UNREGISTERED.fetch_add(1, Ordering::Relaxed) + 1;
    if count.is_power_of_two() {
        log::warn!("ublk: {count} requests without registered pages so far; served by copy");
    }
}

/// The disk engine runs on tokio. The result must be in the channel before
/// the queue thread wakes up. With a join handle, the thread could wake too
/// early, see nothing, and sleep until the 20 s io_uring idle timeout.
async fn on_tokio<T: Send + 'static>(
    runtime: &Handle,
    waker: &QueueWaker,
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let waker = waker.clone();
    runtime.spawn(async move {
        let _ = sender.send(work.await);
        waker.wake();
    });
    receiver.await.map_err(|_| anyhow!("the disk task stopped without a result"))?
}

/// The queue thread sleeps inside io_uring, so waking its smol task is not
/// enough: only an io_uring completion wakes the thread. tokio writes to this
/// eventfd, and the queue always has a read on it.
#[derive(Clone)]
struct QueueWaker {
    fd: Arc<OwnedFd>,
}

impl QueueWaker {
    fn new() -> Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }) })
    }

    fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    async fn keep_reading(&self, buffer: *mut u8) {
        loop {
            let read = opcode::Read::new(types::Fd(self.fd.as_raw_fd()), buffer, 8).build();
            match ublk_submit_sqe_async(read, UblkUringData::Target as u64).await {
                Ok(result) if result >= 0 => {}
                _ => break,
            }
        }
    }
}
