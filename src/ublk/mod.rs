//! The kernel interface: a ublk block device for each attached disk.
//!
//! ublk only turns the block requests of the kernel into calls to the disk
//! engine. See `device.rs` for the device, `queue.rs` for the threads that
//! serve the requests, and `request.rs` for the zero copy of the data.

pub mod device;
mod queue;
mod request;

pub use device::UblkDevice;
