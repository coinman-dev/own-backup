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
//! obk_live_probe.exe volume <source volume, e.g. S:> <work dir> <restore target>
//! obk_live_probe.exe disk   <source disk number> <work dir> <target disk number>
//! ```
//!
//! The `disk` mode is the bootable whole-disk copy: one image holding the real
//! GPT and every selected partition, with the restore manifest sealed inside it
//! rather than in a guest meta partition.
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
        if args.len() != 5 {
            return Err("usage: obk_live_probe <volume|disk> <source> <work dir> <target>".into());
        }
        let work = PathBuf::from(&args[3]);
        std::fs::create_dir_all(&work)?;
        match args[1].as_str() {
            "volume" => run_volume(args[2].clone(), work, args[4].clone()),
            "disk" => run_disk(args[2].parse()?, work, args[4].parse()?),
            other => Err(format!("unknown mode {other:?}: expected 'volume' or 'disk'").into()),
        }
    }

    /// The keys of a throwaway job, made here and gone with the VM. The
    /// production KDF profile, not a test one: this is the ordinary path.
    fn lab_keys() -> Result<ArchiveKeys, Box<dyn std::error::Error>> {
        let pair = JobKeyPair::generate()?;
        let recovery = RecoveryKey::generate()?;
        let started = Instant::now();
        let envelope =
            SealedPrivateKey::seal(JOB, &pair, PASSWORD, &recovery, &KdfParams::strong())?;
        say!("PROBE-KDF ok ms={}", started.elapsed().as_millis());
        Ok(ArchiveKeys::from_private(envelope, pair.private)?)
    }

    fn run_volume(
        source: String,
        work: PathBuf,
        target: String,
    ) -> Result<(), Box<dyn std::error::Error>> {

        say!("PROBE-BEGIN source={source} work={} target={target}", work.display());
        let keys = lab_keys()?;

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
            zstd_level: own_backup_backup::qcow2::DEFAULT_ZSTD_LEVEL,
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

        // The chain THIS copy belongs to, found by its own leaf. A work folder
        // can hold more than one chain of the same job — the whole-disk phase
        // leaves one — and taking "the first" would consolidate a stranger.
        let chains = list_job_chains(&work, JOB)?;
        let chain = chains
            .into_iter()
            .find(|c| c.images.iter().any(|i| i.image_path == inc2))
            .ok_or("the work folder holds no chain containing this copy")?;
        say!("PROBE-CHAIN images={}", chain.images.len());

        // --- 6. Consolidation: fold the oldest increment into the base ---
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
        let after_chain = list_job_chains(&work, JOB)?
            .into_iter()
            .find(|c| c.images.iter().any(|i| i.image_path == inc2));
        say!(
            "PROBE-CHAIN-AFTER images={} increments_ever={} base_still_encrypted={}",
            after_chain.as_ref().map(|c| c.images.len()).unwrap_or(0),
            after_chain.as_ref().map(|c| c.increments_ever()).unwrap_or(0),
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

    /// The bootable whole-disk copy: one encrypted image holding the real GPT,
    /// every partition, and the restore manifest — sealed inside the container,
    /// because an `.obk` has no guest meta partition to put it in.
    ///
    /// This is the half nothing else can check. A whole-disk image is written
    /// through a different planner, a different composite reader and a
    /// different restore than a volume copy, and all three are Windows-only.
    fn run_disk(
        source_disk: u32,
        work: PathBuf,
        target_disk: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use own_backup_backup::engine_win::{
            backup_disk, restore_disk, DiskBackupRequest, DiskPartitionBackup, DiskRestoreRequest,
        };

        say!("PROBE-DISK-BEGIN source_disk={source_disk} target_disk={target_disk}");
        let keys = lab_keys()?;

        // Every partition the source disk has, except the Microsoft Reserved
        // one: it holds no filesystem, and its GPT entry is recreated from the
        // manifest on restore rather than copied.
        let layout = own_backup_backup::gpt_reader::read_disk_layout(source_disk)?;
        let partitions: Vec<DiskPartitionBackup> = layout
            .partitions
            .iter()
            .filter(|p| {
                !p.type_guid
                    .eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_MSR)
            })
            .map(|p| {
                let offset = p.first_lba * layout.sector_size as u64;
                // The VOLUME, not the raw partition range. A raw `part:` spec
                // has no volume root, so nothing takes a VSS snapshot of it and
                // nothing can walk its files — the exclusion rules would find
                // no matches and the copy would silently carry the lot.
                let device = own_backup_backup::volume_win::volume_root_at_offset(
                    source_disk,
                    offset,
                )
                .unwrap_or_else(|| format!("part:{source_disk}:{offset}:{}",
                    p.size_bytes(layout.sector_size)));
                say!("PROBE-DISK-PART index={} device={device}", p.index);
                DiskPartitionBackup {
                    device,
                    part_index: p.index,
                    passphrase: None,
                }
            })
            .collect();
        say!("PROBE-DISK-LAYOUT partitions={}", partitions.len());

        let image = work.join("lab_disk0_full_1.obk");
        let _ = std::fs::remove_file(&image);
        let started = Instant::now();
        let summary = backup_disk(
            &DiskBackupRequest {
                disk_number: source_disk,
                disk_model: "LAB VIRTUAL DISK".into(),
                partitions,
                image_path: image.clone(),
                block_size: DEFAULT_BLOCK_SIZE,
                image_format: ImageFormat::Qcow2,
                job_id: Some(JOB.into()),
                encryption: Some(keys.clone()),
                zstd_level: own_backup_backup::qcow2::DEFAULT_ZSTD_LEVEL,
                exclusion_patterns_windows: vec![EXCLUDED.to_string()],
                exclusion_patterns_linux: Vec::new(),
                kind: BackupType::Full,
                parent_image: None,
                source_disk: None,
                bootable: false,
            },
            |_| Ok(()),
        )?;
        say!(
            "PROBE-DISK-BACKUP ok blocks={} bytes={} excluded_files={} secs={}",
            summary.blocks_copied,
            summary.bytes_copied,
            summary.excluded_files,
            started.elapsed().as_secs()
        );
        say!("PROBE-DISK-IMAGE bytes={}", std::fs::metadata(&image)?.len());

        // The same refusals as a volume copy, plus the one that matters here:
        // the restore manifest is INSIDE the sealed half, so a reader without
        // the key cannot even learn the disk's layout.
        match own_backup_backup::sidecar::detect_container_format(&image) {
            Ok(kind) => say!("PROBE-DISK-CONTAINER {kind}"),
            Err(e) => say!("PROBE-DISK-CONTAINER-FAIL {e}"),
        }
        match own_backup_backup::sidecar::read_image_sidecar(&image) {
            Ok(_) => say!("PROBE-DISK-MANIFEST-FAIL the manifest was readable without a key"),
            Err(e) => say!("PROBE-DISK-MANIFEST-NOKEY refused: {e}"),
        }

        let portable = ArchiveKeys::from_image_password(&image, PASSWORD)?;
        let key = portable.open_chain_of(&image)?.key;
        let sidecar = own_backup_backup::sidecar::read_sealed_sidecar(&image, &key)?;
        let dm = sidecar
            .disk_manifest
            .as_ref()
            .ok_or("the sealed sidecar carries no whole-disk manifest")?;
        say!(
            "PROBE-DISK-MANIFEST partitions={} disk_guid={} blocks={}",
            dm.images.len(),
            dm.layout.disk_guid,
            sidecar.checksums.len()
        );

        let started = Instant::now();
        let verified = verify_image_with_keys(&image, Some(&portable), |_, _| Ok(()))?;
        say!(
            "PROBE-DISK-VERIFY ok blocks={} mismatched={} secs={}",
            verified.blocks_checked,
            verified.mismatched_blocks.len(),
            started.elapsed().as_secs()
        );

        let started = Instant::now();
        let restored = restore_disk(
            &DiskRestoreRequest {
                manifest_path: image.clone(),
                target_disk,
                verify_checksums: true,
                encryption: Some(portable),
            },
            |_| Ok(()),
        )?;
        say!(
            "PROBE-DISK-RESTORE ok partitions={} bytes={} secs={}",
            restored.partitions,
            restored.bytes_written,
            started.elapsed().as_secs()
        );
        say!("PROBE-DISK-DONE");
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
