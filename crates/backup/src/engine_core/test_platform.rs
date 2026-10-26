//! An in-memory platform for the shared engine's tests.
//!
//! Devices are byte buffers registered under a path; sessions are frozen or
//! not as the test says; the OS knows no filesystem (no native used map), has
//! no unlock and no directory walk; VHDX images are made in userspace; the
//! removal passes never run. Plain Rust only, so it builds for Windows too.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use own_backup_common::gpt::DiskLayout;
use own_backup_common::{OwnBackupError, Result};

use super::platform::{
    userspace_vhdx, DiskIdentity, DiskPlatform, DiskRef, DiskWindow, EnginePlatform, FinishedImage,
    NativeUsedMap, OpenedContainer, PhysicalVolume, SourceDevice, SourceSession, VolumeSpan,
};
use crate::block_reader::{BlockReader, RawVolumeBlockReader};
use crate::efivars::BootBackup;
use crate::exclusions::{ByteRanges, ExcludedPath, ExclusionScan};
use crate::fat_map::RawVolume;
use crate::removal_types::InsideRemoval;
use crate::sidecar::BlockChecksum;
use crate::used_sectors::UsedSectorMap;
use crate::vhdx_format::VhdxWriter;

#[derive(Clone)]
struct MemDisk {
    data: Arc<Vec<u8>>,
    sector_size: u32,
}

/// Every registered device. Global, because [`SourceDevice::open_read`] has
/// no platform to ask; each test registers under its own [`unique_path`].
fn devices() -> &'static Mutex<HashMap<String, MemDisk>> {
    static DEVICES: OnceLock<Mutex<HashMap<String, MemDisk>>> = OnceLock::new();
    DEVICES.get_or_init(Default::default)
}

/// A device path no other test uses.
pub(crate) fn unique_path(name: &str) -> String {
    format!("mem://{}/{name}", uuid::Uuid::new_v4().simple())
}

/// Make `data` the device at `path`.
pub(crate) fn register_device(path: &str, data: Vec<u8>, sector_size: u32) {
    devices().lock().unwrap().insert(
        path.to_string(),
        MemDisk {
            data: Arc::new(data),
            sector_size,
        },
    );
}

/// What the system has cached of a device, by path: read instead of the
/// device until [`SourceDevice::drop_cache`] is asked of it.
fn stale_caches() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
    static STALE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    STALE.get_or_init(Default::default)
}

/// Make `stale` what reads of the device at `path` return until its cache
/// is dropped — as a partition's page cache that device-mapper wrote under.
pub(crate) fn register_stale_cache(path: &str, stale: Vec<u8>) {
    stale_caches()
        .lock()
        .unwrap()
        .insert(path.to_string(), Arc::new(stale));
}

/// A registered device, opened.
pub(crate) struct MemDevice {
    path: String,
    disk: MemDisk,
}

impl RawVolume for MemDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.disk.data.len() as u64)
            .ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "{}: read of {} bytes at {offset} past the end",
                    self.path,
                    buf.len()
                ))
            })?;
        let stale = stale_caches().lock().unwrap().get(&self.path).cloned();
        let data = stale.as_ref().unwrap_or(&self.disk.data);
        buf.copy_from_slice(&data[offset as usize..end as usize]);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.disk.data.len() as u64
    }
}

impl SourceDevice for MemDevice {
    fn open_read(path: &str) -> Result<Self> {
        let disk = devices()
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| OwnBackupError::Device(format!("no test device {path}")))?;
        Ok(Self {
            path: path.to_string(),
            disk,
        })
    }

    fn sector_size(&self) -> u32 {
        self.disk.sector_size
    }

    fn drop_cache(&mut self) -> Result<()> {
        stale_caches().lock().unwrap().remove(&self.path);
        Ok(())
    }
}

/// The OS of the tests knows no filesystem: this map can never be made.
pub(crate) enum NoNativeMap {}

impl UsedSectorMap for NoNativeMap {
    fn is_block_used(&self, _block_number: u64) -> bool {
        match *self {}
    }

    fn used_block_count(&self) -> u64 {
        match *self {}
    }
}

impl NativeUsedMap for NoNativeMap {
    fn open(read_path: &str, _block_size: u32) -> Result<Self> {
        Err(OwnBackupError::Device(format!(
            "{read_path}: the test platform has no native used map"
        )))
    }

    fn cluster_size(&self) -> u32 {
        match *self {}
    }

    fn apply_excluded_ranges(&mut self, _ranges: &ByteRanges) -> Result<()> {
        match *self {}
    }

    fn release_bitmap(&mut self) {
        match *self {}
    }
}

/// How a test source is held.
#[derive(Debug, Clone)]
pub(crate) enum TestSource {
    /// The device itself is read; `frozen` says whether anything writes it.
    Direct { frozen: bool },
    /// A snapshot registered at `read_path` is read when one is wanted; it is
    /// frozen, the live device under it is not.
    Snapshot { read_path: String },
    /// An LVM physical volume in use: the partition itself is read where no
    /// logical volume of `pv` maps, each volume from its own device.
    PhysicalVolume(PhysicalVolume),
    /// A partition whose LUKS container the system has open: its mapping,
    /// registered at `mapping` and starting `data_offset` bytes into the
    /// partition, is read — through `pv`'s volumes when it is an LVM
    /// physical volume in use (LUKS → LVM).
    Opened {
        mapping: String,
        data_offset: u64,
        pv: Option<PhysicalVolume>,
    },
}

pub(crate) struct TestSession {
    read_path: String,
    snapshot: bool,
    frozen: bool,
    completed: bool,
    key_store: bool,
    pv: Option<PhysicalVolume>,
    container: Option<OpenedContainer>,
    /// Not `Send`, like a VSS session: the engine must not need it to be.
    _not_send: PhantomData<*const ()>,
}

impl SourceSession for TestSession {
    fn read_path(&self) -> &str {
        &self.read_path
    }

    fn snapshot_used(&self) -> bool {
        self.snapshot
    }

    fn frozen(&self) -> bool {
        self.frozen
    }

    /// A snapshot is taken with its filesystem frozen; a device read directly
    /// is at most held still.
    fn quiesced(&self) -> bool {
        self.snapshot
    }

    fn complete(&mut self) -> Option<Result<()>> {
        if !self.snapshot || self.completed {
            return None;
        }
        self.completed = true;
        Some(Ok(()))
    }

    fn must_exclude_key_store(&self) -> bool {
        self.key_store
    }

    fn physical_volume(&self) -> Option<&PhysicalVolume> {
        self.pv.as_ref()
    }

    fn opened_container(&self) -> Option<&OpenedContainer> {
        self.container.as_ref()
    }
}

/// The platform itself: which sources may be held, and how, and where the
/// partitions of the test disks lie.
#[derive(Default)]
pub(crate) struct TestPlatform {
    sources: HashMap<String, TestSource>,
    /// Partition device → (disk number, byte offset).
    slots: HashMap<String, (u32, u64)>,
    /// Sources said to hold the service's key directory.
    key_stores: std::collections::HashSet<String>,
}

impl TestPlatform {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Allow `source` to be held, as `how`. Any other source is refused.
    pub(crate) fn source(mut self, source: &str, how: TestSource) -> Self {
        self.sources.insert(source.to_string(), how);
        self
    }

    /// Say that `source` holds the service's key directory.
    pub(crate) fn key_store_on(mut self, source: &str) -> Self {
        self.key_stores.insert(source.to_string());
        self
    }

    /// Say where the partition `device` lies; others are "cannot tell".
    pub(crate) fn slot(mut self, device: &str, disk: u32, offset: u64) -> Self {
        self.slots.insert(device.to_string(), (disk, offset));
        self
    }
}

impl EnginePlatform for TestPlatform {
    type Dev = MemDevice;
    type NativeMap = NoNativeMap;
    type Session = TestSession;
    type OsUnlock = Infallible;

    const SOURCE_OS: &'static str = "test";
    const METADATA_NEEDS_FROZEN: bool = true;

    fn normalize_source(&self, source: &str) -> String {
        source.trim().to_string()
    }

    fn prepare_privileges(&self) {}

    fn open_volume_session(&self, source: &str, use_snapshot: bool) -> Result<TestSession> {
        let how = self.sources.get(source).ok_or_else(|| {
            OwnBackupError::NoSnapshotProvider(format!("test platform: {source} is not prepared"))
        })?;
        let (read_path, snapshot, frozen) = match how {
            TestSource::Direct { frozen } => (source.to_string(), false, *frozen),
            TestSource::Snapshot { read_path } if use_snapshot => (read_path.clone(), true, true),
            TestSource::Snapshot { .. } => (source.to_string(), false, false),
            TestSource::PhysicalVolume(_) => (source.to_string(), true, true),
            TestSource::Opened { mapping, .. } => (mapping.clone(), true, true),
        };
        Ok(TestSession {
            pv: match how {
                TestSource::PhysicalVolume(pv) => Some(pv.clone()),
                TestSource::Opened { pv, .. } => pv.clone(),
                _ => None,
            },
            container: match how {
                TestSource::Opened {
                    mapping,
                    data_offset,
                    ..
                } => Some(OpenedContainer {
                    partition: source.to_string(),
                    data_offset: *data_offset,
                    mapping: mapping.clone(),
                }),
                _ => None,
            },
            key_store: self.key_stores.contains(source),
            read_path,
            snapshot,
            frozen,
            completed: false,
            _not_send: PhantomData,
        })
    }

    fn block_reader(&self, read_path: &str, block_size: u32) -> Result<Box<dyn BlockReader>> {
        Ok(Box::new(RawVolumeBlockReader::new(
            Box::new(MemDevice::open_read(read_path)?),
            block_size,
        )?))
    }

    fn ciphertext_extents(&self, read_path: &str) -> Result<Vec<DiskWindow>> {
        let length = MemDevice::open_read(read_path)?.len();
        Ok(vec![DiskWindow {
            device: read_path.to_string(),
            offset: 0,
            length,
        }])
    }

    fn unlock_with_os(&self, read_path: &str, _passphrase: &str) -> Result<Infallible> {
        Err(OwnBackupError::NotImplemented(format!(
            "{read_path}: the test platform cannot unlock volumes"
        )))
    }

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

    fn create_vhdx_child(&self, path: &Path, _parent: &Path) -> Result<VhdxWriter> {
        Err(OwnBackupError::NotImplemented(format!(
            "{}: the test platform makes no differencing VHDX yet",
            path.display()
        )))
    }

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

    fn entries_kept_reason(&self) -> own_backup_common::lang::Localized {
        own_backup_common::lang::Localized::plain("test platform: entries kept")
    }
}

impl DiskPlatform for TestPlatform {
    fn open_partition_session(&self, device: &str) -> Result<TestSession> {
        self.open_volume_session(device, true)
    }

    fn read_disk_layout(&self, disk: &DiskRef) -> Result<DiskLayout> {
        let path = disk.device.as_deref().ok_or_else(|| {
            OwnBackupError::Config(format!("test disk {} has no device", disk.number))
        })?;
        let mut dev = MemDevice::open_read(path)?;
        let sector = dev.sector_size();
        let want = (34 * sector as u64).min(dev.len()) as usize;
        let mut head = vec![0u8; want];
        dev.read_at(0, &mut head)?;
        crate::gpt_reader::parse_disk_layout(&head, sector, dev.len())
    }

    fn partition_in_slot(&self, device: &str, disk: &DiskRef, offset: u64) -> Option<bool> {
        self.slots
            .get(device)
            .map(|&(number, at)| number == disk.number && at == offset)
    }

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
            "the test platform has no firmware".into(),
        ))
    }

    fn remove_from_disk_image(
        &self,
        _image: &FinishedImage<'_>,
        _image_size: u64,
        _partitions: &[(u64, u64, Vec<ExcludedPath>)],
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

    /// An `$MFT` scan that could not reach every record is not traded for a
    /// directory walk on any platform — the walk would lose what only the
    /// scan knows (kept content, records to clear, the key-directory proof),
    /// and an encrypted volume has none: the partial scan is used as it is,
    /// and the result screen is told how many records it could not read. A
    /// complete scan says nothing.
    #[test]
    fn an_mft_scan_short_of_records_is_said_not_replaced() {
        let partial = crate::ntfs_scan::MftScan {
            records_unreachable: 7,
            files_excluded: 3,
            ..Default::default()
        };
        let note = crate::engine_core::platform::unreachable_note(&partial).expect("a note");
        assert!(note.contains('7'), "{note:?}");
        assert_eq!(
            crate::engine_core::platform::unreachable_note(&crate::ntfs_scan::MftScan::default()),
            None
        );
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// What a copy saw of its source.
    struct ReadBack {
        data: Vec<u8>,
        snapshot: bool,
        frozen: bool,
        completed: Option<Result<()>>,
    }

    /// What the engine does with a session, written against the traits only:
    /// hold the source, read every block, let go.
    fn read_through<P: EnginePlatform>(
        platform: &P,
        source: &str,
        use_snapshot: bool,
        block_size: u32,
    ) -> Result<ReadBack> {
        let mut session = platform.open_volume_session(source, use_snapshot)?;
        let mut reader = platform.block_reader(session.read_path(), block_size)?;
        let mut data = Vec::new();
        for block in 0..reader.total_blocks() {
            data.extend(reader.read_block(block)?);
        }
        Ok(ReadBack {
            data,
            snapshot: session.snapshot_used(),
            frozen: session.frozen(),
            completed: session.complete(),
        })
    }

    #[test]
    fn devices_open_by_path() {
        let path = unique_path("vol");
        register_device(&path, pattern(4096), 4096);
        let mut dev = <TestPlatform as EnginePlatform>::Dev::open_read(&path).unwrap();
        assert_eq!(dev.len(), 4096);
        assert_eq!(SourceDevice::sector_size(&dev), 4096);
        let mut buf = [0u8; 16];
        dev.read_at(1000, &mut buf).unwrap();
        assert_eq!(&buf[..], &pattern(4096)[1000..1016]);
        assert!(dev.read_at(4090, &mut buf).is_err(), "past the end");
        assert!(MemDevice::open_read(&unique_path("absent")).is_err());
    }

    #[test]
    fn sessions_are_frozen_only_as_prepared() {
        let live = unique_path("live");
        let idle = unique_path("idle");
        let origin = unique_path("origin");
        let snap = unique_path("snap");
        register_device(&live, pattern(3000), 512);
        register_device(&idle, pattern(2000), 512);
        register_device(&origin, vec![0xEE; 1000], 512);
        register_device(&snap, pattern(1000), 512);
        let p = TestPlatform::new()
            .source(&live, TestSource::Direct { frozen: false })
            .source(&idle, TestSource::Direct { frozen: true })
            .source(
                &origin,
                TestSource::Snapshot {
                    read_path: snap.clone(),
                },
            );

        // Blocks of 1024 over 3000 bytes: the last one is short, nothing lost.
        let live_read = read_through(&p, &live, true, 1024).unwrap();
        assert_eq!(live_read.data, pattern(3000));
        assert!(!live_read.snapshot && !live_read.frozen);
        assert!(
            live_read.completed.is_none(),
            "no snapshot, nothing to complete"
        );

        let idle_read = read_through(&p, &idle, true, 1024).unwrap();
        assert!(!idle_read.snapshot && idle_read.frozen);
        assert!(idle_read.completed.is_none());

        let snap_read = read_through(&p, &origin, true, 512).unwrap();
        assert_eq!(
            snap_read.data,
            pattern(1000),
            "the snapshot is read, not the origin"
        );
        assert!(snap_read.snapshot && snap_read.frozen);
        assert!(matches!(snap_read.completed, Some(Ok(()))));

        // No snapshot asked for: the live origin, and nothing holds it still.
        let origin_read = read_through(&p, &origin, false, 512).unwrap();
        assert_eq!(origin_read.data, vec![0xEE; 1000]);
        assert!(!origin_read.snapshot && !origin_read.frozen);

        // A snapshot is completed once.
        let mut session = p.open_volume_session(&origin, true).unwrap();
        assert!(session.complete().is_some());
        assert!(session.complete().is_none());

        let unprepared = unique_path("unprepared");
        register_device(&unprepared, pattern(512), 512);
        assert!(matches!(
            p.open_volume_session(&unprepared, true),
            Err(OwnBackupError::NoSnapshotProvider(_))
        ));
        assert_eq!(p.normalize_source(" x "), "x");
        p.prepare_privileges();
    }

    #[test]
    fn the_os_knows_no_filesystem_and_unlocks_nothing() {
        let path = unique_path("enc");
        register_device(&path, pattern(8192), 512);
        let p = TestPlatform::new();
        assert!(<TestPlatform as EnginePlatform>::NativeMap::open(&path, 4096).is_err());
        assert_eq!(
            p.ciphertext_extents(&path).unwrap(),
            vec![DiskWindow {
                device: path.clone(),
                offset: 0,
                length: 8192
            }]
        );
        assert!(matches!(
            p.unlock_with_os(&path, "pass"),
            Err(OwnBackupError::NotImplemented(_))
        ));
        let walked = p
            .walk_exclusions(&path, 4096, &["*.tmp".into()], &mut |_| Ok(()))
            .unwrap();
        assert!(walked.is_none(), "no walk — the engine must say so");
    }

    #[test]
    fn vhdx_is_made_in_userspace_with_windows_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.vhdx");
        let p = TestPlatform::new();
        // Not a whole MiB: CreateVirtualDisk rounds it up, and so do we.
        let mut writer = p.create_vhdx(&path, 3 * MIB + 5, 512).unwrap();
        assert_eq!(writer.length(), 4 * MIB);
        writer.write_at(MIB, &[0x5A; 4096]).unwrap();
        writer.finish().unwrap();
        let alloc = crate::vhdx_bat::allocated_payload_blocks(&path).unwrap();
        assert_eq!(alloc.block_size, MIB);
        assert_eq!(alloc.virtual_size, 4 * MIB);
        assert_eq!(alloc.blocks, vec![1]);

        assert!(matches!(
            p.create_vhdx_child(&dir.path().join("c.avhdx"), &path),
            Err(OwnBackupError::NotImplemented(_))
        ));
    }

    #[test]
    fn removal_passes_do_not_run() {
        let p = TestPlatform::new();
        let image = FinishedImage {
            path: Path::new("unused.qcow2"),
            format: ImageFormat::Qcow2,
            encryption: None,
            block_size: MIB as u32,
            zstd_level: 0,
        };
        let span = VolumeSpan {
            data_offset: MIB,
            volume_len: 8 * MIB,
        };
        let mut checksums = vec![BlockChecksum {
            block_number: 0,
            checksum: 7,
        }];
        let mut tick = |_: u64, _: u64| {};
        let inside = p
            .exclude_inside_image(&image, span, &["*.tmp".into()], &mut checksums, &mut tick)
            .unwrap();
        assert!(inside.is_none());
        let path = ExcludedPath {
            rel: r"Temp\a.tmp".into(),
            is_dir: false,
            is_reparse: false,
            whole_records: Vec::new(),
        };
        let removed = p
            .remove_from_image(
                &image,
                span,
                std::slice::from_ref(&path),
                &mut checksums,
                &mut tick,
            )
            .unwrap();
        assert!(removed.is_none());
        let removed = p
            .remove_from_disk_image(
                &image,
                16 * MIB,
                &[(MIB, 8 * MIB, vec![path])],
                &mut checksums,
                &mut tick,
            )
            .unwrap();
        assert!(removed.is_none());
        assert_eq!(checksums.len(), 1, "nothing touched the checksums");
        assert!(!p.entries_kept_reason().is_empty());
    }

    #[test]
    fn disks_answer_from_their_gpt() {
        let layout = DiskLayout {
            disk_guid: "11111111-2222-3333-4444-555555555555".into(),
            sector_size: 512,
            size_bytes: 16 * MIB,
            partitions: vec![
                GptPartitionEntry {
                    index: 1,
                    type_guid: own_backup_common::gpt::GPT_TYPE_MSR.into(),
                    part_guid: "AAAAAAAA-0000-0000-0000-000000000001".into(),
                    first_lba: 2048,
                    last_lba: 4095,
                    attributes: 0,
                    name: "msr".into(),
                },
                GptPartitionEntry {
                    index: 3,
                    type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
                    part_guid: "AAAAAAAA-0000-0000-0000-000000000003".into(),
                    first_lba: 4096,
                    last_lba: 20479,
                    attributes: 0,
                    name: "data".into(),
                },
            ],
        };
        let mut disk = vec![0u8; layout.size_bytes as usize];
        for op in crate::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap() {
            let at = op.offset as usize;
            disk[at..at + op.bytes.len()].copy_from_slice(&op.bytes);
        }
        let path = unique_path("disk");
        register_device(&path, disk, 512);
        let part = unique_path("part3");
        register_device(&part, pattern(512), 512);
        let p = TestPlatform::new()
            .source(&part, TestSource::Direct { frozen: true })
            .slot(&part, 2, 4096 * 512);
        let disk_ref = DiskRef {
            number: 2,
            device: Some(path.clone()),
        };

        assert_eq!(p.read_disk_layout(&disk_ref).unwrap(), layout);
        assert!(p.read_disk_layout(&DiskRef::number(2)).is_err());

        assert_eq!(
            p.partition_in_slot(&part, &disk_ref, 4096 * 512),
            Some(true)
        );
        assert_eq!(
            p.partition_in_slot(&part, &disk_ref, 2048 * 512),
            Some(false)
        );
        assert_eq!(
            p.partition_in_slot(&part, &DiskRef::number(1), 4096 * 512),
            Some(false)
        );
        assert_eq!(p.partition_in_slot(&path, &disk_ref, 0), None);

        assert_eq!(p.disk_identity(&disk_ref).source_id, path);
        assert_eq!(
            p.disk_identity(&DiskRef::number(2)),
            DiskIdentity {
                device_id: "disk2".into(),
                source_id: "disk2".into()
            }
        );
        assert!(matches!(
            p.export_boot_entries(),
            Err(OwnBackupError::NotImplemented(_))
        ));
        let session = p.open_partition_session(&part).unwrap();
        assert!(session.frozen() && session.read_path() == part);
    }
}
