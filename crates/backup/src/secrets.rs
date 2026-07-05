//! OS-backed secret storage for saved volume passwords.
//!
//! When the user ticks "remember password", the passphrase is persisted so
//! scheduled backups of an encrypted volume can run unattended. We never store
//! it in the clear:
//!
//! - **Windows**: DPAPI at machine scope ([`DpapiSecretStore`]). The OS holds
//!   the key, so the stored blob is bound to this machine — a copied database
//!   file cannot be decrypted elsewhere — yet the backup service (a service
//!   account) can still decrypt it for scheduled runs. An app-specific entropy
//!   value is mixed in as defense in depth.
//! - **Linux**: not implemented yet; the methods return an error rather than
//!   ever writing plaintext. A libsecret / root-only key-file backend will land
//!   with Linux support.

use own_backup_common::Result;
use zeroize::Zeroizing;

/// Encrypt/decrypt small secrets (volume passwords) with an OS-backed key.
pub trait SecretStore {
    /// Encrypt `plaintext` into an opaque blob safe to store in the database.
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Recover the plaintext from a blob produced by [`Self::protect`].
    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>>;
}

/// DPAPI-backed store (Windows). Machine scope so the backup service can decrypt
/// saved passwords for scheduled runs even when no user is logged in.
pub struct DpapiSecretStore;

/// Application-specific DPAPI entropy — mixed into every blob so another app on
/// the same machine cannot unprotect our secrets without also knowing this.
#[cfg(target_os = "windows")]
const ENTROPY: &[u8] = b"MyOwnBackup/secret/v1";

#[cfg(target_os = "windows")]
mod imp {
    use super::{DpapiSecretStore, SecretStore, ENTROPY};
    use own_backup_common::{OwnBackupError, Result};
    use zeroize::Zeroizing;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE,
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

#[cfg(not(target_os = "windows"))]
impl SecretStore for DpapiSecretStore {
    fn protect(&self, _plaintext: &[u8]) -> Result<Vec<u8>> {
        Err(unsupported())
    }
    fn unprotect(&self, _blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "windows"))]
fn unsupported() -> own_backup_common::OwnBackupError {
    own_backup_common::OwnBackupError::Device(
        "secret store: DPAPI is Windows-only; the Linux backend is not implemented yet".into(),
    )
}
