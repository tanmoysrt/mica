//! The S3 bucket of a disk, and the credentials to reach it.
//!
//! Each disk names its own bucket: the user passes the credentials with
//! each command, and the daemon keeps them only for the disks it serves.

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a disk lives. Loaded from a TOML file or the environment.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StoreConfig {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

fn default_region() -> String {
    "auto".into()
}

impl StoreConfig {
    /// Reads `path`, or without a path the variables `S3_ENDPOINT`,
    /// `S3_BUCKET`, `S3_PREFIX`, `S3_REGION`, `S3_ACCESS_KEY_ID` and
    /// `S3_SECRET_ACCESS_KEY`.
    pub fn load(path: Option<&std::path::Path>) -> Result<Self> {
        let config: Self = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            None => {
                let var = |name: &str| std::env::var(name).unwrap_or_default();
                Self {
                    endpoint: var("S3_ENDPOINT"),
                    bucket: var("S3_BUCKET"),
                    prefix: var("S3_PREFIX"),
                    region: Some(var("S3_REGION")).filter(|region| !region.is_empty()).unwrap_or_else(default_region),
                    access_key_id: var("S3_ACCESS_KEY_ID"),
                    secret_access_key: var("S3_SECRET_ACCESS_KEY"),
                }
            }
        };
        ensure!(
            !config.endpoint.is_empty() && !config.bucket.is_empty(),
            "no bucket: pass --store <file> or set S3_ENDPOINT and S3_BUCKET"
        );
        Ok(config)
    }

    /// Two configs with the same place reach the same disks.
    pub fn place(&self) -> String {
        format!("{}/{}/{}", self.endpoint.trim_end_matches('/'), self.bucket, self.prefix.trim_matches('/'))
    }
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StoreConfig({})", self.place())
    }
}

/// The result of a conditional write.
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    /// The new version tag of the object.
    Done(String),
    /// Another writer changed the object first. Nothing was written.
    Conflict,
}

/// A bucket and a key prefix. Counts requests and bytes for `mica status`.
pub struct Bucket {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    pub counts: Counts,
}

#[derive(Default, Serialize, Debug)]
pub struct Counts {
    pub get: AtomicU64,
    pub put: AtomicU64,
    pub list: AtomicU64,
    pub delete: AtomicU64,
    pub copy: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

/// S3 deletes at most this many keys in one request.
const DELETE_BATCH: usize = 1000;

impl Bucket {
    pub fn connect(config: &StoreConfig) -> Result<Self> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_allow_http(config.endpoint.starts_with("http://"))
            .build()
            .context("create S3 client")?;
        Ok(Self::over(Arc::new(store), &config.prefix))
    }

    /// A bucket in memory, for tests.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::over(Arc::new(object_store::memory::InMemory::new()), "test")
    }

    fn over(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self { store, prefix: prefix.trim_matches('/').to_string(), counts: Counts::default() }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        Ok(self.get_tagged(key).await?.map(|(data, _)| data))
    }

    /// The object and its version tag (ETag), or `None` when it is missing.
    pub async fn get_tagged(&self, key: &str) -> Result<Option<(Bytes, String)>> {
        add(&self.counts.get, 1);
        let result = match self.store.get(&self.path(key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("get {key}")),
        };
        let tag = result.meta.e_tag.clone().unwrap_or_default();
        let data = result.bytes().await.with_context(|| format!("read {key}"))?;
        add(&self.counts.bytes_in, data.len() as u64);
        Ok(Some((data, tag)))
    }

    /// mica locks a disk with conditional writes. A store that ignores the
    /// conditions would let two nodes write one disk, so it is refused.
    pub async fn check_conditional_writes(&self) -> Result<()> {
        let key = format!("probe-{}", crate::store::head::random_id());
        let result = async {
            let Written::Done(tag) = self.put_new(&key, "1").await? else { bail!("the probe exists already") };
            ensure!(matches!(self.put_new(&key, "2").await?, Written::Conflict), "the store ignores If-None-Match");
            ensure!(matches!(self.put_if_match(&key, "3", "\"wrong\"").await?, Written::Conflict), "the store ignores If-Match");
            ensure!(matches!(self.put_if_match(&key, "4", &tag).await?, Written::Done(_)), "the store refuses a correct If-Match");
            Ok(())
        }
        .await;
        let _ = self.delete_all(vec![key]).await;
        result.context("this store cannot hold mica disks: it needs conditional writes")
    }

    pub async fn put(&self, key: &str, data: impl Into<Bytes>) -> Result<()> {
        self.put_with(key, data.into(), PutMode::Overwrite).await.map(|_| ())
    }

    /// Writes only if no object has the key (`If-None-Match: *`).
    pub async fn put_new(&self, key: &str, data: impl Into<Bytes>) -> Result<Written> {
        self.put_with(key, data.into(), PutMode::Create).await
    }

    /// Writes only if the object still has version `tag` (`If-Match`).
    pub async fn put_if_match(&self, key: &str, data: impl Into<Bytes>, tag: &str) -> Result<Written> {
        let version = UpdateVersion { e_tag: Some(tag.to_string()), version: None };
        self.put_with(key, data.into(), PutMode::Update(version)).await
    }

    async fn put_with(&self, key: &str, data: Bytes, mode: PutMode) -> Result<Written> {
        add(&self.counts.put, 1);
        add(&self.counts.bytes_out, data.len() as u64);
        let options = PutOptions { mode, ..PutOptions::default() };
        match self.store.put_opts(&self.path(key), PutPayload::from(data), options).await {
            Ok(result) => Ok(Written::Done(result.e_tag.unwrap_or_default())),
            Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) => {
                Ok(Written::Conflict)
            }
            Err(error) => Err(error).with_context(|| format!("put {key}")),
        }
    }

    /// Copies inside the store: the data does not pass through this node.
    pub async fn copy(&self, from: &str, to: &str) -> Result<()> {
        add(&self.counts.copy, 1);
        self.store.copy(&self.path(from), &self.path(to)).await.with_context(|| format!("copy {from} to {to}"))
    }

    /// Deletes keys, 1,000 for each request. Missing keys are no error.
    pub async fn delete_all(&self, keys: Vec<String>) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        add(&self.counts.delete, keys.len().div_ceil(DELETE_BATCH) as u64);
        let paths: Vec<_> = keys.iter().map(|key| Ok(self.path(key))).collect();
        let results: Vec<_> = self.store.delete_stream(futures::stream::iter(paths).boxed()).collect().await;
        for result in results {
            match result {
                Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(error).context("delete objects"),
            }
        }
        Ok(())
    }

    /// Every key below `prefix`, at any depth, without the bucket prefix.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let objects: Vec<_> =
            self.store.list(Some(&self.path(prefix))).try_collect().await.with_context(|| format!("list {prefix}"))?;
        add(&self.counts.list, (objects.len() as u64).div_ceil(1000).max(1));
        Ok(objects.into_iter().map(|object| self.key_of(&object.location)).collect())
    }

    /// The names of the folders directly below `prefix`.
    pub async fn list_folders(&self, prefix: &str) -> Result<Vec<String>> {
        add(&self.counts.list, 1);
        let listing = self.store.list_with_delimiter(Some(&self.path(prefix))).await.with_context(|| format!("list {prefix}"))?;
        Ok(listing.common_prefixes.iter().filter_map(|folder| folder.filename().map(String::from)).collect())
    }

    /// True when any object is below `prefix`.
    pub async fn has_any(&self, prefix: &str) -> Result<bool> {
        add(&self.counts.list, 1);
        let first = self.store.list(Some(&self.path(prefix))).next().await;
        Ok(first.transpose().with_context(|| format!("list {prefix}"))?.is_some())
    }

    fn path(&self, key: &str) -> Path {
        match self.prefix.is_empty() {
            true => Path::from(key),
            false => Path::from(format!("{}/{key}", self.prefix)),
        }
    }

    fn key_of(&self, path: &Path) -> String {
        let full = path.as_ref();
        match self.prefix.is_empty() {
            true => full.to_string(),
            false => full.strip_prefix(&format!("{}/", self.prefix)).unwrap_or(full).to_string(),
        }
    }
}

fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}
