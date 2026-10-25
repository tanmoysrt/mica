//! Deletes one ublk device, as a reboot would: the crash test uses it so
//! that a restarted daemon cannot take the paused device over.
use io_uring::{IoUring, squeue};
use libublk::UblkError;
use libublk::ctrl::UblkCtrl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let id: i32 = std::env::args().nth(1).ok_or("usage: ublk-del <id>")?.parse()?;
    libublk::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            *ring = Some(IoUring::<squeue::Entry128>::builder().build(32).map_err(UblkError::IOError)?);
        }
        Ok(())
    })?;
    UblkCtrl::new_simple(id)?.del_dev()?;
    println!("deleted ublk device {id}");
    Ok(())
}
