//! Writing a volume image: the target in its container, the GPT of a full
//! image, the reader the copy reads the source through, and — once the copy
//! is done — the provisional sidecar, the meta partition's placeholder and
//! the container's own finish.

use std::path::{Path, PathBuf};

use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::BackupProgress;
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, ImageFormat, PartitionTable,
};
use own_backup_common::{OwnBackupError, Result};

use super::platform::{EnginePlatform, SourceDevice};
use super::source::VolumeSource;
use super::target::ImageTarget;
use super::volume::VolumeBackupRequest;
use crate::block_reader::BlockReader;
use crate::exclusions::{ByteRanges, ZeroingBlockReader};
use crate::fat_map::RawVolume;
use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
use crate::raw_copy::CopyOutcome;
use crate::sidecar::{ChainRole, Sidecar, SidecarInfo};

pub(crate) const MIB: u64 = 1024 * 1024;

/// Create the image a volume copy writes, `disk_size` bytes of virtual disk:
/// a full image, or a child of `parent`. Returns the target and the offset
/// the volume starts at inside it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_volume_target<P: EnginePlatform>(
    platform: &P,
    req: &VolumeBackupRequest,
    owned_file: Option<std::fs::File>,
    parent: &Option<(PathBuf, Sidecar)>,
    kind: BackupType,
    backup_id: Uuid,
    disk_size: u64,
    sector_size: u32,
) -> Result<(ImageTarget, u64)> {
    // An encrypted run writes `.obk`: the same userspace path as qcow2, with
    // every cluster sealed. The sidecar goes inside the container, so the guest
    // meta partition is neither written nor needed (§7.3) — but its size is
    // still what the reservation for the final sidecar is measured by.
    let (target, data_offset) = if let Some(keys) = &req.encryption {
        let (backing_file, data_offset, chain) = match &parent {
            None => (None, DEFAULT_PARTITION_OFFSET, keys.start_chain()?),
            Some((parent_path, parent_sidecar)) => {
                let backing = if parent_path.parent() == req.image_path.parent() {
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
                // The chain key travels inside the parent image, so continuing
                // a chain needs the parent and this job's key, nothing else.
                let chain = keys.open_chain_of(parent_path)?;
                (Some(backing), parent_sidecar.info.volume_data_offset, chain)
            }
        };
        let role = match (&parent, kind) {
            (None, _) => ChainRole::Full,
            (Some(_), BackupType::Differential) => ChainRole::Differential,
            (Some(_), _) => ChainRole::Incremental,
        };
        let parent_backup_id = parent.as_ref().map(|(_, s)| s.manifest.backup_id);
        let plan = crate::seal::context::ImagePlan {
            backup_id,
            role,
            parent: match (&backing_file, parent_backup_id) {
                (Some(name), Some(id)) => Some((id, name.as_str())),
                _ => None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            virtual_size: disk_size,
            source_id: crate::source_identity::stable_source_id(&req.source),
            consolidated_increments: 0,
            chain_started_at: None,
            compression: crate::seal::obk::ObkCompression::Zstd {
                level: crate::qcow2::zstd_level_or_default(req.zstd_level),
            },
        };
        let writer = Box::new(match owned_file {
            Some(file) => {
                crate::seal::obk::ObkStagedWriter::new(crate::seal::obk::ObkWriter::new(
                    std::io::BufWriter::new(file),
                    &chain.key,
                    keys.writer_options(&chain, plan),
                )?)?
            }
            None => crate::seal::obk::ObkStagedWriter::create(
                &req.image_path,
                &chain.key,
                keys.writer_options(&chain, plan),
            )?,
        });
        (ImageTarget::Obk { writer }, data_offset)
    } else {
        match req.image_format {
            ImageFormat::Qcow2 => {
                // Userspace-written compressed image: no attach, no kernel writer.
                // A child references its parent by FILE NAME (qemu resolves
                // relative backing paths against the image's directory), so the
                // backup folder stays relocatable — same convention as the sidecar.
                let (backing_file, data_offset) = match &parent {
                    None => (None, DEFAULT_PARTITION_OFFSET),
                    Some((parent_path, parent_sidecar)) => {
                        // Same folder (the normal case) → bare file name; a parent
                        // elsewhere falls back to its absolute path.
                        let backing = if parent_path.parent() == req.image_path.parent() {
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
                        (Some(backing), parent_sidecar.info.volume_data_offset)
                    }
                };
                let writer = crate::qcow2::Qcow2StagedWriter::create(
                    &req.image_path,
                    crate::qcow2::Qcow2WriterOptions {
                        virtual_size: disk_size,
                        backing_file,
                        compression: crate::qcow2::Qcow2Compression::Zstd {
                            level: crate::qcow2::zstd_level_or_default(req.zstd_level),
                        },
                        ..Default::default()
                    },
                )?;
                (ImageTarget::Qcow2 { writer }, data_offset)
            }
            ImageFormat::Vhdx => {
                let (writer, data_offset) = match &parent {
                    None => {
                        let writer =
                            platform.create_vhdx(&req.image_path, disk_size, sector_size)?;
                        (writer, DEFAULT_PARTITION_OFFSET)
                    }
                    Some((parent_path, parent_sidecar)) => {
                        let writer = platform.create_vhdx_child(&req.image_path, parent_path)?;
                        (writer, parent_sidecar.info.volume_data_offset)
                    }
                };
                (ImageTarget::Vhdx { writer }, data_offset)
            }
        }
    };
    Ok((target, data_offset))
}

/// Write the GPT of a full image and return its disk GUID; a child inherits
/// its parent's, GPT and all, and nothing is written into it.
pub(crate) fn write_volume_gpt(
    target: &mut ImageTarget,
    req: &VolumeBackupRequest,
    parent: &Option<(PathBuf, Sidecar)>,
    volume_len: u64,
    sector_size: u32,
    meta_size: u64,
) -> Result<String> {
    Ok(match &parent {
        None => {
            // --- GPT with Basic Data partition + OwnBackupMeta partition ---
            let meta_partition_guid = Uuid::new_v4();
            let layout = GptLayout {
                disk_size: target.length(),
                sector_size,
                partition_offset: DEFAULT_PARTITION_OFFSET,
                partition_size: volume_len,
                disk_guid: Uuid::new_v4(),
                partition_guid: Uuid::new_v4(),
                partition_name: "OwnBackup".into(),
                // An encrypted image keeps its sidecar sealed inside the
                // container, so there is no partition to describe.
                meta_partition_size: (req.encryption.is_none()).then_some(meta_size),
                meta_partition_guid: (req.encryption.is_none()).then_some(meta_partition_guid),
            };
            for op in gpt_write_ops(&layout)? {
                target.write_at(op.offset, &op.bytes)?;
            }
            // VHDX only: let partmgr re-read the fresh GPT (the disk stays
            // offline, so no volumes surface), then persist the offline
            // attribute keyed to the new disk GUID — future attaches of this
            // image and of every differencing child arrive offline, closing
            // the auto-mount race for the whole chain. Best-effort: a failure
            // only means the next attach may transiently mount. (qcow2 images
            // are never attached, so there is nothing to persist.)
            layout.disk_guid.to_string()
        }
        Some((_, parent_sidecar)) => parent_sidecar.manifest.source_disk.disk_guid.clone(),
    })
}

/// The reader the copy reads the source through: the device, or the
/// decrypting view of `opened`, with the excluded ranges zeroed, the FAT
/// metadata patches applied and the NTFS backup boot sector put back.
pub(crate) fn volume_block_reader<P: EnginePlatform>(
    platform: &P,
    opened: &VolumeSource<P>,
    read_path: &str,
    req: &VolumeBackupRequest,
    zero_ranges: Option<ByteRanges>,
    mut meta_patches: Vec<(u64, Vec<u8>)>,
) -> Result<Box<dyn BlockReader>> {
    let (luks, bitlocker) = (&opened.luks, &opened.bitlocker);
    let (volume_len, sector_size) = (opened.volume_len, opened.sector_size);

    let mut reader: Box<dyn BlockReader> = if luks.is_some() || bitlocker.is_some() {
        // Read decrypted blocks through the LUKS or BitLocker view.
        Box::new(crate::block_reader::RawVolumeBlockReader::new(
            opened.open_source()?,
            req.block_size,
        )?)
    } else {
        platform.block_reader(read_path, req.block_size)?
    };
    if let Some(ranges) = zero_ranges {
        reader = Box::new(ZeroingBlockReader::new(reader, ranges));
    }
    // FAT/exFAT true-removal metadata patches (directory entries + allocation
    // table). Disjoint from the zeroed data clusters and from the NTFS boot-
    // sector patch below (which only fires for NTFS sources), so ordering is
    // immaterial — the patches win on their own byte ranges either way.
    if !meta_patches.is_empty() {
        info!(
            patches = meta_patches.len(),
            "Applying file-removal metadata patches"
        );
        reader = Box::new(crate::block_reader::MultiPatchReader::new(
            reader,
            std::mem::take(&mut meta_patches),
        ));
    }

    // The NTFS backup boot sector lives at TotalSectors × BytesPerSector —
    // the first sector past the filesystem end (the partition is a few
    // sectors longer than the filesystem). That region cannot be read through
    // a volume handle (short read) and would be stored as zeros; NTFS then
    // "self-heals" the zeroed copy when Windows transiently auto-mounts a
    // differencing chain at attach time, writing into the child image and
    // failing verify on the volume's last block (confirmed by VHDX forensics:
    // exactly one healed sector, at TotalSectors, byte-equal to sector 0).
    // Store the correct copy ourselves: NTFS finds a valid backup boot sector
    // and writes nothing — and even if it writes, the bytes are identical.
    // (Skipped for a decrypted LUKS source — its inner FS is never NTFS — and
    // for a BitLocker one, whose volume device cannot be read at all while it
    // is locked; its backup boot sector comes through the decrypting view with
    // the rest of the volume.)
    if luks.is_none() && bitlocker.is_none() && volume_len > sector_size as u64 {
        let mut sector0 = vec![0u8; sector_size as usize];
        let mut probe = P::Dev::open_read(read_path)?;
        probe.read_at(0, &mut sector0)?;
        drop(probe);
        if &sector0[3..7] == b"NTFS" {
            let bps = u16::from_le_bytes(sector0[0x0B..0x0D].try_into().unwrap()) as u64;
            let total_sectors = u64::from_le_bytes(sector0[0x28..0x30].try_into().unwrap());
            let backup_boot_offset = total_sectors.saturating_mul(bps);
            // Sanity: same sector size as the device, and the location must
            // fall within the volume's last MiB — a corrupt BPB must never
            // graft a boot sector into the middle of the data.
            if bps == sector_size as u64
                && backup_boot_offset + bps <= volume_len
                && backup_boot_offset >= volume_len.saturating_sub(MIB)
            {
                info!(
                    backup_boot_offset,
                    "Patching NTFS backup boot sector into the image tail"
                );
                reader = Box::new(crate::block_reader::PatchReader::new(
                    reader,
                    backup_boot_offset,
                    sector0,
                ));
            } else {
                warn!(
                    total_sectors,
                    bps, "NTFS BPB values out of range — tail patch skipped"
                );
            }
        }
    }

    Ok(reader)
}

/// What a volume image is, settled by the time its blocks are copied: the
/// facts its provisional sidecar records.
pub(crate) struct VolumeImageFacts<'a> {
    pub(crate) req: &'a VolumeBackupRequest,
    /// The source as normalized: the manifest's `device_id`.
    pub(crate) source: &'a String,
    /// The name the image is published under; its sidecar records it.
    pub(crate) published_path: &'a Path,
    pub(crate) parent: &'a Option<(PathBuf, Sidecar)>,
    pub(crate) kind: BackupType,
    pub(crate) backup_id: Uuid,
    pub(crate) volume_len: u64,
    pub(crate) sector_size: u32,
    pub(crate) total_blocks: u64,
    pub(crate) used_blocks: u64,
    pub(crate) smart_mode: bool,
    pub(crate) data_offset: u64,
    pub(crate) disk_guid: &'a String,
    pub(crate) meta_size: u64,
    pub(crate) outcome: &'a CopyOutcome,
}

/// Finish the image the blocks were copied into: write the provisional
/// sidecar into the meta partition's place, then let the container finish.
pub(crate) fn finish_volume_image<P: EnginePlatform>(
    mut target: ImageTarget,
    facts: VolumeImageFacts<'_>,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<()> {
    let VolumeImageFacts {
        req,
        source,
        published_path,
        parent,
        kind,
        backup_id,
        volume_len,
        sector_size,
        total_blocks,
        used_blocks,
        smart_mode,
        data_offset,
        disk_guid,
        meta_size,
        outcome,
    } = facts;

    // Pre-allocate the metadata partition blocks in the VHDX/qcow2 image
    // by writing the initial sidecar blob before finalizing (prevents zero-write optimization).
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;

    // Construct a temporary initial sidecar to allocate the block
    let temp_manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id: parent.as_ref().map(|(_, s)| s.manifest.backup_id),
        source_os: P::SOURCE_OS.into(),
        source_disk: DiskInfo {
            device_id: source.clone(),
            stable_device: String::new(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: disk_guid.clone(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
        partition_table: PartitionTable::Gpt,
        compression_algo: CompressionAlgo::None,
        compression_level: 0,
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: if smart_mode {
            BackupMode::UsedSectors
        } else {
            BackupMode::SectorBySector
        },
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    };
    let temp_image_file_name = published_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temp_info = match &parent {
        None => SidecarInfo::new_full(temp_image_file_name, data_offset),
        Some((parent_path, _)) => {
            let parent_name = parent_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let role = match kind {
                BackupType::Incremental => ChainRole::Incremental,
                _ => ChainRole::Differential,
            };
            SidecarInfo::new_child(temp_image_file_name, role, parent_name, data_offset)
        }
    };
    temp_info.container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION.into()
    } else {
        req.image_format.extension().into()
    };
    temp_info.job_id = req.job_id.clone();
    temp_info.source_id = crate::source_identity::stable_source_id(&req.source);
    temp_info.source_disk_layout = req.source_disk.clone();
    let mut temp_blob = Vec::new();
    crate::sidecar::write_sidecar_v2_to(
        &mut temp_blob,
        &temp_info,
        &temp_manifest,
        &outcome.checksums,
    )?;

    // Ensure the placeholder is at least meta_size to cover the entire partition space.
    if req.encryption.is_none() {
        let mut placeholder = vec![0u8; meta_size as usize];
        let placeholder_len = placeholder.len();
        let temp_blob_len = temp_blob.len();
        if temp_blob_len <= placeholder_len {
            placeholder[..temp_blob_len].copy_from_slice(&temp_blob);
        } else {
            placeholder.copy_from_slice(&temp_blob[..placeholder_len]);
        }
        // Set a non-zero byte at the end of the placeholder to ensure it spans the whole allocation
        if placeholder_len > 0 {
            placeholder[placeholder_len - 1] = 1;
        }
        target.write_at(meta_offset, &placeholder)?;
    }

    // Flushing a dynamic/differencing VHDX and detaching it writes all the
    // still-cached image data back to the destination drive — many seconds to
    // minutes for a large image. Signal a "finalize" phase (blocks_total == 0,
    // backup_id "finalize") so the UI shows progress instead of a frozen 100%.
    // Cancellation here is ignored: the data is already copied.
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: outcome.bytes_copied,
        speed_bps: 0,
        categories: None,
        excluded: None,
    });
    // Timed step by step inside finalize(): the stall lives in ONE of the
    // steps (flush of remaining cache vs handle close vs detach for VHDX;
    // metadata write + sync for qcow2), and the log says which.
    // An encrypted image seals this sidecar now and keeps `meta_size` bytes of
    // room, so the final one — with the exclusion figures and the checksums the
    // deletion pass touched — can take its place in the finished file.
    target.finalize(&temp_blob, meta_size as usize)?;
    Ok(())
}
