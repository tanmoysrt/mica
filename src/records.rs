use crate::bucket::{Bucket, keys};
use crate::content_hash::ContentHash;
use crate::node::NodeIdentity;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// `disks/<disk-id>/head`: the current manifest of a disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    #[serde(default)]
    pub limits: crate::disk_limits::DiskLimits,
    pub manifest: String,
    pub profile: Vec<u32>,
    pub seq: u64,
    pub node: String,
    pub time: String,
}

/// `disks/<disk-id>/attached`: the node that owns the disk now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachedMarker {
    pub node: String,
    pub node_id: String,
    pub since: String,
    /// Random per attach. Two nodes that claim at the same moment both write
    /// a marker; the one that reads back another claim ID backs off.
    #[serde(default)]
    pub claim_id: String,
}

/// `snapshots/<name>`: a named manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub manifest: String,
    pub profile: Vec<u32>,
    /// The disk it was taken from, and when. Older snapshots do not have them.
    #[serde(default)]
    pub disk: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
}

impl Head {
    pub fn new(manifest: &ContentHash, profile: Vec<u32>, seq: u64, node: &str) -> Self {
        Self { limits: Default::default(), manifest: manifest.to_reference(), profile, seq, node: node.to_string(), time: now_text() }
    }

    pub fn with_limits(mut self, limits: crate::disk_limits::DiskLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn manifest_hash(&self) -> Result<ContentHash> {
        ContentHash::from_reference(&self.manifest)
    }

    pub async fn load(bucket: &Bucket, disk_id: &str) -> Result<Option<Self>> {
        load_json(bucket, &keys::head(disk_id)).await
    }

    pub async fn create(&self, bucket: &Bucket, disk_id: &str) -> Result<()> {
        if bucket.conditional_writes() {
            anyhow::ensure!(bucket.put_if_absent(&keys::head(disk_id), serde_json::to_vec_pretty(self)?.into()).await?,
                "disk {disk_id} already exists");
            Ok(())
        } else {
            self.save(bucket, disk_id).await
        }
    }

    pub async fn save(&self, bucket: &Bucket, disk_id: &str) -> Result<()> {
        save_json(bucket, &keys::head(disk_id), self).await
    }
}

impl AttachedMarker {
    pub fn for_node(node: &NodeIdentity) -> Self {
        let claim_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().to_string();
        Self { node: node.hostname.clone(), node_id: node.id.clone(), since: now_text(), claim_id }
    }

    pub async fn load(bucket: &Bucket, disk_id: &str) -> Result<Option<Self>> {
        load_json(bucket, &keys::attached(disk_id)).await
    }

    pub async fn save(&self, bucket: &Bucket, disk_id: &str) -> Result<()> {
        save_json(bucket, &keys::attached(disk_id), self).await
    }

    /// Writes the marker only if the disk has none. False when it has one.
    pub async fn create(&self, bucket: &Bucket, disk_id: &str) -> Result<bool> {
        bucket.put_if_absent(&keys::attached(disk_id), serde_json::to_vec_pretty(self)?.into()).await
    }
}

impl Snapshot {
    pub async fn load(bucket: &Bucket, name: &str) -> Result<Option<Self>> {
        load_json(bucket, &keys::snapshot(name)).await
    }

    pub async fn save(&self, bucket: &Bucket, name: &str) -> Result<()> {
        save_json(bucket, &keys::snapshot(name), self).await
    }
}

pub fn now_text() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

async fn load_json<T: DeserializeOwned>(bucket: &Bucket, key: &str) -> Result<Option<T>> {
    match bucket.get(key).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data).with_context(|| format!("parse {key}"))?)),
        None => Ok(None),
    }
}

async fn save_json<T: Serialize>(bucket: &Bucket, key: &str, value: &T) -> Result<()> {
    bucket.put(key, serde_json::to_vec_pretty(value)?.into()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creating_the_same_disk_twice_does_not_overwrite_its_head() {
        let bucket = Bucket::in_memory();
        let first = Head::new(&ContentHash::of(b"first"), vec![], 0, "one");
        let second = Head::new(&ContentHash::of(b"second"), vec![], 0, "two");
        let (a, b) = tokio::join!(first.create(&bucket, "disk"), second.create(&bucket, "disk"));
        assert_ne!(a.is_ok(), b.is_ok());
        let head = Head::load(&bucket, "disk").await.unwrap().unwrap();
        assert_eq!(head.manifest, if a.is_ok() { first.manifest } else { second.manifest });
    }

    #[test]
    fn old_heads_have_default_limits() {
        let head: Head = serde_json::from_str(r#"{"manifest":"sha256:unused","profile":[],"seq":0,"node":"old","time":"now"}"#).unwrap();
        assert_eq!(head.limits.cache_bytes, None);
        assert_eq!(head.limits.dirty_bytes, None);
    }
}
