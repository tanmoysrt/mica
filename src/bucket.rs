use crate::config::S3Config;
use crate::content_hash::ContentHash;
use anyhow::{Context, Result};
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// One object from a listing. `age_secs` is the time since its last write.
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub age_secs: u64,
}

/// The S3 bucket. mica only needs get, put, delete and exists.
/// No conditional writes, so it works the same on R2, Ceph and Garage.
pub struct Bucket {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    requests: RequestCounters,
}

/// S3 requests since the daemon started. Providers bill per request.
#[derive(Default)]
struct RequestCounters {
    get: AtomicU64,
    put: AtomicU64,
    head: AtomicU64,
    list: AtomicU64,
    delete: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

#[derive(Debug, Serialize)]
pub struct RequestCounts {
    pub get: u64,
    pub put: u64,
    pub head: u64,
    pub list: u64,
    pub delete: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl Bucket {
    pub fn connect(config: &S3Config) -> Result<Self> {
        config.validate()?;
        let store = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_bucket_name(&config.bucket)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_region(&config.region)
            .with_allow_http(config.endpoint.starts_with("http://"))
            .build()
            .context("create S3 client")?;
        Ok(Self {
            store: Arc::new(store),
            prefix: config.prefix.trim_matches('/').to_string(),
            requests: RequestCounters::default(),
        })
    }

    pub fn request_counts(&self) -> RequestCounts {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let requests = &self.requests;
        RequestCounts {
            get: read(&requests.get),
            put: read(&requests.put),
            head: read(&requests.head),
            list: read(&requests.list),
            delete: read(&requests.delete),
            bytes_in: read(&requests.bytes_in),
            bytes_out: read(&requests.bytes_out),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        count(&self.requests.get, 1);
        match self.store.get(&self.path(key)).await {
            Ok(result) => {
                let data = result.bytes().await.with_context(|| format!("read {key}"))?;
                count(&self.requests.bytes_in, data.len() as u64);
                Ok(Some(data))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("get {key}")),
        }
    }

    pub async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        count(&self.requests.put, 1);
        count(&self.requests.bytes_out, data.len() as u64);
        self.store
            .put(&self.path(key), PutPayload::from(data))
            .await
            .with_context(|| format!("put {key}"))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        count(&self.requests.delete, 1);
        match self.store.delete(&self.path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("delete {key}")),
        }
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        count(&self.requests.head, 1);
        match self.store.head(&self.path(key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error).with_context(|| format!("head {key}")),
        }
    }

    /// Names of the folders directly under `prefix`, such as the disk IDs under `disks`.
    pub async fn list_folders(&self, prefix: &str) -> Result<Vec<String>> {
        count(&self.requests.list, 1);
        let listing = self
            .store
            .list_with_delimiter(Some(&self.path(prefix)))
            .await
            .with_context(|| format!("list {prefix}"))?;
        Ok(listing.common_prefixes.iter().filter_map(|folder| folder.filename().map(String::from)).collect())
    }

    /// Names of the objects directly under `prefix`, such as the snapshot names.
    pub async fn list_objects(&self, prefix: &str) -> Result<Vec<String>> {
        count(&self.requests.list, 1);
        let listing = self
            .store
            .list_with_delimiter(Some(&self.path(prefix)))
            .await
            .with_context(|| format!("list {prefix}"))?;
        Ok(listing.objects.iter().filter_map(|object| object.location.filename().map(String::from)).collect())
    }

    /// Every object below `prefix`, at any depth.
    pub async fn list_all(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        use futures::TryStreamExt;
        let objects: Vec<_> = self
            .store
            .list(Some(&self.path(prefix)))
            .try_collect()
            .await
            .with_context(|| format!("list {prefix}"))?;
        // A listing returns at most 1000 keys per request.
        count(&self.requests.list, (objects.len() as u64).div_ceil(1000).max(1));
        Ok(objects
            .into_iter()
            .map(|object| ObjectInfo {
                key: self.key_of(&object.location),
                size: object.size,
                age_secs: age_secs(object.last_modified.timestamp()),
            })
            .collect())
    }

    /// Size and age of one object, or `None` when it does not exist.
    pub async fn info(&self, key: &str) -> Result<Option<ObjectInfo>> {
        count(&self.requests.head, 1);
        match self.store.head(&self.path(key)).await {
            Ok(meta) => Ok(Some(ObjectInfo {
                key: key.to_string(),
                size: meta.size,
                age_secs: age_secs(meta.last_modified.timestamp()),
            })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("head {key}")),
        }
    }

    pub async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Bytes> {
        count(&self.requests.get, 1);
        self.store.get_range(&self.path(key), range).await.with_context(|| format!("get part of {key}"))
    }

    /// The key without the bucket prefix.
    fn key_of(&self, path: &Path) -> String {
        let full = path.as_ref();
        match self.prefix.is_empty() {
            true => full.to_string(),
            false => full.strip_prefix(&format!("{}/", self.prefix)).unwrap_or(full).to_string(),
        }
    }

    fn path(&self, key: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(key)
        } else {
            Path::from(format!("{}/{key}", self.prefix))
        }
    }
}

fn count(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

fn age_secs(unix_time: i64) -> u64 {
    (crate::manifest::unix_now() as i64 - unix_time).max(0) as u64
}

/// Object keys. See "S3 layout" in plan.md.
pub mod keys {
    use super::ContentHash;

    pub fn chunk(hash: &ContentHash) -> String {
        let hex = hash.to_hex();
        format!("chunks/{}/{hex}", &hex[..2])
    }

    pub fn manifest(hash: &ContentHash) -> String {
        format!("manifests/{}", hash.to_hex())
    }

    pub fn head(disk_id: &str) -> String {
        format!("disks/{disk_id}/head")
    }

    pub fn attached(disk_id: &str) -> String {
        format!("disks/{disk_id}/attached")
    }

    pub fn snapshot(name: &str) -> String {
        format!("snapshots/{name}")
    }

    pub fn gc_lock() -> String {
        "gc/lock".to_string()
    }
}
