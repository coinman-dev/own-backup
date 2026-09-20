//! The `.obk` container through the ordinary library — no unit-test KDF profile
//! and no test-only entry points.
//!
//! This is the stage-E3 acceptance in miniature: a chain written to disk, then
//! opened the way a boot medium or another machine would (the image alone, plus
//! a password or the recovery key), and refused outright by `qemu-img`.

use std::path::Path;
use std::process::Command;

use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::obk::reader::read_open_header;
use own_backup_backup::seal::obk::{ObkChain, ObkCompression, ObkWriter, ObkWriterOptions};
use own_backup_backup::seal::recovery::RecoveryKey;
use own_backup_backup::seal::wrap::wrap_chain_key;
use own_backup_backup::seal::ChainKey;
use own_backup_backup::sidecar::ChainRole;
use uuid::Uuid;

const CLUSTER_BITS: u32 = 16;
const CLUSTER: usize = 1 << CLUSTER_BITS;
const VIRTUAL_SIZE: u64 = 32 * CLUSTER as u64;
const PASSWORD: &str = "правильная лошадь батарейка скрепка";

fn noise(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut data = vec![0u8; CLUSTER];
    for b in data.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 33) as u8;
    }
    data
}

fn qemu_img_available() -> bool {
    Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The smallest profile the production library accepts — the write profile is
/// 256 MiB and would only make this test slower, not stricter.
fn kdf() -> KdfParams {
    KdfParams {
        algorithm: "argon2id".into(),
        m_kib: 64 * 1024,
        t: 3,
        p: 1,
    }
}

struct Job {
    chain: ChainKey,
    chain_id: Uuid,
    key_file: SealedPrivateKey,
    pair: JobKeyPair,
}

impl Job {
    fn new(recovery: &RecoveryKey) -> Self {
        let pair = JobKeyPair::generate().unwrap();
        let key_file =
            SealedPrivateKey::seal("job-obk", &pair, PASSWORD, recovery, &kdf()).unwrap();
        Self {
            chain: ChainKey::generate().unwrap(),
            chain_id: Uuid::new_v4(),
            key_file,
            pair,
        }
    }

    fn write(
        &self,
        path: &Path,
        backup_id: Uuid,
        role: ChainRole,
        parent: Option<(Uuid, &str)>,
        clusters: &[(u64, Vec<u8>)],
        metadata: &[u8],
    ) {
        let opts = ObkWriterOptions {
            chain_id: self.chain_id,
            backup_id,
            parent_backup_id: parent.map(|(id, _)| id),
            role,
            created_at: "2026-09-21T12:00:00Z".into(),
            virtual_size: VIRTUAL_SIZE,
            cluster_bits: CLUSTER_BITS,
            compression: ObkCompression::Zstd { level: 3 },
            backing_file: parent.map(|(_, name)| name.to_string()),
            key_file: self.key_file.clone(),
            wrapped_chain_key: wrap_chain_key(
                &self.pair.public,
                self.chain_id.as_bytes(),
                &self.chain,
            )
            .unwrap(),
        };
        let mut writer = ObkWriter::create(path, &self.chain, opts).unwrap();
        for (index, data) in clusters {
            writer.write_cluster(*index, data).unwrap();
        }
        writer.finish(metadata).unwrap();
    }
}

#[test]
fn a_chain_written_here_opens_elsewhere_with_nothing_but_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let job = Job::new(&recovery);

    let full_id = Uuid::new_v4();
    job.write(
        &dir.path().join("full.obk"),
        full_id,
        ChainRole::Full,
        None,
        &[(0, vec![0xAA; CLUSTER]), (1, noise(1)), (2, noise(2))],
        b"sealed sidecar of the full backup",
    );
    job.write(
        &dir.path().join("inc.obk"),
        Uuid::new_v4(),
        ChainRole::Incremental,
        Some((full_id, "full.obk")),
        &[(1, noise(42)), (2, vec![0u8; CLUSTER])],
        b"sealed sidecar of the increment",
    );

    // Everything the other machine has is the files: no job database, no local
    // service key, only what the user typed.
    let leaf = dir.path().join("inc.obk");
    let open = read_open_header(&leaf).unwrap();
    assert_eq!(open.chain_id, job.chain_id);
    assert_eq!(open.role, ChainRole::Incremental);
    assert!(open.chain_key_with_password("не тот пароль").is_err());

    for key in [
        open.chain_key_with_password(PASSWORD).unwrap(),
        open.chain_key_with_recovery(&recovery).unwrap(),
    ] {
        let mut chain = ObkChain::open(&leaf, &key).unwrap();
        assert_eq!(chain.chain_len(), 2);
        assert_eq!(chain.virtual_size(), VIRTUAL_SIZE);
        assert_eq!(&chain.read_cluster(0).unwrap()[..], &vec![0xAA; CLUSTER][..]);
        assert_eq!(&chain.read_cluster(1).unwrap()[..], &noise(42)[..]);
        assert_eq!(&chain.read_cluster(2).unwrap()[..], &vec![0u8; CLUSTER][..]);
        assert_eq!(
            &chain.sealed_metadata().unwrap()[..],
            b"sealed sidecar of the increment"
        );
    }

    // A key of another chain opens nothing, and says so.
    assert!(ObkChain::open(&leaf, &ChainKey::generate().unwrap()).is_err());
}

#[test]
fn qemu_img_refuses_an_encrypted_image_by_name() {
    if !qemu_img_available() {
        eprintln!("qemu-img not found — skipping the foreign-reader test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let job = Job::new(&recovery);
    let path = dir.path().join("full.obk");
    job.write(
        &path,
        Uuid::new_v4(),
        ChainRole::Full,
        None,
        &[(0, noise(1))],
        b"sealed sidecar",
    );

    for args in [vec!["info"], vec!["check"]] {
        let out = Command::new("qemu-img")
            .args(&args)
            .args(["-f", "qcow2"])
            .arg(&path)
            .output()
            .expect("run qemu-img");
        assert!(
            !out.status.success(),
            "qemu-img {args:?} accepted an encrypted image"
        );
        let raw_message = String::from_utf8_lossy(&out.stderr);
        // Printed with --nocapture: this line is the evidence that a foreign
        // reader refuses the file and says which feature it does not know.
        eprintln!("qemu-img {args:?}: {}", raw_message.trim());
        let message = raw_message.to_lowercase();
        assert!(
            message.contains("own-backup encryption v2"),
            "qemu-img {args:?} did not name the feature: {message}"
        );
    }

    // Converting it to raw must fail too — no half-written plaintext output.
    let raw = dir.path().join("out.raw");
    let convert = Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw"])
        .arg(&path)
        .arg(&raw)
        .output()
        .expect("run qemu-img convert");
    assert!(!convert.status.success(), "qemu-img converted an encrypted image");
    assert!(
        std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0) == 0,
        "qemu-img wrote data out of an encrypted image"
    );
}

#[test]
fn nothing_in_an_encrypted_image_is_readable_as_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let job = Job::new(&recovery);
    let path = dir.path().join("full.obk");
    // A cluster of repeated text is the friendliest possible case for anyone
    // grepping the file, and a whole cluster of it must not survive.
    let secret: Vec<u8> = b"secret-payload--".repeat(CLUSTER / 16);
    job.write(
        &path,
        Uuid::new_v4(),
        ChainRole::Full,
        None,
        &[(3, secret.clone())],
        b"machine-name-and-volume-labels-live-in-the-sealed-half",
    );

    let raw = std::fs::read(&path).unwrap();
    assert!(
        !raw.windows(16).any(|w| w == b"secret-payload--"),
        "plaintext data survived into the image"
    );
    assert!(
        !raw
            .windows(12)
            .any(|w| w == b"machine-name"),
        "sealed metadata survived into the image in the clear"
    );
}
