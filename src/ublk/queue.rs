//! One ublk queue thread.
//!
//! The kernel sends the block requests of a device to its queues. Each
//! queue has one thread with one io_uring, and one task for each request
//! tag. A request that only touches local data is served on this thread.
//! All others go to the disk engine on tokio, and the result comes back
//! through an eventfd.
//!
//! An idle queue thread waits in io_uring with requests in flight: its
//! fetch commands. By default the kernel counts such a wait as I/O wait,
//! so an idle host would show I/O pressure near 100 %. So mica waits with
//! `IORING_ENTER_NO_IOWAIT` (Linux 6.15 and later).

use super::request::RequestBuffer;
use crate::disk::Disk;
use anyhow::{Result, anyhow};
use io_uring::{opcode, types};
use libublk::io::{UblkDev, UblkQueue, with_task_io_ring_mut};
use libublk::uring_async::{run_uring_tasks, ublk_reap_io_events_with_update_queue, ublk_submit_sqe_async, ublk_wake_task};
use libublk::{BufDesc, UblkError, UblkUringData, sys};
use std::fs::File;
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::runtime::Handle;

const IDLE_WAIT: Duration = Duration::from_secs(20);
const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_EXT_ARG: u32 = 1 << 3;
const ENTER_NO_IOWAIT: u32 = 1 << 7;

/// Requests whose pages the kernel could not register, since start.
static UNREGISTERED: AtomicU64 = AtomicU64::new(0);

pub fn serve_queue(queue_id: u16, dev: &UblkDev, disk: &Arc<Disk>, runtime: &Handle) {
    // Declared before the queue, so it lives longer than the read into it.
    let mut wake_buffer = Box::new([0u8; 8]);
    let started = (|| -> Result<_> {
        let waker = QueueWaker::new()?;
        let zero = Rc::new(File::open("/dev/zero")?);
        let queue = Rc::new(UblkQueue::new(queue_id, dev)?);
        Ok((waker, zero, queue))
    })();
    let (waker, zero, queue) = match started {
        Ok(parts) => parts,
        Err(error) => return log::error!("ublk queue {queue_id} failed to start: {error:#}"),
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let mut tasks = Vec::new();
    for tag in queue.tags() {
        let (queue, zero, disk, runtime, waker) = (queue.clone(), zero.clone(), disk.clone(), runtime.clone(), waker.clone());
        tasks.push(executor.spawn(async move {
            let buffer = RequestBuffer { queue: queue_id, tag, zero: &zero };
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
        if let Err(error) = handle_events(&queue, run_ops, done).await {
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
    let pages = sys::ublk_auto_buf_reg { index: buffer.tag, flags: sys::UBLK_AUTO_BUF_REG_FALLBACK as u8, ..Default::default() };
    queue.submit_io_prep_cmd(buffer.tag, BufDesc::AutoReg(pages), 0, None).await?;
    loop {
        let request = *queue.get_iod(buffer.tag);
        let result = handle(request, buffer, disk, runtime, waker).await;
        queue.submit_io_commit_cmd(buffer.tag, BufDesc::AutoReg(pages), result).await?;
    }
}

/// Returns the bytes done, or a negative errno.
async fn handle(request: sys::ublksrv_io_desc, buffer: &RequestBuffer<'_>, disk: &Arc<Disk>, runtime: &Handle, waker: &QueueWaker) -> i32 {
    let offset = request.start_sector << 9;
    let len = (request.nr_sectors as usize) << 9;
    let op = request.op_flags & 0xff;
    let durable = request.op_flags & sys::UBLK_IO_F_FUA != 0;
    let registered = request.op_flags & sys::UBLK_IO_F_NEED_REG_BUF == 0;
    if !registered {
        note_unregistered();
    }
    if registered && op == sys::UBLK_IO_OP_READ && let Some(plan) = disk.plan_local_read(offset, len) {
        return buffer.read_local(plan, len).await;
    }
    // A write with forced unit access goes to tokio: its sync must take the
    // one-at-a-time path of a flush.
    if registered && op == sys::UBLK_IO_OP_WRITE && !durable && let Some(plan) = disk.plan_local_write(offset, len) {
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
                let write = async move {
                    match durable {
                        true => disk.write_durable(offset, data).await?,
                        false => disk.write(offset, data).await?,
                    }
                    Ok(len as i32)
                };
                on_tokio(runtime, waker, write).await
            }
            Err(error) => Err(error),
        },
        sys::UBLK_IO_OP_FLUSH => on_tokio(runtime, waker, async move { disk.flush().await.map(|()| 0) }).await,
        sys::UBLK_IO_OP_DISCARD => on_tokio(runtime, waker, async move { disk.discard(offset, len as u64).await.map(|()| 0) }).await,
        sys::UBLK_IO_OP_WRITE_ZEROES => {
            on_tokio(runtime, waker, async move { disk.write_zeroes(offset, len as u64).await.map(|()| 0) }).await
        }
        _ => Ok(-libc::EINVAL),
    };
    outcome.unwrap_or_else(|error| {
        log::error!("I/O at {offset}+{len} failed: {error:#}");
        -libc::EIO
    })
}

/// `libublk::wait_and_handle_io_events`, with a wait that is not I/O wait.
async fn handle_events(queue: &UblkQueue<'_>, run_ops: impl Fn(), done: impl Fn() -> bool) -> Result<(), UblkError> {
    let wait = || async { wait_for_completions(queue) };
    let reap = |timed_out| ublk_reap_io_events_with_update_queue(queue, timed_out, None, |cqe| ublk_wake_task(cqe.user_data(), cqe));
    run_uring_tasks(wait, reap, run_ops, done).await
}

/// Submits the queued operations and waits for one completion. Ok(true)
/// means that the wait timed out.
fn wait_for_completions(queue: &UblkQueue<'_>) -> Result<bool, UblkError> {
    let stopping = queue.is_stopping();
    let result = with_task_io_ring_mut(|ring| {
        let timeout = types::Timespec::new().sec(IDLE_WAIT.as_secs());
        let args = types::SubmitArgs::new().timespec(&timeout);
        let pending = ring.submission().len() as u32;
        // SubmitArgs has the layout of struct io_uring_getevents_arg.
        unsafe { ring.submitter().enter(pending, u32::from(!stopping), ENTER_GETEVENTS | ENTER_EXT_ARG | ENTER_NO_IOWAIT, Some(&args)) }
    });
    match result {
        _ if stopping => Err(UblkError::QueueIsDown),
        Err(error) if error.raw_os_error() == Some(libc::ETIME) => Ok(true),
        Err(error) => Err(UblkError::IOError(error)),
        Ok(_) => Ok(false),
    }
}

/// Rare: the kernel registers the pages unless it runs out of memory.
/// Logged at 1, 2, 4, 8 and so on.
fn note_unregistered() {
    let count = UNREGISTERED.fetch_add(1, Ordering::Relaxed) + 1;
    if count.is_power_of_two() {
        log::warn!("ublk: {count} requests without registered pages so far, served by copy");
    }
}

/// Runs the work on tokio. The result is in the channel before the queue
/// thread wakes. With a join handle, the thread could wake too early, find
/// nothing, and sleep until the idle timeout.
async fn on_tokio<T: Send + 'static>(runtime: &Handle, waker: &QueueWaker, work: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let waker = waker.clone();
    runtime.spawn(async move {
        let _ = sender.send(work.await);
        waker.wake();
    });
    receiver.await.map_err(|_| anyhow!("the disk task stopped without a result"))?
}

/// The queue thread sleeps in io_uring, so only an io_uring completion
/// wakes it. tokio writes to this eventfd, and the queue always reads it.
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
            if !matches!(ublk_submit_sqe_async(read, UblkUringData::Target as u64).await, Ok(result) if result >= 0) {
                break;
            }
        }
    }
}
