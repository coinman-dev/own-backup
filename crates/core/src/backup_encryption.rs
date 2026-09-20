//! Job-side transaction boundary for encrypted archive authorization.
//!
//! The encryption wizard/RPC arrives in E5. Ordinary SaveJob cannot introduce,
//! replace or clear authorization. E4 must replace the fail-closed run guard
//! with a key context threaded through the actual encrypted image pipeline.

use own_backup_backup::seal::keypair::{JobPrivateKey, SealedPrivateKey};
use own_backup_backup::seal::local::{LocalKeyRef, ServiceKeyStore};
use own_backup_common::{OwnBackupError, Result};
use serde::{Deserialize, Serialize};

use crate::{Job, JobStore};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveEncryption {
    pub portable_key: SealedPrivateKey,
    pub local_key: LocalKeyRef,
}

impl JobStore {
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
                "ключ принадлежит другому заданию".into(),
            ));
        }
        job.image_format = own_backup_common::types::ImageFormat::Qcow2;
        let job_id = job.id.clone();
        keys.authorize_and_commit(&job_id, &portable_key, private, |local_key| {
            job.archive_encryption = Some(ArchiveEncryption {
                portable_key: portable_key.clone(),
                local_key,
            });
            self.upsert_inner(job, true).map_err(OwnBackupError::Io)
        })
    }

    /// Opens the service authorization from disk on every run, including after
    /// service/OS restart. Manual mount/restore must not call this method.
    pub(crate) fn scheduled_archive_key(&self, job: &Job) -> Result<Option<JobPrivateKey>> {
        let Some(encryption) = &job.archive_encryption else {
            return Ok(None);
        };
        let data_dir = self
            .jobs_path
            .parent()
            .ok_or_else(|| OwnBackupError::Config("нет каталога состояния службы".into()))?;
        let keys = ServiceKeyStore::new(data_dir).map_err(|e| {
            OwnBackupError::Encryption(format!(
                "Нужно заново разрешить автоматическое выполнение задания: {e}"
            ))
        })?;
        keys.open(&job.id, &encryption.portable_key, &encryption.local_key)
            .map(Some)
    }

    /// Temporary until E3/E4. A saved encryption configuration is not permission
    /// to produce a plaintext image while the encrypted pipeline is unfinished.
    pub(crate) fn archive_encryption_run_error(&self, job: &Job) -> Option<String> {
        match self.scheduled_archive_key(job) {
            Ok(None) => None,
            Ok(Some(_key)) => Some(
                "Ключ задания доступен, но запись зашифрованных образов ещё не подключена. Копия не создавалась.".into()
            ),
            Err(e) => Some(e.to_string()),
        }
    }
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
        return Err(error(
            "разрешение шифрования изменяется только через службу ключей",
        ));
    }
    if let Some(encryption) = &job.archive_encryption {
        encryption
            .portable_key
            .validate()
            .map_err(|e| error(e.to_string()))?;
        if encryption.portable_key.job_id != job.id {
            return Err(error("переносимый ключ не соответствует заданию"));
        }
        if job.image_format != own_backup_common::types::ImageFormat::Qcow2 {
            return Err(error("шифрование поддерживается только для формата qcow2"));
        }
        if let Some(old) = old {
            if old.portable_key != encryption.portable_key {
                return Err(error(
                    "замена ключа существующего задания требует отдельного процесса",
                ));
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
        drop(tx);
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        crate::run_queue_worker(
            sink.clone(),
            store.clone(),
            queue.clone(),
            crate::busy::Busy::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(crate::NoPasswords),
            rx,
        )
        .await;
        let runs = sink.0.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "error");
        assert!(runs[0].image_path.is_empty());
        assert!(!temp.path().join("images").exists());
        assert!(queue.snapshot().current.is_none());
        assert_eq!(store.get("job").unwrap().last_run.unwrap().status, "error");
    }
}
