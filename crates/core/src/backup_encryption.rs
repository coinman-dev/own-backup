//! Job-side transaction boundary for encrypted archive authorization.
//!
//! The encryption wizard/RPC arrives in E5. Ordinary SaveJob cannot introduce,
//! replace or clear authorization. E4 must replace the fail-closed run guard
//! with a key context threaded through the actual encrypted image pipeline.

use own_backup_backup::seal::context::ArchiveKeys;
use own_backup_backup::seal::keypair::{JobPrivateKey, SealedPrivateKey};
use own_backup_backup::seal::local::{LocalKeyRef, ServiceKeyStore};
use own_backup_common::{tr, OwnBackupError, Result};
use serde::{Deserialize, Serialize};

use crate::{Job, JobStore};

/// [`crate::JobRun::error_kind`]: the job's local authorization is missing or
/// no longer opens — authorizing it again is the remedy.
pub const ERROR_KIND_REAUTHORIZE: &str = "archive_reauthorize";
/// [`crate::JobRun::error_kind`]: the machine's key protection (TPM,
/// `systemd-creds`) failed — authorizing again would fail the same way.
pub const ERROR_KIND_KEY_PROTECTION: &str = "key_protection";

/// Why the keys of an encrypted run could not be opened, and which kind of
/// failure that is (`ERROR_KIND_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunKeyError {
    pub message: String,
    pub kind: &'static str,
}

impl RunKeyError {
    /// The kind follows the error's variant, never its text.
    fn from_open(e: &OwnBackupError) -> Self {
        RunKeyError {
            message: e.to_string(),
            kind: match e {
                OwnBackupError::KeyProtection(_) => ERROR_KIND_KEY_PROTECTION,
                _ => ERROR_KIND_REAUTHORIZE,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveEncryption {
    pub portable_key: SealedPrivateKey,
    pub local_key: LocalKeyRef,
}

impl JobStore {
    /// Restore only this machine's authorization from the existing portable
    /// envelope. The caller holds the busy slot and fetched this job from the
    /// store; the revision check prevents a slow unlock from undoing an edit.
    pub fn reauthorize_archive(
        &self,
        job: Job,
        password: Option<&str>,
        recovery_key: Option<&str>,
    ) -> Result<Job> {
        let (portable, private) = unlock_existing_archive(&job, password, recovery_key)?;
        let keys = self.service_key_store()?;
        let saved = self.upsert_with_archive_key(job, portable, &private, &keys)?;
        // A shared reference, if any, is still retained. Do not blindly revoke
        // the previous id: authorization has committed and this full-list sweep
        // is what proves which old files are no longer referenced.
        if let Err(error) = self.sweep_local_archive_keys() {
            tracing::warn!(job = %saved.id, error = %error, "obsolete local archive authorization could not be retired");
        }
        Ok(saved)
    }

    /// Service-only caller: confirm recovery delivery, unlock portable_key and
    /// serialize against active/queued jobs before entering (E5). Job persistence
    /// is the last commit step. No raw secret
    /// ever enters jobs.json and a failed commit preserves previous authorization.
    pub fn upsert_with_archive_key(
        &self,
        mut job: Job,
        portable_key: SealedPrivateKey,
        private: &JobPrivateKey,
        keys: &ServiceKeyStore,
    ) -> Result<Job> {
        if job.id.is_empty() {
            job.id = portable_key.job_id.clone();
        }
        if job.id != portable_key.job_id {
            return Err(OwnBackupError::Config(
                tr!("backup_encryption.key_other_job").into_owned(),
            ));
        }
        job.image_format = own_backup_common::types::ImageFormat::Qcow2;
        let job_id = job.id.clone();
        let expected_revision = job.updated_at.clone();
        keys.authorize_and_commit(&job_id, &portable_key, private, |local_key| {
            job.archive_encryption = Some(ArchiveEncryption {
                portable_key: portable_key.clone(),
                local_key,
            });
            self.upsert_inner_checked(job, true, Some(&expected_revision))
                .map_err(OwnBackupError::Io)
        })
    }

    /// Retire the local authorization of a job that has just been removed.
    ///
    /// Call it only once the job is durably gone: while it is still in
    /// `jobs.json` the reference is live, and revoking first would leave a
    /// scheduled job pointing at a key that is not there. Nothing here can cost
    /// access to the archive — the password and the recovery key live in the
    /// images, and this only takes away this machine's ability to open them
    /// without either.
    ///
    /// Reported, never fatal: the job is already deleted, and refusing to
    /// finish that because one leftover file resisted would be worse than the
    /// leftover.
    pub fn retire_local_archive_key(&self, job: &Job) {
        let Some(encryption) = &job.archive_encryption else {
            return;
        };
        let outcome = self
            .service_key_store()
            .and_then(|keys| keys.revoke(&encryption.local_key));
        match outcome {
            Ok(()) => tracing::info!(
                job = %job.id,
                key_id = %encryption.local_key.id(),
                "local archive authorization retired with the job"
            ),
            Err(e) => tracing::warn!(
                job = %job.id,
                key_id = %encryption.local_key.id(),
                error = %e,
                "local archive authorization outlived its job — it is unreferenced \
                 and can be deleted by hand from the service key store"
            ),
        }
    }

    /// Drop local authorizations no job refers to any more.
    ///
    /// The per-deletion path above is the one that should do this work. This is
    /// what catches the rest: jobs deleted by a build that predates it, a crash
    /// between writing the blob and publishing the job, and a `jobs.json`
    /// restored from an older copy.
    ///
    /// Returns how many were retired. Anything that is not a live job's key is
    /// removed, so this must run where the whole job list is known.
    ///
    /// An EMPTY list is refused, and that is the important part: a `jobs.json`
    /// that could not be parsed is moved aside and the store starts empty, on
    /// purpose, so the jobs can be put back by hand. Sweeping against that
    /// would answer "no job refers to anything" and retire every
    /// authorization on the machine — precisely when the person is in the
    /// middle of recovering them. A leftover file in a SYSTEM-only directory
    /// is the cheaper mistake.
    pub fn sweep_local_archive_keys(&self) -> Result<usize> {
        let jobs = self.list();
        if jobs.is_empty() {
            return Ok(0);
        }
        let gone = self
            .service_key_store()?
            .retain(&referenced_local_keys(&jobs))?;
        for id in &gone {
            tracing::info!(key_id = %id, "unreferenced local archive authorization retired");
        }
        Ok(gone.len())
    }

    /// Opens the service authorization from disk on every run, including after
    /// service/OS restart. Manual mount/restore must not call this method.
    pub(crate) fn scheduled_archive_key(&self, job: &Job) -> Result<Option<JobPrivateKey>> {
        let Some(encryption) = &job.archive_encryption else {
            return Ok(None);
        };
        let data_dir = self.jobs_path.parent().ok_or_else(|| {
            OwnBackupError::Config(tr!("backup_encryption.no_state_dir").into_owned())
        })?;
        let keys = ServiceKeyStore::new(data_dir).map_err(|e| match e {
            OwnBackupError::KeyProtection(_) => e,
            e => OwnBackupError::Encryption(
                tr!("backup_encryption.reauthorize_error", error = e).into_owned(),
            ),
        })?;
        keys.open(&job.id, &encryption.portable_key, &encryption.local_key)
            .map(Some)
    }

    /// The keys this run works with, opened once before it starts (§7.4).
    ///
    /// `Ok(None)` means the job is not encrypted. An error means the local
    /// authorization is missing, damaged or unreadable — the run then fails
    /// with that message instead of waiting for a password or, worse, writing
    /// the copy in the clear.
    pub(crate) fn archive_keys_for_run(
        &self,
        job: &Job,
    ) -> std::result::Result<Option<ArchiveKeys>, RunKeyError> {
        let Some(encryption) = &job.archive_encryption else {
            return Ok(None);
        };
        let private = self
            .scheduled_archive_key(job)
            .map_err(|e| RunKeyError::from_open(&e))?
            .ok_or_else(|| RunKeyError {
                message: tr!("backup_encryption.reauthorize").into_owned(),
                kind: ERROR_KIND_REAUTHORIZE,
            })?;
        ArchiveKeys::from_private(encryption.portable_key.clone(), private)
            .map(Some)
            .map_err(|e| RunKeyError::from_open(&e))
    }

    /// The message a run dies with when its keys cannot be opened.
    #[cfg(test)]
    pub(crate) fn archive_encryption_run_error(&self, job: &Job) -> Option<String> {
        self.archive_keys_for_run(job).err().map(|e| e.message)
    }
}

fn unlock_existing_archive(
    job: &Job,
    password: Option<&str>,
    recovery_key: Option<&str>,
) -> Result<(SealedPrivateKey, JobPrivateKey)> {
    let encryption = job.archive_encryption.as_ref().ok_or_else(|| {
        OwnBackupError::Config(tr!("archive_setup.reauthorize_not_encrypted").into_owned())
    })?;
    let portable = encryption.portable_key.clone();
    let private = match (password, recovery_key) {
        (Some(password), None) => portable.unseal_with_password(password)?,
        (None, Some(typed)) => {
            let recovery = own_backup_backup::seal::recovery::RecoveryKey::parse(typed)?;
            portable.unseal_with_recovery(&recovery)?
        }
        _ => {
            return Err(OwnBackupError::Config(
                tr!("archive_setup.reauthorize_one_secret").into_owned(),
            ))
        }
    };
    Ok((portable, private))
}

/// Every local key the live jobs name.
///
/// Its own function because a sweep deletes whatever is NOT in it: a job missed
/// here is a scheduled job that stops overnight and asks a person for a
/// password the schedule must never ask for. Encrypted or not, present or not —
/// the answer has to come from the whole list every time.
fn referenced_local_keys(jobs: &[Job]) -> std::collections::BTreeSet<uuid::Uuid> {
    jobs.iter()
        .filter_map(|job| job.archive_encryption.as_ref())
        .map(|e| e.local_key.id())
        .collect()
}

pub(crate) fn validate_update(
    job: &mut Job,
    previous: Option<&Job>,
    authorized: bool,
) -> std::io::Result<()> {
    fn error(message: impl Into<String>) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
    }
    let old = previous.and_then(|job| job.archive_encryption.as_ref());
    // Older clients know nothing about this field: editing another setting
    // must not silently turn an encrypted job into a plaintext job.
    if !authorized && job.archive_encryption.is_none() {
        job.archive_encryption = old.cloned();
    }
    if !authorized && job.archive_encryption.as_ref() != old {
        return Err(error(tr!("backup_encryption.only_via_key_service")));
    }
    if let Some(encryption) = &job.archive_encryption {
        encryption
            .portable_key
            .validate()
            .map_err(|e| error(e.to_string()))?;
        if encryption.portable_key.job_id != job.id {
            return Err(error(tr!("backup_encryption.portable_key_mismatch")));
        }
        if job.image_format != own_backup_common::types::ImageFormat::Qcow2 {
            return Err(error(tr!("backup_encryption.qcow2_only")));
        }
        if let Some(old) = old {
            if old.portable_key != encryption.portable_key {
                return Err(error(tr!("backup_encryption.key_replace_separate")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_backup::seal::keypair::{
        JobPublicKey, KdfParams, Slot, SlotKind, KEY_FILE_FORMAT,
    };
    use own_backup_backup::seal::SCHEME;

    fn portable_job() -> (Job, own_backup_backup::seal::recovery::RecoveryKey) {
        use own_backup_backup::seal::keypair::JobKeyPair;
        let pair = JobKeyPair::generate().unwrap();
        let recovery = own_backup_backup::seal::recovery::RecoveryKey::generate().unwrap();
        let portable = SealedPrivateKey::seal(
            "repair-job",
            &pair,
            "archive repair password",
            &recovery,
            &KdfParams {
                algorithm: "argon2id".into(),
                m_kib: 64 * 1024,
                t: 3,
                p: 1,
            },
        )
        .unwrap();
        let reference =
            serde_json::from_value(serde_json::json!({"id":uuid::Uuid::new_v4()})).unwrap();
        (
            Job {
                id: "repair-job".into(),
                name: "repair".into(),
                image_format: own_backup_common::types::ImageFormat::Qcow2,
                archive_encryption: Some(ArchiveEncryption {
                    portable_key: portable,
                    local_key: reference,
                }),
                ..Default::default()
            },
            recovery,
        )
    }

    #[test]
    fn reauthorization_unlocks_the_same_portable_identity_by_either_secret() {
        let (job, recovery) = portable_job();
        let original = &job.archive_encryption.as_ref().unwrap().portable_key;
        for (password, key) in [
            (Some("archive repair password"), None),
            (None, Some(recovery.to_string_grouped().as_str())),
        ] {
            let (portable, private) = unlock_existing_archive(&job, password, key).unwrap();
            assert_eq!(&portable, original);
            assert_eq!(private.public(), original.public_key().unwrap());
        }
    }

    #[test]
    fn refused_reauthorization_does_not_rewrite_the_job_or_local_reference() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let (job, recovery) = portable_job();
        let job = store.upsert_inner(job, true).unwrap();
        let before = std::fs::read(dir.path().join("jobs.json")).unwrap();
        let key = recovery.to_string_grouped();
        for (password, key) in [
            (None, None),
            (Some("wrong password"), None),
            (None, Some("invalid recovery")),
            (Some("archive repair password"), Some(key.as_str())),
        ] {
            assert!(store
                .reauthorize_archive(job.clone(), password, key)
                .is_err());
            assert_eq!(std::fs::read(dir.path().join("jobs.json")).unwrap(), before);
        }
        assert!(
            unlock_existing_archive(&Job::default(), Some("archive repair password"), None)
                .is_err()
        );
    }

    #[test]
    fn stale_reauthorization_cannot_undo_an_edit_or_recreate_a_deleted_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let (job, _) = portable_job();
        let saved = store.upsert_inner(job, true).unwrap();
        let mut edit = saved.clone();
        edit.name = "new name".into();
        store.upsert(edit).unwrap();
        let mut repaired = saved.clone();
        repaired.archive_encryption.as_mut().unwrap().local_key =
            serde_json::from_value(serde_json::json!({"id":uuid::Uuid::new_v4()})).unwrap();
        assert!(store
            .upsert_inner_checked(repaired.clone(), true, Some(&saved.updated_at))
            .is_err());
        assert_eq!(store.get(&saved.id).unwrap().name, "new name");
        store.delete(&saved.id).unwrap();
        assert!(store
            .upsert_inner_checked(repaired, true, Some(&saved.updated_at))
            .is_err());
        assert!(store.get(&saved.id).is_none());
    }

    /// What the sweep is allowed to keep, from a list that mixes the three
    /// shapes a real `jobs.json` holds: encrypted, plain, and an encrypted one
    /// whose copy of the key differs.
    #[test]
    fn the_keep_set_names_every_encrypted_jobs_key_and_nothing_else() {
        let a = encrypted_job();
        let mut b = encrypted_job();
        b.id = "second".into();
        b.archive_encryption.as_mut().unwrap().portable_key.job_id = "second".into();
        b.archive_encryption.as_mut().unwrap().local_key =
            serde_json::from_value(serde_json::json!({ "id": uuid::Uuid::new_v4() })).unwrap();
        let plain = Job {
            id: "plain".into(),
            ..Default::default()
        };

        let keep = referenced_local_keys(&[a.clone(), plain, b.clone()]);
        assert_eq!(
            keep.len(),
            2,
            "one per encrypted job, and the plain job adds none"
        );
        for job in [&a, &b] {
            let id = job.archive_encryption.as_ref().unwrap().local_key.id();
            assert!(keep.contains(&id), "job {} kept its key", job.id);
        }
        assert!(referenced_local_keys(&[]).is_empty());
    }

    /// An empty job list stops the sweep before it reaches the key store —
    /// that is the whole guard, and the way to see it is that the store's own
    /// error never gets a chance to appear.
    #[test]
    fn an_empty_job_list_sweeps_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        // An actual store is available to SYSTEM on Windows. Force a storage
        // error instead of assuming that the platform/context cannot open it.
        let blocked = dir
            .path()
            .join(own_backup_backup::seal::local::KEY_DIRECTORY_NAME);
        std::fs::write(&blocked, b"not a key directory").unwrap();
        let store = JobStore::load(dir.path());
        assert_eq!(store.sweep_local_archive_keys().unwrap(), 0);
        assert_eq!(std::fs::read(&blocked).unwrap(), b"not a key directory");

        store
            .upsert(Job {
                name: "nightly".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(
            store.sweep_local_archive_keys().is_err(),
            "with a job present it does reach the deliberately blocked key store"
        );
    }

    fn encrypted_job() -> Job {
        let public = JobPublicKey::from_bytes([0; 32]);
        let slot = |kind| Slot {
            kind,
            kdf: KdfParams::strong(),
            salt: format!("{}==", "A".repeat(22)),
            nonce: "A".repeat(16),
            sealed: "A".repeat(64),
        };
        let portable_key = SealedPrivateKey {
            format: KEY_FILE_FORMAT.into(),
            scheme: SCHEME.into(),
            job_id: "job".into(),
            public: format!("{}=", "A".repeat(43)),
            fingerprint: public.fingerprint(),
            slots: vec![slot(SlotKind::Password), slot(SlotKind::Recovery)],
        };
        let local_key = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4()
        }))
        .unwrap();
        Job {
            id: "job".into(),
            name: "Encrypted".into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            archive_encryption: Some(ArchiveEncryption {
                portable_key,
                local_key,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn ordinary_save_cannot_introduce_or_replace_encryption() {
        let temp = tempfile::tempdir().unwrap();
        let store = JobStore::load(temp.path());
        let job = encrypted_job();
        assert!(store.upsert(job.clone()).is_err());
        assert!(store.list().is_empty());
        // Internal fixture, not a real DPAPI authorization.
        let saved = store.upsert_inner(job.clone(), true).unwrap();
        let mut forged = saved.clone();
        forged.archive_encryption = encrypted_job().archive_encryption;
        assert!(store.upsert(forged).is_err());
        let mut replaced = saved.clone();
        replaced
            .archive_encryption
            .as_mut()
            .unwrap()
            .portable_key
            .job_id = "other".into();
        assert!(store.upsert_inner(replaced, true).is_err());
        assert_eq!(
            store.get("job").unwrap().archive_encryption,
            saved.archive_encryption
        );
    }

    #[test]
    fn an_old_client_cannot_drop_encryption_by_omitting_the_field() {
        let temp = tempfile::tempdir().unwrap();
        let store = JobStore::load(temp.path());
        let saved = store.upsert_inner(encrypted_job(), true).unwrap();
        let mut old_client = saved.clone();
        old_client.archive_encryption = None;
        old_client.name = "Renamed".into();
        let updated = store.upsert(old_client).unwrap();
        assert_eq!(updated.archive_encryption, saved.archive_encryption);
        assert_eq!(
            JobStore::load(temp.path())
                .get("job")
                .unwrap()
                .archive_encryption,
            saved.archive_encryption
        );
        let mut wrong_format = updated;
        wrong_format.image_format = own_backup_common::types::ImageFormat::Vhdx;
        assert!(store.upsert(wrong_format).is_err());
    }

    #[test]
    fn encrypted_jobs_fail_before_the_plaintext_writer() {
        let temp = tempfile::tempdir().unwrap();
        let store = JobStore::load(temp.path());
        assert!(store
            .archive_encryption_run_error(&Job::default())
            .is_none());
        let message = store
            .archive_encryption_run_error(&encrypted_job())
            .unwrap();
        assert!(message.contains("автоматическое выполнение"), "{message}");
        assert!(!temp.path().join("jobs.json").exists());
    }

    /// The kind of a key failure follows the error's variant: the machine's
    /// key protection failing is not "authorize again", whatever the words.
    #[test]
    fn a_key_failure_is_classified_by_its_variant_not_its_words() {
        let tpm = RunKeyError::from_open(&OwnBackupError::KeyProtection("TPM2".into()));
        assert_eq!(tpm.kind, ERROR_KIND_KEY_PROTECTION);
        assert!(tpm.message.contains("TPM2"));
        // Worded like a TPM failure, but a lost authorization by its kind.
        let gone =
            RunKeyError::from_open(&OwnBackupError::Encryption("TPM2 device not found".into()));
        assert_eq!(gone.kind, ERROR_KIND_REAUTHORIZE);
        // The kind travels with the run, and only when there is one.
        let run = crate::JobRun {
            error_kind: ERROR_KIND_KEY_PROTECTION.into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&run).unwrap();
        assert_eq!(json["error_kind"], "key_protection");
        let plain = serde_json::to_value(crate::JobRun::default()).unwrap();
        assert!(plain.get("error_kind").is_none());
    }

    #[tokio::test]
    async fn queued_encrypted_job_records_failure_without_creating_an_image() {
        use std::sync::{atomic::AtomicBool, Arc, Mutex};
        struct Sink(Mutex<Vec<crate::JobRun>>);
        impl crate::EventSink for Sink {
            fn job_progress(&self, _: crate::JobProgress) {}
            fn queue_changed(&self, _: crate::QueueSnapshot) {}
            fn job_finished(&self, run: crate::JobRun) {
                self.0.lock().unwrap().push(run);
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let store = JobStore::load(temp.path());
        let mut job = encrypted_job();
        job.dest_folder = temp.path().join("images").to_string_lossy().into_owned();
        let job = store.upsert_inner(job, true).unwrap();
        let queue = crate::RunQueue::new();
        queue.enqueue(&job.id);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(job.id.clone()).unwrap();
        let retry_tx = tx.downgrade();
        drop(tx);
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        crate::run_queue_worker(
            sink.clone(),
            store.clone(),
            queue.clone(),
            crate::busy::Busy::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(crate::NoPasswords),
            rx,
            retry_tx,
            stop_rx,
        )
        .await;
        let runs = sink.0.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "error");
        // Which of the two depends on this machine's key protection; either
        // way the run says it by kind, not only in words.
        assert!(
            [ERROR_KIND_REAUTHORIZE, ERROR_KIND_KEY_PROTECTION]
                .contains(&runs[0].error_kind.as_str()),
            "{:?}",
            runs[0].error_kind
        );
        assert!(runs[0].image_path.is_empty());
        assert!(!temp.path().join("images").exists());
        assert!(queue.snapshot().current.is_none());
        assert_eq!(store.get("job").unwrap().last_run.unwrap().status, "error");
    }
}
