//! One partition of a whole-disk copy, prepared before a single block is
//! copied: its session, what it is (a plain volume, a LUKS or BitLocker
//! container, opened or not), its used-block map and its file exclusions.

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use super::partition_prep::{linux_fs_exclusions, PreparedMap};
use super::platform::{EnginePlatform, NativeUsedMap, SourceDevice};
use super::source::BitLockerSource;
use super::target::no_patterns;
use super::volume_exclusions::scan_ntfs_exclusions_by_mft;
use crate::exclusions::ByteRanges;
use crate::used_sectors::UsedSectorMap;

/// One prepared partition: its VSS snapshot, decrypt params, used-map and the
/// GPT entry it fills. Snapshots for all partitions are taken up front so the
/// image is cross-volume consistent, then copied in one pass.
pub(crate) struct PreparedPartition<P: EnginePlatform> {
    pub(crate) part_index: u32,
    pub(crate) device: String,
    pub(crate) read_path: String,
    pub(crate) luks: Option<crate::luks::LuksParams>,
    /// Ключ и окно на диск для запертого BitLocker-тома. Читать такой том через
    /// его собственное устройство нельзя — оно отвечает 0x80310000 на всё.
    pub(crate) bitlocker: Option<BitLockerSource>,
    /// Тот же том, но пароля нет: копируется шифротекстом через окно на диск.
    pub(crate) locked_window: Option<(String, u64, u64)>,
    /// Запрет записи на диск-источник: держится, пока раздел, открытый силами
    /// Windows, остаётся смонтированным — то есть до конца копирования.
    #[allow(dead_code)]
    pub(crate) ro_guard: Option<P::OsUnlock>,
    /// The container's own header, captured so a restore can put the
    /// encryption back (D19). `None` when the partition was not encrypted or
    /// the header could not be read.
    pub(crate) luks_header: Option<own_backup_common::image::LuksHeaderBackup>,
    pub(crate) used: Box<dyn UsedSectorMap>,
    pub(crate) volume_len: u64,
    pub(crate) part_offset: u64,
    pub(crate) filesystem: String,
    /// Excluded byte ranges (VOLUME-relative) and FAT/exFAT metadata patches for
    /// this partition (plan D2); empty when no exclusions apply.
    pub(crate) zero_ranges: ByteRanges,
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2).
    pub(crate) removal_paths: Vec<crate::exclusions::ExcludedPath>,
    /// The encrypted container this partition holds, if any — recorded whether
    /// or not it was opened, so the manifest can say how it was captured.
    pub(crate) container: Option<own_backup_common::image::VolumeEncryptionKind>,
    /// The container was read in the clear — opened by the engine with its
    /// passphrase, or by the system ([`super::platform::OpenedContainer`]).
    pub(crate) opened: bool,
    /// An LVM physical volume in use, assembled from its volumes' snapshots
    /// and itself (`disk_pv`); `None` for any other partition.
    pub(crate) pv: Option<super::disk_pv::PvPrepared>,
    /// The partition's session (its VSS shadow copy), kept alive until the
    /// copy is done.
    pub(crate) session: P::Session,
}

/// Build a partition's used-map and apply file exclusions (plan D2). The
/// filesystem is determined from the map that parses (over the DECRYPTED view
/// for a LUKS volume, so an unlocked Linux root reports "btrfs", not "unknown").
///
/// The exclusion mask set is chosen by filesystem family: `patterns_windows`
/// for NTFS/ReFS/FAT, `patterns_linux` for ext/btrfs/xfs. NTFS/ReFS drop + zero
/// the excluded clusters (the catalog entry survives as a zero-filled ghost —
/// whole-disk NTFS catalog true-removal is a later step); FAT32/exFAT also patch
/// the metadata so the files vanish. Enforcement on ext/btrfs/xfs is a follow-up
/// (no Linux-FS exclusion scanner yet), so Linux patterns are selected but not
/// yet acted on; LUKS-decrypted volumes are captured whole.
///
/// `tick` reports "prepare"-phase progress (and can cancel by returning `Err`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_partition_map<P: EnginePlatform>(
    platform: &P,
    read_path: &str,
    block_size: u32,
    luks: &Option<crate::luks::LuksParams>,
    patterns_windows: &[String],
    patterns_linux: &[String],
    // Источник заморожен (снимок VSS или офлайн на носителе) — только тогда
    // можно разбирать сырые метаданные: том под записью для этого не годится.
    frozen: bool,
    // Captured with the filesystem frozen: XFS is read past a log that is
    // dirty only because of the freeze (`xfs_map::build_xfs_used_map_frozen`).
    quiesced: bool,
    // `tick(entries_done, entries_total)`; total 0 = «сколько всего, неизвестно»
    // (обход каталога этого не знает, чтение $MFT — знает).
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> Result<PreparedMap> {
    // NTFS/ReFS/FAT use the Windows masks; ext/btrfs/xfs use the Linux masks.
    let patterns = patterns_windows;
    let none = |fs: &str, map: Option<Box<dyn UsedSectorMap>>| PreparedMap {
        map,
        filesystem: fs.into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        kept_named: 0,
        excluded_bytes: 0,
        categories: None,
        skipped: None,
        left: Vec::new(),
        notes: Vec::new(),
    };
    // The same, with the reason nobody walked this partition's files.
    let none_because = |fs: &str, map: Option<Box<dyn UsedSectorMap>>, why: &str| PreparedMap {
        skipped: Some(why.into()),
        ..none(fs, map)
    };
    // Every exclusion scanner reads through this. The alignment wrapper is not
    // a nicety: a raw Windows device refuses reads that do not start on a
    // sector boundary, and ext inodes are 256 bytes — the root inode among the
    // unaligned ones. Without it the ext scan failed on its first read, skipped
    // the whole tree, and reported "excluded_files=0" as if it had worked.
    let open = || -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        let dev = P::Dev::open_read(read_path)?;
        Ok(match luks {
            Some(p) => Box::new(crate::aligned_volume::AlignedVolume::new(
                crate::decrypt::DecryptingVolume::new(Box::new(dev), p.clone()),
            )),
            None => Box::new(crate::aligned_volume::AlignedVolume::new(dev)),
        })
    };
    // A platform whose own parsers need a still source (Linux) does not read
    // the metadata of a partition that may be written meanwhile: a block
    // allocated after its bitmap was read would be left out. It is copied
    // whole, and the result says why. Windows parses as before (its constant
    // is false).
    if P::METADATA_NEEDS_FROZEN && !frozen {
        warn!(part = %read_path, "partition not held still — copied whole, metadata not parsed");
        return Ok(none_because(
            "unknown",
            None,
            &tr!("engine.skip_not_frozen"),
        ));
    }
    // NTFS first (only on a plain, non-decrypted volume), then FAT/ext/btrfs/xfs.
    if luks.is_none() {
        if let Ok(mut m) = P::NativeMap::open(read_path, block_size) {
            let mut out = none("ntfs", None);
            if !patterns.is_empty() {
                // Сначала по $MFT — как и в копии одного тома (B1): вдвое
                // быстрее и не теряет pagefile.sys/swapfile.sys, которые обходу
                // не открыть. Обход остаётся запасным: ReFS (у неё нет MFT),
                // грязный том, неразобранная запись.
                let mut mft_tick = |n: u64, total: u64| tick(n, total);
                let fast = if frozen {
                    scan_ntfs_exclusions_by_mft::<P::Dev>(read_path, patterns, &mut mft_tick)
                } else {
                    Err(OwnBackupError::Device(
                        tr!("engine_win.source_not_frozen").into_owned(),
                    ))
                };
                // Records out of reach: the partial scan stands, said so
                // (`platform::unreachable_note`).
                if let Ok(mft) = &fast {
                    out.notes.extend(super::platform::unreachable_note(mft));
                }
                match fast {
                    Ok(mft) => {
                        out.excluded_files = mft.files_excluded;
                        out.excluded_bytes = mft.bytes_excluded;
                        out.categories = Some(mft.categories);
                        out.left = super::volume_exclusions::ntfs_left(&mft.kept);
                        out.removal_paths = mft
                            .paths
                            .iter()
                            .map(|p| crate::exclusions::ExcludedPath {
                                rel: p.rel.clone(),
                                is_dir: p.is_dir,
                                is_reparse: p.is_reparse,
                                whole_records: p.whole_records.clone(),
                            })
                            .collect();
                        info!(
                            part = %read_path,
                            files = mft.files_excluded,
                            bytes = mft.bytes_excluded,
                            kept = mft.kept.len(),
                            "whole-disk: exclusions resolved from the MFT"
                        );
                        if !mft.ranges.is_empty() {
                            m.apply_excluded_ranges(&mft.ranges)?;
                            out.zero_ranges = mft.ranges;
                        }
                    }
                    Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                    Err(e) => {
                        warn!(part = %read_path, error = %e,
                              "whole-disk: MFT scan unavailable — falling back to the directory walk");
                        let mut walk_tick = |n: u64| tick(n, 0);
                        let walked = platform.walk_exclusions(
                            read_path,
                            m.cluster_size(),
                            patterns,
                            &mut walk_tick,
                        );
                        if matches!(&walked, Err(OwnBackupError::Cancelled)) {
                            return Err(OwnBackupError::Cancelled);
                        }
                        if let Ok(Some(mut scan)) = walked {
                            out.excluded_files = scan.files_excluded;
                            out.excluded_bytes = scan.bytes_excluded;
                            out.categories = Some(scan.categories);
                            out.removal_paths = std::mem::take(&mut scan.paths);
                            if !scan.ranges.is_empty() {
                                m.apply_excluded_ranges(&scan.ranges)?;
                                out.zero_ranges = std::mem::take(&mut scan.ranges);
                            }
                        } else if let Ok(None) = walked {
                            // No walk on this platform: the MFT was the only
                            // way to the files.
                            out.skipped = Some(tr!("engine.skip_no_walk", error = e).into_owned());
                        } else {
                            warn!(part = %read_path, "whole-disk: NTFS exclusion scan failed — keeping all files");
                            out.skipped =
                                Some(tr!("engine_win.skip_list_failed", fs = "NTFS").into_owned());
                        }
                    }
                }
            } else {
                out.skipped = Some(no_patterns());
            }
            m.release_bitmap();
            out.map = Some(Box::new(m));
            return Ok(out);
        }
    }
    let Ok(mut src) = open() else {
        return Ok(none_because(
            "unknown",
            None,
            &tr!("engine_win.skip_partition_unreadable"),
        ));
    };
    if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), block_size) {
        let fs = m.filesystem.to_lowercase();
        let is_fat = m.filesystem == "FAT32" || m.filesystem == "exFAT";
        drop(src);
        if is_fat && !patterns.is_empty() {
            let mut out = none(&fs, None);
            if let Ok(mut dev) = P::Dev::open_read(read_path) {
                let scan =
                    crate::fat_exclusions::scan_fat_exclusions(&mut dev, patterns, |n| tick(n, 0));
                if matches!(scan, Err(OwnBackupError::Cancelled)) {
                    return Err(OwnBackupError::Cancelled);
                }
                if let Ok(scan) = scan {
                    out.excluded_files = scan.files_excluded;
                    out.excluded_bytes = scan.bytes_excluded;
                    out.categories = Some(scan.categories);
                    out.left = scan.left;
                    if !scan.ranges.is_empty() {
                        // Rebuild the map so excluded clusters drop out.
                        if let Ok(mut dev2) = P::Dev::open_read(read_path) {
                            if let Ok(rebuilt) = crate::fat_map::build_fat_used_map_excl(
                                &mut dev2,
                                block_size,
                                Some(&scan.ranges),
                            ) {
                                out.map = Some(Box::new(rebuilt));
                            }
                        }
                        out.zero_ranges = scan.ranges;
                        out.patches = scan.patches;
                    }
                }
            }
            if out.map.is_none() {
                // Exclusion rebuild failed → fall back to the full FAT map.
                if let Ok(mut src2) = open() {
                    if let Ok(m2) = crate::fat_map::build_fat_used_map(src2.as_mut(), block_size) {
                        out.map = Some(Box::new(m2));
                    }
                }
            }
            return Ok(out);
        }
        return Ok(none_because(
            &fs,
            Some(Box::new(m)),
            &if patterns.is_empty() {
                no_patterns()
            } else {
                tr!("engine_win.skip_fat_only").into_owned()
            },
        ));
    }
    if let Some(out) = linux_fs_exclusions(
        src,
        open,
        read_path,
        block_size,
        patterns_linux,
        quiesced,
        &mut tick,
    )? {
        return Ok(out);
    }
    Ok(none_because(
        "unknown",
        None,
        &tr!("engine_win.skip_fs_unknown"),
    ))
}
