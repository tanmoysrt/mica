use crate::bucket::{Bucket, keys};
use crate::node::NodeIdentity;
use crate::records::AttachedMarker;
use crate::disk::Disk;
use anyhow::{Result, bail};
use std::sync::Arc;
use std::time::Duration;

const CLAIM_SETTLE: Duration = Duration::from_secs(1);
const WATCH_INTERVAL: Duration = Duration::from_secs(60);

pub enum Owner {
    Nobody,
    ThisNode,
    OtherNode(AttachedMarker),
}

pub async fn owner_of(bucket: &Bucket, disk_id: &str, node: &NodeIdentity) -> Result<Owner> {
    Ok(match AttachedMarker::load(bucket, disk_id).await? {
        None => Owner::Nobody,
        Some(marker) if marker.node_id == node.id => Owner::ThisNode,
        Some(marker) => Owner::OtherNode(marker),
    })
}

/// Writes our marker. Without `force`, it fails when another node owns the disk.
pub async fn claim(bucket: &Bucket, disk_id: &str, node: &NodeIdentity, force: bool) -> Result<()> {
    match owner_of(bucket, disk_id, node).await? {
        Owner::ThisNode => return Ok(()),
        Owner::OtherNode(marker) if !force => bail!(
            "disk {disk_id} is attached on {} (node id {}) since {}. Use --force only when that node is down",
            marker.node,
            marker.node_id,
            marker.since
        ),
        _ => {}
    }
    let ours = AttachedMarker::for_node(node);
    ours.save(bucket, disk_id).await?;
    // Without conditional writes two claims can land at the same moment,
    // and the last write wins. Each claimer waits, then reads back; the
    // one that finds another claim ID backs off.
    tokio::time::sleep(CLAIM_SETTLE).await;
    match AttachedMarker::load(bucket, disk_id).await? {
        Some(marker) if marker.claim_id == ours.claim_id => Ok(()),
        Some(marker) => bail!("disk {disk_id} was attached on {} at the same moment. Try again later", marker.node),
        None => bail!("the attached marker of disk {disk_id} vanished during the attach. Try again"),
    }
}

/// Checks the marker once a minute, also for an idle disk, which makes no
/// checkpoints. A node that lost its disk stops serving it soon.
pub async fn watch(disk: Arc<Disk>) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        if disk.is_closed() || disk.ensure_usable().is_err() {
            return;
        }
        match owner_of(&disk.bucket, &disk.id, &disk.node).await {
            Ok(Owner::ThisNode) => {}
            Ok(Owner::Nobody) => drop(disk.lose_ownership("the attached marker is gone")),
            Ok(Owner::OtherNode(marker)) => drop(disk.lose_ownership(&format!("it is attached on {} now", marker.node))),
            Err(error) => log::warn!("disk {}: cannot check the attached marker: {error:#}", disk.id),
        }
    }
}

/// Deletes the marker, but only if it is ours. Read and delete are two
/// requests; a `--force` claim between them would lose its marker. That is
/// one more reason to use `--force` only when the old node is down.
pub async fn release(bucket: &Bucket, disk_id: &str, node: &NodeIdentity) -> Result<()> {
    if let Owner::ThisNode = owner_of(bucket, disk_id, node).await? {
        bucket.delete(&keys::attached(disk_id)).await?;
    }
    Ok(())
}
