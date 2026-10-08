//! The keys one run of an encrypted job works with, opened once at its start.
//!
//! Plan §7.4: a scheduled run must not ask anybody anything. The service opens
//! the job's private key from its own protected store (`local.rs`) before the
//! run begins, and that one context then serves every step — reading the
//! parent, comparing checksums, writing, the service mount, the exclusions
//! pass, the merge, the verify and the finalize. Manual opening (mount,
//! restore, another machine) builds the same context from a password or a
//! recovery key instead, and never from the service's store.
//!
//! What it is not: a cache of unlocked jobs. It holds one job's key material
//! for the length of one operation, and the caller drops it when that operation
//! ends.

use std::path::Path;

use own_backup_common::Result;
use uuid::Uuid;

use super::keypair::{JobPrivateKey, JobPublicKey, SealedPrivateKey};
use super::obk::header::ObkOpenHeader;
use super::obk::{ObkCompression, ObkWriterOptions};
use super::recovery::RecoveryKey;
use super::wrap::{wrap_chain_key, WrappedChainKey};
use super::{bad, ChainKey};
use crate::sidecar::ChainRole;

/// Cluster size of every encrypted image we write: 64 KiB, the backup block
/// size, so one block is one record.
pub const OBK_CLUSTER_BITS: u32 = 16;

/// One job's key material for one operation.
///
/// Cloneable because one run copies several volumes and each of them needs its
/// own context; a clone holds the same key material and is wiped with it.
#[derive(Clone)]
pub struct ArchiveKeys {
    key_file: SealedPrivateKey,
    private: JobPrivateKey,
    public: JobPublicKey,
}

/// One chain's master key, and what an image needs to name it.
pub struct ChainSession {
    pub chain_id: Uuid,
    pub key: ChainKey,
    pub wrapped: WrappedChainKey,
}

/// What distinguishes the next image of a chain from the previous one.
pub struct ImagePlan<'a> {
    pub backup_id: Uuid,
    pub role: ChainRole,
    /// Identity and file name of the parent image, for a child.
    pub parent: Option<(Uuid, &'a str)>,
    pub created_at: String,
    pub virtual_size: u64,
    /// Listing fields (see [`ObkOpenHeader`]).
    pub source_id: Option<String>,
    pub consolidated_increments: u32,
    pub chain_started_at: Option<String>,
    pub compression: ObkCompression,
}

impl ArchiveKeys {
    /// Stable, domain-separated ownership key for unfinished NEW images.
    /// Chain keys cannot reclaim a full image interrupted before its first
    /// header: that new chain key has not been published anywhere yet.
    pub(crate) fn new_image_owner_key(&self) -> ChainKey {
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        hkdf::Hkdf::<sha2::Sha256>::new(None, self.private.storage_bytes())
            .expand(b"own-backup/v2/new-image-owner", &mut bytes[..])
            .expect("fixed-size ownership key");
        ChainKey::from_bytes(*bytes)
    }

    /// The scheduled path: the service already holds the private key.
    pub fn from_private(key_file: SealedPrivateKey, private: JobPrivateKey) -> Result<Self> {
        key_file.validate()?;
        let public = key_file.public_key()?;
        if private.public() != public {
            return Err(bad("this private key belongs to another job"));
        }
        Ok(Self {
            key_file,
            private,
            public,
        })
    }

    /// Manual opening, anywhere: the portable password slot of an image.
    pub fn from_password(key_file: SealedPrivateKey, password: &str) -> Result<Self> {
        let private = key_file.unseal_with_password(password)?;
        Self::from_private(key_file, private)
    }

    /// Manual opening without the password: the independent recovery slot.
    pub fn from_recovery(key_file: SealedPrivateKey, recovery: &RecoveryKey) -> Result<Self> {
        let private = key_file.unseal_with_recovery(recovery)?;
        Self::from_private(key_file, private)
    }

    /// Open an image's own envelope with a password — what a boot medium or
    /// another machine has: the files, and what the user typed.
    pub fn from_image_password(image: &Path, password: &str) -> Result<Self> {
        Self::from_password(image_envelope(image)?, password)
    }

    pub fn from_image_recovery(image: &Path, recovery: &RecoveryKey) -> Result<Self> {
        Self::from_recovery(image_envelope(image)?, recovery)
    }

    pub fn job_id(&self) -> &str {
        &self.key_file.job_id
    }

    pub fn public(&self) -> &JobPublicKey {
        &self.public
    }

    pub fn key_file(&self) -> &SealedPrivateKey {
        &self.key_file
    }

    /// Start a new chain: every full backup gets its own random master key, so
    /// one chain's key says nothing about another's.
    pub fn start_chain(&self) -> Result<ChainSession> {
        let chain_id = Uuid::new_v4();
        let key = ChainKey::generate()?;
        let wrapped = wrap_chain_key(&self.public, chain_id.as_bytes(), &key)?;
        Ok(ChainSession {
            chain_id,
            key,
            wrapped,
        })
    }

    /// Continue the chain an existing image belongs to. The wrapped key travels
    /// in that image, so nothing outside the archive has to remember it.
    pub fn open_chain(&self, header: &ObkOpenHeader) -> Result<ChainSession> {
        if header.key_file.job_id != self.key_file.job_id {
            return Err(bad("this image belongs to another job"));
        }
        if header.public_key()? != self.public {
            return Err(bad("this image was written for another key of this job"));
        }
        let key = header.chain_key_with_private(&self.private)?;
        Ok(ChainSession {
            chain_id: header.chain_id,
            key,
            wrapped: header.wrapped()?,
        })
    }

    /// The same, reading the header from a file.
    pub fn open_chain_of(&self, image: &Path) -> Result<ChainSession> {
        self.open_chain(&super::obk::reader::read_open_header(image)?)
    }

    /// Everything the writer needs for the next image of `chain`: the plan's
    /// own fields plus this job's envelope and the chain's wrapped key, which
    /// travel in every image so any machine can open the chain.
    pub fn writer_options(&self, chain: &ChainSession, plan: ImagePlan<'_>) -> ObkWriterOptions {
        ObkWriterOptions {
            chain_id: chain.chain_id,
            backup_id: plan.backup_id,
            parent_backup_id: plan.parent.map(|(id, _)| id),
            role: plan.role,
            created_at: plan.created_at,
            virtual_size: plan.virtual_size,
            cluster_bits: OBK_CLUSTER_BITS,
            compression: plan.compression,
            backing_file: plan.parent.map(|(_, name)| name.to_string()),
            key_file: self.key_file.clone(),
            wrapped_chain_key: chain.wrapped.clone(),
            source_id: plan.source_id,
            consolidated_increments: plan.consolidated_increments,
            chain_started_at: plan.chain_started_at,
        }
    }
}

impl std::fmt::Debug for ArchiveKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nothing secret reaches a log through `{:?}`.
        write!(f, "ArchiveKeys(job={})", self.key_file.job_id)
    }
}

impl std::fmt::Debug for ChainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChainSession(chain={})", self.chain_id)
    }
}

fn image_envelope(image: &Path) -> Result<SealedPrivateKey> {
    Ok(super::obk::reader::read_open_header(image)?.key_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::keypair::{JobKeyPair, KdfParams};
    use crate::seal::obk::container_tests::Fixture;
    use crate::seal::obk::{ObkChain, ObkWriter};

    fn envelope(pair: &JobKeyPair, recovery: &RecoveryKey) -> SealedPrivateKey {
        SealedPrivateKey::seal(
            "job-context",
            pair,
            "пароль архива",
            recovery,
            &KdfParams::test_profile(),
        )
        .unwrap()
    }

    #[test]
    fn a_new_chain_gets_its_own_key_and_reopens_from_the_image_it_wrote() {
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let keys = ArchiveKeys::from_private(envelope(&pair, &recovery), pair.private).unwrap();
        assert_eq!(keys.job_id(), "job-context");

        let first = keys.start_chain().unwrap();
        let second = keys.start_chain().unwrap();
        assert_ne!(first.chain_id, second.chain_id, "a chain per full backup");
        assert_ne!(
            first.key.as_bytes(),
            second.key.as_bytes(),
            "and a key per chain"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let backup_id = Uuid::new_v4();
        let options = keys.writer_options(
            &first,
            ImagePlan {
                backup_id,
                role: ChainRole::Full,
                parent: None,
                created_at: "2026-09-21T12:00:00Z".into(),
                virtual_size: 8 << 20,
                source_id: Some("volume".into()),
                consolidated_increments: 0,
                chain_started_at: None,
                compression: ObkCompression::Zstd { level: 3 },
            },
        );
        let mut writer = ObkWriter::create(&path, &first.key, options).unwrap();
        writer
            .write_cluster(0, &vec![5u8; 1 << OBK_CLUSTER_BITS])
            .unwrap();
        writer.finish(b"sealed sidecar").unwrap();

        // The next run opens the same chain from the image alone.
        let again = keys.open_chain_of(&path).unwrap();
        assert_eq!(again.chain_id, first.chain_id);
        assert_eq!(again.key.as_bytes(), first.key.as_bytes());
        let mut chain = ObkChain::open(&path, &again.key).unwrap();
        assert_eq!(
            &chain.read_cluster(0).unwrap()[..],
            &vec![5u8; 1 << OBK_CLUSTER_BITS][..]
        );

        // And so does a password or a recovery key on a machine that has
        // neither the service store nor the job database.
        for opened in [
            ArchiveKeys::from_image_password(&path, "пароль архива").unwrap(),
            ArchiveKeys::from_image_recovery(&path, &recovery).unwrap(),
        ] {
            let session = opened.open_chain_of(&path).unwrap();
            assert_eq!(session.key.as_bytes(), first.key.as_bytes());
        }
        assert!(ArchiveKeys::from_image_password(&path, "не тот").is_err());
    }

    #[test]
    fn a_key_of_another_job_opens_nothing_and_says_which_way_it_is_wrong() {
        let recovery = RecoveryKey::generate().unwrap();
        let pair = JobKeyPair::generate().unwrap();
        let other = JobKeyPair::generate().unwrap();

        // A private key that does not match its own envelope is refused before
        // it can be used for anything.
        assert!(ArchiveKeys::from_private(envelope(&pair, &recovery), other.private).is_err());

        // An image of another job, and an image of the same job written for a
        // different key pair, are both refused by name.
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stranger.obk");
        let mut writer = ObkWriter::create(
            &path,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        writer.write_cluster(0, &vec![1u8; 4096]).unwrap();
        writer.finish(b"sealed").unwrap();

        let pair = JobKeyPair::generate().unwrap();
        let keys = ArchiveKeys::from_private(envelope(&pair, &recovery), pair.private).unwrap();
        let error = keys.open_chain_of(&path).unwrap_err().to_string();
        assert!(error.contains("another job"), "unexpected: {error}");
    }
}
