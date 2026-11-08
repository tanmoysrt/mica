//! The objects of one disk in its bucket:
//!
//! ```text
//! d/<disk>/head                             the head
//! d/<disk>/<transaction>/root               root of that version
//! d/<disk>/<transaction>/p/<page index>     a page
//! d/<disk>/<transaction>/c/<chunk index>    chunk data
//! ```
//!
//! The transaction folder is 16 hexadecimal digits, so folders sort by
//! number. An object's key follows from its entry: the birth names the
//! folder, the position names the file. mica never writes a key twice.

use super::bucket::{Bucket, Written};
use super::format::{self, Entry, Hash, Page, Root};
use super::head::Head;
use anyhow::{Context, Result, anyhow, bail};
use std::sync::Arc;

/// Another node changed the head first. The write did not happen.
#[derive(Debug)]
pub struct HeadConflict;

impl std::fmt::Display for HeadConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the head changed: another node took the disk")
    }
}

impl std::error::Error for HeadConflict {}

/// A head and the version tag that a conditional write needs.
#[derive(Clone, Debug)]
pub struct Versioned {
    pub head: Head,
    pub tag: String,
}

#[derive(Clone)]
pub struct DiskStore {
    pub bucket: Arc<Bucket>,
    pub disk: String,
}

impl DiskStore {
    pub fn new(bucket: Arc<Bucket>, disk: &str) -> Self {
        Self { bucket, disk: disk.to_string() }
    }

    pub fn prefix(&self) -> String {
        format!("d/{}/", self.disk)
    }

    pub fn folder(&self, transaction: u64) -> String {
        format!("d/{}/{transaction:016x}/", self.disk)
    }

    pub fn root_key(&self, transaction: u64) -> String {
        format!("{}root", self.folder(transaction))
    }

    pub fn page_key(&self, birth: u64, page: usize) -> String {
        format!("{}p/{page}", self.folder(birth))
    }

    pub fn chunk_key(&self, birth: u64, chunk: usize) -> String {
        format!("{}c/{chunk}", self.folder(birth))
    }

    fn head_key(&self) -> String {
        format!("d/{}/head", self.disk)
    }

    pub async fn head(&self) -> Result<Option<Versioned>> {
        let Some((data, tag)) = self.bucket.get_tagged(&self.head_key()).await? else { return Ok(None) };
        let head = serde_json::from_slice(&data).with_context(|| format!("parse the head of disk {}", self.disk))?;
        Ok(Some(Versioned { head, tag }))
    }

    pub async fn require_head(&self) -> Result<Versioned> {
        self.head().await?.ok_or_else(|| anyhow!("disk {} does not exist", self.disk))
    }

    /// Makes the head of a new disk. Fails if the disk exists. The read
    /// first also protects a disk on a store that ignores `If-None-Match`.
    pub async fn create_head(&self, head: &Head) -> Result<Versioned> {
        if self.head().await?.is_some() {
            bail!("disk {} exists already", self.disk);
        }
        match self.bucket.put_new(&self.head_key(), serde_json::to_vec(head)?).await? {
            Written::Done(tag) => Ok(Versioned { head: head.clone(), tag }),
            Written::Conflict => bail!("disk {} exists already", self.disk),
        }
    }

    /// Replaces the head if nobody changed it since `current` was read.
    /// The error is `HeadConflict` when somebody did. The read first finds
    /// most conflicts also on a store that ignores `If-Match`.
    pub async fn replace_head(&self, current: &Versioned, head: Head) -> Result<Versioned> {
        if self.head().await?.is_none_or(|now| now.tag != current.tag) {
            return Err(HeadConflict.into());
        }
        match self.bucket.put_if_match(&self.head_key(), serde_json::to_vec(&head)?, &current.tag).await? {
            Written::Done(tag) => Ok(Versioned { head, tag }),
            Written::Conflict => Err(HeadConflict.into()),
        }
    }

    pub async fn delete_head(&self) -> Result<()> {
        self.bucket.delete_all(vec![self.head_key()]).await
    }

    /// The root of a version. Version 0 is the empty disk.
    pub async fn root(&self, head: &Head, transaction: u64) -> Result<Root> {
        if transaction == 0 {
            return Ok(Root::empty(head.chunk_size, head.size));
        }
        let key = self.root_key(transaction);
        let data = self.bucket.get(&key).await?.ok_or_else(|| anyhow!("{key} is missing"))?;
        Root::decode(&data).with_context(|| format!("read {key}"))
    }

    /// The current version, grown to the size in the head.
    pub async fn current_root(&self, head: &Head) -> Result<Root> {
        let mut root = self.root(head, head.transaction).await?;
        root.grow(head.size);
        Ok(root)
    }

    pub async fn root_exists(&self, transaction: u64) -> Result<bool> {
        Ok(self.bucket.get(&self.root_key(transaction)).await?.is_some())
    }

    pub async fn put_root(&self, root: &Root) -> Result<()> {
        self.bucket.put(&self.root_key(root.transaction), root.encode()).await
    }

    pub async fn page(&self, entry: &Entry, page: usize) -> Result<Page> {
        if entry.is_zero() {
            return Ok(Page::zero());
        }
        let key = self.page_key(entry.birth, page);
        let packed = self.bucket.get(&key).await?.ok_or_else(|| anyhow!("{key} is missing"))?;
        Page::unpack(&packed, &entry.hash).with_context(|| format!("read {key}"))
    }

    /// Uploads a page into the folder of `transaction`. A zero page is not stored.
    pub async fn put_page(&self, transaction: u64, page: usize, content: &Page) -> Result<Entry> {
        if content.is_zero() {
            return Ok(Entry::ZERO);
        }
        let (hash, packed) = content.pack()?;
        self.bucket.put(&self.page_key(transaction, page), packed).await?;
        Ok(Entry { birth: transaction, hash })
    }

    /// Downloads a chunk and checks it against its entry.
    pub async fn chunk(&self, entry: &Entry, chunk: usize, chunk_size: u64) -> Result<Vec<u8>> {
        let key = self.chunk_key(entry.birth, chunk);
        let packed = self.bucket.get(&key).await?.ok_or_else(|| anyhow!("{key} is missing"))?;
        let hash = entry.hash;
        tokio::task::spawn_blocking(move || format::unpack_chunk(&packed, &hash, chunk_size))
            .await?
            .with_context(|| format!("read {key}"))
    }

    pub async fn put_chunk(&self, transaction: u64, chunk: usize, packed: Vec<u8>, hash: Hash) -> Result<Entry> {
        self.bucket.put(&self.chunk_key(transaction, chunk), packed).await?;
        Ok(Entry { birth: transaction, hash })
    }

    /// Deletes a whole transaction folder.
    pub async fn delete_folder(&self, transaction: u64) -> Result<()> {
        let keys = self.bucket.list(&self.folder(transaction)).await?;
        self.bucket.delete_all(keys).await
    }

    pub async fn folder_has_objects(&self, transaction: u64) -> Result<bool> {
        self.bucket.has_any(&self.folder(transaction)).await
    }
}

/// The names of all disks in a bucket: one listing.
pub async fn disk_names(bucket: &Bucket) -> Result<Vec<String>> {
    bucket.list_folders("d/").await
}

pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    anyhow::ensure!(valid, "bad name {name:?}: use letters, digits, '-', '_' and '.'");
    Ok(())
}
