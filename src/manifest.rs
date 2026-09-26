use crate::bucket::{Bucket, keys};
use crate::content_hash::ContentHash;
use anyhow::{Result, anyhow, bail, ensure};
use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const CHUNK_SIZE: u64 = 4 << 20;

const MAGIC: &[u8; 4] = b"MICA";
const VERSION: u32 = 1;

/// The full chunk list of a disk at one commit. `None` is an all-zero chunk.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub disk_id: String,
    pub chunk_size: u64,
    pub disk_size: u64,
    pub seq: u64,
    pub time: u64,
    pub parent: Option<ContentHash>,
    pub chunks: Vec<Option<ContentHash>>,
}

impl Manifest {
    pub fn empty(disk_id: &str, disk_size: u64) -> Self {
        let disk_size = round_up_to_chunk(disk_size);
        Self {
            disk_id: disk_id.to_string(),
            chunk_size: CHUNK_SIZE,
            disk_size,
            seq: 0,
            time: unix_now(),
            parent: None,
            chunks: vec![None; (disk_size / CHUNK_SIZE) as usize],
        }
    }

    pub async fn load(bucket: &Bucket, hash: &ContentHash) -> Result<Self> {
        let data = bucket
            .get(&keys::manifest(hash))
            .await?
            .ok_or_else(|| anyhow!("manifest {hash} is missing in S3"))?;
        hash.verify(&data)?;
        Self::decode(data)
    }

    /// Writes the manifest to S3 and returns its hash.
    pub async fn save(&self, bucket: &Bucket) -> Result<ContentHash> {
        let data = self.encode();
        let hash = ContentHash::of(&data);
        bucket.put(&keys::manifest(&hash), data).await?;
        Ok(hash)
    }

    /// The next manifest: same chunks, one seq higher. A clone shares the
    /// manifest of its snapshot, so the disk ID can change here.
    pub fn next(&self, disk_id: &str, parent: ContentHash) -> Self {
        Self {
            disk_id: disk_id.to_string(),
            seq: self.seq + 1,
            time: unix_now(),
            parent: Some(parent),
            ..self.clone()
        }
    }

    /// Adds zero chunks at the end. It never removes chunks.
    pub fn grow_to(&mut self, disk_size: u64) -> Result<()> {
        let disk_size = round_up_to_chunk(disk_size);
        ensure!(disk_size >= self.disk_size, "a disk cannot be made smaller");
        self.disk_size = disk_size;
        self.chunks.resize((disk_size / self.chunk_size) as usize, None);
        Ok(())
    }

    pub fn is_blank(&self) -> bool {
        self.chunks.iter().all(Option::is_none)
    }

    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(128 + self.chunks.len() * 32);
        out.put_slice(MAGIC);
        out.put_u32_le(VERSION);
        out.put_u32_le(self.chunk_size as u32);
        out.put_u64_le(self.disk_size);
        out.put_u64_le(self.seq);
        out.put_u64_le(self.time);
        out.put_slice(&self.parent.unwrap_or(ContentHash::ZERO).0);
        out.put_u16_le(self.disk_id.len() as u16);
        out.put_slice(self.disk_id.as_bytes());
        for chunk in &self.chunks {
            out.put_slice(&chunk.unwrap_or(ContentHash::ZERO).0);
        }
        out.freeze()
    }

    pub fn decode(mut data: Bytes) -> Result<Self> {
        ensure!(data.len() >= 70 && &data[..4] == MAGIC, "not a mica manifest");
        data.advance(4);
        let version = data.get_u32_le();
        ensure!(version == VERSION, "unknown manifest version {version}");
        let chunk_size = data.get_u32_le() as u64;
        let disk_size = data.get_u64_le();
        let seq = data.get_u64_le();
        let time = data.get_u64_le();
        let parent = read_hash(&mut data);
        let id_len = data.get_u16_le() as usize;
        ensure!(data.len() >= id_len, "manifest is cut short");
        let disk_id = String::from_utf8(data.split_to(id_len).to_vec())?;
        let chunk_count = (disk_size / chunk_size) as usize;
        if data.len() != chunk_count * 32 {
            bail!("manifest has {} bytes of chunks, expected {}", data.len(), chunk_count * 32);
        }
        let chunks = (0..chunk_count).map(|_| read_hash(&mut data)).collect();
        Ok(Self { disk_id, chunk_size, disk_size, seq, time, parent, chunks })
    }
}

pub fn round_up_to_chunk(size: u64) -> u64 {
    size.div_ceil(CHUNK_SIZE) * CHUNK_SIZE
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
