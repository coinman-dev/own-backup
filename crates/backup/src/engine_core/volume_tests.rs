//! The volume pipeline driven through the test platform: an in-memory volume
//! is copied by `backup_volume_inner` exactly as Windows copies a real one,
//! and the image it wrote is read back, checked and compared.

use std::path::{Path, PathBuf};

use own_backup_common::types::{BackupType, ImageFormat};

use super::{backup_volume_inner, VolumeBackupRequest, VolumeBackupSummary};
use crate::engine_core::platform::SourceDevice;
use crate::engine_core::read::{load_sidecar_of, ImageReadSource};
use crate::engine_core::target::no_patterns;
use crate::engine_core::test_platform::{
    register_device, unique_path, MemDevice, TestPlatform, TestSource,
};
use crate::engine_core::verify::verify_image;
use crate::fat_writer::{build_fat16, FatFile};
use crate::sidecar::ChainRole;

/// Small blocks, so a few-megabyte volume has hundreds of them.
const BS: u32 = 4096;
/// A child's blocks must cover whole qcow2 clusters (64 KiB): a cluster it
/// writes only part of is filled with zeros, not with the parent's bytes —
/// production blocks are 1 MiB, and a child test at 4 KiB would test that.
const CHILD_BS: u32 = 64 * 1024;

fn request(source: &str, image: &Path, format: ImageFormat) -> VolumeBackupRequest {
    VolumeBackupRequest {
        source: source.into(),
        image_path: image.to_path_buf(),
        full_image_path: None,
        block_size: BS,
        all_sectors: false,
        use_vss: true,
        kind: BackupType::Full,
        parent_image: None,
        exclusion_patterns: Vec::new(),
        exclusion_patterns_linux: Vec::new(),
        job_id: Some("job-volume-tests".into()),
        passphrase: None,
        image_format: format,
        source_disk: None,
        encryption: None,
        zstd_level: 0,
    }
}

fn run(platform: &TestPlatform, req: &VolumeBackupRequest) -> VolumeBackupSummary {
    backup_volume_inner(platform, req, None, &req.image_path, |_| Ok(())).unwrap()
}

fn container(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Vhdx => "vhdx",
        ImageFormat::Qcow2 => "qcow2",
    }
}

/// The volume as the image holds it, read through the container (and its
/// parents) from where the sidecar says it starts.
fn volume_in(image: &Path, len: usize) -> Vec<u8> {
    let sidecar = load_sidecar_of(image).unwrap();
    let mut reader = ImageReadSource::open(image, &sidecar.info.container).unwrap();
    let mut out = vec![0u8; len];
    reader
        .read_at(sidecar.info.volume_data_offset, &mut out)
        .unwrap();
    out
}

/// `a == b`, saying where they first differ rather than printing megabytes.
#[track_caller]
fn assert_same(a: &[u8], b: &[u8], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length");
    if let Some(at) = a.iter().zip(b).position(|(x, y)| x != y) {
        panic!(
            "{what}: first difference at byte {at}: {:#04x} != {:#04x}",
            a[at], b[at]
        );
    }
}

fn assert_verifies(image: &Path, blocks: u64) {
    let verified = verify_image(image, |_, _| Ok(())).unwrap();
    assert!(verified.valid(), "{}: {verified:?}", image.display());
    assert_eq!(verified.blocks_checked, blocks);
}

fn fat16_volume(readme: &[u8]) -> Vec<u8> {
    let files = [
        FatFile {
            path: r"EFI\BOOT\BOOTX64.EFI".into(),
            data: (0..200_000u32).map(|i| (i % 253) as u8).collect(),
        },
        FatFile {
            path: "README.TXT".into(),
            data: readme.to_vec(),
        },
    ];
    build_fat16(&files, "OWNTEST").unwrap()
}

fn blocks_of(volume: &[u8], block_size: u32) -> u64 {
    (volume.len() as u64).div_ceil(block_size as u64)
}

#[test]
fn a_fat16_volume_is_copied_whole_into_vhdx_and_qcow2() {
    for format in [ImageFormat::Vhdx, ImageFormat::Qcow2] {
        let dir = tempfile::tempdir().unwrap();
        let volume = fat16_volume(b"hello");
        // The live device holds something else: the copy must come from the
        // snapshot the session reads, and still be named after the device.
        let origin = unique_path("origin");
        let snap = unique_path("snap");
        register_device(&origin, vec![0xEE; volume.len()], 512);
        register_device(&snap, volume.clone(), 512);
        let platform = TestPlatform::new().source(
            &origin,
            TestSource::Snapshot {
                read_path: snap.clone(),
            },
        );
        let image = dir.path().join(format!("full.{}", container(format)));

        let summary = run(&platform, &request(&origin, &image, format));

        let blocks = blocks_of(&volume, BS);
        assert_eq!(summary.backup_kind, BackupType::Full);
        assert!(summary.vss_used, "{format:?}: read through the snapshot");
        // FAT16 has no used map of ours: every block goes in.
        assert!(!summary.smart_mode);
        assert_eq!(summary.source_bytes, volume.len() as u64);
        assert_eq!(summary.total_blocks, blocks);
        assert_eq!(summary.used_blocks, blocks);
        assert_eq!(summary.blocks_copied, blocks);
        assert_eq!(summary.blocks_unchanged, 0);
        assert_eq!(summary.file_scan_skipped, Some(no_patterns()));
        assert_eq!(summary.excluded_entries_kept, None);

        assert_same(
            &volume_in(&image, volume.len()),
            &volume,
            &format!("{format:?}"),
        );
        assert_verifies(&image, blocks);
        let sidecar = load_sidecar_of(&image).unwrap();
        assert_eq!(sidecar.manifest.source_os, "test");
        assert_eq!(sidecar.manifest.source_disk.device_id, origin);
        assert_eq!(sidecar.info.container, container(format));
        assert_eq!(sidecar.info.job_id.as_deref(), Some("job-volume-tests"));
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
    }
}

#[test]
fn an_incremental_qcow2_writes_only_the_changed_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let source = unique_path("vol");
    let v1 = fat16_volume(&[b'1'; 20_000]);
    register_device(&source, v1.clone(), 512);
    let platform = TestPlatform::new().source(&source, TestSource::Direct { frozen: true });
    let full = dir.path().join("full.qcow2");
    let full_summary = run(
        &platform,
        &VolumeBackupRequest {
            block_size: CHILD_BS,
            ..request(&source, &full, ImageFormat::Qcow2)
        },
    );
    assert!(!full_summary.vss_used);

    // The same file rewritten: only its clusters differ.
    let v2 = fat16_volume(&[b'2'; 20_000]);
    assert_eq!(v1.len(), v2.len());
    let changed = v1
        .chunks(CHILD_BS as usize)
        .zip(v2.chunks(CHILD_BS as usize))
        .filter(|(a, b)| a != b)
        .count() as u64;
    let blocks = blocks_of(&v2, CHILD_BS);
    assert!(changed > 0 && changed < blocks, "{changed} of {blocks}");
    register_device(&source, v2.clone(), 512);

    let child = dir.path().join("inc.qcow2");
    let summary = run(
        &platform,
        &VolumeBackupRequest {
            block_size: CHILD_BS,
            kind: BackupType::Incremental,
            parent_image: Some(full.clone()),
            ..request(&source, &child, ImageFormat::Qcow2)
        },
    );

    assert_eq!(summary.backup_kind, BackupType::Incremental);
    assert_eq!(summary.used_blocks, blocks);
    assert_eq!(summary.blocks_copied, changed);
    assert_eq!(summary.blocks_unchanged, blocks - changed);
    assert_same(&volume_in(&child, v2.len()), &v2, "the child");
    assert_same(&volume_in(&full, v1.len()), &v1, "the parent is untouched");
    assert_verifies(&child, blocks);
    assert_verifies(&full, blocks);

    let parent = load_sidecar_of(&full).unwrap();
    let sidecar = load_sidecar_of(&child).unwrap();
    assert_eq!(sidecar.info.chain_role, ChainRole::Incremental);
    assert_eq!(
        sidecar.info.parent_image_file_name.as_deref(),
        Some("full.qcow2")
    );
    assert_eq!(
        sidecar.manifest.parent_backup_id,
        Some(parent.manifest.backup_id)
    );
}

/// A FAT32 volume with 4 KiB clusters (one per backup block):
///
/// ```text
/// root (cl 2)  KEEP.BIN (cl 3), JUNK.TMP (cl 4-5), TEMP\ (cl 6)
/// TEMP (cl 6)  NOTE.TMP (cl 7)
/// ```
///
/// Two FAT copies, so a freed chain has to be freed in both.
struct Fat32 {
    image: Vec<u8>,
}

impl Fat32 {
    const SECTOR: usize = 512;
    const CLUSTER: usize = 4096;
    const RESERVED: usize = 32;
    const FAT_SECTORS: usize = 4;
    const FATS: usize = 2;
    const CLUSTERS: usize = 256;

    fn data_start() -> usize {
        (Self::RESERVED + Self::FATS * Self::FAT_SECTORS) * Self::SECTOR
    }

    fn cluster(n: usize) -> usize {
        Self::data_start() + (n - 2) * Self::CLUSTER
    }

    fn fat(copy: usize) -> usize {
        (Self::RESERVED + copy * Self::FAT_SECTORS) * Self::SECTOR
    }

    fn build() -> Self {
        let spc = Self::CLUSTER / Self::SECTOR;
        let total = Self::RESERVED + Self::FATS * Self::FAT_SECTORS + Self::CLUSTERS * spc;
        let mut img = vec![0u8; total * Self::SECTOR];
        img[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        img[3..11].copy_from_slice(b"MSWIN4.1");
        img[11..13].copy_from_slice(&(Self::SECTOR as u16).to_le_bytes());
        img[13] = spc as u8;
        img[14..16].copy_from_slice(&(Self::RESERVED as u16).to_le_bytes());
        img[16] = Self::FATS as u8;
        img[32..36].copy_from_slice(&(total as u32).to_le_bytes());
        img[36..40].copy_from_slice(&(Self::FAT_SECTORS as u32).to_le_bytes());
        img[44..48].copy_from_slice(&2u32.to_le_bytes());
        img[510] = 0x55;
        img[511] = 0xAA;

        const EOC: u32 = 0x0FFF_FFFF;
        let chains = [
            (0, 0x0FFF_FFF8),
            (1, EOC),
            (2, EOC),
            (3, EOC),
            (4, 5),
            (5, EOC),
            (6, EOC),
            (7, EOC),
        ];
        for copy in 0..Self::FATS {
            for (n, v) in chains {
                let at = Self::fat(copy) + n * 4;
                img[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }

        let entry =
            |img: &mut [u8], at: usize, name: &[u8; 11], attr: u8, first: u32, size: u32| {
                img[at..at + 11].copy_from_slice(name);
                img[at + 11] = attr;
                img[at + 20..at + 22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
                img[at + 26..at + 28].copy_from_slice(&(first as u16).to_le_bytes());
                img[at + 28..at + 32].copy_from_slice(&size.to_le_bytes());
            };
        let root = Self::cluster(2);
        entry(
            &mut img,
            root,
            b"KEEP    BIN",
            0x20,
            3,
            Self::CLUSTER as u32,
        );
        entry(
            &mut img,
            root + 32,
            b"JUNK    TMP",
            0x20,
            4,
            2 * Self::CLUSTER as u32,
        );
        entry(&mut img, root + 64, b"TEMP       ", 0x10, 6, 0);
        entry(
            &mut img,
            Self::cluster(6),
            b"NOTE    TMP",
            0x20,
            7,
            Self::CLUSTER as u32,
        );

        for (cluster, fill) in [(3, 0x4B), (4, 0x4A), (5, 0x4A), (7, 0x4E)] {
            let at = Self::cluster(cluster);
            img[at..at + Self::CLUSTER].fill(fill);
        }
        Self { image: img }
    }

    fn fat_entry(volume: &[u8], copy: usize, n: usize) -> u32 {
        let at = Self::fat(copy) + n * 4;
        u32::from_le_bytes(volume[at..at + 4].try_into().unwrap()) & 0x0FFF_FFFF
    }
}

#[test]
fn fat32_exclusions_remove_the_entries_and_leave_the_data_out() {
    for format in [ImageFormat::Vhdx, ImageFormat::Qcow2] {
        let dir = tempfile::tempdir().unwrap();
        let fat = Fat32::build();
        let source = unique_path("fat32");
        register_device(&source, fat.image.clone(), 512);
        let platform = TestPlatform::new().source(&source, TestSource::Direct { frozen: true });
        let image = dir.path().join(format!("full.{}", container(format)));

        let summary = run(
            &platform,
            &VolumeBackupRequest {
                exclusion_patterns: vec!["*.tmp".into()],
                ..request(&source, &image, format)
            },
        );

        assert!(summary.smart_mode);
        assert_eq!(summary.excluded_files, 2, "{format:?}");
        assert_eq!(summary.excluded_bytes, 3 * Fat32::CLUSTER as u64);
        assert_eq!(summary.file_scan_skipped, None);
        assert_eq!(summary.excluded_entries_kept, None);
        assert!(summary.categories.is_some());
        // Boot area and FATs (blocks 0-4), root, KEEP.BIN, TEMP: the excluded
        // files' clusters are not even read.
        assert_eq!(summary.used_blocks, 8);
        assert_eq!(summary.blocks_copied, 8);
        assert_verifies(&image, 8);

        let copied = volume_in(&image, fat.image.len());
        // The entries are deleted and the chains freed in both FATs…
        assert_eq!(copied[Fat32::cluster(2) + 32], 0xE5, "JUNK.TMP entry");
        assert_eq!(copied[Fat32::cluster(6)], 0xE5, "TEMP\\NOTE.TMP entry");
        for copy in 0..Fat32::FATS {
            for n in [4, 5, 7] {
                assert_eq!(Fat32::fat_entry(&copied, copy, n), 0, "FAT {copy}, cl {n}");
            }
            assert_eq!(Fat32::fat_entry(&copied, copy, 3), 0x0FFF_FFFF);
        }
        // …their data never entered the image, and what stays is intact.
        for n in [4, 5, 7] {
            let at = Fat32::cluster(n);
            assert!(copied[at..at + Fat32::CLUSTER].iter().all(|&b| b == 0));
        }
        let keep = Fat32::cluster(3);
        assert_eq!(
            copied[keep..keep + Fat32::CLUSTER],
            fat.image[keep..keep + Fat32::CLUSTER]
        );

        // Walked again, the image has nothing left to exclude.
        let back = unique_path("back");
        register_device(&back, copied, 512);
        let mut vol = MemDevice::open_read(&back).unwrap();
        let rescan =
            crate::fat_exclusions::scan_fat32_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(()))
                .unwrap();
        assert_eq!(rescan.files_excluded, 0);
        let kept =
            crate::fat_exclusions::scan_fat32_exclusions(&mut vol, &["*.bin".into()], |_| Ok(()))
                .unwrap();
        assert_eq!(kept.files_excluded, 1, "KEEP.BIN is still there");
    }
}

/// A platform whose parsers need a frozen source, given one that is not: the
/// FAT32 the frozen run above copies by its map — and excludes from — is
/// copied whole here, its metadata never read and its rules not applied, and
/// the result says why.
#[test]
fn a_source_not_held_still_is_copied_whole_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let fat = Fat32::build();
    let source = unique_path("live-fat32");
    register_device(&source, fat.image.clone(), 512);
    let platform = TestPlatform::new().source(&source, TestSource::Direct { frozen: false });
    let image = dir.path().join("full.qcow2");

    let summary = run(
        &platform,
        &VolumeBackupRequest {
            exclusion_patterns: vec!["*.tmp".into()],
            ..request(&source, &image, ImageFormat::Qcow2)
        },
    );

    let blocks = blocks_of(&fat.image, BS);
    assert!(!summary.smart_mode);
    assert_eq!(summary.used_blocks, blocks);
    assert_eq!(summary.blocks_copied, blocks);
    assert_eq!(summary.excluded_files, 0);
    assert!(summary.categories.is_none());
    assert_eq!(
        summary.file_scan_skipped.as_deref(),
        Some(&*own_backup_common::tr!("engine.skip_not_frozen"))
    );
    assert_same(
        &volume_in(&image, fat.image.len()),
        &fat.image,
        "copied as it is",
    );
    assert_verifies(&image, blocks);
    let sidecar = load_sidecar_of(&image).unwrap();
    assert_eq!(
        sidecar.manifest.backup_mode,
        own_backup_common::types::BackupMode::SectorBySector
    );
    assert_eq!(sidecar.info.filesystem, None, "nothing parsed it");
}

/// A dirty XFS log is taken as covered only when the filesystem was frozen as
/// it was captured. A device that merely holds still may carry the log of a
/// crash, whose btrees are behind: that one is copied whole.
#[test]
fn a_dirty_xfs_log_is_trusted_only_from_a_quiesced_source() {
    let dir = tempfile::tempdir().unwrap();
    let volume = crate::xfs_map::tests::tiny_xfs_image(true);
    let blocks = blocks_of(&volume, BS);

    let origin = unique_path("xfs-origin");
    let snap = unique_path("xfs-snap");
    register_device(&origin, vec![0; volume.len()], 512);
    register_device(&snap, volume.clone(), 512);
    let platform = TestPlatform::new().source(&origin, TestSource::Snapshot { read_path: snap });
    let image = dir.path().join("snapshot.qcow2");
    let summary = run(&platform, &request(&origin, &image, ImageFormat::Qcow2));
    assert!(summary.smart_mode, "a snapshot's dirty log is covered");
    assert!(summary.used_blocks < blocks);
    let sidecar = load_sidecar_of(&image).unwrap();
    assert_eq!(sidecar.info.filesystem.as_deref(), Some("xfs"));

    let idle = unique_path("xfs-idle");
    register_device(&idle, volume.clone(), 512);
    let platform = TestPlatform::new().source(&idle, TestSource::Direct { frozen: true });
    let image = dir.path().join("idle.qcow2");
    let summary = run(&platform, &request(&idle, &image, ImageFormat::Qcow2));
    assert!(
        !summary.smart_mode,
        "held still, never frozen: the log may be a crash's"
    );
    assert_eq!(summary.used_blocks, blocks);
    assert_same(&volume_in(&image, volume.len()), &volume, "copied whole");
}

#[test]
fn a_source_the_platform_does_not_hold_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = unique_path("unprepared");
    register_device(&source, fat16_volume(b"x"), 512);
    let image: PathBuf = dir.path().join("full.qcow2");
    let err = backup_volume_inner(
        &TestPlatform::new(),
        &request(&source, &image, ImageFormat::Qcow2),
        None,
        &image,
        |_| Ok(()),
    )
    .err()
    .expect("an unprepared source must not be read");
    assert!(
        matches!(
            err,
            own_backup_common::OwnBackupError::NoSnapshotProvider(_)
        ),
        "{err}"
    );
    assert!(!image.exists(), "nothing was written");
}

/// Every name in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn backup_volume_publishes_plain_and_encrypted_images_under_their_names_only() {
    let keys = crate::engine_core::testkit::test_keys();
    for encrypted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let volume = fat16_volume(b"published");
        let source = unique_path("published");
        register_device(&source, volume.clone(), 512);
        let platform = TestPlatform::new().source(&source, TestSource::Direct { frozen: true });
        let name = if encrypted { "full.obk" } else { "full.qcow2" };
        let image = dir.path().join(name);
        let mut req = request(&source, &image, ImageFormat::Qcow2);
        if encrypted {
            req.encryption = Some(keys.clone());
        }
        let mut phases = Vec::new();

        let summary = super::backup_volume(&platform, &req, |p| {
            phases.push(p.backup_id.clone());
            Ok(())
        })
        .unwrap();

        assert_eq!(summary.image_path, image, "encrypted: {encrypted}");
        assert_eq!(
            summary.sidecar_path,
            crate::sidecar::sidecar_path_for(&image)
        );
        // Asked last, after the copy and the final sidecar.
        assert_eq!(phases.last().map(String::as_str), Some("publish"));
        // Nothing private is left beside the published image.
        assert_eq!(names_in(dir.path()), [name], "encrypted: {encrypted}");
        let verified = crate::engine_core::verify::verify_image_with_keys(
            &image,
            req.encryption.as_ref(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(verified.valid(), "encrypted: {encrypted}: {verified:?}");
        assert_eq!(verified.blocks_checked, blocks_of(&volume, BS));
    }
}

#[test]
fn backup_volume_refused_at_publication_leaves_no_image() {
    let dir = tempfile::tempdir().unwrap();
    let source = unique_path("refused");
    register_device(&source, fat16_volume(b"refused"), 512);
    let platform = TestPlatform::new().source(&source, TestSource::Direct { frozen: true });
    let image = dir.path().join("full.qcow2");

    let err = super::backup_volume(
        &platform,
        &request(&source, &image, ImageFormat::Qcow2),
        |p| {
            if p.backup_id == "publish" {
                Err(own_backup_common::OwnBackupError::Cancelled)
            } else {
                Ok(())
            }
        },
    )
    .err()
    .expect("a refused publication fails the run");

    assert!(
        matches!(err, own_backup_common::OwnBackupError::Cancelled),
        "{err}"
    );
    assert!(
        names_in(dir.path()).is_empty(),
        "{:?}",
        names_in(dir.path())
    );
}
