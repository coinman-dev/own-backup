//! The Linux answers to the shared engine's questions
//! ([`crate::engine_core::platform`]).
//!
//! Linux has no VSS and no filesystem driver the engine could ask about a
//! volume. A source is read through the snapshot its caller prepared
//! (`own_backup_snapshot::linux`) — never a device nobody vouched for; the NTFS
//! used map comes from the engine's own `$Bitmap` reader; VHDX images are made
//! in userspace with the geometry Windows gives them; and nothing deletes the
//! names of excluded files inside a finished image yet, which the engine then
//! says in so many words.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;

use own_backup_common::gpt::DiskLayout;
use own_backup_common::{tr, OwnBackupError, Result};
use own_backup_snapshot::Snapshot;

use crate::block_reader::{BlockReader, RawVolumeBlockReader};
use crate::efivars::BootBackup;
use crate::engine_core::platform::{
    userspace_vhdx, DiskIdentity, DiskPlatform, DiskRef, DiskWindow, EnginePlatform, FinishedImage,
    NativeUsedMap, SourceSession, VolumeSpan,
};
use crate::exclusions::{ByteRanges, ExcludedPath, ExclusionScan};
use crate::linux_device::LinuxRawDevice;
use crate::ntfs_map::NtfsUsedMap;
use crate::removal_types::InsideRemoval;
use crate::sidecar::BlockChecksum;
use crate::used_sectors::UsedSectorMap;
use crate::vhdx_format::VhdxWriter;

/// The platform the Linux engine runs on: the snapshots its caller took, by
/// the source device each was taken of.
pub(crate) struct LinuxPlatform {
    snapshots: HashMap<String, Snapshot>,
}

impl LinuxPlatform {
    /// Each source is read through its snapshot as the caller took it
    /// (`LinuxSnapshotProvider::create_snapshot`); a source without one is
    /// refused. The caller keeps the snapshots: it checks they are still whole
    /// before an image is published and releases them afterwards.
    pub(crate) fn new(snapshots: impl IntoIterator<Item = Snapshot>) -> Self {
        Self {
            snapshots: snapshots
                .into_iter()
                .map(|s| (s.source_device.clone(), s))
                .collect(),
        }
    }

    fn session(&self, source: &str) -> Result<LinuxSession> {
        let snapshot = self.snapshots.get(source).ok_or_else(|| {
            OwnBackupError::Config(format!(
                "{source}: no snapshot was prepared for this source, and it is not read without one"
            ))
        })?;
        Ok(LinuxSession {
            read_path: snapshot.snapshot_device_path.clone(),
            snapshot: matches!(
                snapshot.metadata.get("mode").map(String::as_str),
                Some("lvm" | "dm")
            ),
            frozen: own_backup_snapshot::linux::taken_frozen(snapshot),
        })
    }
}

/// A source as its snapshot shows it.
pub(crate) struct LinuxSession {
    read_path: String,
    snapshot: bool,
    frozen: bool,
}

impl SourceSession for LinuxSession {
    fn read_path(&self) -> &str {
        &self.read_path
    }

    /// An LVM or device-mapper snapshot, not the device itself.
    fn snapshot_used(&self) -> bool {
        self.snapshot
    }

    /// Taken with the filesystem frozen (LVM, device-mapper).
    fn frozen(&self) -> bool {
        self.frozen
    }

    /// Nothing to do here: the snapshot is the caller's to check and release.
    fn complete(&mut self) -> Option<Result<()>> {
        None
    }
}

/// The NTFS used map, read from the volume's own `$Bitmap` — what Windows
/// hands out through `FSCTL_GET_VOLUME_BITMAP` is not available here.
pub(crate) struct LinuxNtfsMap {
    map: NtfsUsedMap,
    read_path: String,
    block_size: u32,
    /// Everything excluded so far: each exclusion rebuilds the map from the
    /// bitmap, and the rebuilt map must keep the earlier ones out too.
    excluded: ByteRanges,
}

impl UsedSectorMap for LinuxNtfsMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.map.is_block_used(block_number)
    }

    fn used_block_count(&self) -> u64 {
        self.map.used_block_count()
    }
}

impl NativeUsedMap for LinuxNtfsMap {
    fn open(read_path: &str, block_size: u32) -> Result<Self> {
        let mut dev = LinuxRawDevice::open_read(read_path)?;
        Ok(Self {
            map: crate::ntfs_map::build_ntfs_used_map(&mut dev, block_size)?,
            read_path: read_path.to_string(),
            block_size,
            excluded: ByteRanges::new(),
        })
    }

    fn cluster_size(&self) -> u32 {
        self.map.cluster_size
    }

    /// The map is built again without the excluded clusters, as for NTFS
    /// inside a decrypted container: whether a block holds nothing but
    /// excluded data shows only on clusters, and a finished map has none.
    fn apply_excluded_ranges(&mut self, ranges: &ByteRanges) -> Result<()> {
        for (start, end) in ranges.iter() {
            self.excluded.add(start, end);
        }
        self.excluded.finalize();
        let mut dev = LinuxRawDevice::open_read(&self.read_path)?;
        self.map = crate::ntfs_map::build_ntfs_used_map_excl(
            &mut dev,
            self.block_size,
            Some(&self.excluded),
        )?;
        Ok(())
    }

    /// The map keeps no bitmap: nothing to free.
    fn release_bitmap(&mut self) {}
}

impl EnginePlatform for LinuxPlatform {
    type Dev = LinuxRawDevice;
    type NativeMap = LinuxNtfsMap;
    type Session = LinuxSession;
    type OsUnlock = Infallible;

    const SOURCE_OS: &'static str = "linux";
    const METADATA_NEEDS_FROZEN: bool = true;

    /// Device paths are used as given: a job keeps the stable `/dev/disk/…`
    /// name, and resolving it here would record the unstable one.
    fn normalize_source(&self, source: &str) -> String {
        source.to_string()
    }

    /// The service runs as root, which reads every device and file.
    fn prepare_privileges(&self) {}

    /// Whether to snapshot was the caller's decision, taken when it prepared
    /// the snapshot (`use_snapshot` is a Windows wish); an unprepared source is
    /// refused rather than read live.
    fn open_volume_session(&self, source: &str, _use_snapshot: bool) -> Result<LinuxSession> {
        self.session(source)
    }

    fn block_reader(&self, read_path: &str, block_size: u32) -> Result<Box<dyn BlockReader>> {
        Ok(Box::new(RawVolumeBlockReader::new(
            Box::new(LinuxRawDevice::open_read(read_path)?),
            block_size,
        )?))
    }

    /// The device itself: no filter driver sits above a locked volume here,
    /// so its ciphertext reads through its own device node.
    fn ciphertext_extents(&self, read_path: &str) -> Result<Vec<DiskWindow>> {
        let length = LinuxRawDevice::open_read(read_path)?.length();
        Ok(vec![DiskWindow {
            device: read_path.to_string(),
            offset: 0,
            length,
        }])
    }

    fn unlock_with_os(&self, read_path: &str, _passphrase: &str) -> Result<Infallible> {
        Err(OwnBackupError::NotImplemented(format!(
            "{read_path}: unlocking a volume through the system is not available on Linux"
        )))
    }

    /// No directory walk: the MFT read from a frozen snapshot is the only way
    /// to the files here, and without it the engine says the rules were not
    /// applied.
    fn walk_exclusions(
        &self,
        _read_path: &str,
        _cluster_size: u32,
        _patterns: &[String],
        _tick: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<Option<ExclusionScan>> {
        Ok(None)
    }

    fn create_vhdx(&self, path: &Path, size: u64, sector_size: u32) -> Result<VhdxWriter> {
        userspace_vhdx(path, size, sector_size)
    }

    fn create_vhdx_child(&self, path: &Path, parent: &Path) -> Result<VhdxWriter> {
        // TODO(phase V1): `vhdx_format::create_differencing` — the has-parent
        // bit and a parent locator with the DataWriteGuid linkage — then
        // `VhdxWriter::open_existing`, as on Windows.
        Err(OwnBackupError::NotImplemented(format!(
            "{}: a differencing VHDX over {} cannot be made on Linux yet",
            path.display(),
            parent.display()
        )))
    }

    /// Deleting files inside an image needs a filesystem driver over it (an
    /// NBD overlay and ntfs-3g, plan N1) — not there yet.
    fn exclude_inside_image(
        &self,
        _image: &FinishedImage<'_>,
        _span: VolumeSpan,
        _patterns: &[String],
        _checksums: &mut Vec<BlockChecksum>,
        _tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<InsideRemoval>> {
        Ok(None)
    }

    /// See [`Self::exclude_inside_image`]: the names stay, and the engine
    /// reports them with [`Self::entries_kept_reason`].
    fn remove_from_image(
        &self,
        _image: &FinishedImage<'_>,
        _span: VolumeSpan,
        _paths: &[ExcludedPath],
        _checksums: &mut Vec<BlockChecksum>,
        _tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>> {
        Ok(None)
    }

    fn entries_kept_reason(&self) -> String {
        tr!("engine.entries_kept_linux").into_owned()
    }
}

impl DiskPlatform for LinuxPlatform {
    fn open_partition_session(&self, device: &str) -> Result<LinuxSession> {
        self.session(device)
    }

    fn read_disk_layout(&self, disk: &DiskRef) -> Result<DiskLayout> {
        let path = disk.device.as_deref().ok_or_else(|| {
            OwnBackupError::Config(format!(
                "disk {} has no device path — a Linux disk is named by /dev/disk/by-id",
                disk.number
            ))
        })?;
        let dev = LinuxRawDevice::open_read(path)?;
        let sector = dev.sector_size();
        let disk_size = dev.length();
        // Protective MBR, primary header and a 128-entry array, as on Windows.
        let want = (34 * sector as u64).min(disk_size) as usize;
        let mut head = vec![0u8; want];
        dev.read_at(0, &mut head)?;
        crate::gpt_reader::parse_disk_layout(&head, sector, disk_size)
    }

    /// From sysfs: the partition's one extent must be on this disk (its device
    /// number) at `offset`. `None` when the disk has no device or either side
    /// cannot be resolved.
    fn partition_in_slot(&self, device: &str, disk: &DiskRef, offset: u64) -> Option<bool> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let meta = std::fs::metadata(disk.device.as_deref()?).ok()?;
        if !meta.file_type().is_block_device() {
            return None;
        }
        let rdev = meta.rdev();
        let key = crate::source_identity::linux::disk_key(libc::major(rdev), libc::minor(rdev));
        crate::source_identity::extents_of_source(device).map(|extents| {
            matches!(extents.as_slice(),
                [e] if e.disk == key && e.offset == offset)
        })
    }

    /// The by-id path (decision R1): it names the same disk after a reboot,
    /// which `/dev/sdb` does not.
    fn disk_identity(&self, disk: &DiskRef) -> DiskIdentity {
        let name = disk
            .device
            .clone()
            .unwrap_or_else(|| format!("disk{}", disk.number));
        DiskIdentity {
            device_id: name.clone(),
            source_id: name,
        }
    }

    fn export_boot_entries(&self) -> Result<BootBackup> {
        Err(OwnBackupError::NotImplemented(
            "reading the firmware boot entries (efivarfs) on Linux".into(),
        ))
    }

    fn remove_from_disk_image(
        &self,
        _image: &FinishedImage<'_>,
        _image_size: u64,
        _partitions: &[(u64, Vec<ExcludedPath>)],
        _checksums: &mut Vec<BlockChecksum>,
        _tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::gpt::GptPartitionEntry;
    use own_backup_common::types::ImageFormat;

    const MIB: u64 = 1024 * 1024;

    fn snapshot(source: &str, read: &str, mode: &str) -> Snapshot {
        Snapshot {
            provider_name: "linux".into(),
            source_device: source.into(),
            snapshot_device_path: read.into(),
            created_at: chrono::Utc::now(),
            metadata: HashMap::from([("mode".to_string(), mode.to_string())]),
        }
    }

    fn file_with(dir: &Path, name: &str, data: &[u8]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn vhdx_has_the_geometry_windows_gives_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.vhdx");
        let p = LinuxPlatform::new([]);
        let mut writer = p.create_vhdx(&path, 5 * MIB + 512, 4096).unwrap();
        assert_eq!(writer.length(), 6 * MIB, "rounded up to a MiB");
        writer.write_at(2 * MIB, &[0xA5; 4096]).unwrap();
        writer.finish().unwrap();
        let alloc = crate::vhdx_bat::allocated_payload_blocks(&path).unwrap();
        assert_eq!(alloc.block_size, MIB, "1 MiB blocks, as CreateVirtualDisk");
        assert_eq!(alloc.virtual_size, 6 * MIB);
        assert_eq!(alloc.blocks, vec![2]);
        let mut back = [0u8; 4096];
        crate::vhdx_bat::read_vhdx_guest_at(&path, 2 * MIB, &mut back).unwrap();
        assert_eq!(back, [0xA5; 4096]);

        assert!(matches!(
            p.create_vhdx_child(&dir.path().join("c.avhdx"), &path),
            Err(OwnBackupError::NotImplemented(_))
        ));
    }

    #[test]
    fn a_source_without_a_snapshot_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let file = file_with(dir.path(), "src.img", &data);
        let p = LinuxPlatform::new([
            snapshot(&file, &file, "file"),
            snapshot("/dev/vg/root", "/dev/vg/obk-snap-1", "lvm"),
            snapshot("/dev/sdz1", "/dev/sdz1", "idle"),
        ]);

        assert!(matches!(
            p.open_volume_session("/dev/sdz2", true),
            Err(OwnBackupError::Config(_))
        ));
        assert!(p.open_partition_session("/dev/sdz2").is_err());

        // A file is read as it is: no snapshot, and nothing vouches it holds still.
        let mut s = p.open_volume_session(&file, false).unwrap();
        assert_eq!(s.read_path(), file);
        assert!(!s.snapshot_used() && !s.frozen());
        assert!(s.complete().is_none(), "the caller releases its snapshot");

        let s = p.open_volume_session("/dev/vg/root", true).unwrap();
        assert_eq!(s.read_path(), "/dev/vg/obk-snap-1");
        assert!(s.snapshot_used() && s.frozen());

        // Idle: read directly, and not taken with a freeze.
        let s = p.open_partition_session("/dev/sdz1").unwrap();
        assert!(!s.snapshot_used() && !s.frozen());

        let mut reader = p.block_reader(&file, 1024).unwrap();
        assert_eq!(reader.total_blocks(), 3);
        assert_eq!(reader.read_block(2).unwrap(), data[2048..]);
        assert_eq!(
            p.ciphertext_extents(&file).unwrap(),
            vec![DiskWindow {
                device: file.clone(),
                offset: 0,
                length: 3000
            }]
        );
        assert_eq!(p.normalize_source("/dev/disk/by-id/x"), "/dev/disk/by-id/x");
        assert_eq!(<LinuxPlatform as EnginePlatform>::SOURCE_OS, "linux");
    }

    #[test]
    fn ntfs_map_is_the_bitmap_reader_and_keeps_every_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        let image = crate::ntfs_map::tests::tiny_ntfs_image();
        let path = file_with(dir.path(), "ntfs.img", &image);
        let mut dev = LinuxRawDevice::open_read(&path).unwrap();
        let blocks = image.len() as u64 / 512;
        let same = |a: &dyn UsedSectorMap, b: &dyn UsedSectorMap| {
            a.used_block_count() == b.used_block_count()
                && (0..blocks).all(|n| a.is_block_used(n) == b.is_block_used(n))
        };

        let mut map = LinuxNtfsMap::open(&path, 512).unwrap();
        let plain = crate::ntfs_map::build_ntfs_used_map(&mut dev, 512).unwrap();
        assert!(same(&map, &plain));
        assert_eq!(map.cluster_size(), 512);
        assert_eq!(map.used_block_count(), 6);

        // Clusters 16 and 17, then 20: the second rebuild keeps the first out.
        let mut first = ByteRanges::new();
        first.add(16 * 512, 18 * 512);
        first.finalize();
        map.apply_excluded_ranges(&first).unwrap();
        let want = crate::ntfs_map::build_ntfs_used_map_excl(&mut dev, 512, Some(&first)).unwrap();
        assert!(same(&map, &want));
        assert_eq!(map.used_block_count(), 4);

        let mut second = ByteRanges::new();
        second.add(20 * 512, 21 * 512);
        second.finalize();
        map.apply_excluded_ranges(&second).unwrap();
        let mut both = first.clone();
        both.add(20 * 512, 21 * 512);
        both.finalize();
        let want = crate::ntfs_map::build_ntfs_used_map_excl(&mut dev, 512, Some(&both)).unwrap();
        assert!(same(&map, &want));
        assert_eq!(map.used_block_count(), 3);

        map.release_bitmap();
        assert_eq!(map.used_block_count(), 3, "the map stays valid");

        let other = file_with(dir.path(), "zero.img", &[0u8; 64 * 512]);
        assert!(LinuxNtfsMap::open(&other, 512).is_err(), "not NTFS");
    }

    #[test]
    fn disk_layout_is_read_off_the_disk() {
        let layout = DiskLayout {
            disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
            sector_size: 512,
            size_bytes: 32 * MIB,
            partitions: vec![
                GptPartitionEntry {
                    index: 1,
                    type_guid: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(),
                    part_guid: "11111111-0000-0000-0000-000000000001".into(),
                    first_lba: 2048,
                    last_lba: 6143,
                    attributes: 0,
                    name: "EFI system partition".into(),
                },
                GptPartitionEntry {
                    index: 2,
                    type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into(),
                    part_guid: "11111111-0000-0000-0000-000000000002".into(),
                    first_lba: 6144,
                    last_lba: 63487,
                    attributes: 0,
                    name: "root".into(),
                },
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(layout.size_bytes).unwrap();
        {
            use std::os::unix::fs::FileExt;
            for op in crate::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap() {
                file.write_all_at(&op.bytes, op.offset).unwrap();
            }
        }
        drop(file);
        let device = path.to_str().unwrap().to_string();
        let p = LinuxPlatform::new([]);
        let disk = DiskRef {
            number: 0,
            device: Some(device.clone()),
        };

        assert_eq!(p.read_disk_layout(&disk).unwrap(), layout);
        assert!(matches!(
            p.read_disk_layout(&DiskRef::number(0)),
            Err(OwnBackupError::Config(_))
        ));

        assert_eq!(
            p.disk_identity(&disk),
            DiskIdentity {
                device_id: device.clone(),
                source_id: device.clone()
            }
        );
        assert_eq!(p.disk_identity(&DiskRef::number(3)).source_id, "disk3");
        // A file is no disk: where a partition lies on it cannot be told.
        assert_eq!(p.partition_in_slot(&device, &disk, 2048 * 512), None);
        assert_eq!(p.partition_in_slot(&device, &DiskRef::number(0), 0), None);
    }

    #[test]
    fn what_linux_cannot_do_is_said_not_faked() {
        let dir = tempfile::tempdir().unwrap();
        let file = file_with(dir.path(), "vol.img", &[0u8; 4096]);
        let p = LinuxPlatform::new([]);
        assert!(matches!(
            p.unlock_with_os(&file, "pass"),
            Err(OwnBackupError::NotImplemented(_))
        ));
        assert!(p
            .walk_exclusions(&file, 4096, &["*.tmp".into()], &mut |_| Ok(()))
            .unwrap()
            .is_none());
        assert!(matches!(
            p.export_boot_entries(),
            Err(OwnBackupError::NotImplemented(_))
        ));

        let image = FinishedImage {
            path: Path::new(&file),
            format: ImageFormat::Vhdx,
            encryption: None,
            block_size: MIB as u32,
            zstd_level: 0,
        };
        let span = VolumeSpan {
            data_offset: MIB,
            volume_len: 4096,
        };
        let path = ExcludedPath {
            rel: "pagefile.sys".into(),
            is_dir: false,
            is_reparse: false,
        };
        let mut checksums = Vec::new();
        let mut tick = |_: u64, _: u64| {};
        assert!(p
            .exclude_inside_image(&image, span, &["*.sys".into()], &mut checksums, &mut tick)
            .unwrap()
            .is_none());
        assert!(p
            .remove_from_image(
                &image,
                span,
                std::slice::from_ref(&path),
                &mut checksums,
                &mut tick
            )
            .unwrap()
            .is_none());
        assert!(p
            .remove_from_disk_image(
                &image,
                8 * MIB,
                &[(MIB, vec![path])],
                &mut checksums,
                &mut tick
            )
            .unwrap()
            .is_none());
        // Honest about the names left behind, and no promise about the next run.
        let reason = p.entries_kept_reason();
        assert!(!reason.is_empty() && reason != "engine.entries_kept_linux");
    }
}
