//! Пара ключей задания и файл, в котором закрытый ключ лежит запечатанным.
//!
//! У каждого задания своя пара X25519. Открытый ключ живёт на машине в
//! открытую: им можно только запереть ([`super::wrap`]), и ночной копии по
//! расписанию его достаточно. Закрытый ключ никогда не лежит в открытом виде —
//! только запечатанным, в двух слотах: под паролем и под ключом восстановления
//! ([`super::recovery`]). Каждый слот — Argon2id над секретом → 32 байта →
//! AES-256-GCM поверх закрытого ключа. Открытый ключ идёт в GCM как
//! дополнительные данные: подменить его в файле, не зная секрета, нельзя —
//! слот перестанет открываться.
//!
//! Тот же запечатанный файл кладётся в каждый образ (§7.2): диск с копиями,
//! унесённый от умершей машины, должен открываться одним паролем.
//!
//! Файл — JSON: его читают и наш код, и человек, который через год захочет
//! понять, что это за строки у него в бумагах. Все байты — base64.

use argon2::{Algorithm, Argon2, Params, Version};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::recovery::RecoveryKey;
use super::{bad, fill_random, SCHEME};
use own_backup_common::Result;

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

    /// Первые восемь байт SHA-256 открытого ключа, шестнадцатеричными. Этим
    /// образ говорит, чей он, а файл ключа — что он тот самый.
    pub fn fingerprint(&self) -> String {
        let h = Sha256::digest(self.0);
        h[..8].iter().map(|b| format!("{b:02x}")).collect()
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

/// Параметры Argon2id. Пишутся в файл рядом со слотом, поэтому их можно
/// менять со временем, не ломая старые ключи.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub algorithm: String,
    /// Память в КиБ.
    pub m_kib: u32,
    /// Проходы.
    pub t: u32,
    /// Параллелизм.
    pub p: u32,
}

impl KdfParams {
    /// Боевые параметры: 256 МиБ, три прохода, четыре потока — около секунды
    /// на этой машине и годы у того, кто перебирает пароль по файлу. Носитель
    /// с 4 ГБ памяти их тянет.
    pub fn strong() -> Self {
        KdfParams { algorithm: "argon2id".into(), m_kib: 256 * 1024, t: 3, p: 4 }
    }

    /// Для тестов: та же схема, но мгновенно.
    pub fn light() -> Self {
        KdfParams { algorithm: "argon2id".into(), m_kib: 1024, t: 1, p: 1 }
    }

    fn derive(&self, secret: &[u8], salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        if self.algorithm != "argon2id" {
            return Err(bad(format!("неизвестный KDF в файле ключа: {}", self.algorithm)));
        }
        let params = Params::new(self.m_kib, self.t, self.p, Some(32))
            .map_err(|e| bad(format!("параметры Argon2id не годятся: {e}")))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = Zeroizing::new([0u8; 32]);
        argon
            .hash_password_into(secret, salt, &mut out[..])
            .map_err(|e| bad(format!("Argon2id не отработал: {e}")))?;
        Ok(out)
    }
}

/// Чем открывается слот.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Password,
    Recovery,
}

/// Один способ открыть закрытый ключ.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Slot {
    pub kind: SlotKind,
    pub kdf: KdfParams,
    /// 16 байт, base64.
    pub salt: String,
    /// 12 байт, base64.
    pub nonce: String,
    /// 32 байта ключа + 16 байт тега, base64.
    pub sealed: String,
}

/// Файл ключа задания. То, что лежит в `data_dir` и в каждом образе.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedPrivateKey {
    pub format: String,
    pub scheme: String,
    pub job_id: String,
    /// Открытый ключ, base64. Лежит здесь же, чтобы файл был самодостаточен.
    pub public: String,
    pub fingerprint: String,
    pub slots: Vec<Slot>,
}

pub const KEY_FILE_FORMAT: &str = "own-backup-job-key";

impl SealedPrivateKey {
    /// Запечатать закрытый ключ паролем и ключом восстановления.
    pub fn seal(
        job_id: &str,
        pair: &JobKeyPair,
        password: &str,
        recovery: &RecoveryKey,
        kdf: &KdfParams,
    ) -> Result<Self> {
        let slots = vec![
            seal_slot(SlotKind::Password, password.as_bytes(), &pair.public, &pair.private, kdf)?,
            seal_slot(SlotKind::Recovery, recovery.as_bytes(), &pair.public, &pair.private, kdf)?,
        ];
        Ok(SealedPrivateKey {
            format: KEY_FILE_FORMAT.into(),
            scheme: SCHEME.into(),
            job_id: job_id.into(),
            public: B64.encode(pair.public.as_bytes()),
            fingerprint: pair.public.fingerprint(),
            slots,
        })
    }

    pub fn public_key(&self) -> Result<JobPublicKey> {
        let bytes = B64.decode(&self.public).map_err(|_| bad("открытый ключ в файле не base64"))?;
        let arr: [u8; 32] =
            bytes.as_slice().try_into().map_err(|_| bad("открытый ключ должен быть 32 байта"))?;
        let public = JobPublicKey(arr);
        if public.fingerprint() != self.fingerprint {
            return Err(bad("отпечаток в файле ключа не сходится с открытым ключом — файл правлен"));
        }
        Ok(public)
    }

    pub fn unseal_with_password(&self, password: &str) -> Result<JobPrivateKey> {
        self.unseal(SlotKind::Password, password.as_bytes())
    }

    pub fn unseal_with_recovery(&self, recovery: &RecoveryKey) -> Result<JobPrivateKey> {
        self.unseal(SlotKind::Recovery, recovery.as_bytes())
    }

    fn unseal(&self, kind: SlotKind, secret: &[u8]) -> Result<JobPrivateKey> {
        if self.format != KEY_FILE_FORMAT {
            return Err(bad(format!("это не файл ключа задания: формат «{}»", self.format)));
        }
        if self.scheme != SCHEME {
            return Err(bad(format!(
                "файл ключа схемы «{}», а эта программа понимает «{SCHEME}»",
                self.scheme
            )));
        }
        let public = self.public_key()?;
        let slot = self
            .slots
            .iter()
            .find(|s| s.kind == kind)
            .ok_or_else(|| bad(format!("в файле ключа нет слота {kind:?}")))?;

        let salt = B64.decode(&slot.salt).map_err(|_| bad("соль слота не base64"))?;
        let nonce = B64.decode(&slot.nonce).map_err(|_| bad("nonce слота не base64"))?;
        let sealed = B64.decode(&slot.sealed).map_err(|_| bad("содержимое слота не base64"))?;

        let slot_key = slot.kdf.derive(secret, &salt)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&slot_key[..]));
        let opened = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: &sealed, aad: public.as_bytes() },
            )
            .map_err(|_| match kind {
                // Единственная причина отказа тега при целом файле — не тот
                // секрет. Говорим это, а не «ошибка расшифровки».
                SlotKind::Password => bad("пароль не подходит"),
                SlotKind::Recovery => bad("ключ восстановления не подходит"),
            })?;
        let opened = Zeroizing::new(opened);
        let arr: [u8; 32] = opened
            .as_slice()
            .try_into()
            .map_err(|_| bad("в слоте не 32 байта закрытого ключа — файл повреждён"))?;
        let private = JobPrivateKey::from_bytes(arr);
        // Закрытый ключ обязан порождать тот открытый, что записан рядом.
        // Иначе кто-то подменил слот целиком, вместе с секретом.
        if private.public() != public {
            return Err(bad("закрытый ключ из слота не соответствует открытому — файл подменён"));
        }
        Ok(private)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| bad(format!("файл ключа не сериализуется: {e}")))
    }

    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| bad(format!("это не файл ключа задания: {e}")))
    }
}

fn seal_slot(
    kind: SlotKind,
    secret: &[u8],
    public: &JobPublicKey,
    private: &JobPrivateKey,
    kdf: &KdfParams,
) -> Result<Slot> {
    let mut salt = [0u8; 16];
    fill_random(&mut salt)?;
    let mut nonce = [0u8; 12];
    fill_random(&mut nonce)?;
    let slot_key = kdf.derive(secret, &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&slot_key[..]));
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &private.0[..], aad: public.as_bytes() })
        .map_err(|_| bad("AES-GCM не запечатал ключ"))?;
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
        let file =
            SealedPrivateKey::seal("job-1", &pair, "correct horse", &recovery, &KdfParams::light())
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
        let err = file.unseal_with_password("correct horse").unwrap_err().to_string();
        assert!(err.contains("не подходит"), "{err}");
    }

    #[test]
    fn a_stale_fingerprint_is_caught_before_any_kdf_runs() {
        let (_, _, mut file) = sealed();
        file.fingerprint = "0000000000000000".into();
        let err = file.public_key().unwrap_err().to_string();
        assert!(err.contains("отпечаток"), "{err}");
    }

    #[test]
    fn a_file_of_another_scheme_is_refused_by_name() {
        let (_, _, mut file) = sealed();
        file.scheme = "own-backup/v0".into();
        let err = file.unseal_with_password("correct horse").unwrap_err().to_string();
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
}
