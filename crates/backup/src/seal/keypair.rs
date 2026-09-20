//! Portable password and recovery slots for a job's private X25519 key.
//!
//! The service's local DPAPI envelope is separate and must never be serialized
//! into this document. Scheduled jobs need that private-key access after reboot.
//! Every field is bounded before KDF work; slots authenticate their role, KDF
//! parameters, full recipient key and job identity. See FORMAT.md.

use argon2::{Algorithm, Argon2, Block, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::recovery::RecoveryKey;
use super::{aead_open, aead_seal, bad, fill_random, SCHEME};
use own_backup_common::Result;

pub const MAX_KEY_FILE_BYTES: usize = 16 * 1024;
pub const MAX_JOB_ID_BYTES: usize = 128;
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub const KEY_FILE_FORMAT: &str = "own-backup-job-key";

/// Открытый ключ задания. Не секрет: печатается, сравнивается, лежит в образе.
#[derive(Clone, PartialEq, Eq)]
pub struct JobPublicKey([u8; 32]);

impl JobPublicKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        JobPublicKey(b)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Полный SHA-256 открытого ключа, шестнадцатеричными. Этим
    /// образ говорит, чей он, а файл ключа — что он тот самый.
    pub fn fingerprint(&self) -> String {
        let h = Sha256::digest(self.0);
        h.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(super) fn dalek(&self) -> PublicKey {
        PublicKey::from(self.0)
    }
}

impl std::fmt::Debug for JobPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JobPublicKey({})", self.fingerprint())
    }
}

/// Закрытый ключ задания. В памяти — только по делу, обнуляется при сбросе.
pub struct JobPrivateKey(Zeroizing<[u8; 32]>);

impl JobPrivateKey {
    /// Only the sibling service-key store can persist this, protected by DPAPI.
    pub(super) fn storage_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        JobPrivateKey(Zeroizing::new(b))
    }

    pub fn public(&self) -> JobPublicKey {
        JobPublicKey(*PublicKey::from(&self.dalek()).as_bytes())
    }

    pub(super) fn dalek(&self) -> StaticSecret {
        StaticSecret::from(*self.0)
    }
}

impl std::fmt::Debug for JobPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobPrivateKey(…)")
    }
}

/// Новая пара для нового задания.
pub struct JobKeyPair {
    pub public: JobPublicKey,
    pub private: JobPrivateKey,
}

impl JobKeyPair {
    pub fn generate() -> Result<Self> {
        let mut raw = Zeroizing::new([0u8; 32]);
        fill_random(&mut raw[..])?;
        let private = JobPrivateKey(raw);
        let public = private.public();
        Ok(JobKeyPair { public, private })
    }
}

/// Argon2id v1.3 parameters, encoded explicitly in each portable slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KdfParams {
    pub algorithm: String,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    /// Initial production profile; latency must be measured on Windows/WinPE.
    pub fn strong() -> Self {
        Self {
            algorithm: "argon2id".into(),
            m_kib: 256 * 1024,
            t: 3,
            p: 4,
        }
    }

    #[cfg(test)]
    fn light() -> Self {
        Self {
            algorithm: "argon2id".into(),
            m_kib: 1024,
            t: 1,
            p: 1,
        }
    }

    #[cfg(test)]
    pub(super) fn test_profile() -> Self {
        Self::light()
    }

    fn validate(&self) -> Result<()> {
        if self.algorithm != "argon2id" {
            return Err(bad("unknown key-file KDF"));
        }
        #[cfg(test)]
        if *self == Self::light() {
            return Ok(());
        }
        if !(64 * 1024..=512 * 1024).contains(&self.m_kib)
            || !(3..=6).contains(&self.t)
            || !(1..=8).contains(&self.p)
            || u64::from(self.m_kib) * u64::from(self.t) > 1536 * 1024
        {
            return Err(bad(
                "Argon2id parameters outside the supported resource/security limits",
            ));
        }
        Ok(())
    }

    fn derive(&self, secret: &[u8], salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
        self.validate()?;
        if secret.is_empty() || secret.len() > MAX_PASSWORD_BYTES {
            return Err(bad("invalid secret length"));
        }
        let params = Params::new(self.m_kib, self.t, self.p, Some(32))
            .map_err(|_| bad("invalid Argon2id parameters"))?;
        let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = Zeroizing::new([0; 32]);
        argon
            .hash_password_into_with_memory(secret, salt, &mut out[..], &mut memory[..])
            .map_err(|_| bad("Argon2id key derivation failed"))?;
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Password,
    Recovery,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub kind: SlotKind,
    pub kdf: KdfParams,
    /// Canonical padded base64: salt=16, nonce=12, ciphertext+tag=48 bytes.
    pub salt: String,
    pub nonce: String,
    pub sealed: String,
}

/// Portable envelope only. Exactly one password and one recovery slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedPrivateKey {
    pub format: String,
    pub scheme: String,
    pub job_id: String,
    pub public: String,
    pub fingerprint: String,
    pub slots: Vec<Slot>,
}

impl SealedPrivateKey {
    pub fn seal(
        job_id: &str,
        pair: &JobKeyPair,
        password: &str,
        recovery: &RecoveryKey,
        kdf: &KdfParams,
    ) -> Result<Self> {
        validate_job_id(job_id)?;
        kdf.validate()?;
        if pair.private.public() != pair.public {
            return Err(bad("job key pair does not match"));
        }
        let slots = vec![
            seal_slot(job_id, SlotKind::Password, password.as_bytes(), pair, kdf)?,
            seal_slot(job_id, SlotKind::Recovery, recovery.as_bytes(), pair, kdf)?,
        ];
        Ok(Self {
            format: KEY_FILE_FORMAT.into(),
            scheme: SCHEME.into(),
            job_id: job_id.into(),
            public: B64.encode(pair.public.as_bytes()),
            fingerprint: pair.public.fingerprint(),
            slots,
        })
    }

    pub fn public_key(&self) -> Result<JobPublicKey> {
        let public = JobPublicKey(decode_fixed::<32>(&self.public)?);
        if self.fingerprint != public.fingerprint() {
            return Err(bad("отпечаток открытого ключа не совпадает"));
        }
        Ok(public)
    }

    /// Validate the entire document before doing any expensive work.
    pub fn validate(&self) -> Result<()> {
        if self.format != KEY_FILE_FORMAT || self.scheme != SCHEME {
            return Err(bad("неизвестный формат/версия схемы файла ключа"));
        }
        validate_job_id(&self.job_id)?;
        self.public_key()?;
        if self.slots.len() != 2
            || self
                .slots
                .iter()
                .filter(|s| s.kind == SlotKind::Password)
                .count()
                != 1
            || self
                .slots
                .iter()
                .filter(|s| s.kind == SlotKind::Recovery)
                .count()
                != 1
        {
            return Err(bad("expected one password slot and one recovery slot"));
        }
        for slot in &self.slots {
            slot.kdf.validate()?;
            decode_fixed::<16>(&slot.salt)?;
            decode_fixed::<12>(&slot.nonce)?;
            decode_fixed::<48>(&slot.sealed)?;
        }
        Ok(())
    }

    pub fn unseal_with_password(&self, password: &str) -> Result<JobPrivateKey> {
        self.unseal(SlotKind::Password, password.as_bytes())
    }

    pub fn unseal_with_recovery(&self, recovery: &RecoveryKey) -> Result<JobPrivateKey> {
        self.unseal(SlotKind::Recovery, recovery.as_bytes())
    }

    fn unseal(&self, kind: SlotKind, secret: &[u8]) -> Result<JobPrivateKey> {
        self.validate()?;
        let public = self.public_key()?;
        let slot = self
            .slots
            .iter()
            .find(|s| s.kind == kind)
            .ok_or_else(|| bad("missing key slot"))?;
        let salt = decode_fixed::<16>(&slot.salt)?;
        let nonce = decode_fixed::<12>(&slot.nonce)?;
        let sealed = decode_fixed::<48>(&slot.sealed)?;
        let key = slot.kdf.derive(secret, &salt)?;
        let aad = slot_aad(&self.job_id, &public, kind, &slot.kdf, &salt, &nonce);
        let opened = aead_open(&key, &nonce, &aad, &sealed).map_err(|_| match kind {
            SlotKind::Password => bad("пароль не подходит или файл ключа повреждён"),
            SlotKind::Recovery => bad("ключ восстановления не подходит или файл ключа повреждён"),
        })?;
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&opened);
        let private = JobPrivateKey::from_bytes(bytes);
        if private.public() != public {
            return Err(bad("private key does not match the declared recipient"));
        }
        Ok(private)
    }

    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        let json =
            serde_json::to_string_pretty(self).map_err(|_| bad("cannot serialize key file"))?;
        if json.len() > MAX_KEY_FILE_BYTES {
            return Err(bad("key file exceeds the size limit"));
        }
        Ok(json)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_KEY_FILE_BYTES {
            return Err(bad("key file exceeds the size limit"));
        }
        let file: Self = serde_json::from_str(json).map_err(|_| bad("invalid key-file JSON"))?;
        file.validate()?;
        Ok(file)
    }
}

fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty() || job_id.len() > MAX_JOB_ID_BYTES || job_id.chars().any(char::is_control)
    {
        return Err(bad("invalid job identity"));
    }
    Ok(())
}

fn decode_fixed<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N.div_ceil(3) * 4 {
        return Err(bad("invalid encoded key-field length"));
    }
    let bytes = B64
        .decode(value)
        .map_err(|_| bad("invalid key-field base64"))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| bad("invalid decoded key-field length"))
}

fn slot_aad(
    job_id: &str,
    public: &JobPublicKey,
    kind: SlotKind,
    kdf: &KdfParams,
    salt: &[u8; 16],
    nonce: &[u8; 12],
) -> Vec<u8> {
    let mut aad = b"OBKS\0\x02".to_vec();
    aad.extend_from_slice(&(job_id.len() as u16).to_be_bytes());
    aad.extend_from_slice(job_id.as_bytes());
    aad.extend_from_slice(public.as_bytes());
    aad.push(match kind {
        SlotKind::Password => 1,
        SlotKind::Recovery => 2,
    });
    for parameter in [kdf.m_kib, kdf.t, kdf.p] {
        aad.extend_from_slice(&parameter.to_be_bytes());
    }
    aad.extend_from_slice(salt);
    aad.extend_from_slice(nonce);
    aad
}

fn seal_slot(
    job_id: &str,
    kind: SlotKind,
    secret: &[u8],
    pair: &JobKeyPair,
    kdf: &KdfParams,
) -> Result<Slot> {
    let mut salt = [0; 16];
    fill_random(&mut salt)?;
    let mut nonce = [0; 12];
    fill_random(&mut nonce)?;
    let key = kdf.derive(secret, &salt)?;
    let aad = slot_aad(job_id, &pair.public, kind, kdf, &salt, &nonce);
    let sealed = aead_seal(&key, &nonce, &aad, &pair.private.0[..])?;
    Ok(Slot {
        kind,
        kdf: kdf.clone(),
        salt: B64.encode(salt),
        nonce: B64.encode(nonce),
        sealed: B64.encode(sealed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed() -> (JobKeyPair, RecoveryKey, SealedPrivateKey) {
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let file = SealedPrivateKey::seal(
            "job-1",
            &pair,
            "correct horse",
            &recovery,
            &KdfParams::light(),
        )
        .unwrap();
        (pair, recovery, file)
    }

    #[test]
    fn the_private_key_comes_back_with_either_secret() {
        let (pair, recovery, file) = sealed();
        let by_password = file.unseal_with_password("correct horse").unwrap();
        let by_recovery = file.unseal_with_recovery(&recovery).unwrap();
        assert_eq!(by_password.public(), pair.public);
        assert_eq!(by_recovery.public(), pair.public);
    }

    #[test]
    fn a_wrong_password_is_named_as_such() {
        let (_, _, file) = sealed();
        let err = file.unseal_with_password("wrong").unwrap_err().to_string();
        assert!(err.contains("пароль не подходит"), "{err}");
        let other = RecoveryKey::generate().unwrap();
        let err = file.unseal_with_recovery(&other).unwrap_err().to_string();
        assert!(err.contains("ключ восстановления не подходит"), "{err}");
    }

    #[test]
    fn the_file_survives_json_and_says_who_it_is() {
        let (pair, _, file) = sealed();
        let json = file.to_json().unwrap();
        assert!(json.contains(KEY_FILE_FORMAT));
        assert!(json.contains(&pair.public.fingerprint()));
        let back = SealedPrivateKey::from_json(&json).unwrap();
        assert_eq!(back.public_key().unwrap(), pair.public);
        assert_eq!(back.slots.len(), 2);
        back.unseal_with_password("correct horse").unwrap();
    }

    /// Открытый ключ в файле — часть того, что защищает тег. Подменить его и
    /// оставить слот открывающимся нельзя.
    #[test]
    fn swapping_the_public_key_in_the_file_is_caught() {
        let (_, _, mut file) = sealed();
        let other = JobKeyPair::generate().unwrap();
        file.public = B64.encode(other.public.as_bytes());
        file.fingerprint = other.public.fingerprint();
        let err = file
            .unseal_with_password("correct horse")
            .unwrap_err()
            .to_string();
        assert!(err.contains("не подходит"), "{err}");
    }

    #[test]
    fn a_stale_fingerprint_is_caught_before_any_kdf_runs() {
        let (_, _, mut file) = sealed();
        file.fingerprint = "0".repeat(64);
        let err = file.public_key().unwrap_err().to_string();
        assert!(err.contains("отпечаток"), "{err}");
    }

    #[test]
    fn a_file_of_another_scheme_is_refused_by_name() {
        let (_, _, mut file) = sealed();
        file.scheme = "own-backup/v0".into();
        let err = file
            .unseal_with_password("correct horse")
            .unwrap_err()
            .to_string();
        assert!(err.contains("схемы"), "{err}");
    }

    #[test]
    fn two_seals_of_the_same_key_differ_in_bytes() {
        // Соль и nonce случайны: два файла одного ключа не совпадают, и по
        // ним нельзя понять, что ключ один.
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let a = SealedPrivateKey::seal("j", &pair, "pw", &recovery, &KdfParams::light()).unwrap();
        let b = SealedPrivateKey::seal("j", &pair, "pw", &recovery, &KdfParams::light()).unwrap();
        assert_ne!(a.slots[0].sealed, b.slots[0].sealed);
    }

    #[test]
    fn keys_do_not_print_themselves() {
        let pair = JobKeyPair::generate().unwrap();
        assert_eq!(format!("{:?}", pair.private), "JobPrivateKey(…)");
        assert!(format!("{:?}", pair.public).starts_with("JobPublicKey("));
    }

    #[test]
    fn malformed_fields_and_resource_bombs_fail_before_kdf() {
        let (_, _, file) = sealed();
        let mut bad_files = Vec::new();
        for size in [0, 1, 11, 13, 32, 1024] {
            let mut f = file.clone();
            f.slots[0].nonce = B64.encode(vec![0; size]);
            bad_files.push(f);
        }
        for field in ["salt", "sealed", "public"] {
            let mut f = file.clone();
            match field {
                "salt" => f.slots[0].salt = B64.encode([0; 15]),
                "sealed" => f.slots[0].sealed = B64.encode([0; 47]),
                _ => f.public = B64.encode([0; 31]),
            }
            bad_files.push(f);
        }
        for params in [
            KdfParams {
                m_kib: u32::MAX,
                ..KdfParams::strong()
            },
            KdfParams {
                t: u32::MAX,
                ..KdfParams::strong()
            },
            KdfParams {
                p: u32::MAX,
                ..KdfParams::strong()
            },
            KdfParams {
                m_kib: 512 * 1024,
                t: 6,
                ..KdfParams::strong()
            },
            KdfParams {
                m_kib: 8,
                t: 1,
                p: 1,
                ..KdfParams::strong()
            },
        ] {
            let mut f = file.clone();
            f.slots[0].kdf = params;
            bad_files.push(f);
        }
        let mut duplicate = file.clone();
        duplicate.slots[1].kind = SlotKind::Password;
        bad_files.push(duplicate);
        let mut missing = file.clone();
        missing.slots.pop();
        bad_files.push(missing);
        for f in bad_files {
            assert!(f.unseal_with_password("correct horse").is_err());
            let json = serde_json::to_string(&f).unwrap();
            assert!(SealedPrivateKey::from_json(&json).is_err());
        }
        assert!(SealedPrivateKey::from_json(&" ".repeat(MAX_KEY_FILE_BYTES + 1)).is_err());
        let mut unknown = serde_json::to_value(&file).unwrap();
        unknown["local_dpapi_key"] = "must not be portable".into();
        assert!(SealedPrivateKey::from_json(&unknown.to_string()).is_err());
    }

    #[test]
    fn job_identity_slot_role_and_kdf_parameters_are_authenticated() {
        let (_, _, file) = sealed();
        let mut renamed = file.clone();
        renamed.job_id = "job-2".into();
        assert!(renamed.unseal_with_password("correct horse").is_err());
        let mut roles = file.clone();
        roles.slots[0].kind = SlotKind::Recovery;
        roles.slots[1].kind = SlotKind::Password;
        assert!(roles.unseal_with_password("correct horse").is_err());
        let mut altered = file.clone();
        altered.slots[0].kdf.t += 1;
        assert!(altered.unseal_with_password("correct horse").is_err());
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        assert!(SealedPrivateKey::seal("", &pair, "pw", &recovery, &KdfParams::light()).is_err());
        assert!(SealedPrivateKey::seal("j", &pair, "", &recovery, &KdfParams::light()).is_err());
        assert!(file
            .unseal_with_password(&"x".repeat(MAX_PASSWORD_BYTES + 1))
            .is_err());
    }
}
