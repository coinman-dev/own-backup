//! Live probe of the encrypted backup path, for a **disposable Windows VM**.
//!
//! Everything about `.obk` that can be tested on Linux already is. What cannot:
//! the Windows engine — VSS, `$MFT` exclusions, raw volume reads, the iSCSI
//! mount — and that is exactly the half a cross-compiler only proves compiles.
//! This probe runs that half against a real NTFS volume inside the laboratory
//! VM (`scripts/wsl-windows-lab.sh`) and says, line by line, what happened.
//!
//! It builds a whole chain — full, two increments — and then consolidates it,
//! because the one thing consolidation must never do is change what the
//! surviving increment reads. That is checked the way CLAUDE.md states it for
//! the plaintext containers: every block of the virtual disk, read through the
//! chain before and after, compared.
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
//! It writes only inside the work directory, onto the source volume (files of
//! its own, to give the increments something to carry) and onto the restore
//! target, which it overwrites. Give it a throwaway volume and nothing else.

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

    use own_backup_backup::chains::{list_job_chains, plan_consolidation};
    use own_backup_backup::checksum::compute_block_checksum;
    use own_backup_backup::consolidate::consolidate_step_with_keys;
    use own_backup_backup::engine_win::{
        backup_volume, restore_volume, verify_image_with_keys, VolumeBackupRequest,
        VolumeRestoreRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use own_backup_backup::seal::obk::ObkRandomReader;
    use own_backup_backup::seal::recovery::RecoveryKey;
    use own_backup_common::types::{BackupType, ImageFormat};

    /// The name of the file the job excludes, created by the lab script.
    const EXCLUDED: &str = "excluded-secret.bin";
    const JOB: &str = "lab-encrypted-job";

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
        let envelope = SealedPrivateKey::seal(JOB, &pair, PASSWORD, &recovery, &kdf)?;
        say!("PROBE-KDF ok ms={}", started.elapsed().as_millis());
        let keys = ArchiveKeys::from_private(envelope, pair.private)?;

        let full = work.join("lab_C_full_1.obk");
        let inc1 = work.join("lab_C_inc_2.obk");
        let inc2 = work.join("lab_C_inc_3.obk");
        for path in [&full, &inc1, &inc2] {
            let _ = std::fs::remove_file(path);
        }

        let request = |path: &Path, kind: BackupType, parent: Option<&Path>| VolumeBackupRequest {
            source: source.clone(),
            image_path: path.to_path_buf(),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: false,
            use_vss: true,
            kind,
            parent_image: parent.map(|p| p.to_path_buf()),
            // The rule belongs to the job, so every run carries it — including
            // the increments, whose in-image deletion pass runs over a child.
            exclusion_patterns: vec![EXCLUDED.to_string()],
            job_id: Some(JOB.into()),
            passphrase: None,
            image_format: ImageFormat::Qcow2,
            encryption: Some(keys.clone()),
            source_disk: None,
        };

        // --- 1. The full copy, with a file exclusion rule ---
        // The excluded file's data is never read (the `$MFT` road subtracts its
        // clusters), and its catalog entry is deleted inside the image by the
        // filesystem driver — over a sealed scratch, with the container rebuilt
        // afterwards. Both halves are what the restore below checks.
        let started = Instant::now();
        let summary = backup_volume(&request(&full, BackupType::Full, None), |_| Ok(()))?;
        say!(
            "PROBE-BACKUP ok blocks={} bytes={} excluded_files={} secs={}",
            summary.blocks_copied,
            summary.bytes_copied,
            summary.excluded_files,
            started.elapsed().as_secs()
        );
        say!("PROBE-IMAGE bytes={}", std::fs::metadata(&full)?.len());

        // --- 2. What the file is, and is not ---
        match own_backup_backup::sidecar::detect_container_format(&full) {
            Ok(kind) => say!("PROBE-CONTAINER {kind}"),
            Err(e) => say!("PROBE-CONTAINER-FAIL {e}"),
        }
        match own_backup_backup::qcow2::Qcow2RandomReader::open(&full) {
            Ok(_) => say!("PROBE-PLAIN-READER-FAIL a plaintext reader opened the image"),
            Err(e) => say!("PROBE-PLAIN-READER refused: {e}"),
        }
        say!("PROBE-SIDECAR-FILE exists={}", work.join("lab_C_full_1.obk.obkm").exists());
        say!("PROBE-PLAINTEXT-IN-IMAGE {}", marker_in_file(&full, MARKER)?);

        // --- 3. Verify: without a key, with the wrong one, with the right one ---
        match verify_image_with_keys(&full, None, |_, _| Ok(())) {
            Ok(_) => say!("PROBE-VERIFY-NOKEY-FAIL it verified without a key"),
            Err(e) => say!("PROBE-VERIFY-NOKEY refused: {e}"),
        }
        match ArchiveKeys::from_image_password(&full, "не тот пароль") {
            Ok(_) => say!("PROBE-WRONG-PASSWORD-FAIL a wrong password opened the envelope"),
            Err(e) => say!("PROBE-WRONG-PASSWORD refused: {e}"),
        }
        // The password path is what another machine has: the image and what the
        // user types, no job database and no service key.
        let started = Instant::now();
        let portable = ArchiveKeys::from_image_password(&full, PASSWORD)?;
        say!("PROBE-PASSWORD ok ms={}", started.elapsed().as_millis());
        let started = Instant::now();
        let verified = verify_image_with_keys(&full, Some(&portable), |_, _| Ok(()))?;
        say!(
            "PROBE-VERIFY ok blocks={} mismatched={} secs={}",
            verified.blocks_checked,
            verified.mismatched_blocks.len(),
            started.elapsed().as_secs()
        );

        // --- 4. Two increments, each with something new to carry ---
        for (n, (image, parent)) in [(&inc1, &full), (&inc2, &inc1)].iter().enumerate() {
            write_generation(&source, n as u8 + 1)?;
            let started = Instant::now();
            let summary = backup_volume(
                &request(image, BackupType::Incremental, Some(parent)),
                |_| Ok(()),
            )?;
            say!(
                "PROBE-INCREMENT {} kind={:?} blocks={} unchanged={} bytes={} excluded_files={} secs={}",
                n + 1,
                summary.backup_kind,
                summary.blocks_copied,
                summary.blocks_unchanged,
                summary.bytes_copied,
                summary.excluded_files,
                started.elapsed().as_secs()
            );
            say!("PROBE-INCREMENT-IMAGE {} bytes={}", n + 1, std::fs::metadata(image)?.len());
        }

        // --- 5. What the chain reads, block by block, before consolidation ---
        let started = Instant::now();
        let before = chain_block_sums(&inc2, &keys)?;
        say!(
            "PROBE-CHAIN-READ blocks={} secs={}",
            before.len(),
            started.elapsed().as_secs()
        );

        let chains = list_job_chains(&work, JOB)?;
        say!(
            "PROBE-CHAIN chains={} images={}",
            chains.len(),
            chains.first().map(|c| c.images.len()).unwrap_or(0)
        );

        // --- 6. Consolidation: fold the oldest increment into the base ---
        let chain = chains.into_iter().next().ok_or("no chain in the work folder")?;
        let step = plan_consolidation(&chain, 1).ok_or("nothing planned: 2 increments, limit 1")?;
        say!(
            "PROBE-CONSOLIDATE-PLAN fold={} into={} repoint={:?}",
            step.victim.image_path.file_name().unwrap_or_default().to_string_lossy(),
            step.base.image_path.file_name().unwrap_or_default().to_string_lossy(),
            step.next
                .as_ref()
                .map(|n| n.image_path.file_name().unwrap_or_default().to_string_lossy().into_owned())
        );
        let started = Instant::now();
        let outcome = consolidate_step_with_keys(&step, Some(&keys))?;
        say!(
            "PROBE-CONSOLIDATE ok orphaned={} secs={} folded_gone={}",
            outcome.bytes_orphaned,
            started.elapsed().as_secs(),
            !inc1.exists()
        );

        // --- 7. The acceptance criterion: nothing the leaf reads may change ---
        let after = chain_block_sums(&inc2, &keys)?;
        let differing = before
            .iter()
            .zip(after.iter())
            .filter(|(a, b)| a != b)
            .count();
        say!(
            "PROBE-CONSOLIDATE-IDENTICAL blocks={} differing={} (expected 0)",
            after.len(),
            differing
        );
        if before.len() != after.len() {
            say!("PROBE-FAIL the virtual disk changed size across consolidation");
        }
        let chains = list_job_chains(&work, JOB)?;
        say!(
            "PROBE-CHAIN-AFTER images={} base_still_encrypted={}",
            chains.first().map(|c| c.images.len()).unwrap_or(0),
            own_backup_backup::qcow2::Qcow2RandomReader::open(&full).is_err()
        );

        // --- 8. The shortened chain still verifies and still restores ---
        let started = Instant::now();
        let portable = ArchiveKeys::from_image_password(&inc2, PASSWORD)?;
        let verified = verify_image_with_keys(&inc2, Some(&portable), |_, _| Ok(()))?;
        say!(
            "PROBE-VERIFY-AFTER ok blocks={} mismatched={} secs={}",
            verified.blocks_checked,
            verified.mismatched_blocks.len(),
            started.elapsed().as_secs()
        );

        let started = Instant::now();
        let restored = restore_volume(
            &VolumeRestoreRequest {
                image_path: inc2.clone(),
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

    /// Give the next increment something of its own to carry: a new file, and
    /// a change to one that is already in the chain.
    fn write_generation(source: &str, generation: u8) -> std::io::Result<()> {
        let root = source.trim_end_matches('\\');
        let fresh = format!("{root}\\gen-{generation}.bin");
        let mut data = vec![generation; 4 << 20];
        data[..MARKER.len()].copy_from_slice(MARKER);
        std::fs::write(&fresh, &data)?;
        // An existing file gets shorter, so the increment has to carry a
        // deletion of clusters as well as an addition.
        let edited = format!("{root}\\keep-1.bin");
        if Path::new(&edited).exists() {
            let mut kept = std::fs::read(&edited)?;
            kept.truncate(1 << 20);
            kept[0] = generation;
            std::fs::write(&edited, &kept)?;
        }
        Ok(())
    }

    /// One checksum per megabyte of the virtual disk, read through the whole
    /// chain. Consolidation must not change a single one of them.
    fn chain_block_sums(
        image: &Path,
        keys: &ArchiveKeys,
    ) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
        const BLOCK: u64 = 1 << 20;
        let key = keys.open_chain_of(image)?.key;
        let mut reader = ObkRandomReader::open(image, &key)?;
        let size = reader.virtual_size();
        let mut sums = Vec::with_capacity((size / BLOCK) as usize + 1);
        let mut buf = vec![0u8; BLOCK as usize];
        let mut at = 0u64;
        while at < size {
            let take = BLOCK.min(size - at) as usize;
            reader.read_at(at, &mut buf[..take])?;
            sums.push(compute_block_checksum(&buf[..take]));
            at += take as u64;
        }
        Ok(sums)
    }

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
