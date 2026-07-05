//! Transparent decrypting view over an encrypted (LUKS) volume.
//!
//! [`DecryptingVolume`] wraps any [`RawVolume`] and a recovered [`LuksParams`]
//! and presents the *decrypted data area* as a plain volume starting at offset
//! 0. The filesystem used-block builders ([`crate::btrfs_map`],
//! [`crate::ext_map`], …) and the block reader read through it unchanged, so a
//! plaintext, used-blocks-only backup of an encrypted volume is just the normal
//! path with this layer spliced under it.
//!
//! Reads decrypt whole AES-XTS sectors: any offset/length is widened to the
//! enclosing sector window, decrypted, then the requested sub-range is copied
//! out — so callers need not align to the cipher's sector size.

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::luks::LuksParams;

/// A decrypting view over an encrypted volume. Offset 0 is the first byte of the
/// decrypted filesystem (i.e. the LUKS data area).
pub struct DecryptingVolume {
    inner: Box<dyn RawVolume + Send>,
    params: LuksParams,
    /// Length of the decrypted data area in bytes.
    length: u64,
}

impl DecryptingVolume {
    pub fn new(inner: Box<dyn RawVolume + Send>, params: LuksParams) -> Self {
        let length = inner.len().saturating_sub(params.data_offset);
        Self { inner, params, length }
    }

    pub fn params(&self) -> &LuksParams {
        &self.params
    }
}

impl RawVolume for DecryptingVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.params.sector_size as u64;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| OwnBackupError::Device("decrypt: read range overflows".into()))?;
        if end > self.length {
            return Err(OwnBackupError::Device(format!(
                "decrypt: read of {} bytes at {offset} exceeds data area {}",
                buf.len(),
                self.length
            )));
        }

        // Widen to whole cipher sectors, decrypt, then copy out the sub-range.
        let aligned_start = offset / ss * ss;
        let aligned_end = end.next_multiple_of(ss);
        let mut window = vec![0u8; (aligned_end - aligned_start) as usize];
        self.inner
            .read_at(self.params.data_offset + aligned_start, &mut window)?;
        self.params.decrypt(aligned_start, &mut window)?;

        let lo = (offset - aligned_start) as usize;
        buf.copy_from_slice(&window[lo..lo + buf.len()]);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::KeyInit;
    use aes::Aes256;
    use own_backup_common::image::VolumeEncryptionKind;
    use xts_mode::{get_tweak_default, Xts128};
    use zeroize::Zeroizing;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            if e > self.0.len() {
                return Err(OwnBackupError::Device("read past end".into()));
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    #[test]
    fn decrypts_data_area_at_any_alignment() {
        let key: Vec<u8> = (0..64u8).collect();
        let data_offset = 4096u64; // pretend the data area starts here
        let sectors = 8usize;
        let plain: Vec<u8> = (0..(sectors * 512)).map(|i| (i * 31 + 7) as u8).collect();

        // Encrypt the plaintext as XTS sectors starting at data-sector 0.
        let mut enc = plain.clone();
        let c1 = Aes256::new(GenericArray::from_slice(&key[..32]));
        let c2 = Aes256::new(GenericArray::from_slice(&key[32..]));
        Xts128::new(c1, c2).encrypt_area(&mut enc, 512, 0, get_tweak_default);

        let mut dev = vec![0u8; data_offset as usize + enc.len()];
        dev[data_offset as usize..].copy_from_slice(&enc);

        let params = LuksParams {
            data_offset,
            sector_size: 512,
            iv_offset: 0,
            master_key: Zeroizing::new(key),
            kind: VolumeEncryptionKind::Luks2,
        };
        let mut vol = DecryptingVolume::new(Box::new(MemVolume(dev)), params);
        assert_eq!(vol.len(), enc.len() as u64);

        // Whole-sector read.
        let mut got = vec![0u8; 512];
        vol.read_at(512, &mut got).unwrap();
        assert_eq!(got, plain[512..1024]);

        // Unaligned read spanning a sector boundary.
        let mut got = vec![0u8; 300];
        vol.read_at(1000, &mut got).unwrap();
        assert_eq!(got, plain[1000..1300]);

        // Reading past the data area fails.
        let mut past = vec![0u8; 512];
        assert!(vol.read_at(enc.len() as u64, &mut past).is_err());
    }
}
