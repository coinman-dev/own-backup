use own_backup_common::error::{OwnBackupError, Result};

/// Compress a block with zstd at the given level.
///
/// Returns `(data, is_compressed)`. If the compressed output is larger than or
/// equal to the original, the original data is returned unchanged with
/// `is_compressed = false`.
pub fn compress_block(data: &[u8], level: i32) -> Result<(Vec<u8>, bool)> {
    if data.is_empty() {
        return Ok((Vec::new(), false));
    }

    let compressed = zstd::bulk::compress(data, level)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;

    if compressed.len() >= data.len() {
        // Compression did not help; return original data uncompressed.
        Ok((data.to_vec(), false))
    } else {
        Ok((compressed, true))
    }
}

/// Maximum allowed decompressed block size (16 MiB).
/// Prevents zip-bomb attacks where a tiny compressed payload expands to gigabytes.
const MAX_DECOMPRESSED_SIZE: usize = 16 * 1024 * 1024;

/// Decompress a zstd-compressed block.
/// Rejects output larger than MAX_DECOMPRESSED_SIZE to prevent zip-bomb DoS.
pub fn decompress_block(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let decoder = zstd::Decoder::new(data)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    let mut limited = decoder.take(MAX_DECOMPRESSED_SIZE as u64 + 1);
    let mut output = Vec::new();
    limited
        .read_to_end(&mut output)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    if output.len() > MAX_DECOMPRESSED_SIZE {
        return Err(OwnBackupError::Compression(format!(
            "Decompressed block exceeds maximum size ({} > {} bytes)",
            output.len(),
            MAX_DECOMPRESSED_SIZE
        )));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_compressible_data() {
        let original = vec![0u8; 4096];
        let (compressed, is_compressed) = compress_block(&original, 3).unwrap();
        assert!(is_compressed);
        assert!(compressed.len() < original.len());

        let decompressed = decompress_block(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn empty_block() {
        let (data, is_compressed) = compress_block(&[], 3).unwrap();
        assert!(!is_compressed);
        assert!(data.is_empty());
    }

    #[test]
    fn incompressible_data_returns_original() {
        // Pseudo-random bytes are generally incompressible.
        let mut original = vec![0u8; 1024];
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
        for byte in original.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 33) as u8;
        }

        let (data, is_compressed) = compress_block(&original, 3).unwrap();
        if !is_compressed {
            assert_eq!(data, original);
        } else {
            let decompressed = decompress_block(&data).unwrap();
            assert_eq!(decompressed, original);
        }
    }

    #[test]
    fn various_levels() {
        let original = vec![42u8; 8192];
        for level in [1, 3, 10] {
            let (compressed, is_compressed) = compress_block(&original, level).unwrap();
            assert!(is_compressed, "level {level} should compress repetitive data");
            let decompressed = decompress_block(&compressed).unwrap();
            assert_eq!(decompressed, original, "roundtrip failed at level {level}");
        }
    }
}
