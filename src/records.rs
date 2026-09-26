use crate::bucket::{Bucket, keys};
use crate::content_hash::ContentHash;
use crate::node::NodeIdentity;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// `disks/<disk-id>/head`: the current manifest of a disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
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
        Self { manifest: manifest.to_reference(), profile, seq, node: node.to_string(), time: now_text() }
    }

    pub fn manifest_hash(&self) -> Result<ContentHash> {
        ContentHash::from_reference(&self.manifest)
    }

    pub async fn load(bucket: &Bucket, disk_id: &str) -> Result<Option<Self>> {
        load_json(bucket, &keys::head(disk_id)).await
    }

    pub async fn save(&self, bucket: &Bucket, disk_id: &str) -> Result<()> {
        save_json(bucket, &keys::head(disk_id), self).await
    }
}

impl AttachedMarker {
    pub fn for_node(node: &NodeIdentity) -> Self {
        Self { node: node.hostname.clone(), node_id: node.id.clone(), since: now_text() }
    }

    pub async fn load(bucket: &Bucket, disk_id: &str) -> Result<Option<Self>> {
        load_json(bucket, &keys::attached(disk_id)).await
    }

    pub async fn save(&self, bucket: &Bucket, disk_id: &str) -> Result<()> {
        save_json(bucket, &keys::attached(disk_id), self).await
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
