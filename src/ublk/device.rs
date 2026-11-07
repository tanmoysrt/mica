//! The `/dev/ublkbN` block device of one disk.
//!
//! The device tells the kernel that it has a volatile write cache, so the
//! kernel sends a flush when the guest needs durable data. It accepts
//! discards of whole chunks, so `fstrim` frees whole chunks.
//!
//! Devices use ublk user recovery. When mica exits, the kernel keeps the
//! device and holds its I/O. The next mica takes the device over with
//! `recover`, and the guest sees a pause of about one second.

use super::queue::serve_queue;
use crate::disk::Disk;
use anyhow::{Context, Result, anyhow, bail};
use io_uring::{IoUring, squeue};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::UblkDev;
use libublk::{UblkError, UblkFlags, sys};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::Handle;

const QUEUE_DEPTH: u16 = 64;
/// One queue thread for each CPU, up to this many.
const MAX_QUEUES: u16 = 4;
/// The largest request. mica has no buffer of this size: it uses the pages
/// of the request.
const MAX_REQUEST: u32 = 512 << 10;
/// Zero copy needs Linux 6.16 or later.
const ZERO_COPY: u64 = (sys::UBLK_F_AUTO_BUF_REG | sys::UBLK_F_USER_COPY) as u64;
const RECOVERY: u64 = (sys::UBLK_F_USER_RECOVERY | sys::UBLK_F_USER_RECOVERY_REISSUE) as u64;

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

    /// Takes over a device that a mica before this one left paused.
    pub fn recover(disk: Arc<Disk>, runtime: Handle, id: i32) -> Result<Self> {
        control_ring()?;
        let result = UblkCtrl::new_simple(id)?.start_user_recover()?;
        if result < 0 {
            bail!("start the recovery of ublk device {id}: errno {}", -result);
        }
        Self::launch(disk, runtime, Some(id))
    }

    /// Removes the device. The queue threads end, then the device thread.
    pub fn stop(mut self) -> Result<()> {
        control_ring()?;
        self.ctrl.kill_dev().context("stop the ublk device")?;
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| anyhow!("the ublk device thread panicked"))?;
        }
        Ok(())
    }

    /// Leaves the device in place for the next mica.
    pub fn hand_over(&self) {
        self.ctrl.disown();
    }

    fn launch(disk: Arc<Disk>, runtime: Handle, recover: Option<i32>) -> Result<Self> {
        control_ring()?;
        let cpus = std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1);
        let builder = UblkCtrlBuilder::default()
            .name("mica")
            .nr_queues((cpus as u16).clamp(1, MAX_QUEUES))
            .depth(QUEUE_DEPTH)
            .io_buf_bytes(MAX_REQUEST)
            .ctrl_flags(ZERO_COPY | RECOVERY);
        let builder = match recover {
            Some(id) => builder.id(id).dev_flags(UblkFlags::UBLK_DEV_F_RECOVER_DEV),
            None => builder.dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV),
        };
        let ctrl = Arc::new(builder.build().context("create the ublk device (is ublk_drv loaded, and are you root?)")?);
        let id = ctrl.dev_info().dev_id as i32;
        let (ready_sender, ready) = mpsc::channel::<Result<String, String>>();
        let (thread_ctrl, size, chunk_size) = (ctrl.clone(), disk.size, disk.chunk_size as u32);
        let thread = std::thread::Builder::new().name(format!("ublk-{}", disk.name)).spawn(move || {
            if let Err(error) = control_ring() {
                return drop(ready_sender.send(Err(error.to_string())));
            }
            let started = ready_sender.clone();
            let result = thread_ctrl.run_target(
                move |dev| {
                    set_params(dev, size, chunk_size);
                    Ok(())
                },
                move |queue, dev| serve_queue(queue, dev, &disk, &runtime),
                move |ctrl| drop(started.send(Ok(ctrl.get_bdev_path()))),
            );
            if let Err(error) = result {
                let _ = ready_sender.send(Err(error.to_string()));
            }
            let _ = thread_ctrl.del_dev();
        })?;
        let path = ready
            .recv_timeout(Duration::from_secs(30))
            .context("the ublk device did not start")?
            .map_err(|error| anyhow!("the ublk device failed: {error}"))?;
        Ok(Self { ctrl, id, path, thread: Some(thread) })
    }
}

/// Deletes a paused device that no mica will serve, so that its users get
/// errors instead of a wait for ever.
pub fn remove_paused(id: i32) -> Result<()> {
    control_ring()?;
    UblkCtrl::new_simple(id)?.del_dev()?;
    Ok(())
}

pub fn exists(id: i32) -> bool {
    std::path::Path::new(&format!("/sys/class/ublk-char/ublkc{id}")).exists()
}

/// libublk keeps its control ring for each thread. Each thread that sends
/// control commands needs its own ring.
fn control_ring() -> Result<()> {
    libublk::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            *ring = Some(IoUring::<squeue::Entry128>::builder().build(32).map_err(UblkError::IOError)?);
        }
        Ok(())
    })?;
    Ok(())
}

fn set_params(dev: &mut UblkDev, size: u64, chunk_size: u32) {
    // Room in the ring for the ublk commands and the file I/O of the queue
    // thread: one operation for each chunk that a request touches, and the eventfd.
    let depth = (QUEUE_DEPTH as u32 * (MAX_REQUEST.div_ceil(chunk_size) + 3)).min(4096) as u16;
    dev.set_default_params(size);
    dev.tgt.sq_depth = depth;
    dev.tgt.cq_depth = depth;
    let params = &mut dev.tgt.params;
    params.basic.attrs = sys::UBLK_ATTR_VOLATILE_CACHE | sys::UBLK_ATTR_FUA;
    params.types |= sys::UBLK_PARAM_TYPE_DISCARD;
    params.discard = sys::ublk_param_discard {
        discard_granularity: chunk_size,
        max_discard_sectors: u32::MAX >> 9,
        max_write_zeroes_sectors: u32::MAX >> 9,
        max_discard_segments: 1,
        ..Default::default()
    };
}

/// `/dev/mica/<disk>` keeps its name, while the number of `ublkbN` can change.
pub fn link(disk: &str, device: &str) -> Result<String> {
    let link = format!("/dev/mica/{disk}");
    std::fs::create_dir_all("/dev/mica")?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(device, &link).with_context(|| format!("create {link}"))?;
    Ok(link)
}

pub fn unlink(disk: &str) {
    let _ = std::fs::remove_file(format!("/dev/mica/{disk}"));
}

/// True while a program has the device open exclusively, as a mount does.
pub fn in_use(device: &str) -> bool {
    let path = std::ffi::CString::new(device).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_EXCL | libc::O_CLOEXEC) };
    if fd < 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY);
    }
    unsafe { libc::close(fd) };
    false
}
