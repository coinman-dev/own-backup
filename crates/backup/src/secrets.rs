//! OS-backed secret storage for saved volume passwords.
//!
//! When the user ticks "remember password", the passphrase is persisted so
//! scheduled backups of an encrypted volume can run unattended. We never store
//! it in the clear:
//!
//! - **Windows**: DPAPI at machine scope ([`DpapiSecretStore`]). The OS holds
//!   the key, so the stored blob is bound to this machine — a copied database
//!   file cannot be decrypted elsewhere — yet the backup service (a service
//!   account) can still decrypt it for scheduled runs. Machine-scope DPAPI does
//!   not prevent another local user with the blob from decrypting it: the
//!   service stores blobs only in its SYSTEM-only private directory. The
//!   app-specific entropy value is defense in depth, not an access boundary.
//! - **Linux**: not implemented yet; the methods return an error rather than
//!   ever writing plaintext. A libsecret / root-only key-file backend will land
//!   with Linux support.

use std::path::Path;

use own_backup_common::Result;
use zeroize::Zeroizing;

/// Encrypt/decrypt small secrets (volume passwords) with an OS-backed key.
pub trait SecretStore {
    /// Encrypt `plaintext` into an opaque blob safe to store in the database.
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Recover the plaintext from a blob produced by [`Self::protect`].
    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>>;
}

/// The platform's default secret store. Windows uses DPAPI (machine scope);
/// other systems use a machine-local key file at `key_path` (AES-256-GCM).
#[cfg(target_os = "windows")]
pub fn default_store(_key_path: &Path) -> Box<dyn SecretStore> {
    Box::new(DpapiSecretStore)
}

/// The platform's default secret store — see the Windows variant.
#[cfg(not(target_os = "windows"))]
pub fn default_store(key_path: &Path) -> Box<dyn SecretStore> {
    Box::new(KeyfileSecretStore::new(key_path))
}

/// DPAPI-backed store (Windows). Machine scope so the backup service can decrypt
/// saved passwords for scheduled runs even when no user is logged in.
pub struct DpapiSecretStore;

/// Application-specific DPAPI entropy — mixed into every blob so another app on
/// the same machine cannot unprotect our secrets without also knowing this.
#[cfg(target_os = "windows")]
const ENTROPY: &[u8] = b"OwnBackup/secret/v1";

#[cfg(target_os = "windows")]
mod imp {
    use super::{DpapiSecretStore, SecretStore, ENTROPY};
    use own_backup_common::{OwnBackupError, Result};
    use zeroize::Zeroizing;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPT_INTEGER_BLOB,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copy a DPAPI-allocated output blob into a `Vec` and free it with LocalFree.
    ///
    /// # Safety
    /// `out` must be a blob returned by a successful CryptProtect/UnprotectData
    /// call (so `pbData` points at `cbData` valid bytes owned by the caller).
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut _));
        v
    }

    impl SecretStore for DpapiSecretStore {
        fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            let input = blob(plaintext);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB::default();
            // SAFETY: input/entropy point at live slices for the call; `out` is
            // a valid writable blob that DPAPI fills with a LocalAlloc buffer.
            unsafe {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_LOCAL_MACHINE,
                    &mut out,
                )
                .map_err(|e| OwnBackupError::Device(format!("CryptProtectData failed: {e}")))?;
                Ok(take(out))
            }
        }

        fn unprotect(&self, blob_in: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
            let input = blob(blob_in);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB::default();
            // SAFETY: as above; a wrong machine / tampered blob fails cleanly.
            unsafe {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_LOCAL_MACHINE,
                    &mut out,
                )
                .map_err(|e| OwnBackupError::Device(format!("CryptUnprotectData failed: {e}")))?;
                Ok(Zeroizing::new(take(out)))
            }
        }
    }
}

/// Non-Windows stub for the DPAPI type (never selected by [`default_store`] off
/// Windows — see [`KeyfileSecretStore`]).
#[cfg(not(target_os = "windows"))]
impl SecretStore for DpapiSecretStore {
    fn protect(&self, _plaintext: &[u8]) -> Result<Vec<u8>> {
        Err(own_backup_common::OwnBackupError::Device(
            "secret store: DPAPI is Windows-only".into(),
        ))
    }
    fn unprotect(&self, _blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        Err(own_backup_common::OwnBackupError::Device(
            "secret store: DPAPI is Windows-only".into(),
        ))
    }
}

/// Machine-local key-file secret store (Linux/Unix). The analogue of DPAPI at
/// machine scope: a random 32-byte key lives in `<data_dir>/secret.key` with
/// mode 0600, created on first use. Secrets are sealed with AES-256-GCM; the key
/// never leaves the machine (a copied database is useless without it) and no
/// login session is needed, so a system service can decrypt for scheduled runs.
#[cfg(not(target_os = "windows"))]
pub struct KeyfileSecretStore {
    key_path: std::path::PathBuf,
}

#[cfg(not(target_os = "windows"))]
mod keyfile {
    use super::{KeyfileSecretStore, SecretStore};
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use own_backup_common::{OwnBackupError, Result};
    use std::path::{Path, PathBuf};
    use zeroize::Zeroizing;

    const NONCE_LEN: usize = 12;

    fn err(msg: impl std::fmt::Display) -> OwnBackupError {
        OwnBackupError::Device(format!("secret store: {msg}"))
    }

    impl KeyfileSecretStore {
        pub fn new(key_path: impl Into<PathBuf>) -> Self {
            Self {
                key_path: key_path.into(),
            }
        }

        /// Read the 32-byte key; when `create`, generate and persist it (0600)
        /// if absent. `create = false` errors on a missing key (the secret that
        /// was sealed with it is unrecoverable).
        fn key(&self, create: bool) -> Result<Zeroizing<[u8; 32]>> {
            match std::fs::read(&self.key_path) {
                Ok(bytes) => {
                    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                        err(format!("key file {} is corrupt", self.key_path.display()))
                    })?;
                    Ok(Zeroizing::new(arr))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                    if let Some(dir) = self.key_path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    let mut k = Zeroizing::new([0u8; 32]);
                    getrandom::getrandom(&mut k[..]).map_err(|e| err(format!("OS RNG: {e}")))?;
                    write_private(&self.key_path, &k[..])?;
                    Ok(k)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(err("no key file — saved secret cannot be recovered"))
                }
                Err(e) => Err(e.into()),
            }
        }

        fn cipher(&self, create: bool) -> Result<Aes256Gcm> {
            let key = self.key(create)?;
            Aes256Gcm::new_from_slice(&key[..]).map_err(|e| err(format!("bad key: {e}")))
        }
    }

    /// Create/truncate `path` owner-only (0600) and write `data`.
    fn write_private(path: &Path, data: &[u8]) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
        Ok(())
    }

    impl SecretStore for KeyfileSecretStore {
        fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            let cipher = self.cipher(true)?;
            let mut nonce_bytes = [0u8; NONCE_LEN];
            getrandom::getrandom(&mut nonce_bytes).map_err(|e| err(format!("OS RNG: {e}")))?;
            let nonce = Nonce::from_slice(&nonce_bytes);
            let ct = cipher
                .encrypt(nonce, plaintext)
                .map_err(|_| err("encrypt failed"))?;
            let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
            out.extend_from_slice(&nonce_bytes);
            out.extend_from_slice(&ct);
            Ok(out)
        }

        fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
            if blob.len() < NONCE_LEN {
                return Err(err("sealed blob too short"));
            }
            let cipher = self.cipher(false)?;
            let nonce = Nonce::from_slice(&blob[..NONCE_LEN]);
            let pt = cipher
                .decrypt(nonce, &blob[NONCE_LEN..])
                .map_err(|_| err("decrypt failed (wrong key or tampered)"))?;
            Ok(Zeroizing::new(pt))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn keyfile_roundtrip_and_persistence() {
            let dir = tempfile::tempdir().unwrap();
            let key = dir.path().join("secret.key");
            let store = KeyfileSecretStore::new(&key);

            let blob = store.protect(b"hunter2").unwrap();
            assert_ne!(&blob[..], b"hunter2"); // not plaintext
            assert_eq!(store.unprotect(&blob).unwrap().as_slice(), b"hunter2");

            // Key file created 0600 and reused by a fresh store instance.
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let store2 = KeyfileSecretStore::new(&key);
            assert_eq!(store2.unprotect(&blob).unwrap().as_slice(), b"hunter2");

            // A different key can't decrypt.
            let other = KeyfileSecretStore::new(dir.path().join("other.key"));
            assert!(other.unprotect(&blob).is_err());
        }
    }
}
