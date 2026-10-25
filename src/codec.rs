use anyhow::{Context, Result, ensure};

/// Chunks and pages are stored in S3 as zstd frames. Their key is the hash
/// of the plain data, so dedup and the hash check do not change. The local
/// cache keeps the plain data, so local reads need no decompression.
pub const CHUNK_LEVEL: i32 = 1;
pub const PAGE_LEVEL: i32 = 3;

pub fn pack(data: &[u8], level: i32) -> Result<Vec<u8>> {
    zstd::bulk::compress(data, level).context("compress")
}

/// Fails on data that is not one zstd frame of at most `max_len` bytes.
pub fn unpack(data: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let plain = zstd::bulk::decompress(data, max_len).context("decompress")?;
    ensure!(plain.len() <= max_len, "object is larger than {max_len} bytes");
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let zeros = vec![0u8; 256 << 10];
        let packed = pack(&zeros, CHUNK_LEVEL).unwrap();
        assert!(packed.len() < 1024);
        assert_eq!(unpack(&packed, zeros.len()).unwrap(), zeros);
    }

    #[test]
    fn random_data_grows_only_a_little() {
        let data: Vec<u8> = (0..256u32 << 10).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let packed = pack(&data, CHUNK_LEVEL).unwrap();
        assert!(packed.len() <= data.len() + 64);
        assert_eq!(unpack(&packed, data.len()).unwrap(), data);
    }

    #[test]
    fn rejects_plain_and_oversized_data() {
        assert!(unpack(b"not a zstd frame", 1024).is_err());
        let packed = pack(&vec![7u8; 4096], CHUNK_LEVEL).unwrap();
        assert!(unpack(&packed, 1024).is_err());
    }
}
