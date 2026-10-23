//! What the shared engine asks of the system it runs on.
//!
//! The copy pipeline itself — maps, exclusions, the target, the sidecar — is
//! the same everywhere. What differs is how a source is opened and frozen,
//! which filesystem knowledge the OS hands out for free, how a VHDX comes into
//! being and what can delete files inside a finished image. Those questions are
//! the traits below; `engine_platform_win.rs` answers them with today's Windows
//! code, `engine_platform_linux.rs` with the Linux one, and the tests with an
//! in-memory platform (`test_platform.rs`).
//!
//! Nothing here has a default body: a platform that cannot do something says
//! so in its own implementation (`NotImplemented`, `Ok(None)` for a pass that
//! honestly did not run), where the reason can be read next to the code.

use std::path::Path;

use own_backup_common::gpt::DiskLayout;
use own_backup_common::types::ImageFormat;
use own_backup_common::{tr, Result};

use crate::block_reader::BlockReader;
use crate::efivars::BootBackup;
use crate::exclusions::{ByteRanges, ExcludedPath, ExclusionScan};
use crate::fat_map::RawVolume;
use crate::removal_types::InsideRemoval;
use crate::seal::context::ArchiveKeys;
use crate::sidecar::BlockChecksum;
use crate::used_sectors::UsedSectorMap;
use crate::vhdx_format::VhdxWriter;

/// A source device opened for reading: a volume, a snapshot of one, a disk.
///
/// Every filesystem parser and the block reader work over [`RawVolume`]; this
/// adds what the engine needs before it has one — opening by path — and the
/// sector size the block size and the image geometry are checked against.
pub(crate) trait SourceDevice: RawVolume + Send + Sized + 'static {
    /// Open `path` for reading.
    fn open_read(path: &str) -> Result<Self>;

    /// Logical sector size of the device in bytes.
    fn sector_size(&self) -> u32;

    /// Forget what the system cached of this device, so the next reads come
    /// from the device itself. Needed where the same sectors are also
    /// written through another device that bypasses this one's cache — a
    /// partition whose logical volumes device-mapper writes to.
    fn drop_cache(&mut self) -> Result<()>;
}

/// The used-block map the system itself can answer for, tried before the
/// engine's own filesystem parsers: `FSCTL_GET_VOLUME_BITMAP` on Windows
/// (NTFS and ReFS), the NTFS `$Bitmap` read from the device on Linux.
///
/// It is kept typed, not boxed, until exclusions are settled: removing the
/// excluded clusters needs the map's own cluster knowledge.
pub(crate) trait NativeUsedMap: UsedSectorMap + Sized + 'static {
    /// Build the map of the volume at `read_path` in `block_size` blocks. An
    /// error means "not this filesystem" as much as "cannot read": the engine
    /// goes on to its own parsers either way.
    fn open(read_path: &str, block_size: u32) -> Result<Self>;

    /// Cluster size of the filesystem in bytes — what a directory walk
    /// reports file extents in.
    fn cluster_size(&self) -> u32;

    /// Drop every cluster `ranges` covers (volume byte offsets) from the map,
    /// so a block holding only excluded data is not copied at all.
    fn apply_excluded_ranges(&mut self, ranges: &ByteRanges) -> Result<()>;

    /// Free whatever the map kept only for [`Self::apply_excluded_ranges`];
    /// the per-block answers stay valid.
    fn release_bitmap(&mut self);
}

/// How one source is read for the length of a copy: directly, or through a
/// snapshot the session holds.
///
/// Not `Send`: a VSS session is a COM object of the thread that created it.
pub(crate) trait SourceSession {
    /// What the engine opens for every read of the source.
    fn read_path(&self) -> &str;

    /// The data comes from a snapshot rather than the live device (the
    /// summary's "VSS used").
    fn snapshot_used(&self) -> bool;

    /// Nothing changes the source while it is read, so its raw metadata may be
    /// parsed (CLAUDE.md: MFT only from a frozen source).
    fn frozen(&self) -> bool;

    /// The filesystem itself was frozen when the data was captured — a
    /// snapshot, a partition frozen or remounted read-only for its copy: what
    /// it had cached was written back first, so a journal that still reads as
    /// dirty covers only what is already in place
    /// ([`crate::xfs_map::build_xfs_used_map_frozen`]). Implies
    /// [`Self::frozen`], not the other way round: an idle device holds still,
    /// but a filesystem that crashed on it may have a log the btrees are
    /// behind.
    fn quiesced(&self) -> bool;

    /// The copy is done: tell the snapshot machinery and let go of it. `None`
    /// when there was nothing to complete; an error only means the snapshot
    /// could not be released cleanly — the data is already copied.
    fn complete(&mut self) -> Option<Result<()>>;

    /// The service's key directory is on this source and holds something
    /// (decision 6, `seal/LOCAL.md` E4): the copy must leave it out whatever
    /// the job's rules say — exclusions disabled included — or not be made.
    /// The engine adds the rule itself and refuses a copy the rule cannot be
    /// applied to (sector by sector, a source not held still, a filesystem no
    /// scanner reads). Linux asks the mount table; Windows keeps its own
    /// handling, and a test session is told.
    fn must_exclude_key_store(&self) -> bool {
        false
    }

    /// The source is an LVM physical volume whose logical volumes are held
    /// by snapshots of their own: [`Self::read_path`] is the partition, read
    /// for everything no logical volume maps, and each volume is read from
    /// its snapshot. `None` for any other source (and always on Windows).
    fn physical_volume(&self) -> Option<&PhysicalVolume> {
        None
    }
}

/// A partition that is an LVM physical volume in use, as its session holds
/// it (`engine_core::disk_pv`).
#[derive(Debug, Clone, Default)]
pub(crate) struct PhysicalVolume {
    pub(crate) volumes: Vec<PvVolume>,
}

/// One logical volume of a [`PhysicalVolume`].
#[derive(Debug, Clone)]
pub(crate) struct PvVolume {
    /// For messages and the log (`vg/lv`).
    pub(crate) name: String,
    /// Its snapshot: what the copy reads.
    pub(crate) read_path: String,
    /// Bytes.
    pub(crate) len: u64,
    /// Its parts on this partition.
    pub(crate) segments: Vec<own_backup_common::lvm_pv::PvSegment>,
    /// As [`SourceSession::frozen`] and [`SourceSession::quiesced`] for the
    /// volume's own filesystem.
    pub(crate) frozen: bool,
    pub(crate) quiesced: bool,
    /// The service's key directory is on this volume
    /// ([`SourceSession::must_exclude_key_store`]).
    pub(crate) key_store: bool,
}

/// A window onto the device that holds an encrypted volume's ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskWindow {
    /// The device the window is opened on: `\\.\PhysicalDrive2`, `/dev/sdb2`.
    pub(crate) device: String,
    /// Where the volume starts on it, in bytes.
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

/// A finished image a removal pass works on, and how it was written.
// Read by the Windows passes; Linux has none yet (plan N1).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct FinishedImage<'a> {
    pub(crate) path: &'a Path,
    pub(crate) format: ImageFormat,
    /// The job's keys when the image is an encrypted `.obk`.
    pub(crate) encryption: Option<&'a ArchiveKeys>,
    pub(crate) block_size: u32,
    /// The job's zstd level as asked; the passes resolve it with
    /// [`crate::qcow2::zstd_level_or_default`].
    pub(crate) zstd_level: i32,
}

/// Where the volume lies inside a per-volume image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VolumeSpan {
    pub(crate) data_offset: u64,
    pub(crate) volume_len: u64,
}

/// An empty dynamic VHDX made in userspace with the geometry Windows'
/// `CreateVirtualDisk` gives ours (`vhdx::create_dynamic_vhdx`): 1 MiB payload
/// blocks and the size rounded up to a whole MiB. The same image then comes out
/// of either system.
// The Linux and test platforms make their VHDX here; Windows asks its driver.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn userspace_vhdx(path: &Path, size: u64, sector_size: u32) -> Result<VhdxWriter> {
    const MIB: u64 = 1024 * 1024;
    VhdxWriter::create(path, size.div_ceil(MIB) * MIB, MIB, sector_size)
}

/// The system half of a volume backup.
pub(crate) trait EnginePlatform {
    /// A source device opened for reading.
    type Dev: SourceDevice;
    /// The used-block map the OS answers for (see [`NativeUsedMap`]).
    type NativeMap: NativeUsedMap;
    /// A source held for the length of a copy.
    type Session: SourceSession;
    /// What keeps a volume the OS unlocked safe while it is copied: dropping
    /// it lets go.
    type OsUnlock;

    /// `source_os` of the images this platform writes.
    const SOURCE_OS: &'static str;

    /// The engine's own metadata parsers (the used maps and exclusion scans of
    /// FAT, ext, btrfs and xfs) need a frozen session as much as the MFT scan
    /// does. On Windows they read volumes Windows does not mount, which nothing
    /// writes; on Linux they read what the session gives, and only a frozen
    /// session holds still. Where this is true, an unfrozen session's volume
    /// is copied whole, its metadata never parsed.
    const METADATA_NEEDS_FROZEN: bool;

    /// The canonical form of a source path, before anything opens it.
    fn normalize_source(&self, source: &str) -> String;

    /// Make the process able to read everything a backup reads. Best-effort.
    fn prepare_privileges(&self);

    /// Hold `source` (already normalized) for a volume copy; `use_snapshot`
    /// is the request's wish for a snapshot of a live volume.
    fn open_volume_session(&self, source: &str, use_snapshot: bool) -> Result<Self::Session>;

    /// A block reader over the source at `read_path`.
    fn block_reader(&self, read_path: &str, block_size: u32) -> Result<Box<dyn BlockReader>>;

    /// Where the ciphertext of the encrypted volume at `read_path` lies. One
    /// window is the only shape the engine copies; it refuses the others with
    /// its own words.
    fn ciphertext_extents(&self, read_path: &str) -> Result<Vec<DiskWindow>>;

    /// Ask the OS to unlock the encrypted volume at `read_path` (a cipher our
    /// own reader does not know, a filesystem only the OS can list). The volume
    /// then reads like any other while the returned guard lives.
    fn unlock_with_os(&self, read_path: &str, passphrase: &str) -> Result<Self::OsUnlock>;

    /// Walk the mounted filesystem behind `read_path` and collect the extents
    /// of the files `patterns` exclude — the fallback when the MFT cannot be
    /// read. `Ok(None)`: this platform has no such walk, and the engine says
    /// so instead of pretending the rules matched nothing.
    fn walk_exclusions(
        &self,
        read_path: &str,
        cluster_size: u32,
        patterns: &[String],
        tick: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<Option<ExclusionScan>>;

    /// Create an empty dynamic VHDX of `size` bytes with `sector_size`
    /// logical sectors, ready for userspace writes.
    fn create_vhdx(&self, path: &Path, size: u64, sector_size: u32) -> Result<VhdxWriter>;

    /// Create a differencing VHDX over `parent`, ready for userspace writes.
    fn create_vhdx_child(&self, path: &Path, parent: &Path) -> Result<VhdxWriter>;

    /// Match `patterns` inside the finished image of an encrypted source and
    /// delete and erase what they match there. `Ok(None)`: the pass did not run
    /// (it logged why).
    fn exclude_inside_image(
        &self,
        image: &FinishedImage<'_>,
        span: VolumeSpan,
        patterns: &[String],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<InsideRemoval>>;

    /// Delete the catalog entries of `paths` from the finished image, whose
    /// data never entered it. `Some(n)`: the pass ran and `n` entries could
    /// not be deleted; `None`: it did not run (it logged why).
    fn remove_from_image(
        &self,
        image: &FinishedImage<'_>,
        span: VolumeSpan,
        paths: &[ExcludedPath],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>>;

    /// The result screen's sentence for catalog entries the removal pass did
    /// not delete.
    fn entries_kept_reason(&self) -> String;

    /// Whether the removal pass leaves the image's NTFS journal (`$LogFile`)
    /// as the driver kept it — with the old bytes of the records the pass
    /// cleared (`ExcludedPath::whole_records`). The Windows driver does;
    /// ntfs-3g empties the journal when it mounts the volume.
    fn journal_keeps_cleared_content(&self) -> bool {
        false
    }
}

/// The note for the result screen when an `$MFT` scan could not reach every
/// record (`MftScan::records_unreachable`): files the rules never saw. The
/// partial scan stands on every platform — falling back to a directory walk
/// would lose what only the scan knows (the content it leaves behind, the
/// records whose resident content is cleared, the key-directory proof), and
/// an encrypted volume has no walk to fall back to at all.
pub(crate) fn unreachable_note(scan: &crate::ntfs_scan::MftScan) -> Option<String> {
    let count = scan.records_unreachable;

    (count > 0).then(|| tr!("engine.mft_records_unreachable", count = count).into_owned())
}

/// Whether a removal pass that ran over `paths` on `platform` leaves resident
/// content of excluded files in the image's journal: it cleared records
/// (`whole_records`) through a driver that keeps its journal. Said on the
/// result screen — incomplete erasure is reported, never implied complete.
pub(crate) fn resident_content_may_remain_in_journal<P: EnginePlatform>(
    platform: &P,
    paths: &[ExcludedPath],
) -> bool {
    platform.journal_keeps_cleared_content() && paths.iter().any(|p| !p.whole_records.is_empty())
}

/// A whole disk, as a job names it: Windows by number, Linux by its stable
/// `/dev/disk/by-id/…` path (decision R1). `number` is all Windows has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskRef {
    pub(crate) number: u32,
    pub(crate) device: Option<String>,
}

impl DiskRef {
    /// A disk known by its number only, as on Windows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn number(number: u32) -> Self {
        Self {
            number,
            device: None,
        }
    }
}

/// Who a whole disk is, in the two places an image records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskIdentity {
    /// The device the disk is read through: the manifest's `device_id`.
    pub(crate) device_id: String,
    /// The short name the copy is known by: its progress id and the source id
    /// sealed into an encrypted image.
    pub(crate) source_id: String,
}

/// The system half of a whole-disk backup.
pub(crate) trait DiskPlatform: EnginePlatform {
    /// Hold the partition `device` (already normalized) for the copy. All
    /// partitions of a disk are held at once, so the image is one point in
    /// time across them.
    fn open_partition_session(&self, device: &str) -> Result<Self::Session>;

    /// The disk's GPT, read off the disk.
    fn read_disk_layout(&self, disk: &DiskRef) -> Result<DiskLayout>;

    /// Does `device` lie on `disk` exactly at `offset`? `None` when the system
    /// cannot tell; the engine then trusts the job.
    fn partition_in_slot(&self, device: &str, disk: &DiskRef, offset: u64) -> Option<bool>;

    fn disk_identity(&self, disk: &DiskRef) -> DiskIdentity;

    /// The machine's firmware boot entries, for a bootable copy.
    fn export_boot_entries(&self) -> Result<BootBackup>;

    /// Delete the catalog entries of each partition's `paths` (partition
    /// offset, the length of its volume, paths) from the finished whole-disk
    /// image. Same answer as [`EnginePlatform::remove_from_image`].
    fn remove_from_disk_image(
        &self,
        image: &FinishedImage<'_>,
        image_size: u64,
        partitions: &[(u64, u64, Vec<ExcludedPath>)],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>>;
}
