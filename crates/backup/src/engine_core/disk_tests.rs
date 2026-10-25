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
    assert!(skipped.contains('1'), "{skipped:?}");
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
        "{skipped:?}"
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

/// Decision 6/E4 in a whole disk: the partition holding the service's key
/// directory is copied with it left out though the job has no rules; when
/// that partition cannot be walked, the disk is not copied at all.
#[test]
fn the_key_directory_is_left_out_of_a_whole_disk_too() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ext4) = ext4_partition(300_000) else {
        return;
    };
    let disk = TestDisk::new(fat16(&[b'1'; 20_000]), ext4);
    // Walked: the rule runs on the ext4 partition (nothing there carries the
    // name) and the summary says the directory is not in the copy.
    let platform = disk
        .platform(TestSource::Direct { frozen: true })
        .key_store_on(&disk.data);
    let image = dir.path().join("disk.qcow2");
    let summary = run(&platform, &disk.request(&image, ImageFormat::Qcow2)).unwrap();
    assert!(summary.key_store_excluded);
    let data = image_bytes(&image, disk.data_offset(), disk.data_bytes.len());
    assert_same(&data, &disk.data_bytes, "nothing else is touched");

    // Not held still: refused, nothing published.
    let platform = disk
        .platform(TestSource::Direct { frozen: false })
        .key_store_on(&disk.data);
    let refused = dir.path().join("refused.qcow2");
    let e = run(&platform, &disk.request(&refused, ImageFormat::Qcow2))
        .err()
        .expect("refused");
    assert!(e.to_string().contains(&disk.data), "{e}");
    assert!(!refused.exists());

    // The key directory on the FAT16 ESP, which no scanner reads: refused.
    let platform = disk
        .platform(TestSource::Direct { frozen: true })
        .key_store_on(&disk.esp);
    let e = run(&platform, &disk.request(&refused, ImageFormat::Qcow2))
        .err()
        .expect("refused");
    assert!(e.to_string().contains(&disk.esp), "{e}");
}

// ---------------------------------------------------------------------------
// A partition that is an LVM physical volume in use
// ---------------------------------------------------------------------------

mod physical_volume {
    use super::*;
    use crate::engine_core::platform::{PhysicalVolume, PvVolume};
    use own_backup_common::lvm_pv::fixtures::{label, mda, vg_text};
    use own_backup_common::lvm_pv::PvSegment;

    const LINUX_LVM: &str = "E6D6D379-F507-44C2-A23C-238F2A3DF928";
    /// The PV: 24 MiB at 8 MiB on the disk; 4 MiB extents from 1 MiB, five
    /// of them (1..21 MiB), then a 3 MiB tail.
    const PV_AT: u64 = 8 * MIB;
    const PV_LEN: u64 = 24 * MIB;
    const EXT: u64 = 4 * MIB;
    const PE_START: u64 = MIB;

    fn extent(n: u64) -> u64 {
        PE_START + n * EXT
    }

    /// The disk: an ESP and the PV. Logical volumes: `root` on extents 0-1
    /// and `data` on extent 3, both active (read from their snapshots);
    /// `idle` on extent 4, inactive (read from the partition); extent 2 free.
    struct PvDisk {
        device: String,
        esp: String,
        part: String,
        /// The partition as it is now, written to while it is copied.
        live: Vec<u8>,
        root: (String, Vec<u8>),
        data: (String, Vec<u8>),
    }

    impl PvDisk {
        fn new(root: Vec<u8>, metadata_readable: bool) -> Self {
            Self::build(root, metadata_readable, None)
        }

        /// The PV inside a LUKS container (LUKS → LVM) opened with `pass`:
        /// `part` is then the container's mapping, `container` the partition.
        fn in_luks(root: Vec<u8>, pass: &str) -> (Self, String, Vec<u8>) {
            let mut made = None;
            let disk = Self::build(root, true, Some((pass, &mut made)));
            let (path, bytes) = made.expect("the container");
            (disk, path, bytes)
        }

        #[allow(clippy::type_complexity)]
        fn build(
            root: Vec<u8>,
            metadata_readable: bool,
            luks: Option<(&str, &mut Option<(String, Vec<u8>)>)>,
        ) -> Self {
            assert_eq!(root.len() as u64, 2 * EXT);
            let text = vg_text(
                5,
                &[
                    ("root", vec![("striped", vec![("pv0", 0)], 2)]),
                    ("data", vec![("striped", vec![("pv0", 3)], 1)]),
                    ("idle", vec![("striped", vec![("pv0", 4)], 1)]),
                ],
            );
            let mut live: Vec<u8> = (0..PV_LEN).map(|i| (i / 512 % 241) as u8 ^ 0x33).collect();
            let head = label(PV_LEN, PE_START, &[(4096, PE_START - 4096)]);
            live[..head.len()].copy_from_slice(&head);
            let mut area = mda(PE_START - 4096, 8192, &text);
            if !metadata_readable {
                area[4] ^= 0xFF;
            }
            live[4096..PE_START as usize].copy_from_slice(&area);
            // What the volumes hold on the live partition by now: not what
            // their snapshots froze.
            live[extent(0) as usize..extent(2) as usize].fill(0xEE);
            live[extent(2) as usize..extent(3) as usize].fill(0xFF);
            live[extent(3) as usize..extent(4) as usize].fill(0xDD);
            let data: Vec<u8> = (0..EXT).map(|i| (i % 253) as u8 ^ 0x5C).collect();
            // The partition: the PV itself, or the container around it.
            let container = luks
                .as_ref()
                .map(|(pass, _)| crate::luks::test_luks1_container(pass.as_bytes(), &live));
            let on_part: Vec<u8> = container.clone().unwrap_or_else(|| live.clone());
            let slot = on_part.len() as u64;

            let mut esp_data = fat16(&[b'1'; 20_000]);
            esp_data.resize(6 * MIB as usize, 0);
            let layout = DiskLayout {
                disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
                sector_size: 512,
                size_bytes: PV_AT + slot + MIB,
                partitions: vec![
                    GptPartitionEntry {
                        index: 1,
                        type_guid: ESP.into(),
                        part_guid: "11111111-0000-0000-0000-000000000001".into(),
                        first_lba: MIB / 512,
                        last_lba: 7 * MIB / 512 - 1,
                        attributes: 0,
                        name: "EFI system partition".into(),
                    },
                    GptPartitionEntry {
                        index: 2,
                        type_guid: LINUX_LVM.into(),
                        part_guid: "11111111-0000-0000-0000-000000000002".into(),
                        first_lba: PV_AT / 512,
                        last_lba: (PV_AT + slot) / 512 - 1,
                        attributes: 0,
                        name: "lvm".into(),
                    },
                ],
            };
            let disk = Self {
                device: unique_path("disk"),
                esp: unique_path("esp"),
                part: unique_path("pv"),
                live,
                root: (unique_path("root-snap"), root),
                data: (unique_path("data-snap"), data),
            };
            let mut bytes = vec![0u8; layout.size_bytes as usize];
            for op in crate::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap() {
                let at = op.offset as usize;
                bytes[at..at + op.bytes.len()].copy_from_slice(&op.bytes);
            }
            bytes[MIB as usize..7 * MIB as usize].copy_from_slice(&esp_data);
            bytes[PV_AT as usize..(PV_AT + slot) as usize].copy_from_slice(&on_part);
            register_device(&disk.device, bytes, 512);
            register_device(&disk.esp, esp_data, 512);
            register_device(&disk.part, disk.live.clone(), 512);
            register_device(&disk.root.0, disk.root.1.clone(), 512);
            register_device(&disk.data.0, disk.data.1.clone(), 512);
            if let (Some((_, made)), Some(container)) = (luks, container) {
                let path = unique_path("luks-part");
                register_device(&path, container.clone(), 512);
                *made = Some((path, container));
            }
            disk
        }

        fn pv(&self, key_store: Option<&str>) -> PhysicalVolume {
            let volume = |name: &str, read: &str, len: u64, at: u64| PvVolume {
                name: format!("vg0/{name}"),
                read_path: read.to_string(),
                len,
                segments: vec![PvSegment {
                    pv_offset: at,
                    lv_offset: 0,
                    length: len,
                }],
                frozen: true,
                quiesced: true,
                key_store: key_store == Some(name),
            };
            PhysicalVolume {
                volumes: vec![
                    volume("root", &self.root.0, 2 * EXT, extent(0)),
                    volume("data", &self.data.0, EXT, extent(3)),
                ],
            }
        }

        fn platform(&self, pv: PhysicalVolume) -> TestPlatform {
            TestPlatform::new()
                .source(&self.esp, TestSource::Direct { frozen: true })
                .source(&self.part, TestSource::PhysicalVolume(pv))
                .slot(&self.esp, DISK, MIB)
                .slot(&self.part, DISK, PV_AT)
        }

        fn request(&self, image: &Path) -> DiskBackupRequest {
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
                        device: self.part.clone(),
                        part_index: 2,
                        passphrase: None,
                    },
                ],
                image_path: image.to_path_buf(),
                full_image_path: None,
                block_size: BS,
                image_format: ImageFormat::Qcow2,
                job_id: Some("disk-pv-tests".into()),
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

        /// The partition's bytes `from..to` in `image`.
        fn in_image(&self, image: &Path, from: u64, to: u64) -> Vec<u8> {
            image_bytes(image, PV_AT + from, (to - from) as usize)
        }

        /// Does the image store any block of the partition's `from..to`?
        fn stored(&self, image: &Path, from: u64, to: u64) -> bool {
            let bs = BS as u64;
            let (first, last) = ((PV_AT + from) / bs, (PV_AT + to - 1) / bs);
            load_sidecar_of(image)
                .unwrap()
                .checksums
                .iter()
                .any(|c| (first..=last).contains(&c.block_number))
        }
    }

    fn raw_root() -> Vec<u8> {
        (0..2 * EXT).map(|i| (i % 249) as u8 ^ 0xA7).collect()
    }

    /// An 8 MiB ext4 holding `keep.txt` and `junk.tmp` (300 000 bytes of
    /// 0x5A); `None` without mkfs.ext4.
    fn ext4_root() -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir(&content).unwrap();
        std::fs::write(content.join("keep.txt"), b"kept\n").unwrap();
        std::fs::write(content.join("junk.tmp"), vec![0x5A; 300_000]).unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(2 * EXT)
            .unwrap();
        let made = std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-b", "4096", "-d"])
            .arg(&content)
            .arg(&img)
            .output()
            .ok()?
            .status
            .success();
        made.then(|| std::fs::read(&img).unwrap())
    }

    /// The partition comes out as LVM keeps it: its label and metadata as
    /// they are, each active volume as its snapshot froze it, the inactive
    /// one and the tail from the partition, and the free extent not at all.
    #[test]
    fn a_physical_volume_is_assembled_from_its_volumes_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let disk = PvDisk::new(raw_root(), true);
        let platform = disk.platform(disk.pv(None));
        let image = dir.path().join("disk.qcow2");
        let summary = run(&platform, &disk.request(&image)).unwrap();
        assert_eq!(summary.partitions, 2);

        assert_same(
            &disk.in_image(&image, 0, PE_START),
            &disk.live[..PE_START as usize],
            "the label and metadata area",
        );
        assert_same(
            &disk.in_image(&image, extent(0), extent(2)),
            &disk.root.1,
            "root, as its snapshot holds it",
        );
        assert_same(
            &disk.in_image(&image, extent(3), extent(4)),
            &disk.data.1,
            "data, as its snapshot holds it",
        );
        assert_same(
            &disk.in_image(&image, extent(4), PV_LEN),
            &disk.live[extent(4) as usize..],
            "the inactive volume and the tail, from the partition",
        );
        assert!(
            !disk.stored(&image, extent(2), extent(3)),
            "the free extent is not copied"
        );
        assert!(disk
            .in_image(&image, extent(2), extent(3))
            .iter()
            .all(|&b| b == 0));

        let sidecar = load_sidecar_of(&image).unwrap();
        let fs: Vec<_> = sidecar
            .disk_manifest
            .unwrap()
            .images
            .iter()
            .map(|i| i.filesystem.clone())
            .collect();
        assert_eq!(fs, ["unknown", "LVM2_member"]);
        // Neither volume's files were walked (nothing parsed them): each is
        // named in the result.
        let skipped = summary.file_scan_skipped.unwrap();
        assert!(
            skipped.contains("vg0/root") && skipped.contains("vg0/data"),
            "{skipped:?}"
        );
        assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// LUKS → LVM: the PV is the mapping of a container the system opened.
    /// Its volumes come from their snapshots as on a plain PV, the image
    /// holds the PV in the clear where the partition starts, and the
    /// container's header is kept for the restore to put back (D19).
    #[test]
    fn a_physical_volume_inside_an_opened_container_keeps_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let (disk, part, container) = PvDisk::in_luks(raw_root(), "pv words");
        let header = (container.len() as u64 - PV_LEN) as usize;
        let platform = TestPlatform::new()
            .source(&disk.esp, TestSource::Direct { frozen: true })
            .source(
                &part,
                TestSource::Opened {
                    mapping: disk.part.clone(),
                    data_offset: header as u64,
                    pv: Some(disk.pv(None)),
                },
            )
            .slot(&disk.esp, DISK, MIB)
            .slot(&part, DISK, PV_AT);
        let image = dir.path().join("disk.qcow2");
        let mut req = disk.request(&image);
        req.partitions[1].device = part.clone();
        run(&platform, &req).unwrap();

        assert_same(
            &disk.in_image(&image, 0, PE_START),
            &disk.live[..PE_START as usize],
            "the label and metadata area, in the clear",
        );
        assert_same(
            &disk.in_image(&image, extent(0), extent(2)),
            &disk.root.1,
            "root, as its snapshot holds it",
        );
        assert_same(
            &disk.in_image(&image, extent(4), PV_LEN),
            &disk.live[extent(4) as usize..],
            "the inactive volume and the tail, from the mapping",
        );
        let sidecar = load_sidecar_of(&image).unwrap();
        let [h] = sidecar.info.luks_headers.as_slice() else {
            panic!("one header: {}", sidecar.info.luks_headers.len());
        };
        assert_eq!((h.part_index, h.data_offset), (2, header as u64));
        assert!(crate::luks::decode_stored_header(h).unwrap() == container[..header]);
        let dm = sidecar.disk_manifest.unwrap();
        assert_eq!(dm.images[1].filesystem, "LVM2_member");
        let enc = dm.images[1].encryption.as_ref().expect("the container");
        assert_eq!(
            enc.capture_mode,
            own_backup_common::image::VolumeCaptureMode::Plaintext
        );
        assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());
    }

    /// The partition's metadata area with `text` in it.
    fn put_metadata(live: &mut [u8], text: &str) {
        live[4096..PE_START as usize].copy_from_slice(&mda(PE_START - 4096, 8192, text));
    }

    /// The partition's page cache does not see what device-mapper wrote under
    /// it: an inactive volume written (activated, written, deactivated) since
    /// the cache was filled, and metadata that then still called its extent
    /// free. The copy drops the cache first and takes the device as it is.
    #[test]
    fn what_the_partition_cache_kept_from_before_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let disk = PvDisk::new(raw_root(), true);
        let mut stale = disk.live.clone();
        stale[extent(4) as usize..extent(5) as usize].fill(0x44);
        put_metadata(
            &mut stale,
            &vg_text(
                5,
                &[
                    ("root", vec![("striped", vec![("pv0", 0)], 2)]),
                    ("data", vec![("striped", vec![("pv0", 3)], 1)]),
                ],
            ),
        );
        crate::engine_core::test_platform::register_stale_cache(&disk.part, stale);
        let image = dir.path().join("disk.qcow2");
        run(&disk.platform(disk.pv(None)), &disk.request(&image)).unwrap();
        assert_same(
            &disk.in_image(&image, extent(4), extent(5)),
            &disk.live[extent(4) as usize..extent(5) as usize],
            "the inactive volume as the device holds it now",
        );
        assert!(disk.stored(&image, extent(4), extent(5)));
    }

    /// A child image over a PV whose allocation changed: `data` (extent 3)
    /// was removed and `new` created on the extent that was free (2). The
    /// new volume's data is in the child over the parent's nothing; the
    /// freed extent is not part of the child's state — its parent bytes
    /// are inherited and never restored — and the chain verifies.
    #[test]
    fn a_child_follows_extents_freed_and_taken_since_its_parent() {
        for kind in [BackupType::Incremental, BackupType::Differential] {
            let dir = tempfile::tempdir().unwrap();
            let mut disk = PvDisk::new(raw_root(), true);
            let full = dir.path().join("full.qcow2");
            run(&disk.platform(disk.pv(None)), &disk.request(&full)).unwrap();
            assert!(
                disk.stored(&full, extent(3), extent(4)),
                "data in the parent"
            );

            // Since then: data removed, new on extent 2, idle still on 4.
            put_metadata(
                &mut disk.live,
                &vg_text(
                    5,
                    &[
                        ("root", vec![("striped", vec![("pv0", 0)], 2)]),
                        ("new", vec![("striped", vec![("pv0", 2)], 1)]),
                        ("idle", vec![("striped", vec![("pv0", 4)], 1)]),
                    ],
                ),
            );
            register_device(&disk.part, disk.live.clone(), 512);
            let new_snap = unique_path("new-snap");
            let new_data: Vec<u8> = (0..EXT).map(|i| (i % 239) as u8 ^ 0x6B).collect();
            register_device(&new_snap, new_data.clone(), 512);
            let mut pv = disk.pv(None);
            pv.volumes[1] = PvVolume {
                name: "vg0/new".into(),
                read_path: new_snap,
                len: EXT,
                segments: vec![PvSegment {
                    pv_offset: extent(2),
                    lv_offset: 0,
                    length: EXT,
                }],
                frozen: true,
                quiesced: true,
                key_store: false,
            };
            let child = dir.path().join("child.qcow2");
            let sum = run(
                &disk.platform(pv),
                &DiskBackupRequest {
                    kind,
                    parent_image: Some(full.clone()),
                    ..disk.request(&child)
                },
            )
            .unwrap();
            assert_eq!(sum.backup_kind, kind);
            assert!(sum.blocks_copied > 0);
            assert_same(
                &disk.in_image(&child, extent(2), extent(3)),
                &new_data,
                "the new volume, over the parent's free extent",
            );
            assert!(disk.stored(&child, extent(2), extent(3)));
            assert!(
                !disk.stored(&child, extent(3), extent(4)),
                "{kind}: the freed extent is no longer part of the image's state"
            );
            assert_same(
                &disk.in_image(&child, extent(0), extent(2)),
                &disk.root.1,
                "root, unchanged through the chain",
            );
            assert!(verify_image(&child, |_, _| Ok(())).unwrap().valid());
        }
    }

    /// A volume group that also lies on a PV outside this copy is copied,
    /// and the run's summary says the group restored alone is incomplete.
    #[test]
    fn a_group_with_physical_volumes_elsewhere_is_named_in_the_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = PvDisk::new(raw_root(), true);
        let image = dir.path().join("alone.qcow2");
        let sum = run(&disk.platform(disk.pv(None)), &disk.request(&image)).unwrap();
        assert!(sum.notes.is_empty(), "{:?}", sum.notes);

        let text = vg_text(
            5,
            &[
                ("root", vec![("striped", vec![("pv0", 0)], 2)]),
                ("data", vec![("striped", vec![("pv0", 3)], 1)]),
                ("idle", vec![("striped", vec![("pv0", 4)], 1)]),
            ],
        )
        .replace(
            "physical_volumes {\n",
            "physical_volumes {\npv1 {\nid = \"Other0-0000-0000-0000-0000-0000-000000\"\n\
             pe_start = 2048\npe_count = 10\n}\n",
        );
        put_metadata(&mut disk.live, &text);
        register_device(&disk.part, disk.live.clone(), 512);
        let image = dir.path().join("spread.qcow2");
        let sum = run(&disk.platform(disk.pv(None)), &disk.request(&image)).unwrap();
        assert_eq!(sum.notes.len(), 1, "{:?}", sum.notes);
        assert!(sum.notes[0].contains("vg0"), "{:?}", sum.notes[0]);
        assert!(
            !disk.stored(&image, extent(2), extent(3)),
            "its metadata believed"
        );

        // That other PV is in the copy after all — as a plain partition, no
        // volume of it active: nothing to say.
        let mut other = label(6 * MIB, PE_START, &[(4096, PE_START - 4096)]);
        let uuid = "Other0-0000-0000-0000-0000-0000-000000".replace('-', "");
        other[512 + 32..512 + 64].copy_from_slice(uuid.as_bytes());
        let crc = own_backup_common::lvm_pv::calc_crc(0xf597_a6cf, &other[512 + 20..1024]);
        other[512 + 16..512 + 20].copy_from_slice(&crc.to_le_bytes());
        other.resize(6 * MIB as usize, 0);
        register_device(&disk.esp, other, 512);
        let image = dir.path().join("both.qcow2");
        let sum = run(&disk.platform(disk.pv(None)), &disk.request(&image)).unwrap();
        assert!(sum.notes.is_empty(), "{:?}", sum.notes);
    }

    /// A volume group on two partitions of one disk, a volume on both: the
    /// volume is scanned once, its exclusions apply on both partitions and
    /// its figures are counted once.
    #[test]
    fn a_volume_on_two_partitions_is_scanned_and_counted_once() {
        let Some(ext4) = ext4_root() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let half = EXT as usize;
        let (p1_at, p2_at, part_len) = (MIB, 14 * MIB, 12 * MIB);
        let pv_part = |data: &[u8]| {
            let mut live = vec![0x33u8; part_len as usize];
            live[..own_backup_common::lvm_pv::LABEL_SCAN_BYTES].copy_from_slice(&label(
                part_len,
                PE_START,
                &[(4096, PE_START - 4096)],
            ));
            // Unreadable metadata: nothing is free, the parts are what counts.
            live[PE_START as usize..PE_START as usize + half].copy_from_slice(data);
            live
        };
        let (p1, p2) = (unique_path("pv1"), unique_path("pv2"));
        let (live1, live2) = (
            pv_part(&[0xEE; EXT as usize]),
            pv_part(&[0xEE; EXT as usize]),
        );
        register_device(&p1, live1.clone(), 512);
        register_device(&p2, live2.clone(), 512);
        let snap = unique_path("span-snap");
        register_device(&snap, ext4.clone(), 512);
        let layout = DiskLayout {
            disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
            sector_size: 512,
            size_bytes: 28 * MIB,
            partitions: [(1u32, p1_at), (2, p2_at)]
                .iter()
                .map(|&(index, at)| GptPartitionEntry {
                    index,
                    type_guid: LINUX_LVM.into(),
                    part_guid: format!("11111111-0000-0000-0000-00000000000{index}"),
                    first_lba: at / 512,
                    last_lba: (at + part_len) / 512 - 1,
                    attributes: 0,
                    name: format!("pv{index}"),
                })
                .collect(),
        };
        let device = unique_path("disk2pv");
        let mut bytes = vec![0u8; layout.size_bytes as usize];
        for op in crate::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap() {
            let at = op.offset as usize;
            bytes[at..at + op.bytes.len()].copy_from_slice(&op.bytes);
        }
        register_device(&device, bytes, 512);
        let span = |lv_offset: u64| PhysicalVolume {
            volumes: vec![PvVolume {
                name: "vg0/span".into(),
                read_path: snap.clone(),
                len: 2 * EXT,
                segments: vec![PvSegment {
                    pv_offset: PE_START,
                    lv_offset,
                    length: EXT,
                }],
                frozen: true,
                quiesced: true,
                key_store: false,
            }],
        };
        let platform = TestPlatform::new()
            .source(&p1, TestSource::PhysicalVolume(span(0)))
            .source(&p2, TestSource::PhysicalVolume(span(EXT)))
            .slot(&p1, DISK, p1_at)
            .slot(&p2, DISK, p2_at);
        let image = dir.path().join("two.qcow2");
        let req = DiskBackupRequest {
            disk_number: DISK,
            disk_device: Some(device),
            disk_model: "Test disk".into(),
            partitions: vec![
                DiskPartitionBackup {
                    device: p1,
                    part_index: 1,
                    passphrase: None,
                },
                DiskPartitionBackup {
                    device: p2,
                    part_index: 2,
                    passphrase: None,
                },
            ],
            image_path: image.clone(),
            full_image_path: None,
            block_size: BS,
            image_format: ImageFormat::Qcow2,
            job_id: None,
            encryption: None,
            exclusion_patterns_windows: Vec::new(),
            exclusion_patterns_linux: vec!["*.tmp".into()],
            kind: BackupType::Full,
            parent_image: None,
            source_disk: None,
            bootable: false,
            zstd_level: 0,
        };
        let sum = run(&platform, &req).unwrap();
        assert_eq!(sum.excluded_files, 1, "counted once");
        assert_eq!(sum.excluded_bytes, 300_000);
        // Each half of the volume is where its partition keeps it, the
        // junk in neither.
        let a = image_bytes(&image, p1_at + PE_START, half);
        let b = image_bytes(&image, p2_at + PE_START, half);
        for part in [&a, &b] {
            assert!(!part.chunks(4096).any(|c| c.iter().all(|&x| x == 0x5A)));
            assert!(!part.chunks(4096).any(|c| c.iter().all(|&x| x == 0xEE)));
        }
        assert_eq!(&a[1024 + 56..1024 + 58], &[0x53, 0xEF], "root's superblock");
        assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());
    }

    /// Without readable metadata nothing counts as free: every byte outside
    /// the active volumes is copied from the partition.
    #[test]
    fn without_its_metadata_nothing_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let disk = PvDisk::new(raw_root(), false);
        let platform = disk.platform(disk.pv(None));
        let image = dir.path().join("disk.qcow2");
        run(&platform, &disk.request(&image)).unwrap();
        assert!(disk.stored(&image, extent(2), extent(3)));
        assert_same(
            &disk.in_image(&image, extent(2), extent(3)),
            &disk.live[extent(2) as usize..extent(3) as usize],
            "the extent nobody could call free",
        );
        assert_same(
            &disk.in_image(&image, extent(0), extent(2)),
            &disk.root.1,
            "root still from its snapshot",
        );
    }

    /// A partition that is no PV any more is refused, nothing published; so
    /// are volumes that overlap.
    #[test]
    fn a_partition_whose_label_is_gone_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = PvDisk::new(raw_root(), true);
        disk.live[..PE_START as usize].fill(0);
        register_device(&disk.part, disk.live.clone(), 512);
        let platform = disk.platform(disk.pv(None));
        let image = dir.path().join("disk.qcow2");
        let e = run(&platform, &disk.request(&image))
            .err()
            .expect("refused");
        assert!(matches!(e, OwnBackupError::NoSnapshotProvider(_)), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let disk = PvDisk::new(raw_root(), true);
        let mut pv = disk.pv(None);
        pv.volumes[1].segments[0].pv_offset = extent(1);
        let e = run(&disk.platform(pv), &disk.request(&image))
            .err()
            .expect("refused");
        assert!(matches!(e, OwnBackupError::Config(_)), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Inside a logical volume the rules apply as in a volume copy, and the
    /// service's key directory (decision 6/E4) is left out of the volume
    /// that holds it — or the disk is not copied.
    #[test]
    fn exclusions_and_the_key_directory_apply_inside_the_volumes() {
        let Some(ext4) = ext4_root() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let disk = PvDisk::new(ext4, true);
        let image = dir.path().join("rules.qcow2");
        let summary = run(
            &disk.platform(disk.pv(None)),
            &DiskBackupRequest {
                exclusion_patterns_linux: vec!["*.tmp".into()],
                ..disk.request(&image)
            },
        )
        .unwrap();
        assert_eq!(summary.excluded_files, 1);
        assert_eq!(summary.excluded_bytes, 300_000);
        let root = disk.in_image(&image, extent(0), extent(2));
        assert!(
            !root.windows(4096).any(|w| w.iter().all(|&b| b == 0x5A)),
            "the excluded file's data is not in the image"
        );
        assert!(verify_image(&image, |_, _| Ok(())).unwrap().valid());
        // The ext4 volume's free space was not copied either.
        let blocks = (extent(0)..extent(2))
            .step_by(BS as usize)
            .filter(|&at| disk.stored(&image, at, at + BS as u64))
            .count() as u64;
        assert!(
            blocks < 2 * EXT / BS as u64,
            "{blocks} blocks of root stored"
        );

        // The key directory on root: left out, though the job has no rules.
        let platform = disk
            .platform(disk.pv(Some("root")))
            .key_store_on(&disk.part);
        let keyed = dir.path().join("keyed.qcow2");
        let summary = run(&platform, &disk.request(&keyed)).unwrap();
        assert!(summary.key_store_excluded);

        // On the raw volume no scanner reads: refused, the volume named.
        let platform = disk
            .platform(disk.pv(Some("data")))
            .key_store_on(&disk.part);
        let refused = dir.path().join("refused.qcow2");
        let e = run(&platform, &disk.request(&refused))
            .err()
            .expect("refused");
        assert!(e.to_string().contains("vg0/data"), "{e}");
        assert!(!refused.exists());

        // On the PV, but on none of its volumes: refused.
        let platform = disk.platform(disk.pv(None)).key_store_on(&disk.part);
        let e = run(&platform, &disk.request(&refused))
            .err()
            .expect("refused");
        assert!(e.to_string().contains(&disk.part), "{e}");
        assert!(!refused.exists());
    }
}
