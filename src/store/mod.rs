//! The object store side of mica.
//!
//! A disk lives in an S3 bucket as immutable objects and one head. Each
//! disk owns its objects. No object is shared between two disks, so each
//! disk can clean up after itself, and no job ever reads the whole bucket.

pub mod bucket;
pub mod cleanup;
pub mod disk_store;
pub mod format;
pub mod head;

pub use bucket::{Bucket, StoreConfig};
pub use disk_store::{DiskStore, HeadConflict, Versioned, validate_name};
pub use format::{Entry, Hash, Page, Root};
pub use head::{Head, Limits, Owner, Snapshot};
