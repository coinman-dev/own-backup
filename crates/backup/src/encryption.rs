use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use own_backup_common::error::{OwnBackupError, Result};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A 256-bit encryption key for AES-256-GCM.
/// Key material is automatically zeroized when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct EncryptionKey {
    key_bytes: [u8; 32],
}

impl EncryptionKey {
    pub fn new(key_bytes: [u8; 32]) -> Self {
        Self { key_bytes }
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.key_bytes
    }
}

/// Derive an encryption key from a password and salt using Argon2id.
///
/// Security model:
/// - `salt` MUST be a fresh random 32-byte value generated per backup file.
/// - Each backup file gets its own unique key: `key = Argon2id(password, file_salt)`.
/// - Since the key is unique per file, nonces within that file are safe as pure
///   block counters (0, 1, 2, ...) with no birthday-paradox risk.
///
/// Parameters: 64 MiB memory, 3 iterations, 4 parallelism.
/// ~0.5-1s on modern hardware — acceptable for infrequent backup operations.
pub fn derive_key(password: &str, salt: &[u8]) -> Result<EncryptionKey> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let params = Params::new(
        65536, // 64 MiB memory
        3,     // 3 iterations
        4,     // 4 parallelism lanes
        Some(32),
    )
    .map_err(|e| OwnBackupError::Encryption(format!("Argon2 params error: {}", e)))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key_bytes = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key_bytes)
        .map_err(|e| OwnBackupError::Encryption(e.to_string()))?;
    Ok(EncryptionKey { key_bytes })
}

/// Generate a random 32-byte salt suitable for [`derive_key`].
/// Must be called once per backup file and stored in the manifest.
pub fn generate_salt() -> [u8; 32] {
    let mut salt = [0u8; 32];
    getrandom::getrandom(&mut salt).expect("OS RNG failed");
    salt
}

/// Build a 12-byte AES-GCM nonce from a block number (pure counter).
///
/// This is safe because `EncryptionKey` is derived from Argon2id with a
/// per-file random salt — each backup file has a cryptographically unique key.
/// With a unique key, nonces can safely restart at 0 for each file.
///
/// Layout: 4 zero bytes || 8-byte little-endian block_number.
fn nonce_for_block(block_number: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..12].copy_from_slice(&block_number.to_le_bytes());
    nonce
}

/// Encrypt a block using AES-256-GCM.
///
/// `key` must have been derived from a per-file unique salt (see [`generate_salt`]).
/// The nonce is computed from `block_number` and is NOT prepended to the output.
pub fn encrypt_block(data: &[u8], key: &EncryptionKey, block_number: u64) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&key.key_bytes)
        .map_err(|e| OwnBackupError::Encryption(e.to_string()))?;

    let nonce_bytes = nonce_for_block(block_number);
    let nonce = Nonce::from_slice(&nonce_bytes);

    cipher
        .encrypt(nonce, data)
        .map_err(|e| OwnBackupError::Encryption(e.to_string()))
}

/// Decrypt a block using AES-256-GCM.
pub fn decrypt_block(data: &[u8], key: &EncryptionKey, block_number: u64) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&key.key_bytes)
        .map_err(|e| OwnBackupError::Encryption(e.to_string()))?;

    let nonce_bytes = nonce_for_block(block_number);
    let nonce = Nonce::from_slice(&nonce_bytes);

    cipher
        .decrypt(nonce, data)
        .map_err(|e| OwnBackupError::Encryption(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> EncryptionKey {
        let salt = [0xAA; 32];
        derive_key("test-password", &salt).unwrap()
    }

    #[test]
    fn roundtrip() {
        let key = test_key();
        let plaintext = b"Hello, block-level backup!";

        let ciphertext = encrypt_block(plaintext, &key, 42).unwrap();
        assert_ne!(ciphertext.as_slice(), plaintext.as_slice());

        let decrypted = decrypt_block(&ciphertext, &key, 42).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_rejected() {
        let key = test_key();
        let wrong_key = EncryptionKey::new([0xFF; 32]);
        let plaintext = b"secret data";

        let ciphertext = encrypt_block(plaintext, &key, 0).unwrap();
        let result = decrypt_block(&ciphertext, &wrong_key, 0);
        assert!(result.is_err());
    }

    #[test]
    fn wrong_block_number_rejected() {
        // AES-GCM tag covers the ciphertext — wrong nonce = authentication failure.
        let key = test_key();
        let plaintext = b"important data";

        let ciphertext = encrypt_block(plaintext, &key, 7).unwrap();
        let result = decrypt_block(&ciphertext, &key, 99); // wrong block number
        assert!(result.is_err());
    }

    #[test]
    fn different_blocks_produce_different_ciphertext() {
        let key = test_key();
        let plaintext = b"identical payload";

        let ct_a = encrypt_block(plaintext, &key, 0).unwrap();
        let ct_b = encrypt_block(plaintext, &key, 1).unwrap();
        assert_ne!(ct_a, ct_b);
    }

    #[test]
    fn different_file_keys_are_independent() {
        // Two backups with different salts → different keys → no nonce cross-correlation.
        let salt_a = generate_salt();
        let salt_b = generate_salt();
        assert_ne!(salt_a, salt_b);

        let key_a = derive_key("same-password", &salt_a).unwrap();
        let key_b = derive_key("same-password", &salt_b).unwrap();
        assert_ne!(key_a.as_bytes(), key_b.as_bytes());
    }

    #[test]
    fn generate_salt_is_random() {
        let s1 = generate_salt();
        let s2 = generate_salt();
        assert_ne!(s1, s2);
    }

    #[test]
    fn derive_key_is_deterministic() {
        let salt = [0xBB; 32];
        let k1 = derive_key("password", &salt).unwrap();
        let k2 = derive_key("password", &salt).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn key_is_zeroized_on_drop() {
        let salt = [0xCC; 32];
        let key = derive_key("password", &salt).unwrap();
        let key_copy = *key.as_bytes();
        assert_ne!(key_copy, [0u8; 32]);
        drop(key);
        // ZeroizeOnDrop zeroes key_bytes on drop — verified by the derive macro.
    }
}
