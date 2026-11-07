//! The head: the one object of a disk that mica writes more than once.
//!
//! It names the current version, the owner and the snapshots. Every write
//! of it is conditional, so the head is also the lock of the disk.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Head {
    /// The current version. 0 means a disk that was never written.
    pub transaction: u64,
    /// The folder of the next transaction. Every claim and every
    /// transaction moves it on, so no two writers share a folder.
    pub next: u64,
    #[serde(default)]
    pub owner: Option<Owner>,
    /// Folders of transactions that never finished. The owner deletes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost: Vec<u64>,
    #[serde(default)]
    pub snapshots: BTreeMap<String, Snapshot>,
    pub size: u64,
    pub chunk_size: u64,
    pub limits: Limits,
    /// The chunks that the guest read first in its last session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile: Vec<u32>,
    /// The disk is being deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// Unix time of the last transaction.
    #[serde(default)]
    pub time: u64,
}

/// The node that has the disk attached.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Owner {
    /// The host name, for people.
    pub node: String,
    /// A random identifier of the node, kept in its data folder.
    pub id: String,
    pub since: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub transaction: u64,
    pub time: u64,
    /// A delete of this snapshot started. It cannot be restored any more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleting: bool,
}

/// Budgets of the disk on a node. They follow the disk between nodes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    /// Clean chunks kept on the node.
    pub cache_bytes: u64,
    /// Data not in the store yet. Guest writes wait at this limit.
    pub unsaved_bytes: u64,
}

impl Head {
    pub fn new(size: u64, chunk_size: u64, limits: Limits, owner: Option<Owner>) -> Self {
        Self {
            transaction: 0,
            next: 1,
            owner,
            lost: Vec::new(),
            snapshots: BTreeMap::new(),
            size,
            chunk_size,
            limits,
            profile: Vec::new(),
            deleted: false,
            time: 0,
        }
    }

    pub fn owned_by(&self, id: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.id == id)
    }
}
