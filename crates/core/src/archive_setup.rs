//! Turning archive encryption on for a job (plan §7.5, stage E5).
//!
//! Two steps, and the order is the whole point. A job's private key exists in
//! exactly two places a person can reach it from: the password they choose, and
//! the recovery key the service generates. The recovery key is shown once and
//! never stored, so committing the authorization *before* the operator has it
//! would create a job whose archives are one forgotten password away from being
//! unreadable — with nothing left to say so.
//!
//! So [`ArchiveSetup::begin`] prepares everything **in memory** and hands back
//! the recovery key; nothing is written. [`ArchiveSetup::confirm`] asks for that
//! key back and only then commits, through
//! [`crate::JobStore::upsert_with_archive_key`], which writes the local
//! protected copy and the job as one transaction.
//!
//! Echoing the key back is not ceremony: it is bech32 with a checksum, so a
//! mistyped one is *named* as mistyped rather than quietly accepted, and a
//! dialog somebody clicked through without reading cannot pass.
//!
//! # What is held, and for how long
//!
//! A pending authorization holds the job's private key in memory. It expires
//! ([`PENDING_TTL`]), it is single-use, and a wrong echo costs an attempt — so
//! it cannot sit around indefinitely and cannot be brute-forced by a caller who
//! got hold of a token but not the screen it was shown on.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use own_backup_backup::seal::keypair::{JobKeyPair, JobPrivateKey, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::local::ServiceKeyStore;
use own_backup_backup::seal::recovery::RecoveryKey;
use own_backup_common::{OwnBackupError, Result};
use zeroize::Zeroizing;

use crate::{Job, JobStore};

/// How long a prepared authorization waits for its confirmation.
///
/// Long enough to write a recovery key on paper and check it, short enough that
/// a forgotten wizard does not keep a private key in the service's memory for
/// the rest of the day.
pub const PENDING_TTL: Duration = Duration::from_secs(15 * 60);

/// How many times the recovery key may be typed back before the whole step has
/// to be repeated. Typos happen; guessing does not get to be free.
pub const MAX_CONFIRM_ATTEMPTS: u8 = 3;

/// Shortest password accepted. This is the only secret that will exist on the
/// operator's side of an archive, and the KDF is deliberately slow — but slow
/// hashing does not rescue a four-character password.
pub const MIN_PASSWORD_CHARS: usize = 8;

/// What [`ArchiveSetup::begin`] hands back. The recovery key is in it **once**:
/// it is not stored, not logged, and not obtainable again.
pub struct BegunAuthorization {
    /// Opaque, single-use handle for the confirmation step.
    pub token: String,
    /// Grouped for copying onto paper.
    pub recovery_key: Zeroizing<String>,
    /// Public identity of the job's key, safe to show and to record.
    pub fingerprint: String,
    /// The id the job will have — a new job gets one here, because the portable
    /// key is sealed against it before the job exists.
    pub job_id: String,
}

impl std::fmt::Debug for BegunAuthorization {
    /// The recovery key is the one thing in here that must never reach a log,
    /// a panic message or an assertion's output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BegunAuthorization")
            .field("job_id", &self.job_id)
            .field("fingerprint", &self.fingerprint)
            .field("recovery_key", &"<не показывается>")
            .finish()
    }
}

struct Pending {
    job: Job,
    portable: SealedPrivateKey,
    private: JobPrivateKey,
    recovery: RecoveryKey,
    created_at: Instant,
    attempts: u8,
}

/// The prepared-but-uncommitted authorizations of one service process.
#[derive(Default)]
pub struct ArchiveSetup {
    pending: Mutex<HashMap<String, Pending>>,
}

impl ArchiveSetup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepare an authorization. Writes nothing.
    ///
    /// Slow on purpose: sealing the portable envelope runs Argon2id at the
    /// production profile, seconds rather than milliseconds. Call it off the
    /// async runtime.
    pub fn begin(&self, mut job: Job, password: &str) -> Result<BegunAuthorization> {
        if password.chars().count() < MIN_PASSWORD_CHARS {
            return Err(OwnBackupError::Config(format!(
                "пароль архива короче {MIN_PASSWORD_CHARS} символов"
            )));
        }
        if job.archive_encryption.is_some() {
            return Err(OwnBackupError::Config(
                "у задания уже есть ключ архива — замена ключа это отдельный процесс".into(),
            ));
        }
        // The portable envelope is sealed against the job's identity, so a new
        // job needs one now rather than at the commit. Same shape the store
        // would have given it.
        if job.id.is_empty() {
            job.id = uuid::Uuid::new_v4().to_string();
        }
        // Encryption writes our own container, which is the qcow2 path with the
        // seal bit — say so here rather than letting the commit refuse later.
        job.image_format = own_backup_common::types::ImageFormat::Qcow2;

        let pair = JobKeyPair::generate()?;
        let recovery = RecoveryKey::generate()?;
        let portable = SealedPrivateKey::seal(
            &job.id,
            &pair,
            password,
            &recovery,
            &KdfParams::strong(),
        )?;
        let fingerprint = portable.public_key()?.fingerprint();
        let token = uuid::Uuid::new_v4().to_string();
        let begun = BegunAuthorization {
            token: token.clone(),
            recovery_key: recovery.to_string_grouped(),
            fingerprint,
            job_id: job.id.clone(),
        };

        let mut pending = self.lock()?;
        sweep(&mut pending);
        pending.insert(
            token,
            Pending {
                job,
                portable,
                private: pair.private,
                recovery,
                created_at: Instant::now(),
                attempts: 0,
            },
        );
        Ok(begun)
    }

    /// Commit the authorization once the operator types the recovery key back.
    ///
    /// The job is persisted as the last step of the key-store transaction, so a
    /// failure anywhere leaves the job exactly as it was — unencrypted, or
    /// absent.
    pub fn confirm(
        &self,
        store: &JobStore,
        token: &str,
        recovery_echo: &str,
    ) -> Result<Job> {
        // Taken out of the map for the whole attempt: a token is single-use,
        // and only a survivable typo puts it back.
        let mut entry = {
            let mut pending = self.lock()?;
            sweep(&mut pending);
            pending.remove(token).ok_or_else(|| {
                OwnBackupError::Config(
                    "запрос на включение шифрования не найден или устарел — начните заново".into(),
                )
            })?
        };

        let typed = match RecoveryKey::parse(recovery_echo) {
            Ok(key) => key,
            Err(e) => return Err(self.keep_for_retry(token, entry, e)),
        };
        if typed.as_bytes() != entry.recovery.as_bytes() {
            return Err(self.keep_for_retry(
                token,
                entry,
                OwnBackupError::Config("это не тот ключ восстановления".into()),
            ));
        }

        let keys = store.service_key_store()?;
        let job = std::mem::take(&mut entry.job);
        store.upsert_with_archive_key(job, entry.portable.clone(), &entry.private, &keys)
    }

    /// Drop a prepared authorization — the operator went back or closed the
    /// wizard. Nothing was written, so there is nothing else to undo.
    pub fn cancel(&self, token: &str) {
        if let Ok(mut pending) = self.lock() {
            pending.remove(token);
            sweep(&mut pending);
        }
    }

    /// How many authorizations are waiting. For diagnostics and tests.
    pub fn pending_count(&self) -> usize {
        self.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Put a pending entry back after a survivable mistake, or let it go when
    /// the attempts are spent.
    fn keep_for_retry(
        &self,
        token: &str,
        mut entry: Pending,
        error: OwnBackupError,
    ) -> OwnBackupError {
        entry.attempts += 1;
        if entry.attempts >= MAX_CONFIRM_ATTEMPTS {
            return OwnBackupError::Config(format!(
                "{error} — попытки исчерпаны, включение шифрования нужно начать заново"
            ));
        }
        let left = MAX_CONFIRM_ATTEMPTS - entry.attempts;
        match self.lock() {
            Ok(mut pending) => {
                pending.insert(token.to_string(), entry);
                OwnBackupError::Config(format!("{error} (осталось попыток: {left})"))
            }
            Err(e) => e,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Pending>>> {
        self.pending
            .lock()
            .map_err(|_| OwnBackupError::Config("состояние мастера шифрования повреждено".into()))
    }
}

fn sweep(pending: &mut HashMap<String, Pending>) {
    pending.retain(|_, p| p.created_at.elapsed() < PENDING_TTL);
}

impl JobStore {
    /// The service's own protected key store, beside the jobs file.
    pub fn service_key_store(&self) -> Result<ServiceKeyStore> {
        let data_dir = self
            .jobs_path
            .parent()
            .ok_or_else(|| OwnBackupError::Config("нет каталога состояния службы".into()))?;
        ServiceKeyStore::new(data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, std::sync::Arc<JobStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        (dir, store)
    }

    fn job() -> Job {
        Job {
            name: "зашифрованное задание".into(),
            ..Default::default()
        }
    }

    /// The password is the only secret on the operator's side, and a slow KDF
    /// does not make a short one safe.
    #[test]
    fn a_short_password_is_refused_before_any_work() {
        let setup = ArchiveSetup::new();
        let e = setup.begin(job(), "1234567").unwrap_err().to_string();
        assert!(e.contains("короче"), "{e}");
        assert_eq!(setup.pending_count(), 0, "nothing was prepared");
    }

    /// The one test that pays for the real thing: `begin` seals a portable
    /// envelope at the production KDF profile, hands back a recovery key we can
    /// read back, settles the container format, and writes nothing at all.
    #[test]
    fn beginning_prepares_in_memory_and_persists_nothing() {
        let (dir, store) = store();
        let setup = ArchiveSetup::new();
        let mut j = job();
        j.image_format = own_backup_common::types::ImageFormat::Vhdx;
        let begun = setup.begin(j, "пароль архива 1").unwrap();

        assert!(!begun.job_id.is_empty());
        assert!(!begun.fingerprint.is_empty());
        RecoveryKey::parse(&begun.recovery_key).expect("the key shown must be one we can read");
        assert!(
            !format!("{begun:?}").contains(&begun.recovery_key[..8]),
            "the recovery key must not reach a debug line"
        );
        assert_eq!(setup.pending_count(), 1);
        assert!(store.list().is_empty(), "no job was created");
        assert!(
            !dir.path().join(".own-backup-service-keys").exists(),
            "no protected key was written"
        );

        // Encryption is our own container, and that is settled here rather than
        // left to fail at the commit or to a screen to remember.
        let pending = setup.pending.lock().unwrap();
        let prepared = pending.get(&begun.token).unwrap();
        assert_eq!(
            prepared.job.image_format,
            own_backup_common::types::ImageFormat::Qcow2
        );
        assert_eq!(prepared.portable.job_id, begun.job_id);
        // The password opens it, and the recovery key opens it independently —
        // the two slots an archive travels with.
        assert!(prepared.portable.unseal_with_password("пароль архива 1").is_ok());
        let typed = RecoveryKey::parse(&begun.recovery_key).unwrap();
        assert!(prepared.portable.unseal_with_recovery(&typed).is_ok());
    }

    /// A prepared authorization without paying for Argon2id.
    ///
    /// Everything below the sealing step is a state machine — attempts, expiry,
    /// single use — and running the production KDF once per test would cost
    /// half a minute to exercise it. The portable key here is structurally a
    /// key and cryptographically nothing, which is all these paths look at.
    fn prepare(setup: &ArchiveSetup, job: Job) -> (String, Zeroizing<String>) {
        use own_backup_backup::seal::keypair::{
            JobKeyPair, JobPublicKey, Slot, SlotKind, KEY_FILE_FORMAT,
        };
        use own_backup_backup::seal::SCHEME;

        let slot = |kind| Slot {
            kind,
            kdf: KdfParams::strong(),
            salt: format!("{}==", "A".repeat(22)),
            nonce: "A".repeat(16),
            sealed: "A".repeat(64),
        };
        let recovery = RecoveryKey::generate().unwrap();
        let shown = recovery.to_string_grouped();
        let token = uuid::Uuid::new_v4().to_string();
        setup.pending.lock().unwrap().insert(
            token.clone(),
            Pending {
                job,
                portable: SealedPrivateKey {
                    format: KEY_FILE_FORMAT.into(),
                    scheme: SCHEME.into(),
                    job_id: "j1".into(),
                    public: format!("{}=", "A".repeat(43)),
                    fingerprint: JobPublicKey::from_bytes([0; 32]).fingerprint(),
                    slots: vec![slot(SlotKind::Password), slot(SlotKind::Recovery)],
                },
                private: JobKeyPair::generate().unwrap().private,
                recovery,
                created_at: Instant::now(),
                attempts: 0,
            },
        );
        (token, shown)
    }

    /// A token is single-use and a wrong key costs an attempt — but a typo
    /// must not throw away seconds of Argon2id work.
    #[test]
    fn a_mistyped_recovery_key_can_be_retried_and_then_cannot() {
        let (_dir, store) = store();
        let setup = ArchiveSetup::new();
        let (token, _) = prepare(&setup, job());
        let begun = BegunAuthorization {
            token,
            recovery_key: Zeroizing::new(String::new()),
            fingerprint: String::new(),
            job_id: String::new(),
        };

        for attempt in 1..MAX_CONFIRM_ATTEMPTS {
            let e = setup
                .confirm(&store, &begun.token, "это не ключ")
                .unwrap_err()
                .to_string();
            assert!(e.contains("осталось попыток"), "attempt {attempt}: {e}");
            assert_eq!(setup.pending_count(), 1, "the request survives a typo");
        }
        let e = setup
            .confirm(&store, &begun.token, "это не ключ")
            .unwrap_err()
            .to_string();
        assert!(e.contains("исчерпаны"), "{e}");
        assert_eq!(setup.pending_count(), 0, "and then it is gone");
        assert!(store.list().is_empty(), "no job was ever created");
    }

    /// A token nobody prepared, and one that was cancelled, are the same thing.
    #[test]
    fn an_unknown_or_cancelled_token_confirms_nothing() {
        let (_dir, store) = store();
        let setup = ArchiveSetup::new();
        assert!(setup.confirm(&store, "нет такого", "ключ").is_err());

        let (token, shown) = prepare(&setup, job());
        setup.cancel(&token);
        assert_eq!(setup.pending_count(), 0);
        let e = setup.confirm(&store, &token, &shown).unwrap_err().to_string();
        assert!(e.contains("не найден"), "{e}");
    }

    /// An expired request is not a request. (The clock is not mocked; the
    /// entry is aged by hand, which is what the sweep actually reads.)
    #[test]
    fn a_stale_request_is_swept_before_it_can_be_confirmed() {
        let (_dir, store) = store();
        let setup = ArchiveSetup::new();
        let (token, shown) = prepare(&setup, job());
        {
            let mut pending = setup.pending.lock().unwrap();
            let entry = pending.get_mut(&token).unwrap();
            entry.created_at = Instant::now() - PENDING_TTL - Duration::from_secs(1);
        }
        assert!(setup.confirm(&store, &token, &shown).is_err());
        assert_eq!(setup.pending_count(), 0);
    }

    /// A job that already has a key is not re-keyed by this path: replacing a
    /// key means every archive written with the old one becomes unreadable
    /// unless it is rewritten, which is a different operation. The commit
    /// refuses it too (`backup_encryption::validate_update`); this is the early
    /// refusal, before seconds of Argon2id have been spent on a dead end.
    #[test]
    fn a_job_that_already_has_a_key_is_refused_before_any_work() {
        use own_backup_backup::seal::keypair::{JobPublicKey, Slot, SlotKind, KEY_FILE_FORMAT};
        use own_backup_backup::seal::SCHEME;

        // Structurally a key, cryptographically nothing — `begin` looks at
        // whether the field is there, which is the whole point of the guard.
        let slot = |kind| Slot {
            kind,
            kdf: KdfParams::strong(),
            salt: format!("{}==", "A".repeat(22)),
            nonce: "A".repeat(16),
            sealed: "A".repeat(64),
        };
        let mut j = job();
        j.id = "j1".into();
        j.archive_encryption = Some(crate::backup_encryption::ArchiveEncryption {
            portable_key: SealedPrivateKey {
                format: KEY_FILE_FORMAT.into(),
                scheme: SCHEME.into(),
                job_id: "j1".into(),
                public: format!("{}=", "A".repeat(43)),
                fingerprint: JobPublicKey::from_bytes([0; 32]).fingerprint(),
                slots: vec![slot(SlotKind::Password), slot(SlotKind::Recovery)],
            },
            local_key: serde_json::from_value(serde_json::json!({ "id": uuid::Uuid::new_v4() }))
                .unwrap(),
        });

        let setup = ArchiveSetup::new();
        let e = setup.begin(j, "пароль архива 2").unwrap_err().to_string();
        assert!(e.contains("уже есть ключ"), "{e}");
        assert_eq!(setup.pending_count(), 0);
    }
}
