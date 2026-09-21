//! Live probe of the encrypted backup path, for a **disposable Windows VM**.
//!
//! Everything about `.obk` that can be tested on Linux already is. What cannot:
//! the Windows engine — VSS, `$MFT` exclusions, raw volume reads, the iSCSI
//! mount — and that is exactly the half a cross-compiler only proves compiles.
//! This probe runs that half against a real NTFS volume inside the laboratory
//! VM (`scripts/wsl-windows-lab.sh`) and says, line by line, what happened.
//!
//! It is not a product entry point. The command line cannot make an encrypted
//! copy yet on purpose (asking for the password is E5), so this exists to let
//! the engine be exercised before that lands.
//!
//! Usage, inside the VM, as Administrator or SYSTEM:
//!
//! ```text
//! obk_live_probe.exe <source volume, e.g. S:> <work dir> <restore target, e.g. R:>
//! ```
//!
//! It writes only inside the work directory and onto the restore target, which
//! it overwrites. Give it a throwaway volume and nothing else.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This probe exercises the Windows engine; run it inside the laboratory VM.");
}

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe::run()
}

#[cfg(target_os = "windows")]
mod probe {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use own_backup_backup::engine_win::{
        backup_volume, restore_volume, verify_image_with_keys, VolumeBackupRequest,
        VolumeRestoreRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use own_backup_backup::seal::recovery::RecoveryKey;
    use own_backup_common::types::{BackupType, ImageFormat};

    /// The name of the file the job excludes, created by the lab script.
    const EXCLUDED: &str = "excluded-secret.bin";

    macro_rules! say {
        ($($arg:tt)*) => {{
            println!($($arg)*);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }};
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 4 {
            return Err("usage: obk_live_probe <source volume> <work dir> <restore target>".into());
        }
        let source = args[1].clone();
        let work = PathBuf::from(&args[2]);
        let target = args[3].clone();
        std::fs::create_dir_all(&work)?;

        say!("PROBE-BEGIN source={source} work={} target={target}", work.display());

        // The keys of a job, made here and thrown away with the VM. The
        // production KDF profile, not a test one: this is the ordinary path.
        let pair = JobKeyPair::generate()?;
        let recovery = RecoveryKey::generate()?;
        let kdf = KdfParams::strong();
        let started = Instant::now();
        let envelope = SealedPrivateKey::seal("lab-encrypted-job", &pair, PASSWORD, &recovery, &kdf)?;
        say!("PROBE-KDF ok ms={}", started.elapsed().as_millis());
        let keys = ArchiveKeys::from_private(envelope, pair.private)?;

        let image = work.join("lab_full.obk");
        let _ = std::fs::remove_file(&image);

        let request = |patterns: Vec<String>, path: &Path| VolumeBackupRequest {
            source: source.clone(),
            image_path: path.to_path_buf(),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: false,
            use_vss: true,
            kind: BackupType::Full,
            parent_image: None,
            exclusion_patterns: patterns,
            job_id: Some("lab-encrypted-job".into()),
            passphrase: None,
            image_format: ImageFormat::Qcow2,
            encryption: Some(keys.clone()),
            source_disk: None,
        };

        // --- 1a. With file exclusions: refused, and nothing written ---
        // Deleting the excluded files' catalog entries needs the container
        // rebuilt, which is not done yet. The run must stop rather than ship a
        // copy whose catalog still names them.
        let refused_path = work.join("lab_refused.obk");
        let _ = std::fs::remove_file(&refused_path);
        match backup_volume(&request(vec![EXCLUDED.to_string()], &refused_path), |_| Ok(())) {
            Ok(_) => say!("PROBE-EXCLUSIONS-FAIL an encrypted copy with exclusions was written"),
            Err(e) => say!(
                "PROBE-EXCLUSIONS refused: {e} | image_left={}",
                refused_path.exists()
            ),
        }

        // --- 1b. The copy itself ---
        let started = Instant::now();
        let summary = backup_volume(&request(Vec::new(), &image), |_| Ok(()))?;
        say!(
            "PROBE-BACKUP ok blocks={} bytes={} excluded_files={} secs={}",
            summary.blocks_copied,
            summary.bytes_copied,
            summary.excluded_files,
            started.elapsed().as_secs()
        );
        say!("PROBE-IMAGE bytes={}", std::fs::metadata(&image)?.len());

        // --- 2. What the file is, and is not ---
        match own_backup_backup::sidecar::detect_container_format(&image) {
            Ok(kind) => say!("PROBE-CONTAINER {kind}"),
            Err(e) => say!("PROBE-CONTAINER-FAIL {e}"),
        }
        match own_backup_backup::qcow2::Qcow2RandomReader::open(&image) {
            Ok(_) => say!("PROBE-PLAIN-READER-FAIL a plaintext reader opened the image"),
            Err(e) => say!("PROBE-PLAIN-READER refused: {e}"),
        }
        say!("PROBE-SIDECAR-FILE exists={}", work.join("lab_full.obk.obkm").exists());
        // The listing sees the chain without any key at all.
        match own_backup_backup::chains::list_job_chains(&work, "lab-encrypted-job") {
            Ok(chains) => say!(
                "PROBE-LISTING chains={} images={} container={}",
                chains.len(),
                chains.first().map(|c| c.images.len()).unwrap_or(0),
                chains
                    .first()
                    .and_then(|c| c.images.first())
                    .map(|i| i.container.clone())
                    .unwrap_or_default()
            ),
            Err(e) => say!("PROBE-LISTING-FAIL {e}"),
        }
        say!("PROBE-PLAINTEXT-IN-IMAGE {}", marker_in_file(&image, MARKER)?);

        // --- 3. Verify: without a key, with the wrong one, with the right one ---
        match verify_image_with_keys(&image, None, |_, _| Ok(())) {
            Ok(_) => say!("PROBE-VERIFY-NOKEY-FAIL it verified without a key"),
            Err(e) => say!("PROBE-VERIFY-NOKEY refused: {e}"),
        }
        match ArchiveKeys::from_image_password(&image, "не тот пароль") {
            Ok(_) => say!("PROBE-WRONG-PASSWORD-FAIL a wrong password opened the envelope"),
            Err(e) => say!("PROBE-WRONG-PASSWORD refused: {e}"),
        }
        // The password path is what another machine has: the image and what the
        // user types, no job database and no service key.
        let started = Instant::now();
        let portable = ArchiveKeys::from_image_password(&image, PASSWORD)?;
        say!("PROBE-PASSWORD ok ms={}", started.elapsed().as_millis());
        let started = Instant::now();
        let verified = verify_image_with_keys(&image, Some(&portable), |_, _| Ok(()))?;
        say!(
            "PROBE-VERIFY ok blocks={} mismatched={} secs={}",
            verified.blocks_checked,
            verified.mismatched_blocks.len(),
            started.elapsed().as_secs()
        );

        // --- 4. Restore onto a throwaway volume ---
        let started = Instant::now();
        let restored = restore_volume(
            &VolumeRestoreRequest {
                image_path: image.clone(),
                target: target.clone(),
                verify_checksums: true,
                encryption: Some(portable),
            },
            |_| Ok(()),
        )?;
        say!(
            "PROBE-RESTORE ok blocks={} bytes={} secs={}",
            restored.blocks_restored,
            restored.bytes_restored,
            started.elapsed().as_secs()
        );

        say!("PROBE-DONE");
        Ok(())
    }

    /// The password of a throwaway laboratory job, and nothing else.
    const PASSWORD: &str = "лабораторный пароль архива";
    /// The marker the lab script writes into the source files.
    const MARKER: &[u8] = b"own-backup-lab-marker--";

    /// Whether a byte string appears anywhere in a file. Reads in windows that
    /// overlap by the needle's length, so a match on a chunk boundary counts.
    fn marker_in_file(path: &Path, needle: &[u8]) -> std::io::Result<bool> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                return Ok(false);
            }
            let mut window = carry.clone();
            window.extend_from_slice(&buf[..read]);
            if window.windows(needle.len()).any(|w| w == needle) {
                return Ok(true);
            }
            let keep = window.len().saturating_sub(needle.len() - 1);
            carry = window[keep..].to_vec();
        }
    }
}
