use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::fmt;

/// SHA-256 of a chunk or a manifest. Written as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub const ZERO: ContentHash = ContentHash([0; 32]);

    pub fn of(data: &[u8]) -> Self {
        ContentHash(Sha256::digest(data).into())
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(text: &str) -> Result<Self> {
        let bytes = hex::decode(text).map_err(|_| anyhow!("bad hash: {text}"))?;
        let array: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("bad hash length: {text}"))?;
        Ok(ContentHash(array))
    }

    /// Reference form used in JSON records: `sha256:<hex>`.
    pub fn to_reference(self) -> String {
        format!("sha256:{}", self.to_hex())
    }

    pub fn from_reference(text: &str) -> Result<Self> {
        match text.strip_prefix("sha256:") {
            Some(hex) => Self::from_hex(hex),
            None => bail!("bad hash reference: {text}"),
        }
    }

    pub fn verify(&self, data: &[u8]) -> Result<()> {
        let actual = ContentHash::of(data);
        if actual != *self {
            bail!("hash mismatch: expected {self}, got {actual}");
        }
        Ok(())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}

pub fn is_all_zero(data: &[u8]) -> bool {
    data.iter().all(|byte| *byte == 0)
}
