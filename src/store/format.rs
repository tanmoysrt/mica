//! The binary objects of a disk in the store: chunks, pages and roots.
//!
//! A root lists one entry per page, and a page lists one entry per chunk.
//! An entry says which transaction wrote the object (its birth) and the
//! SHA-256 of its plain data. The key of the object follows from the disk,
//! the birth and the position of the entry, see `keys.rs`.

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

pub const CHUNKS_PER_PAGE: usize = 1024;
pub const MIN_CHUNK_SIZE: u64 = 64 << 10;
pub const MAX_CHUNK_SIZE: u64 = 4 << 20;
pub const DEFAULT_CHUNK_SIZE: u64 = 1 << 20;

const ENTRY_BYTES: usize = 40;
const PAGE_MAGIC: &[u8; 4] = b"MPAG";
const ROOT_MAGIC: &[u8; 4] = b"MICA";
const ROOT_VERSION: u32 = 2;
const ROOT_HEADER: usize = 44;
const CHUNK_LEVEL: i32 = 1;
const PAGE_LEVEL: i32 = 3;

/// SHA-256 of the plain data of an object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        hex::decode(text).ok()?.try_into().ok().map(Self)
    }

    pub fn verify(&self, data: &[u8]) -> Result<()> {
        ensure!(Self::of(data) == *self, "hash mismatch: expected {}", self.hex());
        Ok(())
    }
}

impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.hex()[..12])
    }
}

/// Where an object is and what it holds. Birth 0 is the zero entry: a chunk
/// or a page of zeros, which is never stored.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Entry {
    pub birth: u64,
    pub hash: Hash,
}

impl Entry {
    pub const ZERO: Entry = Entry { birth: 0, hash: Hash([0; 32]) };

    pub fn is_zero(&self) -> bool {
        self.birth == 0
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.birth.to_le_bytes());
        out.extend_from_slice(&self.hash.0);
    }

    fn read(bytes: &[u8]) -> Self {
        let birth = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        Self { birth, hash: Hash(bytes[8..ENTRY_BYTES].try_into().unwrap()) }
    }
}

/// The entries of 1,024 chunks. Page `k` covers chunks `1024 k` to `1024 k + 1023`.
#[derive(Clone, PartialEq, Debug)]
pub struct Page(pub Vec<Entry>);

impl Page {
    pub fn zero() -> Self {
        Self(vec![Entry::ZERO; CHUNKS_PER_PAGE])
    }

    pub fn is_zero(&self) -> bool {
        self.0.iter().all(Entry::is_zero)
    }

    /// The plain bytes, whose hash goes into the root.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + CHUNKS_PER_PAGE * ENTRY_BYTES);
        out.extend_from_slice(PAGE_MAGIC);
        out.extend_from_slice(&(CHUNKS_PER_PAGE as u32).to_le_bytes());
        self.0.iter().for_each(|entry| entry.write(&mut out));
        out
    }

    pub fn decode(plain: &[u8]) -> Result<Self> {
        ensure!(plain.len() == 8 + CHUNKS_PER_PAGE * ENTRY_BYTES && &plain[..4] == PAGE_MAGIC, "not a page");
        Ok(Self(plain[8..].chunks_exact(ENTRY_BYTES).map(Entry::read).collect()))
    }

    /// The page as it is stored: a zstd frame of the plain bytes.
    pub fn pack(&self) -> Result<(Hash, Vec<u8>)> {
        let plain = self.encode();
        Ok((Hash::of(&plain), zstd::bulk::compress(&plain, PAGE_LEVEL)?))
    }

    pub fn unpack(packed: &[u8], expected: &Hash) -> Result<Self> {
        let plain = zstd::bulk::decompress(packed, 8 + CHUNKS_PER_PAGE * ENTRY_BYTES).context("decompress page")?;
        expected.verify(&plain)?;
        Self::decode(&plain)
    }
}

/// One full version of a disk: its size and one entry per page.
#[derive(Clone, PartialEq, Debug)]
pub struct Root {
    pub chunk_size: u64,
    pub disk_size: u64,
    pub transaction: u64,
    /// The transaction before this one, or 0.
    pub parent: u64,
    /// Unix time of the transaction.
    pub time: u64,
    pub pages: Vec<Entry>,
}

impl Root {
    /// The version of a disk that was never written: all pages are zero.
    pub fn empty(chunk_size: u64, disk_size: u64) -> Self {
        let pages = vec![Entry::ZERO; page_count(disk_size, chunk_size)];
        Self { chunk_size, disk_size, transaction: 0, parent: 0, time: 0, pages }
    }

    pub fn chunk_count(&self) -> usize {
        (self.disk_size / self.chunk_size) as usize
    }

    /// Makes the disk larger. The new pages are zero.
    pub fn grow(&mut self, disk_size: u64) {
        self.disk_size = self.disk_size.max(disk_size);
        self.pages.resize(page_count(self.disk_size, self.chunk_size), Entry::ZERO);
    }

    /// Not compressed, so a range read can get the size of a disk.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ROOT_HEADER + self.pages.len() * ENTRY_BYTES);
        out.extend_from_slice(ROOT_MAGIC);
        out.extend_from_slice(&ROOT_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.chunk_size as u32).to_le_bytes());
        for value in [self.disk_size, self.transaction, self.parent, self.time] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        self.pages.iter().for_each(|entry| entry.write(&mut out));
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= ROOT_HEADER && &bytes[..4] == ROOT_MAGIC, "not a root");
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if u32_at(4) != ROOT_VERSION {
            bail!("root version {} is not supported", u32_at(4));
        }
        let root = Self {
            chunk_size: u32_at(8) as u64,
            disk_size: u64_at(12),
            transaction: u64_at(20),
            parent: u64_at(28),
            time: u64_at(36),
            pages: bytes[ROOT_HEADER..].chunks_exact(ENTRY_BYTES).map(Entry::read).collect(),
        };
        validate_chunk_size(root.chunk_size)?;
        ensure!(root.pages.len() == page_count(root.disk_size, root.chunk_size), "root has the wrong page count");
        Ok(root)
    }
}

/// A chunk as it is stored: a zstd frame of the plain data.
pub fn pack_chunk(plain: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(plain, CHUNK_LEVEL)?)
}

/// Decompresses a chunk and checks it against its entry.
pub fn unpack_chunk(packed: &[u8], expected: &Hash, chunk_size: u64) -> Result<Vec<u8>> {
    let plain = zstd::bulk::decompress(packed, chunk_size as usize).context("decompress chunk")?;
    ensure!(plain.len() as u64 == chunk_size, "chunk has {} bytes, not {chunk_size}", plain.len());
    expected.verify(&plain)?;
    Ok(plain)
}

pub fn page_count(disk_size: u64, chunk_size: u64) -> usize {
    ((disk_size / chunk_size) as usize).div_ceil(CHUNKS_PER_PAGE)
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<()> {
    ensure!(
        chunk_size.is_power_of_two() && (MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size),
        "chunk size must be a power of two from 64K to 4M"
    );
    Ok(())
}

pub fn is_all_zero(data: &[u8]) -> bool {
    data.iter().all(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_and_roots_round_trip() {
        let mut page = Page::zero();
        page.0[7] = Entry { birth: 42, hash: Hash::of(b"seven") };
        let (hash, packed) = page.pack().unwrap();
        assert_eq!(Page::unpack(&packed, &hash).unwrap(), page);
        assert!(Page::unpack(&packed, &Hash::of(b"other")).is_err());

        let mut root = Root::empty(256 << 10, 1 << 30);
        root.pages[2] = Entry { birth: 42, hash };
        root.transaction = 42;
        root.parent = 41;
        assert_eq!(Root::decode(&root.encode()).unwrap(), root);
        assert_eq!(root.pages.len(), 4);
    }

    #[test]
    fn chunks_are_checked_on_unpack() {
        let plain = vec![9u8; MIN_CHUNK_SIZE as usize];
        let packed = pack_chunk(&plain).unwrap();
        assert_eq!(unpack_chunk(&packed, &Hash::of(&plain), MIN_CHUNK_SIZE).unwrap(), plain);
        assert!(unpack_chunk(&packed, &Hash::of(b"x"), MIN_CHUNK_SIZE).is_err());
    }
}
