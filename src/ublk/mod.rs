//! The kernel interface: a ublk block device for each attached disk.
//!
//! ublk only turns the block requests of the kernel into calls to the disk
//! engine. See `device.rs` for the device, `queue.rs` for the threads that
//! serve the requests, `request.rs` for the zero copy of the data, and
//! `names.rs` for the `/dev/mica/<disk>` names.

pub mod device;
pub mod names;
mod queue;
mod request;

pub use device::UblkDevice;
