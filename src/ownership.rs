use crate::bucket::{Bucket, keys};
use crate::node::NodeIdentity;
use crate::records::AttachedMarker;
use anyhow::{Result, bail};

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
    AttachedMarker::for_node(node).save(bucket, disk_id).await
}

/// Deletes the marker, but only if it is ours.
pub async fn release(bucket: &Bucket, disk_id: &str, node: &NodeIdentity) -> Result<()> {
    if let Owner::ThisNode = owner_of(bucket, disk_id, node).await? {
        bucket.delete(&keys::attached(disk_id)).await?;
    }
    Ok(())
}
