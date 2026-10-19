//! What a whole-disk copy produces: the image target it writes (full or a
//! child of the previous version), the firmware boot entries a bootable copy
//! carries, and the summary it reports.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::info;

use own_backup_common::types::{BackupType, ImageFormat};
use own_backup_common::{OwnBackupError, Result};

use super::platform::{DiskPlatform, EnginePlatform};
use super::target::{create_disk_obk_target, DiskImagePlan, ImageTarget};

/// Read the machine's UEFI boot entries, to be embedded in the image.
///
/// Why this is worth anything after a restore: [`crate::gpt_writer`] recreates
/// the disk with every partition's ORIGINAL unique GUID, offset and size, and a
/// boot entry names its partition by exactly those three things. So an entry
/// saved here still points at the right partition on the restored disk — there
/// is nothing to translate, only something to put back.
///
/// Only for a copy the user marked BOOTABLE. On any other disk these entries
/// describe a boot menu that copy has nothing to do with: a data disk's backup
/// carrying the machine's firmware configuration is metadata nobody asked for,
/// and a restore of it onto another machine would be actively wrong.
///
/// `None` is not an error. A machine on a plain BIOS has no such entries, and
/// an unelevated run cannot read them; neither is a reason to spoil a backup
/// that succeeded.
pub(crate) fn boot_entries_for_image<P: DiskPlatform>(
    platform: &P,
    bootable: bool,
) -> Option<crate::efivars::BootBackup> {
    if !bootable {
        info!("not a bootable copy — the machine's boot entries are not part of it");
        return None;
    }
    match platform.export_boot_entries() {
        Ok(backup) => {
            info!(
                entries = backup.readable.len(),
                "boot entries read into the image"
            );
            Some(backup)
        }
        Err(e) => {
            info!(error = %e, "boot entries not read — the copy will carry none");
            None
        }
    }
}

/// Outcome of a completed whole-disk backup.
pub struct DiskBackupSummary {
    pub image_path: PathBuf,
    pub bytes_copied: u64,
    pub blocks_copied: u64,
    pub partitions: u32,
    pub duration: Duration,
    /// What was actually produced — may differ from the requested kind when a
    /// broken/incompatible parent forced a fallback to a full backup.
    pub backup_kind: BackupType,
    /// Used image blocks skipped because they matched the parent version.
    pub blocks_unchanged: u64,
    /// Files excluded across all partitions (plan D2).
    pub excluded_files: u64,
    /// Bytes their extents freed from the image.
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of kept files across all partitions (for the
    /// dashboard); `None` when no exclusion walk ran on any partition.
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Partitions whose files nobody walked, and why — named, because on a
    /// whole-disk copy "исключения не применялись" without saying to WHICH
    /// partition is barely better than silence. `None` when every partition
    /// was walked.
    pub file_scan_skipped: Option<String>,
    /// Why the excluded files' catalog entries were NOT deleted from the
    /// image, in words for the result screen — `None` when they were, or when
    /// there was nothing to delete.
    ///
    /// Their contents never enter the copy either way (the MFT road leaves
    /// their clusters out); what stays is their names and empty shells in the
    /// image's filesystem, which the rules promise not to happen (decision of
    /// 2026-07-28). On 2026-09-25 the pass failed over an iSCSI session left
    /// behind, 746 paths stayed in that night's copy — and the run was a plain
    /// success, with the reason in the service log only.
    pub excluded_entries_kept: Option<String>,
}

/// Create a differencing CHILD target over `parent_path` for an incremental /
/// differential whole-disk backup. The child inherits the parent's virtual size
/// and its GPT (resolved through the chain), so no GPT is written here.
/// `image_size` is the parent's virtual size (used only to size the qcow2 child).
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_disk_child_target<P: EnginePlatform>(
    platform: &P,
    image_path: &Path,
    parent_path: &Path,
    image_size: u64,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    plan: &DiskImagePlan,
    owned_file: Option<std::fs::File>,
) -> Result<(ImageTarget, u32)> {
    if let Some(keys) = encryption {
        return create_disk_obk_target(
            image_path,
            Some(parent_path),
            image_size,
            keys,
            plan,
            owned_file,
        );
    }
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            // Reference the parent by bare file name when it sits in the same
            // folder (qemu resolves relative backing paths against the image's
            // directory) — same convention as the per-volume path.
            let backing = if parent_path.parent() == image_path.parent() {
                parent_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        OwnBackupError::Config(format!(
                            "parent image path has no file name: {}",
                            parent_path.display()
                        ))
                    })?
            } else {
                parent_path.to_string_lossy().into_owned()
            };
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: image_size,
                    backing_file: Some(backing),
                    compression: crate::qcow2::Qcow2Compression::Zstd {
                        level: plan.zstd_level,
                    },
                    ..Default::default()
                },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            let writer = platform.create_vhdx_child(image_path, parent_path)?;
            Ok((ImageTarget::Vhdx { writer }, sector_size))
        }
    }
}

/// Create an image target of `disk_size` bytes with no parent (full backup).
/// Returns the target and its logical sector size.
pub(crate) fn create_disk_image_target<P: EnginePlatform>(
    platform: &P,
    image_path: &Path,
    disk_size: u64,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    plan: &DiskImagePlan,
    owned_file: Option<std::fs::File>,
) -> Result<(ImageTarget, u32)> {
    if let Some(keys) = encryption {
        return create_disk_obk_target(image_path, None, disk_size, keys, plan, owned_file);
    }
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: disk_size,
                    backing_file: None,
                    compression: crate::qcow2::Qcow2Compression::Zstd {
                        level: plan.zstd_level,
                    },
                    ..Default::default()
                },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            let writer = platform.create_vhdx(image_path, disk_size, sector_size)?;
            Ok((ImageTarget::Vhdx { writer }, sector_size))
        }
    }
}
