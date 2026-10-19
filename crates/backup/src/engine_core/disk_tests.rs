//! The whole-disk pipeline driven through the test platform: an in-memory
//! disk with a GPT and two partitions is copied by `backup_disk` exactly as
//! Windows copies a real one, and the image is read back, checked and
//! compared — the first tests this pipeline has had (it was Windows-only code
//! until phase E).

use std::path::Path;

use own_backup_common::gpt::{DiskLayout, GptPartitionEntry};
use own_backup_common::types::{BackupType, ImageFormat};
use own_backup_common::OwnBackupError;

use super::backup_disk;
use crate::engine_core::disk_image::{DiskBackupRequest, DiskPartitionBackup};
use crate::engine_core::read::{load_sidecar_of, ImageReadSource};
use crate::engine_core::test_platform::{register_device, unique_path, TestPlatform, TestSource};
use crate::engine_core::verify::verify_image;
use crate::fat_writer::{build_fat16, FatFile};

const MIB: u64 = 1024 * 1024;
/// Whole qcow2 clusters, so a child holds only what changed.
const BS: u32 = 64 * 1024;
const DISK: u32 = 7;
const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B9B6D1C7C7";
const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// A test disk: its device, its GPT, and its two partitions — a FAT16 "ESP"
/// at 1 MiB and a raw data partition at 8 MiB.
struct TestDisk {
    device: String,
    layout: DiskLayout,
    esp: String,
    esp_data: Vec<u8>,
    data: String,
    data_bytes: Vec<u8>,
}

fn fat16(readme: &[u8]) -> Vec<u8> {
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
    build_fat16(&files, "OWNESP").unwrap()
}

/// A 4 MiB ext4 made by `mkfs.ext4 -d` (no mount, no root) holding
/// `keep.txt` and `junk.tmp` (`junk` bytes of 0x5A); `None` without the tool.
fn ext4_partition(junk: usize) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let content = dir.path().join("content");
    std::fs::create_dir(&content).unwrap();
    std::fs::write(content.join("keep.txt"), b"kept\n").unwrap();
    std::fs::write(content.join("junk.tmp"), vec![0x5A; junk]).unwrap();
    let img = dir.path().join("fs.img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(4 * MIB)
        .unwrap();
    let made = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-d"])
        .arg(&content)
        .arg(&img)
        .output()
        .ok()?
        .status
        .success();
    if !made {
        return None;
    }
    Some(std::fs::read(&img).unwrap())
}

fn data_partition(seed: u8) -> Vec<u8> {
    (0..4 * MIB as u32)
        .map(|i| (i / 4096) as u8 ^ seed ^ (i % 7) as u8)
        .collect()
}

impl TestDisk {
    fn new(esp_data: Vec<u8>, data_bytes: Vec<u8>) -> Self {
        let size = 16 * MIB;
        let esp_lba = MIB / 512;
        let data_lba = 8 * MIB / 512;
        // The ESP slot is 6 MiB; the FAT image is padded up to it.
        let mut esp_data = esp_data;
        assert!(esp_data.len() as u64 <= 6 * MIB);
        esp_data.resize(6 * MIB as usize, 0);
        let layout = DiskLayout {
            disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
            sector_size: 512,
            size_bytes: size,
            partitions: vec![
                GptPartitionEntry {
                    index: 1,
                    type_guid: ESP.into(),
                    part_guid: "11111111-0000-0000-0000-000000000001".into(),
                    first_lba: esp_lba,
                    last_lba: esp_lba + esp_data.len() as u64 / 512 - 1,
                    attributes: 0,
                    name: "EFI system partition".into(),
                },
                GptPartitionEntry {
                    index: 2,
                    type_guid: BASIC_DATA.into(),
                    part_guid: "11111111-0000-0000-0000-000000000002".into(),
                    first_lba: data_lba,
                    last_lba: data_lba + data_bytes.len() as u64 / 512 - 1,
                    attributes: 0,
                    name: "data".into(),
                },
            ],
        };
        let disk = Self {
            device: unique_path("disk"),
            layout,
            esp: unique_path("esp"),
            esp_data,
            data: unique_path("data"),
            data_bytes,
        };
        disk.register();
        disk
    }

    /// (Re-)register the disk and its partitions as they are now.
    fn register(&self) {
        let mut bytes = vec![0u8; self.layout.size_bytes as usize];
        for op in
            crate::gpt_writer::disk_gpt_write_ops(&self.layout, self.layout.size_bytes).unwrap()
        {
            let at = op.offset as usize;
            bytes[at..at + op.bytes.len()].copy_from_slice(&op.bytes);
        }
        for (offset, data) in [
            (self.esp_offset(), &self.esp_data),
            (self.data_offset(), &self.data_bytes),
        ] {
            bytes[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        }
        register_device(&self.device, bytes, 512);
        register_device(&self.esp, self.esp_data.clone(), 512);
        register_device(&self.data, self.data_bytes.clone(), 512);
    }

    fn esp_offset(&self) -> u64 {
        self.layout.partitions[0].first_lba * 512
    }

    fn data_offset(&self) -> u64 {
        self.layout.partitions[1].first_lba * 512
    }

    /// The platform that holds both partitions as `how` and knows their slots.
    fn platform(&self, how: TestSource) -> TestPlatform {
        TestPlatform::new()
            .source(&self.esp, how.clone())
            .source(&self.data, how)
            .slot(&self.esp, DISK, self.esp_offset())
            .slot(&self.data, DISK, self.data_offset())
    }

    fn request(&self, image: &Path, format: ImageFormat) -> DiskBackupRequest {
        DiskBackupRequest {
            disk_number: DISK,
            disk_device: Some(self.device.clone()),
            disk_model: "Test disk".into(),
            partitions: vec![
                DiskPartitionBackup {
                    device: self.esp.clone(),
                    part_index: 1,
                    passphrase: None,
                },
                DiskPartitionBackup {
                    device: self.data.clone(),
                    part_index: 2,
                    passphrase: None,
                },
            ],
            image_path: image.to_path_buf(),
            full_image_path: None,
            block_size: BS,
            image_format: format,
            job_id: Some("disk-tests".into()),
            encryption: None,
            exclusion_patterns_windows: Vec::new(),
            exclusion_patterns_linux: Vec::new(),
            kind: BackupType::Full,
            parent_image: None,
            source_disk: None,
            bootable: false,
            zstd_level: 0,
        }
    }
}

fn container(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Vhdx => "vhdx",
        ImageFormat::Qcow2 => "qcow2",
    }
}

/// `len` bytes of the image at `offset`.
fn image_bytes(image: &Path, offset: u64, len: usize) -> Vec<u8> {
    let sidecar = load_sidecar_of(image).unwrap();
    let mut reader = ImageReadSource::open(image, &sidecar.info.container).unwrap();
    let mut out = vec![0u8; len];
    reader.read_at(offset, &mut out).unwrap();
    out
}

#[track_caller]
fn assert_same(a: &[u8], b: &[u8], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: length");
    if let Some(at) = a.iter().zip(b).position(|(x, y)| x != y) {
        panic!("{what}: first difference at byte {at}");
    }
}

fn run(
    platform: &TestPlatform,
    req: &DiskBackupRequest,
) -> own_backup_common::Result<super::DiskBackupSummary> {
    backup_disk(platform, req, |_| Ok(()))
}

#[test]
fn a_disk_is_copied_with_its_partitions_where_they_lie() {
    for format in [ImageFormat::Qcow2, ImageFormat::Vhdx] {
        let dir = tempfile::tempdir().unwrap();
        let ext4 = ext4_partition(300_000);
        let parsed = ext4.is_some();
        let disk = TestDisk::new(
            fat16(&[b'1'; 20_000]),
            ext4.unwrap_or_else(|| data_partition(1)),
        );
        let platform = disk.platform(TestSource::Direct { frozen: true });
        let image = dir.path().join(format!("disk.{}", container(format)));
        let summary = run(&platform, &disk.request(&image, format)).unwrap();

        assert_eq!(summary.image_path, image);
        assert_eq!(summary.partitions, 2);
        assert_eq!(summary.backup_kind, BackupType::Full);
        // Each partition is in the image at its own offset.
        let esp = image_bytes(&image, disk.esp_offset(), disk.esp_data.len());
        assert_same(&esp, &disk.esp_data, "the ESP");
        let data = image_bytes(&image, disk.data_offset(), disk.data_bytes.len());
        assert_same(&data, &disk.data_bytes, "the data partition");
        // ext4 is read through its own map: its free space was not copied.
        // (FAT16 has no map of its own and is copied whole.)
        if parsed {
            let whole = (disk.esp_data.len() + disk.data_bytes.len()) as u64;
            assert!(summary.bytes_copied < whole, "{}", summary.bytes_copied);
        }

        // The image's GPT has the slots and fresh identities; the manifest
        // keeps the real ones for a restore.
        let head = image_bytes(&image, 0, 34 * 512);
        let in_image =
            crate::gpt_reader::parse_disk_layout(&head, 512, summary_size(&image)).unwrap();
        for (a, b) in in_image.partitions.iter().zip(&disk.layout.partitions) {
            assert_eq!(
                (a.index, a.first_lba, a.last_lba),
                (b.index, b.first_lba, b.last_lba)
            );
            assert_ne!(a.part_guid, b.part_guid, "a fresh GUID, never the source's");
        }
        let sidecar = load_sidecar_of(&image).unwrap();
        let manifest = sidecar.disk_manifest.as_ref().expect("a disk manifest");
        assert_eq!(manifest.disk_number, DISK);
        assert_eq!(manifest.layout.partitions, disk.layout.partitions);
        let fs: Vec<_> = manifest
            .images
            .iter()
            .map(|i| i.filesystem.as_str())
            .collect();
        assert_eq!(fs, ["unknown", if parsed { "ext4" } else { "unknown" }]);
        assert_eq!(sidecar.manifest.source_os, "test");
        assert_eq!(sidecar.manifest.source_disk.device_id, disk.device);
        assert_eq!(sidecar.info.job_id.as_deref(), Some("disk-tests"));
        assert_eq!(sidecar.info.volume_data_offset, 0, "disk-absolute blocks");
        // Nobody walked the files (no rules), and the result says so.
        assert!(summary.file_scan_skipped.is_some());
        let v = verify_image(&image, |_, _| Ok(())).unwrap();
        assert!(v.valid(), "{format}: {:?}", v.mismatched_blocks);
        // The only file in the folder: nothing private is left beside it.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

fn summary_size(image: &Path) -> u64 {
    load_sidecar_of(image)
        .unwrap()
        .manifest
        .source_disk
        .size_bytes
}

#[test]
fn an_increment_of_a_disk_holds_only_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let mut disk = TestDisk::new(fat16(&[b'1'; 20_000]), data_partition(1));
    let platform = disk.platform(TestSource::Direct { frozen: true });
    let full = dir.path().join("full.qcow2");
    let first = run(&platform, &disk.request(&full, ImageFormat::Qcow2)).unwrap();

    // One block of the data partition changes.
    let old = disk.data_bytes.clone();
    disk.data_bytes[3 * BS as usize + 10] ^= 0xFF;
    disk.register();
    let inc = dir.path().join("inc.qcow2");
    let second = run(
        &platform,
        &DiskBackupRequest {
            kind: BackupType::Incremental,
            parent_image: Some(full.clone()),
            ..disk.request(&inc, ImageFormat::Qcow2)
        },
    )
    .unwrap();
    assert_eq!(second.backup_kind, BackupType::Incremental);
    assert_eq!(second.blocks_copied, 1);
    assert_eq!(
        second.blocks_unchanged + second.blocks_copied,
        first.blocks_copied
    );
    let data = image_bytes(&inc, disk.data_offset(), disk.data_bytes.len());
    assert_same(&data, &disk.data_bytes, "the child");
    let data = image_bytes(&full, disk.data_offset(), old.len());
    assert_same(&data, &old, "the parent is untouched");
    assert!(verify_image(&inc, |_, _| Ok(())).unwrap().valid());
    let sidecar = load_sidecar_of(&inc).unwrap();
    assert_eq!(
        sidecar.info.parent_image_file_name.as_deref(),
        Some("full.qcow2")
    );

    // Re-partitioned since: the child would land in yesterday's slots — a
    // full copy instead.
    disk.layout.partitions[1].name = "renamed".into();
    disk.layout.partitions[0].last_lba -= 2048;
    disk.esp_data.truncate(disk.esp_data.len() - MIB as usize);
    disk.register();
    let again = dir.path().join("again.qcow2");
    let third = run(
        &platform,
        &DiskBackupRequest {
            kind: BackupType::Incremental,
            parent_image: Some(inc.clone()),
            ..disk.request(&again, ImageFormat::Qcow2)
        },
    )
    .unwrap();
    assert_eq!(third.backup_kind, BackupType::Full);
}

#[test]
fn exclusions_apply_per_partition_by_filesystem_family() {
    let Some(ext4) = ext4_partition(300_000) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let disk = TestDisk::new(fat16(&[b'X'; 20_000]), ext4);
    let platform = disk.platform(TestSource::Direct { frozen: true });
    let image = dir.path().join("disk.qcow2");
    let summary = run(
        &platform,
        &DiskBackupRequest {
            // The Windows rule would take keep.txt — it is not for ext.
            exclusion_patterns_windows: vec!["*.txt".into()],
            exclusion_patterns_linux: vec!["*.tmp".into()],
            ..disk.request(&image, ImageFormat::Qcow2)
        },
    )
    .unwrap();
    assert_eq!(summary.excluded_files, 1);
    assert_eq!(summary.excluded_bytes, 300_000);
    let data = image_bytes(&image, disk.data_offset(), disk.data_bytes.len());
    assert!(
        !data.windows(4096).any(|w| w.iter().all(|&b| b == 0x5A)),
        "the excluded file's data is not in the image"
    );
    // The FAT16 partition has no parser here: named in the result.
    let skipped = summary.file_scan_skipped.unwrap_or_default();
    assert!(skipped.contains('1'), "{skipped}");
    assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());
}

/// On a platform whose parsers need a source that holds still, a partition
/// that may be written is copied whole and the result says so.
#[test]
fn a_partition_not_held_still_is_copied_whole() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ext4) = ext4_partition(300_000) else {
        return;
    };
    let disk = TestDisk::new(fat16(&[b'1'; 20_000]), ext4);
    let platform = disk.platform(TestSource::Direct { frozen: false });
    let image = dir.path().join("disk.qcow2");
    let summary = run(
        &platform,
        &DiskBackupRequest {
            exclusion_patterns_linux: vec!["*.tmp".into()],
            ..disk.request(&image, ImageFormat::Qcow2)
        },
    )
    .unwrap();
    assert_eq!(summary.excluded_files, 0);
    let skipped = summary.file_scan_skipped.unwrap();
    assert!(
        skipped.contains(&*own_backup_common::tr!("engine.skip_not_frozen")),
        "{skipped}"
    );
    let sidecar = load_sidecar_of(&image).unwrap();
    let fs: Vec<_> = sidecar
        .disk_manifest
        .unwrap()
        .images
        .iter()
        .map(|i| i.filesystem.clone())
        .collect();
    assert_eq!(fs, ["unknown", "unknown"], "nothing parsed");
    let data = image_bytes(&image, disk.data_offset(), disk.data_bytes.len());
    assert_same(&data, &disk.data_bytes, "copied whole, the junk included");
}

/// The job names each partition twice — by slot and by device. When they
/// disagree the copy stops: it would restore a disk nobody had.
#[test]
fn a_partition_in_another_slot_is_refused_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let disk = TestDisk::new(fat16(&[b'1'; 20_000]), data_partition(4));
    let platform = TestPlatform::new()
        .source(&disk.esp, TestSource::Direct { frozen: true })
        .source(&disk.data, TestSource::Direct { frozen: true })
        .slot(&disk.esp, DISK, disk.esp_offset())
        .slot(&disk.data, DISK, disk.esp_offset());
    let image = dir.path().join("disk.qcow2");
    let err = run(&platform, &disk.request(&image, ImageFormat::Qcow2)).err();
    assert!(matches!(err, Some(OwnBackupError::Config(_))), "{err:?}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
