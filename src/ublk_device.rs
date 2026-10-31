use crate::disk::Disk;
use crate::ublk_queue::serve_queue;
use anyhow::{Context, Result, anyhow, bail};
use io_uring::{IoUring, squeue};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::UblkDev;
use libublk::{UblkError, UblkFlags, sys};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::Handle;

const QUEUE_DEPTH: u16 = 64;
/// One queue thread per CPU, up to this many. Parallel I/O then uses more
/// than one CPU. A device taken over after a restart keeps its own count.
const MAX_QUEUES: u16 = 4;
/// The largest request. The daemon has no buffer of this size: it uses the
/// request's own pages (see `queue_io::RequestBuffer`).
const IO_BUFFER_BYTES: u32 = 512 << 10;
/// Zero copy needs Linux 6.16 or later.
const ZERO_COPY: u64 = (sys::UBLK_F_AUTO_BUF_REG | sys::UBLK_F_USER_COPY) as u64;

/// Room in each queue's ring for the ublk commands and the file I/O that
/// the queue thread does itself: one read or write per chunk part (a request
/// can span this many chunks), and the eventfd read.
fn ring_depth(chunk_size: u32) -> u16 {
    let parts = IO_BUFFER_BYTES.div_ceil(chunk_size) + 1;
    (QUEUE_DEPTH as u32 * (parts + 2)).min(4096) as u16
}

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

    pub fn restore_size(&self, size: u64) -> Result<()> {
        ensure_control_ring()?;
        let mut params = sys::ublk_params::default();
        self.ctrl.get_params(&mut params)?;
        if params.basic.dev_sectors << 9 != size {
            let result = self.resize_control()?.update_size(size)?;
            anyhow::ensure!(result >= 0, "restore ublk size: errno {}", -result);
        }
        Ok(())
    }

    pub fn resize_control(&self) -> Result<Arc<UblkCtrl>> {
        anyhow::ensure!(self.ctrl.get_driver_features().is_some_and(|flags| flags & sys::UBLK_F_UPDATE_SIZE as u64 != 0),
            "this kernel does not support online ublk resize; detach the disk first");
        Ok(self.ctrl.clone())
    }

    fn launch(disk: Arc<Disk>, runtime: Handle, recover_id: Option<i32>) -> Result<Self> {
        ensure_control_ring()?;
        let builder = UblkCtrlBuilder::default()
            .name("mica")
            .nr_queues(queue_count())
            .depth(QUEUE_DEPTH)
            .io_buf_bytes(IO_BUFFER_BYTES)
            .ctrl_flags(ZERO_COPY | sys::UBLK_F_USER_RECOVERY as u64 | sys::UBLK_F_USER_RECOVERY_REISSUE as u64);
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
        let (size, chunk_size) = (disk.size(), disk.chunk_size as u32);
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
pub(crate) fn ensure_control_ring() -> Result<()> {
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
#[allow(clippy::cast_possible_truncation)]
fn set_device_params(dev: &mut UblkDev, size: u64, chunk_size: u32) {
    dev.set_default_params(size);
    dev.tgt.sq_depth = ring_depth(chunk_size);
    dev.tgt.cq_depth = ring_depth(chunk_size);
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

fn queue_count() -> u16 {
    let cpus = std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1);
    (cpus as u16).clamp(1, MAX_QUEUES)
}
