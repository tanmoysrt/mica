use crate::bucket::{Bucket, keys};
use crate::codec;
use crate::content_hash::ContentHash;
use anyhow::{Result, anyhow, bail, ensure};
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The chunk size of a new disk when the caller does not choose one.
pub const DEFAULT_CHUNK_SIZE: u64 = 256 << 10;
/// Chunk sizes a disk can have. Each is a power of two.
pub const MIN_CHUNK_SIZE: u64 = 64 << 10;
pub const MAX_CHUNK_SIZE: u64 = 4 << 20;
/// Chunks in one metadata page. A page of 256 KiB chunks covers 256 MiB.
pub const CHUNKS_PER_PAGE: usize = 1024;

const MAGIC: &[u8; 4] = b"MICA";
const VERSION: u32 = 2;
const PAGE_MAGIC: &[u8; 4] = b"MPAG";
const PAGE_BYTES: usize = 8 + CHUNKS_PER_PAGE * 32;

/// The root of a disk at one commit: one hash for each metadata page.
/// `None` is a page of zero chunks, which is not stored.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub disk_id: String,
    pub chunk_size: u64,
    pub disk_size: u64,
    pub seq: u64,
    pub time: u64,
    pub parent: Option<ContentHash>,
    pub pages: Vec<Option<ContentHash>>,
}

/// The chunk hashes of one part of a disk: `CHUNKS_PER_PAGE` chunks, in order.
/// `None` is a zero chunk. Pages are immutable and content-addressed, so
/// checkpoints, snapshots and clones share every page they do not change.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub chunks: Vec<Option<ContentHash>>,
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<()> {
    ensure!(
        chunk_size.is_power_of_two() && (MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size),
        "chunk size must be a power of two from 64K to 4M, not {chunk_size}"
    );
    Ok(())
}

impl Manifest {
    pub fn empty(disk_id: &str, disk_size: u64, chunk_size: u64) -> Result<Self> {
        validate_chunk_size(chunk_size)?;
        let disk_size = round_up(disk_size, chunk_size);
        Ok(Self {
            disk_id: disk_id.to_string(),
            chunk_size,
            disk_size,
            seq: 0,
            time: unix_now(),
            parent: None,
            pages: vec![None; page_count((disk_size / chunk_size) as usize)],
        })
    }

    pub async fn load(bucket: &Bucket, hash: &ContentHash) -> Result<Self> {
        let data = bucket
            .get(&keys::manifest(hash))
            .await?
            .ok_or_else(|| anyhow!("manifest {hash} is missing in S3"))?;
        hash.verify(&data)?;
        Self::decode(data)
    }

    /// Writes the root to S3 and returns its hash. Its pages must be in S3 first.
    pub async fn save(&self, bucket: &Bucket) -> Result<ContentHash> {
        let data = self.encode();
        let hash = ContentHash::of(&data);
        bucket.put(&keys::manifest(&hash), data).await?;
        Ok(hash)
    }

    pub fn chunk_count(&self) -> usize {
        (self.disk_size / self.chunk_size) as usize
    }

    /// The next root: same pages, one seq higher. A clone shares the root of
    /// its snapshot, so the disk ID can change here.
    pub fn next(&self, disk_id: &str, parent: ContentHash) -> Self {
        Self {
            disk_id: disk_id.to_string(),
            seq: self.seq + 1,
            time: unix_now(),
            parent: Some(parent),
            ..self.clone()
        }
    }

    /// Adds zero chunks at the end. It never removes chunks. The last page
    /// already holds zeros past the old end, so it does not change.
    pub fn grow_to(&mut self, disk_size: u64) -> Result<()> {
        let disk_size = round_up(disk_size, self.chunk_size);
        ensure!(disk_size >= self.disk_size, "a disk cannot be made smaller");
        self.disk_size = disk_size;
        self.pages.resize(page_count(self.chunk_count()), None);
        Ok(())
    }

    pub fn is_blank(&self) -> bool {
        self.pages.iter().all(Option::is_none)
    }

    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(128 + self.pages.len() * 32);
        out.put_slice(MAGIC);
        out.put_u32_le(VERSION);
        out.put_u32_le(self.chunk_size as u32);
        // Bytes 12..20: `Catalog` reads the disk size with a range GET.
        out.put_u64_le(self.disk_size);
        out.put_u64_le(self.seq);
        out.put_u64_le(self.time);
        out.put_slice(&self.parent.unwrap_or(ContentHash::ZERO).0);
        out.put_u16_le(self.disk_id.len() as u16);
        out.put_slice(self.disk_id.as_bytes());
        for page in &self.pages {
            out.put_slice(&page.unwrap_or(ContentHash::ZERO).0);
        }
        out.freeze()
    }

    pub fn decode(mut data: Bytes) -> Result<Self> {
        ensure!(data.len() >= 70 && &data[..4] == MAGIC, "not a mica manifest");
        data.advance(4);
        let version = data.get_u32_le();
        ensure!(version == VERSION, "unknown manifest version {version}");
        let chunk_size = data.get_u32_le() as u64;
        validate_chunk_size(chunk_size)?;
        let disk_size = data.get_u64_le();
        let seq = data.get_u64_le();
        let time = data.get_u64_le();
        let parent = read_hash(&mut data);
        let id_len = data.get_u16_le() as usize;
        ensure!(data.len() >= id_len, "manifest is cut short");
        let disk_id = String::from_utf8(data.split_to(id_len).to_vec())?;
        let pages = page_count((disk_size / chunk_size) as usize);
        if data.len() != pages * 32 {
            bail!("manifest has {} bytes of pages, expected {}", data.len(), pages * 32);
        }
        let pages = (0..pages).map(|_| read_hash(&mut data)).collect();
        Ok(Self { disk_id, chunk_size, disk_size, seq, time, parent, pages })
    }
}

impl Page {
    pub fn zero() -> Self {
        Self { chunks: vec![None; CHUNKS_PER_PAGE] }
    }

    pub fn is_zero(&self) -> bool {
        self.chunks.iter().all(Option::is_none)
    }

    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(PAGE_BYTES);
        out.put_slice(PAGE_MAGIC);
        out.put_u32_le(CHUNKS_PER_PAGE as u32);
        for chunk in &self.chunks {
            out.put_slice(&chunk.unwrap_or(ContentHash::ZERO).0);
        }
        out.freeze()
    }

    pub fn decode(mut data: Bytes) -> Result<Self> {
        ensure!(data.len() == PAGE_BYTES && &data[..4] == PAGE_MAGIC, "not a mica page");
        data.advance(4);
        let count = data.get_u32_le() as usize;
        ensure!(count == CHUNKS_PER_PAGE, "page has {count} chunks, expected {CHUNKS_PER_PAGE}");
        Ok(Self { chunks: (0..count).map(|_| read_hash(&mut data)).collect() })
    }

    /// Writes the page to S3, compressed. A zero page is not stored: its hash is `None`.
    pub async fn save(&self, bucket: &Bucket) -> Result<Option<ContentHash>> {
        if self.is_zero() {
            return Ok(None);
        }
        let data = self.encode();
        let hash = ContentHash::of(&data);
        bucket.put(&keys::page(&hash), codec::pack(&data, codec::PAGE_LEVEL)?.into()).await?;
        Ok(Some(hash))
    }

    /// Reads a page from S3, and checks its hash.
    pub async fn load(bucket: &Bucket, hash: &ContentHash) -> Result<Self> {
        let packed = bucket.get(&keys::page(hash)).await?.ok_or_else(|| anyhow!("page {hash} is missing in S3"))?;
        let data = codec::unpack(&packed, PAGE_BYTES)?;
        hash.verify(&data)?;
        Self::decode(Bytes::from(data))
    }
}

/// Metadata pages for a disk of `chunks` chunks.
pub fn page_count(chunks: usize) -> usize {
    chunks.div_ceil(CHUNKS_PER_PAGE)
}

pub fn round_up(size: u64, chunk_size: u64) -> u64 {
    size.div_ceil(chunk_size) * chunk_size
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn read_hash(data: &mut Bytes) -> Option<ContentHash> {
    let mut hash = [0u8; 32];
    data.copy_to_slice(&mut hash);
    (hash != ContentHash::ZERO.0).then_some(ContentHash(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_round_trip() {
        let mut manifest = Manifest::empty("disk-1", 100 << 30, DEFAULT_CHUNK_SIZE).unwrap();
        assert_eq!(manifest.chunk_count(), 409_600);
        assert_eq!(manifest.pages.len(), 400);
        manifest.pages[7] = Some(ContentHash::of(b"page"));
        manifest.parent = Some(ContentHash::of(b"parent"));
        let decoded = Manifest::decode(manifest.encode()).unwrap();
        assert_eq!(decoded.pages, manifest.pages);
        assert_eq!(decoded.parent, manifest.parent);
        assert_eq!(decoded.disk_size, 100 << 30);
        // A 100 GiB root is about 12.5 KiB.
        assert!(manifest.encode().len() < 13 << 10);
    }

    #[test]
    fn disk_size_is_at_bytes_12_to_20() {
        let manifest = Manifest::empty("d", 3 << 30, MIN_CHUNK_SIZE).unwrap();
        let data = manifest.encode();
        assert_eq!(u64::from_le_bytes(data[12..20].try_into().unwrap()), 3 << 30);
    }

    #[test]
    fn page_round_trip() {
        let mut page = Page::zero();
        assert!(page.is_zero());
        page.chunks[0] = Some(ContentHash::of(b"a"));
        page.chunks[1023] = Some(ContentHash::of(b"b"));
        let data = page.encode();
        assert_eq!(data.len(), 8 + 1024 * 32);
        assert_eq!(Page::decode(data).unwrap(), page);
    }

    #[test]
    fn grow_adds_pages_only_past_the_last_one() {
        let mut manifest = Manifest::empty("d", 300 << 20, DEFAULT_CHUNK_SIZE).unwrap();
        assert_eq!(manifest.pages.len(), 2);
        manifest.grow_to(512 << 20).unwrap();
        assert_eq!(manifest.pages.len(), 2);
        manifest.grow_to((512 << 20) + 1).unwrap();
        assert_eq!(manifest.pages.len(), 3);
        assert!(manifest.grow_to(1 << 20).is_err());
    }

    #[test]
    fn chunk_sizes() {
        for size in [64 << 10, 256 << 10, 1 << 20, 4 << 20] {
            validate_chunk_size(size).unwrap();
        }
        for size in [4096, 100 << 10, 8 << 20] {
            assert!(validate_chunk_size(size).is_err());
        }
    }
}
