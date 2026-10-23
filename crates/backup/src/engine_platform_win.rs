//! The Windows answers to the shared engine's questions
//! ([`crate::engine_core::platform`]).
//!
//! Thin wrappers over the Windows code the engine runs: VSS for a live volume,
//! `FSCTL_GET_VOLUME_BITMAP` for the used map, the volume manager for where a
//! locked volume's ciphertext lies, Windows itself for a cipher our reader does
//! not know, `CreateVirtualDisk` for new images and the filesystem driver for
//! deleting excluded files inside them. Each body is the one the volume
//! pipeline used to run inline (moved here verbatim when it was made generic),
//! so running the engine on this platform changes nothing it does.

#![cfg(target_os = "windows")]

use std::path::Path;

use tracing::{info, warn};

use own_backup_common::gpt::DiskLayout;
use own_backup_common::types::ImageFormat;
use own_backup_common::Result;
use own_backup_snapshot::VssSnapshotSession;

use crate::block_reader::BlockReader;
use crate::block_reader_win::WinBlockReader;
use crate::efivars::BootBackup;
use crate::engine_core::platform::{
    DiskIdentity, DiskPlatform, DiskRef, DiskWindow, EnginePlatform, FinishedImage, SourceSession,
    VolumeSpan,
};
use crate::engine_core::target::{entries_kept_reason, snapshot_wanted, volume_root_of};
use crate::exclusions::{ExcludedPath, ExclusionScan};
use crate::removal_types::InsideRemoval;
use crate::sidecar::BlockChecksum;
use crate::used_sectors_win::NtfsUsedSectorMap;
use crate::vhdx::{create_differencing_vhdx, create_dynamic_vhdx, VhdxCreateOptions};
use crate::vhdx_format::VhdxWriter;
use crate::win_device::{normalize_device_path, WinRawDevice};
use crate::windows_unlock_win::ReadOnlyDisk;

/// The platform the Windows engine runs on.
pub(crate) struct WinPlatform;

/// A source held for a copy: read directly, or through a VSS shadow copy that
/// lives as long as the session (or until [`SourceSession::complete`]).
pub(crate) struct WinSession {
    read_path: String,
    /// Whether the read path is a shadow copy — fixed when the session is
    /// opened, so it still says so after the shadow copy is released.
    shadow: bool,
    vss: Option<VssSnapshotSession>,
}

impl WinSession {
    fn new(read_path: String, vss: Option<VssSnapshotSession>) -> Self {
        Self {
            read_path,
            shadow: vss.is_some(),
            vss,
        }
    }
}

impl SourceSession for WinSession {
    fn read_path(&self) -> &str {
        &self.read_path
    }

    fn snapshot_used(&self) -> bool {
        self.shadow
    }

    /// A shadow copy, or the boot medium, where the system on the disk is
    /// offline and nothing writes the volume.
    fn frozen(&self) -> bool {
        self.shadow || own_backup_common::media_mode::is_media_mode()
    }

    /// VSS flushes and freezes the volume for the instant it is taken. Asked
    /// only where `METADATA_NEEDS_FROZEN` holds, which is not here.
    fn quiesced(&self) -> bool {
        self.shadow
    }

    /// VSS BackupComplete, then the shadow copy is released.
    fn complete(&mut self) -> Option<Result<()>> {
        self.vss.take().map(VssSnapshotSession::complete)
    }
}

impl EnginePlatform for WinPlatform {
    type Dev = WinRawDevice;
    type NativeMap = NtfsUsedSectorMap;
    type Session = WinSession;
    type OsUnlock = ReadOnlyDisk;

    const SOURCE_OS: &'static str = "windows";
    const METADATA_NEEDS_FROZEN: bool = false;

    fn normalize_source(&self, source: &str) -> String {
        normalize_device_path(source)
    }

    /// SeBackupPrivilege, so the exclusion walk can list ACL-protected paths
    /// (System Volume Information, protected app data).
    fn prepare_privileges(&self) {
        crate::privileges_win::enable_backup_privileges();
    }

    /// A shadow copy of a live lettered volume; anything else, or a failed
    /// shadow copy, is read directly (crash-consistent).
    fn open_volume_session(&self, source: &str, use_snapshot: bool) -> Result<WinSession> {
        let mut vss: Option<VssSnapshotSession> = None;
        let read_path = if snapshot_wanted(use_snapshot) {
            match volume_root_of(source) {
                Some(root) => match VssSnapshotSession::create(&root) {
                    Ok(session) => {
                        let path = session.device_path().to_string();
                        vss = Some(session);
                        path
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "VSS snapshot failed — reading the live volume (crash-consistent copy)"
                        );
                        source.to_string()
                    }
                },
                None => {
                    info!("Source is not a lettered volume; skipping VSS");
                    source.to_string()
                }
            }
        } else {
            source.to_string()
        };
        Ok(WinSession::new(read_path, vss))
    }

    fn block_reader(&self, read_path: &str, block_size: u32) -> Result<Box<dyn BlockReader>> {
        Ok(Box::new(WinBlockReader::open(read_path, block_size)?))
    }

    /// The volume's extents on its physical disks: a locked BitLocker volume
    /// refuses every read through its own device, the disk under it does not.
    fn ciphertext_extents(&self, read_path: &str) -> Result<Vec<DiskWindow>> {
        Ok(crate::volume_win::volume_extents(read_path)
            .into_iter()
            .map(|e| DiskWindow {
                device: format!(r"\\.\PhysicalDrive{}", e.disk_number),
                offset: e.offset,
                length: e.length,
            })
            .collect())
    }

    fn unlock_with_os(&self, read_path: &str, passphrase: &str) -> Result<ReadOnlyDisk> {
        crate::engine_win::unlock_source_with_windows(read_path, passphrase)
    }

    fn walk_exclusions(
        &self,
        read_path: &str,
        cluster_size: u32,
        patterns: &[String],
        tick: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<Option<ExclusionScan>> {
        crate::exclusions_win::collect_excluded_ranges(read_path, cluster_size, patterns, tick)
            .map(Some)
    }

    fn create_vhdx(&self, path: &Path, size: u64, sector_size: u32) -> Result<VhdxWriter> {
        create_dynamic_vhdx(
            path,
            &VhdxCreateOptions {
                max_size: size,
                logical_sector_size: sector_size,
                physical_sector_size: std::cmp::max(sector_size, 4096),
            },
        )?;
        VhdxWriter::open_existing(path)
    }

    fn create_vhdx_child(&self, path: &Path, parent: &Path) -> Result<VhdxWriter> {
        create_differencing_vhdx(path, parent)?;
        VhdxWriter::open_existing(path)
    }

    fn exclude_inside_image(
        &self,
        image: &FinishedImage<'_>,
        span: VolumeSpan,
        patterns: &[String],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<InsideRemoval>> {
        match image.format {
            ImageFormat::Vhdx => crate::true_removal_win::exclude_inside_image(
                image.path,
                patterns,
                span.data_offset,
                span.volume_len,
                image.block_size,
                checksums,
                tick,
            ),
            // Same pass over a copy-on-write overlay attached over iSCSI: the driver
            // that owns NTFS does the deleting either way, and what it sits on
            // is the image format's business, not the pass's.
            ImageFormat::Qcow2 => crate::qcow2_removal_win::exclude_inside_qcow2(
                image.path,
                patterns,
                span.data_offset,
                span.volume_len,
                image.block_size,
                Some(crate::qcow2::zstd_level_or_default(image.zstd_level)),
                checksums,
                tick,
            ),
        }
    }

    fn remove_from_image(
        &self,
        image: &FinishedImage<'_>,
        span: VolumeSpan,
        paths: &[ExcludedPath],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>> {
        Ok(match image.format {
            ImageFormat::Vhdx => {
                // Differencing child + MergeVirtualDisk via the kernel VHDX driver.
                crate::true_removal_win::remove_excluded_from_image(
                    image.path,
                    paths,
                    span.data_offset,
                    span.volume_len,
                    image.block_size,
                    checksums,
                    tick,
                )?
                .map(|outcome| outcome.entries_failed)
            }
            // An encrypted image gets the same pass over a SEALED scratch, and
            // the changed clusters go back by rebuilding the container: it is
            // written once, so they cannot be folded in place.
            ImageFormat::Qcow2 if image.encryption.is_some() => {
                let keys = image.encryption.expect("checked above");
                let chain = keys.open_chain_of(image.path)?;
                crate::obk_removal_win::remove_excluded_from_obk(
                    image.path,
                    &chain.key,
                    paths,
                    span.data_offset,
                    span.volume_len,
                    image.block_size,
                    crate::qcow2::zstd_level_or_default(image.zstd_level),
                    checksums,
                    tick,
                )?
                .map(|outcome| outcome.entries_failed)
            }
            ImageFormat::Qcow2 => {
                // RW-mount over a CoW overlay, delete, fold the touched
                // clusters back with Qcow2InPlaceMerge (Q-R3).
                crate::qcow2_removal_win::remove_excluded_from_qcow2(
                    image.path,
                    paths,
                    span.data_offset,
                    span.volume_len,
                    image.block_size,
                    Some(crate::qcow2::zstd_level_or_default(image.zstd_level)),
                    checksums,
                    tick,
                )?
                .map(|outcome| outcome.entries_failed)
            }
        })
    }

    fn entries_kept_reason(&self) -> String {
        entries_kept_reason()
    }

    /// The driver replays and keeps its journal; see
    /// `true_removal_win::clear_resident`.
    fn journal_keeps_cleared_content(&self) -> bool {
        true
    }

    fn has_directory_walk(&self) -> bool {
        true
    }
}

impl DiskPlatform for WinPlatform {
    /// A shadow copy of the partition's volume when it has a letter; the
    /// partition itself otherwise.
    fn open_partition_session(&self, device: &str) -> Result<WinSession> {
        let mut vss: Option<VssSnapshotSession> = None;
        let read_path = match volume_root_of(device).filter(|_| snapshot_wanted(true)) {
            Some(root) => match VssSnapshotSession::create(&root) {
                Ok(session) => {
                    let path = session.device_path().to_string();
                    vss = Some(session);
                    path
                }
                Err(e) => {
                    warn!(error = %e, device = %device, "VSS failed — reading the live volume");
                    device.to_string()
                }
            },
            None => device.to_string(),
        };
        Ok(WinSession::new(read_path, vss))
    }

    fn read_disk_layout(&self, disk: &DiskRef) -> Result<DiskLayout> {
        crate::gpt_reader::read_disk_layout(disk.number)
    }

    /// From the volume manager's extents of the device; `None` when Windows
    /// cannot tell where the device lives.
    fn partition_in_slot(&self, device: &str, disk: &DiskRef, offset: u64) -> Option<bool> {
        crate::source_identity::extents_of_source(device).map(|extents| {
            matches!(extents.as_slice(),
                [e] if e.disk == disk.number && e.offset == offset)
        })
    }

    fn disk_identity(&self, disk: &DiskRef) -> DiskIdentity {
        DiskIdentity {
            device_id: format!(r"\\.\PhysicalDrive{}", disk.number),
            source_id: format!("disk{}", disk.number),
        }
    }

    fn export_boot_entries(&self) -> Result<BootBackup> {
        crate::efivars_win::export()
    }

    fn remove_from_disk_image(
        &self,
        image: &FinishedImage<'_>,
        image_size: u64,
        partitions: &[(u64, u64, Vec<ExcludedPath>)],
        checksums: &mut Vec<BlockChecksum>,
        tick: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>> {
        // Windows finds each volume through the attached disk itself; the
        // length the engine knows is not needed there.
        let partitions: Vec<(u64, Vec<ExcludedPath>)> = partitions
            .iter()
            .map(|(offset, _, paths)| (*offset, paths.clone()))
            .collect();
        let partitions = partitions.as_slice();
        Ok(match (image.encryption, image.format) {
            // Encrypted: the same mount-and-delete, over a sealed scratch, with
            // the container rebuilt around whatever the driver changed. A
            // plaintext scratch beside an encrypted copy would hold exactly the
            // filesystem metadata naming the files being removed (§7.4).
            (Some(keys), _) => {
                let chain = keys.open_chain_of(image.path)?;
                crate::obk_removal_win::remove_excluded_from_obk_disk_image(
                    image.path,
                    &chain.key,
                    partitions,
                    image_size,
                    image.block_size,
                    crate::qcow2::zstd_level_or_default(image.zstd_level),
                    checksums,
                    tick,
                )?
                .map(|outcome| outcome.entries_failed)
            }
            (None, ImageFormat::Vhdx) => crate::true_removal_win::remove_excluded_from_disk_image(
                image.path,
                partitions,
                image_size,
                image.block_size,
                checksums,
                tick,
            )?
            .map(|outcome| outcome.entries_failed),
            (None, ImageFormat::Qcow2) => {
                crate::qcow2_removal_win::remove_excluded_from_disk_image_qcow2(
                    image.path,
                    partitions,
                    image_size,
                    image.block_size,
                    Some(crate::qcow2::zstd_level_or_default(image.zstd_level)),
                    checksums,
                    tick,
                )?
                .map(|outcome| outcome.entries_failed)
            }
        })
    }
}
