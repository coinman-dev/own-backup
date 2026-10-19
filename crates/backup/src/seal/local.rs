//! Local authorization for unattended encrypted jobs.
//!
//! This store contains PRIVATE job keys, never passwords: DPAPI-protected on
//! Windows, sealed under a `systemd-creds`-wrapped machine key on Linux. Its
//! files must not be exported or included in backup images (the engine
//! excludes the directory from every copy). Portable password and recovery
//! slots remain in keypair.rs.
//!
//! New authorizations are immutable. Save + flush + reopen succeeds before the
//! caller commits a job referring to the new key. A failed commit removes only
//! that new authorization; an existing reference remains usable.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use own_backup_common::{tr, OwnBackupError, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::keypair::{JobPrivateKey, JobPublicKey, SealedPrivateKey};
use super::{bad, SCHEME};

#[cfg(target_os = "windows")]
#[path = "local_windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "local_linux.rs"]
mod platform;

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub use platform::PrivateSecretDatabase;

#[cfg(target_os = "linux")]
pub use platform::{KeyProtection, MACHINE_KEY_FILE};

pub const KEY_DIRECTORY_NAME: &str = ".own-backup-service-keys";
pub(super) const MAX_LOCAL_BLOB: usize = 16 * 1024;
const MAGIC: &[u8; 8] = b"OBKDP\0\x01\0";

/// A reference, not the DPAPI blob or a file path. Safe in jobs.json/export.
/// A fresh reference is assigned on reauthorization; old files are not replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalKeyRef {
    id: Uuid,
}

impl LocalKeyRef {
    pub fn id(&self) -> Uuid {
        self.id
    }
}

/// Platform-independent transaction logic; the real backends are Windows
/// (DPAPI) and Linux (`systemd-creds`).
pub struct ServiceKeyStore {
    backend: Box<dyn Backend>,
    transaction: Mutex<()>,
}

pub(super) trait Backend: Send + Sync {
    fn directory(&self) -> &Path;
    fn protect(&self, entropy: &[u8], key: &[u8; 32]) -> Result<Vec<u8>>;
    fn unprotect(&self, entropy: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>>;
    fn write_new(&self, id: Uuid, blob: &[u8]) -> Result<()>;
    fn read(&self, id: Uuid) -> Result<Vec<u8>>;
    fn remove(&self, id: Uuid) -> Result<()>;
    /// Every key this store holds, whether or not a job still refers to it.
    fn list_ids(&self) -> Result<Vec<Uuid>>;
}

impl ServiceKeyStore {
    /// Must be called by the installed service, as LocalSystem and without a
    /// thread impersonation token. Foreground GUI/CLI contexts are refused.
    pub fn new(data_dir: &Path) -> Result<Self> {
        if own_backup_common::media_mode::is_media_mode() {
            return Err(bad(tr!("seal_local.media_keeps_no_access")));
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self {
                backend: Box::new(platform::WindowsBackend::new(data_dir)?),
                transaction: Mutex::new(()),
            })
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                backend: Box::new(platform::LinuxBackend::new(data_dir)?),
                transaction: Mutex::new(()),
            })
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = data_dir;
            Err(OwnBackupError::NotImplemented(
                tr!("seal_local.store_windows_only").into_owned(),
            ))
        }
    }

    pub fn directory(&self) -> PathBuf {
        self.backend.directory().to_path_buf()
    }

    /// What keeps this store's keys on this machine, for the window and the
    /// log: `dpapi` on Windows; `tpm2` or `host` on Linux (`systemd-creds`).
    /// An error says why there is no store.
    pub fn protection(data_dir: &Path) -> Result<&'static str> {
        if own_backup_common::media_mode::is_media_mode() {
            return Err(bad(tr!("seal_local.media_keeps_no_access")));
        }
        #[cfg(target_os = "windows")]
        {
            let _ = data_dir;
            Ok("dpapi")
        }
        #[cfg(target_os = "linux")]
        {
            platform::protection(data_dir).map(KeyProtection::as_str)
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = data_dir;
            Err(OwnBackupError::NotImplemented(
                tr!("seal_local.store_windows_only").into_owned(),
            ))
        }
    }

    /// The caller has already unlocked/created the portable key, and must have
    /// confirmed recovery-key delivery before making the job runnable.
    ///
    /// commit must publish the job only after its own atomic durable write;
    /// returning Err promises it published nothing. A crash before commit can
    /// leave an unreferenced protected file, never an enabled unprotected job.
    pub fn authorize_and_commit<T>(
        &self,
        job_id: &str,
        portable: &SealedPrivateKey,
        private: &JobPrivateKey,
        commit: impl FnOnce(LocalKeyRef) -> Result<T>,
    ) -> Result<T> {
        let _guard = self
            .transaction
            .lock()
            .map_err(|_| bad("key-store transaction is poisoned"))?;
        let public = identity(job_id, portable)?;
        if private.public() != public {
            return Err(bad(tr!("seal_local.key_mismatch")));
        }
        let reference = LocalKeyRef { id: Uuid::new_v4() };
        let entropy = entropy(job_id, &public, &reference);
        let protected = self.backend.protect(&entropy, private.storage_bytes())?;
        if protected.is_empty() || protected.len() > MAX_LOCAL_BLOB - MAGIC.len() {
            return Err(bad(tr!("seal_local.bad_wrap_size")));
        }
        let mut blob = Vec::with_capacity(MAGIC.len() + protected.len());
        blob.extend_from_slice(MAGIC);
        blob.extend_from_slice(&protected);
        self.backend.write_new(reference.id, &blob)?;
        // Probe the bytes actually on disk, not the in-memory ciphertext.
        let result = self
            .open(job_id, portable, &reference)
            .and_then(|_| commit(reference.clone()));
        if result.is_err() {
            if let Err(e) = self.backend.remove(reference.id) {
                tracing::warn!(key_id = %reference.id, error = %e,
                    "uncommitted protected key could not be removed; no job was authorized");
            }
        }
        result
    }

    /// Scheduled work uses this; manual mount/restore must use portable slots.
    pub fn open(
        &self,
        job_id: &str,
        portable: &SealedPrivateKey,
        reference: &LocalKeyRef,
    ) -> Result<JobPrivateKey> {
        self.open_inner(job_id, portable, reference).map_err(|e| {
            OwnBackupError::Encryption(tr!("seal_local.reauthorize_job", error = e).into_owned())
        })
    }

    fn open_inner(
        &self,
        job_id: &str,
        portable: &SealedPrivateKey,
        reference: &LocalKeyRef,
    ) -> Result<JobPrivateKey> {
        let public = identity(job_id, portable)?;
        let blob = self.backend.read(reference.id)?;
        if blob.len() <= MAGIC.len() || blob.len() > MAX_LOCAL_BLOB || &blob[..MAGIC.len()] != MAGIC
        {
            return Err(bad(tr!("seal_local.bad_format")));
        }
        let secret = self
            .backend
            .unprotect(&entropy(job_id, &public, reference), &blob[MAGIC.len()..])?;
        if secret.len() != 32 {
            return Err(bad(tr!("seal_local.bad_secret_length")));
        }
        let mut bytes = Zeroizing::new([0; 32]);
        bytes.copy_from_slice(&secret);
        let private = JobPrivateKey::from_bytes(*bytes);
        if private.public() != public {
            return Err(bad(tr!("seal_local.local_key_other_job")));
        }
        Ok(private)
    }

    /// Revoke only after removing/replacing the job reference durably. This
    /// cannot destroy password/recovery access: portable slots are untouched.
    pub fn revoke(&self, reference: &LocalKeyRef) -> Result<()> {
        let _guard = self
            .transaction
            .lock()
            .map_err(|_| bad("key-store transaction is poisoned"))?;
        self.backend.remove(reference.id)
    }

    /// Drop every key no live job refers to, and say which ones went.
    ///
    /// A blob that outlives its job is exactly what this store exists to
    /// prevent someone having: it lets THIS machine's service open an archive
    /// with no password, and nothing left on disk accounts for it. They appear
    /// from an older build that deleted jobs without revoking, and from a crash
    /// between writing the blob and publishing the job.
    ///
    /// `keep` must be the whole live set. Passing a partial list deletes
    /// authorizations that are still in use — which costs no data (the portable
    /// slots are untouched) but does make the affected jobs ask for the
    /// password again.
    pub fn retain(&self, keep: &std::collections::BTreeSet<Uuid>) -> Result<Vec<Uuid>> {
        let _guard = self
            .transaction
            .lock()
            .map_err(|_| bad("key-store transaction is poisoned"))?;
        let mut gone = Vec::new();
        for id in self.backend.list_ids()? {
            if keep.contains(&id) {
                continue;
            }
            self.backend.remove(id)?;
            gone.push(id);
        }
        Ok(gone)
    }
}

fn identity(job_id: &str, portable: &SealedPrivateKey) -> Result<JobPublicKey> {
    portable.validate()?;
    if portable.job_id != job_id {
        return Err(bad(tr!("seal_local.portable_key_other_job")));
    }
    portable.public_key()
}

fn entropy(job_id: &str, public: &JobPublicKey, reference: &LocalKeyRef) -> Vec<u8> {
    let mut out = format!("{SCHEME}/local-service-key/1").into_bytes();
    out.extend_from_slice(reference.id.as_bytes());
    out.extend_from_slice(&(job_id.len() as u16).to_be_bytes());
    out.extend_from_slice(job_id.as_bytes());
    out.extend_from_slice(public.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::keypair::{JobKeyPair, KdfParams};
    use crate::seal::recovery::RecoveryKey;
    use crate::seal::{aead_open, aead_seal, fill_random};
    use std::collections::HashMap;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[derive(Default)]
    struct Disk {
        files: Mutex<HashMap<Uuid, Vec<u8>>>,
        fail_write: AtomicBool,
        corrupt_write: AtomicBool,
    }
    struct TestBackend {
        disk: Arc<Disk>,
        device_key: Zeroizing<[u8; 32]>,
    }
    impl Backend for TestBackend {
        fn directory(&self) -> &Path {
            Path::new("test-store")
        }
        fn protect(&self, entropy: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
            let mut nonce = [0; 12];
            fill_random(&mut nonce)?;
            let mut out = nonce.to_vec();
            out.extend_from_slice(&aead_seal(&self.device_key, &nonce, entropy, key)?);
            Ok(out)
        }
        fn unprotect(&self, entropy: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
            if blob.len() != 60 {
                return Err(bad("bad test blob"));
            }
            aead_open(
                &self.device_key,
                blob[..12].try_into().unwrap(),
                entropy,
                &blob[12..],
            )
        }
        fn write_new(&self, id: Uuid, blob: &[u8]) -> Result<()> {
            if self.disk.fail_write.load(Ordering::SeqCst) {
                return Err(bad("injected write failure"));
            }
            let mut files = self.disk.files.lock().unwrap();
            if files.contains_key(&id) {
                return Err(bad("already exists"));
            }
            let mut copy = blob.to_vec();
            if self.disk.corrupt_write.load(Ordering::SeqCst) {
                *copy.last_mut().unwrap() ^= 1;
            }
            files.insert(id, copy);
            Ok(())
        }
        fn read(&self, id: Uuid) -> Result<Vec<u8>> {
            self.disk
                .files
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or_else(|| bad("missing key"))
        }
        fn remove(&self, id: Uuid) -> Result<()> {
            self.disk.files.lock().unwrap().remove(&id);
            Ok(())
        }
        fn list_ids(&self) -> Result<Vec<Uuid>> {
            Ok(self.disk.files.lock().unwrap().keys().copied().collect())
        }
    }
    fn store(disk: &Arc<Disk>, machine: u8) -> ServiceKeyStore {
        ServiceKeyStore {
            backend: Box::new(TestBackend {
                disk: disk.clone(),
                device_key: Zeroizing::new([machine; 32]),
            }),
            transaction: Mutex::new(()),
        }
    }
    fn portable() -> (JobKeyPair, RecoveryKey, SealedPrivateKey) {
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let key = SealedPrivateKey::seal(
            "job",
            &pair,
            "password",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        (pair, recovery, key)
    }

    #[test]
    fn reopening_needs_no_password_and_portable_json_has_no_local_blob() {
        let disk = Arc::new(Disk::default());
        let (pair, recovery, key) = portable();
        let reference = store(&disk, 1)
            .authorize_and_commit("job", &key, &pair.private, Ok)
            .unwrap();
        drop(pair);
        let reopened = store(&disk, 1);
        assert_eq!(
            reopened.open("job", &key, &reference).unwrap().public(),
            key.public_key().unwrap()
        );
        assert!(store(&disk, 2).open("job", &key, &reference).is_err());
        assert!(reopened.open("other-job", &key, &reference).is_err());
        let encoded = key.to_json().unwrap();
        assert!(!encoded.contains("OBKDP"));
        assert!(!encoded.contains(&reference.id.to_string()));
        reopened.revoke(&reference).unwrap();
        assert!(reopened.open("job", &key, &reference).is_err());
        key.unseal_with_password("password").unwrap();
        key.unseal_with_recovery(&recovery).unwrap();
    }

    #[test]
    fn failures_before_commit_never_publish_a_job_or_destroy_old_authorization() {
        let disk = Arc::new(Disk::default());
        let (pair, _, key) = portable();
        let keys = store(&disk, 1);
        let old = keys
            .authorize_and_commit("job", &key, &pair.private, Ok)
            .unwrap();
        let failed: Result<()> =
            keys.authorize_and_commit("job", &key, &pair.private, |_| Err(bad("job write failed")));
        assert!(failed.is_err());
        assert_eq!(disk.files.lock().unwrap().len(), 1);
        keys.open("job", &key, &old).unwrap();
        for corrupt in [false, true] {
            disk.fail_write.store(!corrupt, Ordering::SeqCst);
            disk.corrupt_write.store(corrupt, Ordering::SeqCst);
            let called = AtomicBool::new(false);
            assert!(keys
                .authorize_and_commit("job", &key, &pair.private, |_| {
                    called.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .is_err());
            assert!(!called.load(Ordering::SeqCst));
            assert_eq!(disk.files.lock().unwrap().len(), 1);
            keys.open("job", &key, &old).unwrap();
        }
    }

    #[test]
    fn swapping_blob_reference_recipient_and_framing_is_refused() {
        let disk = Arc::new(Disk::default());
        let (pair, _, key) = portable();
        let keys = store(&disk, 1);
        let a = keys
            .authorize_and_commit("job", &key, &pair.private, Ok)
            .unwrap();
        let b = keys
            .authorize_and_commit("job", &key, &pair.private, Ok)
            .unwrap();
        let saved = keys.backend.read(a.id).unwrap();
        disk.files.lock().unwrap().insert(b.id, saved.clone());
        assert!(keys.open("job", &key, &b).is_err());
        let other = JobKeyPair::generate().unwrap();
        assert!(keys
            .authorize_and_commit("job", &key, &other.private, Ok)
            .is_err());
        for bytes in [Vec::new(), vec![0; 8], vec![0; MAX_LOCAL_BLOB + 1]] {
            disk.files.lock().unwrap().insert(a.id, bytes);
            assert!(keys.open("job", &key, &a).is_err());
        }
        disk.files.lock().unwrap().insert(a.id, saved);
        keys.open("job", &key, &a).unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    /// A key the live set does not name is retired; one it names is not. The
    /// second half matters as much as the first: a sweep that took a live
    /// authorization with it would make a scheduled job stop overnight and ask
    /// a person for a password it must never ask for.
    #[test]
    fn a_sweep_keeps_the_keys_jobs_still_name_and_drops_the_rest() {
        let disk = Arc::new(Disk::default());
        let (live_pair, _, live_key) = portable();
        let (dead_pair, _, dead_key) = portable();
        let keys = store(&disk, 1);
        let live = keys
            .authorize_and_commit("job", &live_key, &live_pair.private, Ok)
            .unwrap();
        let dead = keys
            .authorize_and_commit("job", &dead_key, &dead_pair.private, Ok)
            .unwrap();

        let keep = std::collections::BTreeSet::from([live.id]);
        let gone = keys.retain(&keep).unwrap();
        assert_eq!(gone, vec![dead.id]);

        assert!(keys.open("job", &live_key, &live).is_ok());
        assert!(keys.open("job", &dead_key, &dead).is_err());
        // Nothing here can cost access to the archive itself.
        dead_key.unseal_with_password("password").unwrap();

        // And it is idempotent: the second pass finds nothing left to do.
        assert!(keys.retain(&keep).unwrap().is_empty());
    }

    #[test]
    fn no_unprotected_non_windows_fallback() {
        assert!(ServiceKeyStore::new(Path::new("unused")).is_err());
    }

    /// The whole transaction over the real Linux backend (with a stand-in for
    /// `systemd-creds`): authorize, reopen from disk with no password, refuse
    /// another machine's key, revoke, sweep.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_backend_carries_the_whole_transaction() {
        use super::platform::tests::{backend, data_dir, FakeCreds};
        let data = data_dir();
        let creds = FakeCreds::new(1);
        let open = |creds: &Arc<FakeCreds>| ServiceKeyStore {
            backend: Box::new(backend(data.path(), creds)),
            transaction: Mutex::new(()),
        };
        let (pair, _, key) = portable();
        let keys = open(&creds);
        let reference = keys
            .authorize_and_commit("job", &key, &pair.private, Ok)
            .unwrap();
        let failed: Result<()> =
            keys.authorize_and_commit("job", &key, &pair.private, |_| Err(bad("job write failed")));
        assert!(failed.is_err());
        assert_eq!(keys.backend.list_ids().unwrap(), vec![reference.id]);

        let reopened = open(&creds);
        assert_eq!(
            reopened.open("job", &key, &reference).unwrap().public(),
            key.public_key().unwrap()
        );
        assert!(open(&FakeCreds::new(2))
            .open("job", &key, &reference)
            .is_err());
        assert!(reopened.open("other-job", &key, &reference).is_err());
        let on_disk = std::fs::read(
            data.path()
                .join(KEY_DIRECTORY_NAME)
                .join(format!("{}.key", reference.id)),
        )
        .unwrap();
        assert!(!on_disk
            .windows(32)
            .any(|w| w == pair.private.storage_bytes()));
        assert!(reopened.retain(&Default::default()).unwrap() == vec![reference.id]);
        assert!(reopened.open("job", &key, &reference).is_err());
    }
}
