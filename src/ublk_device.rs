use crate::disk::Disk;
use anyhow::{Context, Result, anyhow, bail};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use io_uring::{IoUring, opcode, squeue, types};
use libublk::uring_async::ublk_submit_sqe_async;
use libublk::{BufDesc, UblkError, UblkFlags, UblkUringData, sys};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::Handle;

const QUEUE_DEPTH: u16 = 64;
const IO_BUFFER_BYTES: u32 = 512 << 10;

/// The `/dev/ublkbN` block device of one disk. ublk only turns kernel block
/// requests into calls to `Disk`.
///
/// Devices use ublk user recovery: when the daemon exits, the kernel keeps
/// the device and holds its I/O. The next daemon takes it over with `recover`.
pub struct UblkDevice {
    ctrl: Arc<UblkCtrl>,
    pub id: i32,
    pub path: String,
    thread: Option<JoinHandle<()>>,
}

impl UblkDevice {
    /// Makes a new device and waits until the kernel has made it.
    pub fn start(disk: Arc<Disk>, runtime: Handle) -> Result<Self> {
        Self::launch(disk, runtime, None)
    }

    /// Takes over a device that a previous daemon left paused.
    pub fn recover(disk: Arc<Disk>, runtime: Handle, id: i32) -> Result<Self> {
        ensure_control_ring()?;
        let result = UblkCtrl::new_simple(id)?.start_user_recover()?;
        if result < 0 {
            bail!("start recovery of ublk device {id}: errno {}", -result);
        }
        Self::launch(disk, runtime, Some(id))
    }

    /// Removes the device. Its queue threads end, then the device thread ends.
    pub fn stop(mut self) -> Result<()> {
        ensure_control_ring()?;
        self.ctrl.kill_dev().context("stop ublk device")?;
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| anyhow!("ublk device thread panicked"))?;
        }
        Ok(())
    }

    /// Leaves the device in place when the daemon exits, for the next daemon.
    pub fn hand_over(&self) {
        self.ctrl.disown();
    }

    fn launch(disk: Arc<Disk>, runtime: Handle, recover_id: Option<i32>) -> Result<Self> {
        ensure_control_ring()?;
        let builder = UblkCtrlBuilder::default()
            .name("mica")
            .nr_queues(1)
            .depth(QUEUE_DEPTH)
            .io_buf_bytes(IO_BUFFER_BYTES)
            .ctrl_flags(sys::UBLK_F_USER_RECOVERY as u64 | sys::UBLK_F_USER_RECOVERY_REISSUE as u64);
        let builder = match recover_id {
            Some(id) => builder.id(id).dev_flags(UblkFlags::UBLK_DEV_F_RECOVER_DEV),
            None => builder.dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV),
        };
        let ctrl = Arc::new(
            builder.build().context("create ublk device (is the ublk_drv module loaded, and are you root?)")?,
        );
        let id = ctrl.dev_info().dev_id as i32;
        let (ready_sender, ready) = mpsc::channel::<Result<String, String>>();
        let thread_ctrl = ctrl.clone();
        let (size, chunk_size) = (disk.size, disk.chunk_size as u32);
        let thread = std::thread::Builder::new().name(format!("ublk-{}", disk.id)).spawn(move || {
            if let Err(error) = ensure_control_ring() {
                let _ = ready_sender.send(Err(error.to_string()));
                return;
            }
            let started = ready_sender.clone();
            let result = thread_ctrl.run_target(
                move |dev| {
                    set_device_params(dev, size, chunk_size);
                    Ok(())
                },
                move |queue_id, dev| serve_queue(queue_id, dev, &disk, &runtime),
                move |ctrl| drop(started.send(Ok(ctrl.get_bdev_path()))),
            );
            if let Err(error) = result {
                let _ = ready_sender.send(Err(error.to_string()));
            }
            let _ = thread_ctrl.del_dev();
        })?;
        let path = ready
            .recv_timeout(Duration::from_secs(30))
            .context("ublk device did not start")?
            .map_err(|error| anyhow!("ublk device failed: {error}"))?;
        Ok(Self { ctrl, id, path, thread: Some(thread) })
    }
}

/// Deletes a device that no daemon will take over, so its users get I/O
/// errors instead of waiting forever.
pub fn remove_paused_device(id: i32) -> Result<()> {
    ensure_control_ring()?;
    UblkCtrl::new_simple(id)?.del_dev()?;
    Ok(())
}

pub fn device_exists(id: i32) -> bool {
    std::path::Path::new(&format!("/sys/class/ublk-char/ublkc{id}")).exists()
}

/// libublk keeps its control ring per thread. A thread that sends control
/// commands (start, stop) needs its own ring, not only the thread that built `UblkCtrl`.
fn ensure_control_ring() -> Result<()> {
    libublk::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            *ring = Some(IoUring::<squeue::Entry128>::builder().build(32).map_err(UblkError::IOError)?);
        }
        Ok(())
    })?;
    Ok(())
}

/// A volatile write cache makes the kernel send FLUSH and FUA. Discard lets
/// guests free chunks with fstrim.
fn set_device_params(dev: &mut UblkDev, size: u64, chunk_size: u32) {
    dev.set_default_params(size);
    let params = &mut dev.tgt.params;
    params.basic.attrs = sys::UBLK_ATTR_VOLATILE_CACHE | sys::UBLK_ATTR_FUA;
    params.types |= sys::UBLK_PARAM_TYPE_DISCARD;
    params.discard = sys::ublk_param_discard {
        // fstrim then sends only whole chunks, which become zero chunks.
        discard_granularity: chunk_size,
        max_discard_sectors: u32::MAX >> 9,
        max_write_zeroes_sectors: u32::MAX >> 9,
        max_discard_segments: 1,
        ..Default::default()
    };
}

/// Runs on the queue thread. libublk drives one smol executor per queue,
/// with one task per request tag.
fn serve_queue(queue_id: u16, dev: &UblkDev, disk: &Arc<Disk>, runtime: &Handle) {
    // Declared before the queue, so it outlives the io_uring read that points to it.
    let mut wake_buffer = Box::new([0u8; 8]);
    let waker = match QueueWaker::new() {
        Ok(waker) => waker,
        Err(error) => return log::error!("ublk queue {queue_id}: eventfd failed: {error}"),
    };
    let queue = match UblkQueue::new(queue_id, dev) {
        Ok(queue) => Rc::new(queue),
        Err(error) => return log::error!("ublk queue {queue_id} failed: {error}"),
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let mut tasks = Vec::new();
    for tag in queue.tags() {
        let (queue, disk, runtime, waker) = (queue.clone(), disk.clone(), runtime.clone(), waker.clone());
        tasks.push(executor.spawn(async move {
            match serve_tag(&queue, tag, &disk, &runtime, &waker).await {
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
    tag: u16,
    disk: &Arc<Disk>,
    runtime: &Handle,
    waker: &QueueWaker,
) -> Result<(), UblkError> {
    let mut buffer = IoBuf::<u8>::new(queue.dev.dev_info.max_io_buf_bytes as usize);
    queue.submit_io_prep_cmd(tag, BufDesc::Slice(buffer.as_slice()), 0, Some(&buffer)).await?;
    loop {
        let request = *queue.get_iod(tag);
        let result = handle_request(request, buffer.as_mut_slice(), disk, runtime, waker).await;
        queue.submit_io_commit_cmd(tag, BufDesc::Slice(buffer.as_slice()), result).await?;
    }
}

/// Returns bytes done, or a negative errno.
async fn handle_request(
    request: sys::ublksrv_io_desc,
    buffer: &mut [u8],
    disk: &Arc<Disk>,
    runtime: &Handle,
    waker: &QueueWaker,
) -> i32 {
    let offset = request.start_sector << 9;
    let len = (request.nr_sectors as usize) << 9;
    let disk = disk.clone();
    let outcome = match request.op_flags & 0xff {
        sys::UBLK_IO_OP_READ => on_tokio(runtime, waker, async move { disk.read(offset, len).await })
            .await
            .map(|data| {
                buffer[..len].copy_from_slice(&data);
                len as i32
            }),
        sys::UBLK_IO_OP_WRITE => {
            let data = buffer[..len].to_vec();
            let force_unit_access = request.op_flags & sys::UBLK_IO_F_FUA != 0;
            on_tokio(runtime, waker, async move {
                disk.write(offset, data).await?;
                if force_unit_access {
                    disk.flush().await?;
                }
                Ok(len as i32)
            })
            .await
        }
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

/// `/dev/mica/<disk-id>` stays the same name, while the `ublkbN` number can change.
pub fn link_device(disk_id: &str, device: &str) -> Result<String> {
    let link = format!("/dev/mica/{disk_id}");
    std::fs::create_dir_all("/dev/mica")?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(device, &link).with_context(|| format!("create {link}"))?;
    Ok(link)
}

pub fn unlink_device(link: &str) {
    let _ = std::fs::remove_file(link);
}
