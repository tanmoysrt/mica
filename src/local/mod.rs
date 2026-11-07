//! The node side of mica: the files on the local disk of the node.
//!
//! Each attached disk has a folder with its data files and its dirty log.
//! The node has one chunk cache, with a folder and a budget for each disk.

pub mod cache;
pub mod data_files;
pub mod dirty_log;
pub mod folder;

pub use cache::{ChunkCache, NodeCache};
pub use data_files::DataFiles;
pub use dirty_log::DirtyLog;
pub use folder::{DiskFolder, LocalState};
