//! Cryptographic core for encrypted backups (plan §7).
//!
//! Records use authenticated AES-256-GCM with independent writer sessions.
//! X25519 wraps a chain key for a job; portable password/recovery slots protect
//! the job's private key. Scheduled work additionally needs the local service
//! key store (E2), never just the public key.
//!
//! See FORMAT.md in this directory for the v2 wire contract. Container indexes,
//! Windows key storage and engine integration are separate stages.

pub mod cluster;
pub mod context;
pub mod keypair;
pub mod local;
pub mod obk;
pub mod recovery;
pub mod wrap;

#[cfg(test)]
mod vectors;

use own_backup_common::{OwnBackupError, Result};
use zeroize::Zeroizing;

/// Версия схемы. Идёт в `info` каждого HKDF и в файл ключа: сменится схема —
/// сменится и она, и старые ключи не подойдут к новым байтам по ошибке.
pub const SCHEME: &str = "own-backup/v2";

/// Мастер-ключ цепочки: 32 случайных байта. Из него HKDF выводит ключи сеансов и
/// ключ запечатанной части манифеста — сам он ни в одном шифре не участвует.
#[derive(Clone)]
pub struct ChainKey(Zeroizing<[u8; 32]>);

impl ChainKey {
    /// Новый случайный ключ — для новой цепочки.
    pub fn generate() -> Result<Self> {
        let mut k = Zeroizing::new([0u8; 32]);
        fill_random(&mut k[..])?;
        Ok(ChainKey(k))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ChainKey(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Производный ключ на 32 байта под названным назначением.
    ///
    /// Соль не нужна: мастер-ключ случаен и уникален на цепочку, а назначение
    /// разводит ключи данных и манифеста так, что компрометация одного не даёт
    /// другого.
    pub fn derive(&self, purpose: &str) -> Zeroizing<[u8; 32]> {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, &self.0[..]);
        let mut out = Zeroizing::new([0u8; 32]);
        let info = format!("{SCHEME}/{purpose}");
        hk.expand(info.as_bytes(), &mut out[..])
            .expect("32 байта — заведомо в пределах HKDF-SHA256");
        out
    }
}

impl std::fmt::Debug for ChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Ключ в журнал не попадает ни при каком `{:?}`.
        f.write_str("ChainKey(…)")
    }
}

/// Случайные байты из системного источника. Единственная точка, где они
/// берутся, — чтобы ни один ключ не пришёл из чего-то попроще.
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf)
        .map_err(|e| OwnBackupError::Config(format!("источник случайных чисел недоступен: {e}")))
}

/// Ошибки этого модуля — все как `Config`: ни одна не про устройство или
/// формат образа, а текст для человека важнее вида.
pub(crate) fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Config(msg.to_string())
}

/// Encrypt a bounded plaintext using an isolated, wiped work buffer. The tag
/// is appended to the returned ciphertext. Callers own nonce uniqueness.
pub(super) fn aead_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    let cipher = aes_gcm::Aes256Gcm::new(key.into());
    let mut work = Zeroizing::new(plaintext.to_vec());
    let tag = cipher
        .encrypt_in_place_detached(nonce.into(), aad, &mut work)
        .map_err(|_| bad("AES-GCM encryption failed"))?;
    let mut sealed = Vec::with_capacity(work.len() + 16);
    sealed.extend_from_slice(&work);
    sealed.extend_from_slice(&tag);
    Ok(sealed)
}

/// On authentication failure the work buffer is wiped and no plaintext escapes.
pub(super) fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    if sealed.len() < 16 {
        return Err(bad("missing AES-GCM authentication tag"));
    }
    let cipher = aes_gcm::Aes256Gcm::new(key.into());
    let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
    let mut work = Zeroizing::new(ciphertext.to_vec());
    cipher
        .decrypt_in_place_detached(nonce.into(), aad, &mut work, tag.into())
        .map_err(|_| bad("authentication failed: wrong key or damaged data"))?;
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_key_is_random_and_its_derivations_differ_by_purpose() {
        let a = ChainKey::generate().unwrap();
        let b = ChainKey::generate().unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes(), "два вызова — два ключа");

        let data = a.derive("data");
        let manifest = a.derive("manifest");
        assert_ne!(&data[..], &manifest[..], "назначение разводит ключи");
        assert_eq!(
            &a.derive("data")[..],
            &data[..],
            "и при этом вывод детерминирован"
        );
        assert_ne!(
            &data[..],
            &a.as_bytes()[..],
            "производный ключ — не сам мастер"
        );
    }

    #[test]
    fn a_chain_key_never_prints_itself() {
        let k = ChainKey::from_bytes([7u8; 32]);
        assert_eq!(format!("{k:?}"), "ChainKey(…)");
    }
}
