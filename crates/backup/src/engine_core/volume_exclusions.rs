//! File exclusions of a volume copy, settled before the copy: which files
//! the rules keep out, which byte ranges that leaves out of the image, what
//! has to be patched or deleted afterwards — or, in words for the result
//! screen, why the rules could not be applied.

use tracing::{debug, info, warn};

use own_backup_common::progress::BackupProgress;
use own_backup_common::types::BackupStatus;
use own_backup_common::{tr, OwnBackupError, Result};

use super::platform::{EnginePlatform, NativeUsedMap, SourceDevice, SourceSession};
use super::source::VolumeSource;
use super::target::{no_patterns, report_categories};
use super::volume::VolumeBackupRequest;
use super::volume_maps::VolumeMaps;
use crate::exclusions::ByteRanges;

/// What the exclusions of one volume came to before the copy.
pub(crate) struct VolumeExclusions {
    /// Files the rules keep out of the copy, and their bytes.
    pub(crate) excluded_files: u64,
    pub(crate) excluded_bytes: u64,
    /// Excluded byte ranges of the volume, zeroed inside the blocks that are
    /// still copied.
    pub(crate) zero_ranges: Option<ByteRanges>,
    /// Metadata patches that delete the excluded files: FAT/exFAT entries and
    /// tables; on Linux also the ext, btrfs and xfs Stage-2 edits.
    pub(crate) meta_patches: Vec<(u64, Vec<u8>)>,
    /// By-type breakdown of the files in the copy; `None` when nobody walked
    /// them.
    pub(crate) categories: Option<crate::exclusions::CategorySizes>,
    /// Why the file walk did not run, in words for the result screen.
    pub(crate) file_scan_skipped: Option<String>,
    /// Excluded paths whose catalog entries the post-copy pass deletes from
    /// the image.
    pub(crate) removal_paths: Vec<crate::exclusions::ExcludedPath>,
    /// The rules are applied inside the finished image instead: an encrypted
    /// source whose files could not be walked before the copy.
    pub(crate) in_image_exclusions: bool,
    /// Why some excluded files are still listed in the copy's catalog (with
    /// their contents zeroed), in words for the result screen; `None` when
    /// every one of them was removed or none was left behind.
    pub(crate) entries_kept: Option<String>,
    /// The service's key directory was on this source and is left out of
    /// the copy (decision 6/E4).
    pub(crate) key_store_excluded: bool,
}

/// Apply `req`'s exclusion rules to the source before the copy: the matched
/// files' clusters leave `maps`, and what is left to do after the copy is
/// returned. `session` says whether the source's raw metadata may be parsed.
pub(crate) fn resolve_exclusions<P: EnginePlatform>(
    platform: &P,
    session: &P::Session,
    read_path: &str,
    req: &VolumeBackupRequest,
    opened: &VolumeSource<P>,
    maps: &mut VolumeMaps<P>,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeExclusions> {
    let (luks, bitlocker) = (&opened.luks, &opened.bitlocker);

    // The service's key directory on this source is left out of the copy
    // whatever the job's rules say (decision 6/E4): its rule is added here
    // even to an empty list, and a copy it cannot be applied to is refused
    // below instead of being made with the keys in it.
    let key_store = session.must_exclude_key_store();
    if key_store && req.all_sectors {
        return Err(key_store_refusal(
            &req.source,
            &tr!("engine_win.skip_sector_mode"),
        ));
    }
    let with_key_store = |list: &Vec<String>| -> Vec<String> {
        if key_store && list.is_empty() {
            vec![crate::seal::local::KEY_DIRECTORY_NAME.to_string()]
        } else {
            list.clone()
        }
    };
    let patterns = with_key_store(&req.exclusion_patterns);
    let patterns_linux = with_key_store(&req.exclusion_patterns_linux);

    // --- File exclusions (phase 3) ---
    // Both families achieve TRUE removal (like Acronis) — a restored/mounted
    // volume shows no excluded file and reclaims its space — by different,
    // filesystem-appropriate means:
    //
    // * FAT32/exFAT — metadata patches: the raw directory tree is walked
    //   ourselves and the matched files' metadata is patched out of the image
    //   (directory entry deleted, allocation table / bitmap freed). The
    //   simple, fully-specified on-disk structures make this safe and testable.
    //
    // * NTFS/ReFS — two steps. First, CONTENT ZEROING (CLAUDE.md: "exclusions
    //   via FSCTL_GET_RETRIEVAL_POINTERS (clusters zeroed)"): the matched
    //   clusters are dropped from the used map and zeroed, so the file's DATA
    //   never leaves the machine. Then, after the copy, the mount-&-delete
    //   pass (`true_removal_win`, decision 2026-07-07) lets the FS DRIVER
    //   delete the catalog entries inside the image copy — we never parse or
    //   write NTFS/ReFS structures ourselves ("no NTFS parser", CLAUDE.md,
    //   still holds); the driver updates $MFT/$Bitmap/directory indexes (or
    //   the ReFS equivalents) correctly by construction. If that pass cannot
    //   run, the excluded files remain as zero-filled ghosts — a valid backup,
    //   just without the space reclaim.
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut zero_ranges: Option<ByteRanges> = None;
    // Byte-level metadata edits that make excluded FAT/exFAT files truly
    // disappear (delete the directory entry, free the allocation table). Empty
    // for NTFS/ReFS, which still use zeroing for now.
    let mut meta_patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    // "prepare"-phase progress tick. `total` is the number of MFT records the
    // scan will look at, known up front; 0 when whatever is scanning cannot say
    // (a directory walk meets its entries as it goes).
    let mut prepare_tick = |entries: u64, total: u64| {
        progress(&BackupProgress {
            backup_id: "prepare".into(),
            status: BackupStatus::Running,
            blocks_done: entries,
            blocks_total: total,
            bytes_written: 0,
            speed_bps: 0,
            categories: None,
            excluded: None,
        })
    };
    // Why the file walk did not happen — filled in below by whichever branch
    // skips it, and carried all the way to the result screen. The walk is what
    // produces BOTH the exclusions and the by-type breakdown, so one sentence
    // accounts for both being missing.
    let mut file_scan_skipped: Option<String> = None;
    let fat_exclusions = maps
        .fat_map
        .as_ref()
        .is_some_and(|f| f.filesystem == "FAT32" || f.filesystem == "exFAT")
        && !patterns.is_empty();
    // Matched paths for the post-copy true-removal pass (NTFS/ReFS): the FS
    // driver deletes them inside the finished image, so they vanish from the
    // catalog and their space is reclaimed — see `true_removal_win`.
    let mut removal_paths: Vec<crate::exclusions::ExcludedPath> = Vec::new();
    // Исключения решены ДО копии по $MFT — проход внутри образа не нужен:
    // исключённых данных в образе нет, осталось убрать записи из каталога.
    let mut pre_copy_exclusions = false;
    let mut entries_kept: Option<String> = None;
    // Matched content the scan left in the copy, and directories it could not
    // walk — what the key-directory proof below is checked against.
    let mut left: Vec<crate::exclusions::LeftInCopy> = Vec::new();
    // A Linux filesystem the engine parsed itself (its map exists only when
    // the session holds still) and rules written for it.
    let linux_fs = maps.ext_map.is_some() || maps.btrfs_map.is_some() || maps.xfs_map.is_some();
    if let Some(map) = maps.ntfs_map.as_mut() {
        if !patterns.is_empty() {
            // Сначала по $MFT: последовательное чтение вместо обхода дерева.
            // На живом C: обход занимает 45 с, внутри образа на SMR — 16 минут;
            // чтение MFT того же образа — 15 с (замеры B0/B1, 2026-08-19). Плюс
            // ему не нужен дескриптор: обход не смог открыть pagefile.sys,
            // swapfile.sys и DumpStack.log.tmp («файл занят другим процессом»)
            // и оставил в копии 400 МиБ ровно тех файлов, ради которых
            // исключения и существуют.
            //
            // Разбирать метаданные тома, который прямо сейчас пишут, нельзя:
            // берёмся только за замороженный источник — снимок VSS или офлайн
            // на носителе. В остальных случаях (VSS не дали) остаётся обход:
            // он идёт через файловую систему и переживает движение под собой.
            let frozen = session.frozen();
            let fast = if frozen {
                scan_ntfs_exclusions_by_mft::<P::Dev>(read_path, &patterns, &mut prepare_tick)
            } else {
                Err(OwnBackupError::Device(
                    tr!("engine_win.source_not_frozen").into_owned(),
                ))
            };
            match fast {
                Ok(mft) => {
                    excluded_files = mft.files_excluded;
                    excluded_bytes = mft.bytes_excluded;
                    categories = Some(mft.categories);
                    left = ntfs_left(&mft.kept);
                    removal_paths = mft
                        .paths
                        .iter()
                        .map(|p| crate::exclusions::ExcludedPath {
                            rel: p.rel.clone(),
                            is_dir: p.is_dir,
                            is_reparse: p.is_reparse,
                        })
                        .collect();
                    info!(
                        files = mft.files_excluded,
                        bytes = mft.bytes_excluded,
                        kept = mft.kept.len(),
                        paths = removal_paths.len(),
                        records = mft.records_scanned,
                        "exclusions resolved from the MFT"
                    );
                    // Сводка по типам файлов известна уже сейчас — сканер видел
                    // каждый файл с размером. Отправляем её сразу, а не в конце:
                    // человеку, который смотрит на полосу двадцать минут, есть
                    // что почитать, и это ответ на вопрос «а что вообще в копии».
                    report_categories(
                        &mut progress,
                        categories.as_ref(),
                        excluded_files,
                        excluded_bytes,
                    )?;
                    if !mft.ranges.is_empty() {
                        map.apply_excluded_ranges(&mft.ranges)?;
                        zero_ranges = Some(mft.ranges);
                    }
                }
                Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                Err(e) => {
                    // ReFS (у неё нет MFT), грязный том, неразобранная запись —
                    // всё это законные причины вернуться к обходу дерева.
                    warn!(error = %e, "MFT scan unavailable — falling back to the directory walk");
                    let mut walk_tick = |n: u64| prepare_tick(n, 0);
                    match platform.walk_exclusions(
                        read_path,
                        map.cluster_size(),
                        &patterns,
                        &mut walk_tick,
                    )? {
                        Some(mut scan) => {
                            excluded_files = scan.files_excluded;
                            excluded_bytes = scan.bytes_excluded;
                            categories = Some(scan.categories);
                            removal_paths = std::mem::take(&mut scan.paths);
                            if !scan.ranges.is_empty() {
                                map.apply_excluded_ranges(&scan.ranges)?;
                                zero_ranges = Some(scan.ranges);
                            }
                        }
                        // No walk on this platform (never on Windows): the MFT
                        // was the only way to the files, and the result screen
                        // says the rules were not applied rather than "0".
                        None => {
                            warn!(
                                patterns = patterns.len(),
                                reason = %e,
                                "file exclusions were NOT applied on the source"
                            );
                            file_scan_skipped =
                                Some(tr!("engine.skip_no_walk", error = e).into_owned());
                        }
                    }
                }
            }
        } else {
            file_scan_skipped = Some(no_patterns());
        }
        map.release_bitmap();
    } else if let Some(map) = maps.enc_ntfs_map.as_mut() {
        // NTFS внутри контейнера, который Windows не смонтировала. Файлы в нём
        // всё равно перечислимы — по $MFT, прямо через расшифрованное окно
        // (`ntfs_scan`), и это решает исключения ДО копирования: экстенты
        // вычитаются из карты, исключённые данные не читаются и не пишутся.
        // Раньше здесь был отказ, а правила применялись потом, внутри готового
        // образа: скан 16 минут, зануление, merge и пересчёт сумм — прогон
        // 2026-08-19 пользователь отменил.
        if patterns.is_empty() {
            file_scan_skipped = Some(no_patterns());
        } else {
            let mut src = opened.open_source()?;
            let scan_result = {
                let cur = crate::ntfs_scan::RawVolumeCursor::new(src.as_mut());
                // Крейт читает запись MFT по килобайту; без кэша это миллион
                // обращений к носителю (замер: 313 с против 74 с).
                let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
                // Тот же канал «Подготовка: поиск исключаемых файлов», что у
                // обхода дерева: на 128 ГиБ проход идёт больше двух минут, и
                // молчащая полоса читается как зависание.
                let mut report = |n: u64, total: u64| {
                    let _ = progress(&BackupProgress {
                        backup_id: "prepare".into(),
                        status: BackupStatus::Running,
                        blocks_done: n,
                        blocks_total: total,
                        bytes_written: 0,
                        speed_bps: 0,
                        categories: None,
                        excluded: None,
                    });
                };
                crate::ntfs_scan::scan_mft_with(&mut buffered, &patterns, &mut report)
            };
            match scan_result {
                Ok(scan) => {
                    excluded_files = scan.files_excluded;
                    excluded_bytes = scan.bytes_excluded;
                    categories = Some(scan.categories);
                    left = ntfs_left(&scan.kept);
                    report_categories(
                        &mut progress,
                        categories.as_ref(),
                        excluded_files,
                        excluded_bytes,
                    )?;
                    for (path, reason) in &scan.kept {
                        debug!(file = %path, reason = reason.as_str(), "данные остаются в копии");
                    }
                    removal_paths = scan
                        .paths
                        .iter()
                        .map(|p| crate::exclusions::ExcludedPath {
                            rel: p.rel.clone(),
                            is_dir: p.is_dir,
                            is_reparse: p.is_reparse,
                        })
                        .collect();
                    info!(
                        files = scan.files_excluded,
                        bytes = scan.bytes_excluded,
                        kept = scan.kept.len(),
                        paths = removal_paths.len(),
                        records = scan.records_scanned,
                        "exclusions resolved from the MFT BEFORE the copy"
                    );
                    if !scan.ranges.is_empty() {
                        // Карта строится заново, без исключённых кластеров:
                        // блок, целиком занятый исключённым, не копируется
                        // вовсе; в общих блоках лишнее занулит ZeroingBlockReader.
                        let mut src2 = opened.open_source()?;
                        let rebuilt = crate::ntfs_map::build_ntfs_used_map_excl(
                            src2.as_mut(),
                            req.block_size,
                            Some(&scan.ranges),
                        )?;
                        *map = rebuilt;
                        zero_ranges = Some(scan.ranges);
                    }
                    pre_copy_exclusions = true;
                }
                Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                Err(e) => {
                    // Грязный том, битая запись, не NTFS внутри — откат на
                    // проход внутри готового образа (медленный, но рабочий).
                    warn!(error = %e, "MFT scan failed — falling back to the in-image pass");
                }
            }
        }
    } else if let Some(map) = maps.enc_fat_map.as_mut() {
        // FAT/exFAT внутри контейнера, который Windows не смонтировала. Свой
        // разбор FAT читает каталоги прямо из расшифрованного окна — ему, как и
        // NTFS-сканеру, не нужен ни смонтированный том, ни дескриптор файла.
        // Раньше здесь был отказ: ветка исключений знала только NTFS.
        if patterns.is_empty() {
            file_scan_skipped = Some(no_patterns());
        } else {
            let mut src = opened.open_source()?;
            let mut walk_tick = |n: u64| prepare_tick(n, 0);
            match crate::fat_exclusions::scan_fat_exclusions(
                src.as_mut(),
                &patterns,
                &mut walk_tick,
            ) {
                Ok(scan) => {
                    excluded_files = scan.files_excluded;
                    excluded_bytes = scan.bytes_excluded;
                    categories = Some(scan.categories);
                    left = scan.left;
                    info!(
                        files = scan.files_excluded,
                        bytes = scan.bytes_excluded,
                        entries = scan.entries_scanned,
                        patches = scan.patches.len(),
                        "exclusions resolved from FAT BEFORE the copy"
                    );
                    if !scan.ranges.is_empty() {
                        // Карта строится заново, без исключённых кластеров.
                        let mut src2 = opened.open_source()?;
                        *map = crate::fat_map::build_fat_used_map_excl(
                            src2.as_mut(),
                            req.block_size,
                            Some(&scan.ranges),
                        )?;
                        zero_ranges = Some(scan.ranges);
                        // Правки метаданных: запись каталога помечается
                        // удалённой, цепочка в таблице освобождается — файл
                        // исчезает, а не остаётся нулевым призраком.
                        meta_patches = scan.patches;
                    }
                    pre_copy_exclusions = true;
                }
                Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                Err(e) => {
                    warn!(error = %e, "FAT exclusion scan failed — keeping all files");
                    file_scan_skipped =
                        Some(tr!("engine_win.skip_fat_failed", error = e).into_owned());
                }
            }
        }
    } else if fat_exclusions {
        let mut dev = P::Dev::open_read(read_path)?;
        let scan = crate::fat_exclusions::scan_fat_exclusions(&mut dev, &patterns, |n| {
            prepare_tick(n, 0)
        })?;
        excluded_files = scan.files_excluded;
        excluded_bytes = scan.bytes_excluded;
        categories = Some(scan.categories);
        left = scan.left;
        if !scan.ranges.is_empty() {
            // Rebuild the map so excluded clusters drop out (cluster-level).
            let mut dev2 = P::Dev::open_read(read_path)?;
            let rebuilt = crate::fat_map::build_fat_used_map_excl(
                &mut dev2,
                req.block_size,
                Some(&scan.ranges),
            )?;
            maps.fat_map = Some(rebuilt);
            zero_ranges = Some(scan.ranges);
            // Metadata patches: delete the directory entries + free the FAT
            // chains so the files vanish instead of remaining zero-filled ghosts.
            meta_patches = scan.patches;
        }
    } else if linux_fs && !patterns_linux.is_empty() {
        // ext, btrfs, xfs under the Linux rules — the scanners Windows runs on
        // these filesystems inside a whole disk (`partition_prep`), here on a
        // volume the session holds still (only then is there a map at all):
        // the matched files' entries removed where that is safe (Stage 2),
        // their exclusive data zeroed in any case (Stage 1). Inside a LUKS or
        // BitLocker container they read its decrypting view, as NTFS and FAT
        // do: the files are found before the copy, and what the image gets is
        // the decrypted filesystem with them taken out. The view is aligned:
        // ext inodes are 256 bytes, and a decrypting reader works in sectors.
        let mut dev = AlignedSource(crate::aligned_volume::AlignedVolume::new(BoxedVolume(
            opened.open_source()?,
        )));
        let patterns = &patterns_linux;
        let mut walk_tick = |n: u64| prepare_tick(n, 0);
        let scanned = if maps.ext_map.is_some() {
            crate::ext_exclusions::scan_ext_exclusions(&mut dev, patterns, &mut walk_tick)
                .map(|s| LinuxFsScan::from_ext("ext", s))
        } else if maps.btrfs_map.is_some() {
            crate::btrfs_exclusions::scan_btrfs_exclusions(&mut dev, patterns, true, &mut walk_tick)
                .map(|s| LinuxFsScan::from_btrfs("btrfs", s))
        } else if P::METADATA_NEEDS_FROZEN && session.quiesced() {
            // Frozen when captured: the log reads dirty but is covered.
            crate::xfs_exclusions::scan_xfs_exclusions_quiesced(&mut dev, patterns, &mut walk_tick)
                .map(|s| LinuxFsScan::from_xfs("xfs", s))
        } else {
            crate::xfs_exclusions::scan_xfs_exclusions(&mut dev, patterns, true, &mut walk_tick)
                .map(|s| LinuxFsScan::from_xfs("xfs", s))
        };
        match scanned {
            Ok(scan) => {
                excluded_files = scan.files;
                excluded_bytes = scan.bytes;
                categories = Some(scan.categories);
                left = scan.left;
                info!(
                    fs = scan.fs,
                    files = scan.files,
                    bytes = scan.bytes,
                    patches = scan.patches.len(),
                    removed_bytes = scan.droppable.total_bytes(),
                    "exclusions resolved from the filesystem BEFORE the copy"
                );
                report_categories(
                    &mut progress,
                    categories.as_ref(),
                    excluded_files,
                    excluded_bytes,
                )?;
                if !scan.ranges.is_empty() {
                    // Only what Stage 2 freed leaves the map: a file it spared
                    // is still listed, and its blocks must be stored as the
                    // zeros they became.
                    let bs = req.block_size;
                    if let Some(m) = maps.ext_map.as_mut() {
                        m.apply_excluded_ranges(&scan.droppable, bs);
                    } else if let Some(m) = maps.btrfs_map.as_mut() {
                        m.apply_excluded_ranges(&scan.droppable, bs);
                    } else if let Some(m) = maps.xfs_map.as_mut() {
                        m.apply_excluded_ranges(&scan.droppable, bs);
                    }
                    if scan.ranges.total_bytes() > scan.droppable.total_bytes() {
                        entries_kept =
                            Some(tr!("engine.entries_kept_zeroed", fs = scan.fs).into_owned());
                    }
                    zero_ranges = Some(scan.ranges);
                    meta_patches = scan.patches;
                }
            }
            Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
            Err(e) => {
                let fs = maps.detected_fs.clone().unwrap_or_default();
                warn!(fs = %fs, error = %e, "exclusion scan failed — keeping all files");
                file_scan_skipped = Some(tr!("engine_win.skip_list_failed", fs = fs).into_owned());
            }
        }
    } else {
        // Nothing walked the files here. Say which of the reasons it was, in
        // the words the result screen will repeat back to the user.
        let reason = if req.all_sectors {
            tr!("engine_win.skip_sector_mode").into_owned()
        } else if P::METADATA_NEEDS_FROZEN && !session.frozen() {
            // No map was built either (`VolumeMaps::build`): the same reason
            // the copy is sector by sector.
            tr!("engine.skip_not_frozen").into_owned()
        } else if bitlocker.is_some() {
            // `FSCTL_GET_RETRIEVAL_POINTERS` needs a mounted filesystem, and a
            // locked volume has none. Copying the data and deleting only the
            // catalog entries afterwards would leave those bytes in the image's
            // free space — exactly what "true removal" promises not to do.
            tr!("engine_win.skip_bitlocker_locked").into_owned()
        } else if luks.is_some() {
            tr!("engine_win.skip_luks").into_owned()
        } else if patterns.is_empty() {
            no_patterns()
        } else {
            let fs = maps
                .detected_fs
                .clone()
                .unwrap_or_else(|| tr!("engine_win.other_filesystem").into_owned());
            tr!("engine_win.skip_fs_unsupported", fs = fs).into_owned()
        };
        if !patterns.is_empty() {
            warn!(
                patterns = patterns.len(),
                reason = %reason,
                "file exclusions were NOT applied on the source"
            );
        }
        file_scan_skipped = Some(reason);
    }

    // …but a container's files can still be reached — after the copy, inside
    // the image, where their filesystem lies in the clear and Windows will
    // mount it. NTFS only: the pass walks the mounted volume and then reads
    // `$Bitmap` to know what it may zero, and neither exists on ReFS or on a
    // Linux filesystem Windows cannot mount at all.
    let encrypted_source = bitlocker.is_some() || luks.is_some();
    let in_image_exclusions = encrypted_source
        && !pre_copy_exclusions
        && !patterns.is_empty()
        && !req.all_sectors
        && maps.detected_fs.as_deref() == Some("NTFS");

    if in_image_exclusions && req.encryption.is_some() {
        // The fallback pass mounts the finished image and deletes inside it,
        // through a plaintext scratch layer. For an encrypted image that layer
        // must be encrypted too (§7.4), and it is not yet. Skipping the pass
        // instead would ship a copy that quietly kept the excluded files.
        return Err(OwnBackupError::Encryption(
            tr!("engine_win.encrypted_in_image_exclusions").into_owned(),
        ));
    }

    if key_store {
        let unapplied = file_scan_skipped
            .clone()
            .or_else(|| {
                in_image_exclusions.then(|| tr!("engine.key_store_after_copy_only").into_owned())
            })
            // Every scan that walked the files says what it saw by type; none
            // did when this is missing.
            .or_else(|| {
                categories
                    .is_none()
                    .then(|| tr!("engine.key_store_not_walked").into_owned())
            });
        if let Some(why) = unapplied {
            return Err(key_store_refusal(&req.source, &why));
        }
        // The scan walked the files; it must also account for every match of
        // the directory having left the copy (decision 6/E4).
        if let Some(l) = crate::exclusions::key_store_left_behind(&left) {
            return Err(key_store_refusal(
                &req.source,
                &tr!("engine.key_store_left_behind", path = l.shown()),
            ));
        }
        info!(source = %req.source, "the service key directory is left out of this copy");
    }

    Ok(VolumeExclusions {
        key_store_excluded: key_store,
        excluded_files,
        excluded_bytes,
        zero_ranges,
        meta_patches,
        categories,
        file_scan_skipped,
        removal_paths,
        in_image_exclusions,
        entries_kept,
    })
}

/// What an NTFS `$MFT` scan kept, as [`crate::exclusions::LeftInCopy`]. A
/// junction or symlink holds no content of its own.
pub(crate) fn ntfs_left(
    kept: &[(String, crate::ntfs_scan::KeptReason)],
) -> Vec<crate::exclusions::LeftInCopy> {
    use crate::exclusions::{LeftInCopy, LeftReason};
    use crate::ntfs_scan::KeptReason;
    kept.iter()
        .filter_map(|(path, why)| {
            let reason = match why {
                KeptReason::Resident => LeftReason::Inline,
                KeptReason::OtherNameKept => LeftReason::HardLink,
                KeptReason::ExtentsIncomplete | KeptReason::Unreadable => LeftReason::Unreadable,
                KeptReason::Reparse => return None,
            };
            let parts: Vec<String> = path
                .split(['\\', '/'])
                .filter(|c| !c.is_empty())
                .map(str::to_lowercase)
                .collect();
            Some(LeftInCopy::new(&parts, reason))
        })
        .collect()
}

/// The copy of `source` is refused: the service's key directory is on it and
/// `why` the rule that leaves it out cannot be applied.
pub(crate) fn key_store_refusal(source: &str, why: &str) -> OwnBackupError {
    OwnBackupError::Config(
        tr!(
            "engine.key_store_not_excluded",
            source = source,
            reason = why
        )
        .into_owned(),
    )
}

/// The source as the engine opened it — the device, or the decrypting view of
/// a container — for the scanners that take a concrete volume.
struct BoxedVolume(Box<dyn crate::fat_map::RawVolume + Send>);

impl crate::fat_map::RawVolume for BoxedVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn len(&self) -> u64 {
        self.0.len()
    }
}

struct AlignedSource(crate::aligned_volume::AlignedVolume<BoxedVolume>);

impl crate::fat_map::RawVolume for AlignedSource {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn len(&self) -> u64 {
        self.0.len()
    }
}

/// One Linux filesystem's exclusion scan, in the terms the pipeline uses.
struct LinuxFsScan {
    fs: &'static str,
    files: u64,
    bytes: u64,
    categories: crate::exclusions::CategorySizes,
    /// Excluded data, zeroed in the copy.
    ranges: ByteRanges,
    /// The part of `ranges` Stage 2 freed: left out of the copy entirely.
    droppable: ByteRanges,
    patches: Vec<(u64, Vec<u8>)>,
    /// What the scan could not take out (`exclusions::LeftInCopy`).
    left: Vec<crate::exclusions::LeftInCopy>,
}

impl LinuxFsScan {
    fn from_ext(fs: &'static str, s: crate::ext_exclusions::ExtExclusionScan) -> Self {
        Self {
            fs,
            files: s.files_excluded,
            bytes: s.bytes_excluded,
            categories: s.categories,
            ranges: s.ranges,
            droppable: s.droppable,
            patches: s.patches,
            left: s.left,
        }
    }

    fn from_btrfs(fs: &'static str, s: crate::btrfs_exclusions::BtrfsExclusionScan) -> Self {
        Self {
            fs,
            files: s.files_excluded,
            bytes: s.bytes_excluded,
            categories: s.categories,
            ranges: s.ranges,
            droppable: s.droppable,
            patches: s.patches,
            left: s.left,
        }
    }

    fn from_xfs(fs: &'static str, s: crate::xfs_exclusions::XfsExclusionScan) -> Self {
        Self {
            fs,
            files: s.files_excluded,
            bytes: s.bytes_excluded,
            categories: s.categories,
            ranges: s.ranges,
            droppable: s.droppable,
            patches: s.patches,
            left: s.left,
        }
    }
}

/// Разобрать исключения по $MFT тома, который Windows смонтировала.
///
/// Том открывается как устройство и читается насквозь — тем же сканером, что
/// работает над расшифрованным окном BitLocker. Ничего не пишет.
/// `tick(records_done, records_total)`. The total is the whole point: the MFT
/// has a known record count (`$MFT` size / record size), so the wait can be a
/// bar with an end and an estimate instead of a number running up. Only this
/// scanner can say it — a directory walk does not know how many entries it is
/// about to meet, and passes 0.
pub(crate) fn scan_ntfs_exclusions_by_mft<D: SourceDevice>(
    read_path: &str,
    patterns: &[String],
    tick: &mut impl FnMut(u64, u64) -> Result<()>,
) -> Result<crate::ntfs_scan::MftScan> {
    let mut dev = D::open_read(read_path)?;
    let cur = crate::ntfs_scan::RawVolumeCursor::new(&mut dev);
    let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
    crate::ntfs_scan::scan_mft_with_cancel(&mut buffered, patterns, tick)
}
