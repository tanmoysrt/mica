//! Ownership of a disk: the claim.
//!
//! A disk has at most one owner, the node that has it attached. Only the
//! owner changes the head and deletes objects of the disk. To change a
//! detached disk, a node claims it, makes the change, and releases it.
//!
//! The head is the lock. Each write of the head is conditional: it happens
//! only if nobody changed the head since this node read it. So when a write
//! fails with a conflict, another node took the disk, and this node stops.
//!
//! A claim also moves `next` on. The new owner thus never writes into the
//! folder of an old owner. If the old folder holds objects, a transaction
//! did not finish, and the folder goes into `lost`.

use crate::store::{DiskStore, Head, HeadConflict, Owner, Versioned, cleanup};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// This node, as it appears in the head of the disks it owns.
#[derive(Clone, Debug)]
pub struct NodeId {
    pub hostname: String,
    pub id: String,
}

impl NodeId {
    /// The identifier is random, and stays in the data folder for the life of the node.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("node-id");
        let id = match std::fs::read_to_string(&path) {
            Ok(id) => id.trim().to_string(),
            Err(_) => {
                let id = crate::store::head::random_id();
                std::fs::create_dir_all(data_dir)?;
                std::fs::write(&path, &id).with_context(|| format!("write {}", path.display()))?;
                id
            }
        };
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
        Ok(Self { hostname, id })
    }

    fn owner(&self) -> Owner {
        Owner { node: self.hostname.clone(), id: self.id.clone(), since: unix_now() }
    }
}

/// This node owns the disk while it holds a claim.
pub struct Claim {
    pub store: DiskStore,
    current: Versioned,
}

impl Claim {
    /// Takes a disk that no other node holds.
    pub async fn take(store: DiskStore, node: &NodeId) -> Result<Self> {
        let current = store.require_head().await?;
        let head = &current.head;
        if head.deleted {
            bail!("disk {} is being deleted: run `mica disk delete` to finish", store.disk);
        }
        if let Some(owner) = head.owner.as_ref().filter(|owner| owner.id != node.id) {
            bail!("disk {} is attached on {}", store.disk, owner.node);
        }
        let mut new = head.clone();
        // A crash before a head write leaves objects in the folder `next`.
        if store.folder_has_objects(head.next).await? {
            new.lost.push(head.next);
        }
        new.next += 1;
        new.owner = Some(node.owner());
        let current = store.replace_head(&current, new).await.context("claim")?;
        Ok(Self { store, current })
    }

    /// Makes a new disk and claims it, in one write.
    pub async fn create(store: DiskStore, node: &NodeId, mut head: Head) -> Result<Self> {
        head.owner = Some(node.owner());
        let current = store.create_head(&head).await?;
        Ok(Self { store, current })
    }

    pub fn head(&self) -> &Head {
        &self.current.head
    }

    /// Writes a changed head. A conflict means that this node lost the disk.
    ///
    /// After a network error the write may still have happened. So mica
    /// reads the head again: if it is the head that we wrote, the write worked.
    pub async fn write(&mut self, head: Head) -> Result<()> {
        let error = match self.store.replace_head(&self.current, head.clone()).await {
            Ok(current) => {
                self.current = current;
                return Ok(());
            }
            Err(error) if error.is::<HeadConflict>() => return Err(error),
            Err(error) => error,
        };
        match self.store.head().await {
            Ok(Some(current)) if current.head == head => {
                self.current = current;
                Ok(())
            }
            Ok(Some(current)) if !current.head.owned_by(&self.me()) => Err(HeadConflict.into()),
            _ => Err(error),
        }
    }

    pub async fn update(&mut self, change: impl FnOnce(&mut Head)) -> Result<()> {
        let mut head = self.head().clone();
        change(&mut head);
        self.write(head).await
    }

    /// Reads the head, to find out whether another node took the disk.
    pub async fn still_owned(&self) -> Result<bool> {
        let current = self.store.require_head().await?;
        Ok(current.tag == self.current.tag || current.head.owned_by(&self.me()))
    }

    /// Finishes work that a crash stopped: lost folders, deletes after a
    /// transaction, and snapshot deletes. The lost folders stay in the head
    /// until the release, because an old owner can still upload into them.
    pub async fn repair(&mut self) -> Result<()> {
        cleanup::delete_lost(&self.store, &self.head().lost).await?;
        let finished = cleanup::finish_frees(&self.store, self.head()).await?;
        if finished > 0 {
            log::info!("disk {}: finished the deletes of {finished} transactions", self.store.disk);
        }
        let deleting: Vec<String> =
            self.head().snapshots.iter().filter(|(_, snapshot)| snapshot.deleting).map(|(name, _)| name.clone()).collect();
        for name in deleting {
            self.delete_snapshot(&name).await?;
        }
        Ok(())
    }

    /// Deletes a snapshot by the merge rule. A restore from it is no longer
    /// possible from the first write on.
    pub async fn delete_snapshot(&mut self, name: &str) -> Result<()> {
        let Some(snapshot) = self.head().snapshots.get(name).cloned() else {
            bail!("disk {} has no snapshot {name}", self.store.disk)
        };
        if !snapshot.deleting {
            self.update(|head| head.snapshots.get_mut(name).unwrap().deleting = true).await?;
        }
        cleanup::merge_snapshot(&self.store, self.head(), snapshot.transaction).await?;
        self.update(|head| {
            head.snapshots.remove(name);
        })
        .await
    }

    /// Gives the disk back. Late uploads of an old owner go now.
    pub async fn release(&mut self) -> Result<()> {
        cleanup::delete_lost(&self.store, &self.head().lost).await?;
        self.update(|head| {
            head.owner = None;
            head.lost.clear();
        })
        .await
    }

    fn me(&self) -> String {
        self.head().owner.as_ref().map(|owner| owner.id.clone()).unwrap_or_default()
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or(0)
}
