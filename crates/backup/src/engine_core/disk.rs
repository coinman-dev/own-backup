//! A whole-disk copy: several partitions of one disk into ONE image with the
//! disk's real GPT and an embedded restore manifest, on any platform. Each
//! partition is prepared first ([`super::disk_prepare`]) so the image is one
//! point in time across them, then the disk is copied as whole-image blocks
//! ([`super::disk_image`]) into its target ([`super::disk_target`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::{info, warn};
use uuid::Uuid;

use own_backup_common::progress::BackupProgress;
use own_backup_common::types::{BackupStatus, BackupType, ImageFormat};
use own_backup_common::{tr, OwnBackupError, Result};

use super::chain::{child_refusal, validate_disk_parent};
use super::disk_image::{
    container_kind, disk_source_token, DiskBackupRequest, DiskCompositeReader, DiskRegion,
    DiskUsedMap,
};
use super::disk_prepare::{prepare_partition_map, PreparedPartition};
use super::disk_target::{
    boot_entries_for_image, create_disk_child_target, create_disk_image_target, DiskBackupSummary,
};
use super::partition_prep::{prepare_encrypted_partition_map, PreparedMap};
use super::platform::{DiskPlatform, DiskRef, FinishedImage, SourceDevice, SourceSession};
use super::read::ImageReadSource;
use super::source::{disk_window, is_bitlocker_locked, open_bitlocker_source};
use super::target::{
    check_encrypted_target, container_ext, may_publish, refuse_existing_outputs, report_categories,
    settled_image_path, whole_disk_image_manifest, DiskImagePlan,
};
use crate::exclusions::ByteRanges;
use crate::fat_map::RawVolume;
use crate::raw_copy::copy_used_blocks;
use crate::sidecar::{ChainRole, Sidecar, SidecarInfo};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};

/// Back up a whole disk on `platform` (`engine_win::backup_disk` says what it
/// does). The image is written under a private name and published only once
/// the whole copy is done and the caller, asked last ([`may_publish`]), still
/// wants it.
pub(crate) fn backup_disk<P: DiskPlatform>(
    platform: &P,
    req: &DiskBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    check_encrypted_target(req.encryption.as_ref(), req.image_format, &req.image_path)?;
    refuse_existing_outputs(&req.image_path, req.full_image_path.as_deref())?;
    if let Some(parent) = req.image_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::dest_folder::create(parent)?;
        }
    }
    platform.prepare_privileges();
    if let Some(keys) = &req.encryption {
        let pending = crate::seal::obk::new_image::NewImage::create(&req.image_path, keys)?;
        let staged = DiskBackupRequest {
            image_path: pending.path().to_path_buf(),
            ..req.clone()
        };
        let mut summary = backup_disk_inner(
            platform,
            &staged,
            Some(pending.writer_file()?),
            &req.image_path,
            &mut progress,
        )?;
        let published = settled_image_path(
            &req.image_path,
            req.full_image_path.as_deref(),
            req.kind,
            summary.backup_kind,
        );
        may_publish(&mut progress, summary.bytes_copied)?;
        pending.publish_as(&published)?;
        summary.image_path = published;
        return Ok(summary);
    }
    // Private until finished, as in `backup_volume`.
    let pending = crate::pending_image::PendingImage::new(&req.image_path)?;
    let staged = DiskBackupRequest {
        image_path: pending.path().to_path_buf(),
        ..req.clone()
    };
    let mut summary = backup_disk_inner(platform, &staged, None, &req.image_path, &mut progress)?;
    let published = settled_image_path(
        &req.image_path,
        req.full_image_path.as_deref(),
        req.kind,
        summary.backup_kind,
    );
    may_publish(&mut progress, summary.bytes_copied)?;
    pending.publish_as(&published)?;
    summary.image_path = published;
    Ok(summary)
}

fn backup_disk_inner<P: DiskPlatform>(
    platform: &P,
    req: &DiskBackupRequest,
    owned_file: Option<std::fs::File>,
    published_path: &Path,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    let started = Instant::now();
    let bs = req.block_size as u64;

    // 1. Real GPT, filtered to the selected partitions (variant B keeps MSR
    // when the copy carries Windows — `DiskLayout::copy_carries_windows`).
    let disk = DiskRef {
        number: req.disk_number,
        device: req.disk_device.clone(),
    };
    let identity = platform.disk_identity(&disk);
    let mut layout = platform.read_disk_layout(&disk)?;
    let kept: std::collections::HashSet<u32> =
        req.partitions.iter().map(|p| p.part_index).collect();
    // What this copy is about to leave behind, named before it is dropped.
    // `retain_for_backup` removes an unselected partition from the layout
    // ENTIRELY — on restore its space comes back free — and that is too large a
    // decision to record as a count. On 2026-08-19 a bootable copy of a 954 GiB
    // disk ran with the EFI partition alone, because a locked BitLocker volume
    // cannot report `has_os` and the wizard therefore never selected it; the
    // whole trace in the log was `parts=1` beside a disk that has three, and the
    // run reported success in 0 s. The selection bug is fixed in the wizard —
    // this line is so the next one of its kind is legible from the log alone.
    let left_out: Vec<String> = layout
        .partitions
        .iter()
        .filter(|p| {
            !kept.contains(&p.index)
                && !p
                    .type_guid
                    .eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_MSR)
        })
        .map(|p| {
            let mib = p.size_bytes(layout.sector_size) / (1024 * 1024);
            if p.name.is_empty() {
                format!("#{} ({mib} МиБ)", p.index)
            } else {
                format!("#{} «{}» ({mib} МиБ)", p.index, p.name)
            }
        })
        .collect();
    if !left_out.is_empty() {
        warn!(
            disk = req.disk_number,
            kept = req.partitions.len(),
            left_out = %left_out.join(", "),
            "whole-disk backup: partitions are NOT in the copy — their space is free on restore"
        );
    }
    layout.retain_for_backup(&kept, req.source_disk.as_ref());
    let sector = layout.sector_size as u64;
    if bs == 0 || !bs.is_multiple_of(sector) {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the sector size {sector}",
            req.block_size
        )));
    }

    // 2. Snapshot + map every partition up front (cross-volume consistency).
    info!(
        disk = req.disk_number,
        parts = req.partitions.len(),
        "whole-disk backup: preparing partitions"
    );
    let mut prepared: Vec<PreparedPartition<P>> = Vec::new();
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    // Partitions whose files nobody walked, each with its reason (see
    // `PreparedMap::skipped`).
    let mut scan_skips: Vec<String> = Vec::new();
    for p in &req.partitions {
        let entry = layout
            .partitions
            .iter()
            .find(|e| e.index == p.part_index)
            .ok_or_else(|| {
                OwnBackupError::Config(format!(
                    "partition {} not found in the disk GPT",
                    p.part_index
                ))
            })?;
        // Partitions sit at their REAL disk offset (sector-aligned, not
        // necessarily block-aligned — a BOOT partition may start at LBA 34). The
        // disk-composite reader below copies whole-disk blocks, so any offset is
        // fine; the checksum block grid is the image itself.
        let part_offset = entry.first_lba * sector;

        // The device must BE that GPT entry. The job names the partition twice
        // — by slot and by device — and the two drift apart when the disk is
        // re-partitioned or renumbered; copying a volume into another slot
        // would restore a disk nobody had. When Windows cannot tell where the
        // device lives, the old behaviour stands.
        if let Some(in_slot) = platform.partition_in_slot(&p.device, &disk, part_offset) {
            if !in_slot {
                return Err(OwnBackupError::Config(
                    tr!(
                        "engine_win.partition_not_in_slot",
                        device = p.device,
                        index = p.part_index,
                        disk = req.disk_number
                    )
                    .into_owned(),
                ));
            }
        }

        let source = platform.normalize_source(&p.device);
        // VSS for a live lettered volume; else read the source directly.
        let session = platform.open_partition_session(&source)?;
        let read_path = session.read_path().to_string();

        // What sort of container this partition is, asked of its first sectors
        // and of nobody's memory. Read whether or not a passphrase came with
        // it: a container we cannot open is still copied — as ciphertext — and
        // the restore screen has to be able to tell that apart from a container
        // we opened and captured in the clear.
        let mut probe = P::Dev::open_read(&read_path)?;
        let mut header = vec![0u8; 4096];
        // Заблокированный BitLocker-том отвечает отказом на ЛЮБОЕ чтение через
        // своё устройство (0x80310000) — включая это. Раньше здесь стоял `?`, и
        // копия целого диска падала на первом же таком разделе: до 2026-08-19
        // мастер их просто не выбирал, и ветка ни разу не встречалась вживую.
        // Отказ — не ошибка, а ответ: том зашифрован и заперт, читать его надо
        // через окно на диск, как это давно делает копия одного тома.
        let mut locked = match probe.read_at(0, &mut header) {
            Ok(()) => false,
            Err(e) if is_bitlocker_locked(&e) => {
                info!(
                    part = p.part_index,
                    "partition is a locked BitLocker volume"
                );
                true
            }
            Err(e) => return Err(e),
        };
        let container = if locked {
            Some(own_backup_common::image::VolumeEncryptionKind::BitLocker)
        } else {
            container_kind(&header)
        };
        // LUKS unlock (decrypted data area is captured).
        let luks = match &p.passphrase {
            Some(pass) if !locked && crate::luks::is_luks(&header) => {
                Some(crate::luks::open_luks(&mut probe, pass.as_bytes())?)
            }
            _ => None,
        };
        // BitLocker: ключ и окно на диск, за которым лежит шифротекст. Пароля
        // нет — раздел всё равно копируется, но как есть, шифротекстом.
        // Запрет записи на диск, если раздел придётся открывать силами Windows.
        // Живёт до конца копирования: том остаётся смонтированным, и Windows
        // пишет в него, когда ей вздумается.
        let mut part_ro_guard: Option<P::OsUnlock> = None;
        let bitlocker = match &p.passphrase {
            Some(pass) if locked || crate::bitlocker::is_bitlocker(&header) => {
                match open_bitlocker_source(platform, &read_path, pass) {
                    Ok(src) => src,
                    // Свой разбор FVE знает только AES-XTS, а размер сектора
                    // берёт из BPB — которого у ReFS нет вовсе («implausible
                    // sector size 0», замер 2026-08-20). И то и другое не повод
                    // ронять задание: то же самое умеет сама Windows, и после
                    // неё раздел становится обычным смонтированным.
                    Err(e) => {
                        warn!(part = p.part_index, error = %e,
                              "свой разбор BitLocker не справился — прошу открыть Windows");
                        match platform.unlock_with_os(&read_path, pass) {
                            Ok(guard) => {
                                part_ro_guard = Some(guard);
                                locked = false; // том теперь читается как все
                                                // Перечитать заголовок: под ним уже открытая ФС,
                                                // и по ней дальше определяется всё остальное.
                                drop(probe);
                                probe = P::Dev::open_read(&read_path)?;
                                let _ = probe.read_at(0, &mut header);
                                None
                            }
                            Err(e2) => {
                                warn!(part = p.part_index, error = %e2,
                                      "Windows тоже не открыла раздел");
                                return Err(e); // наружу — первая ошибка, она по делу
                            }
                        }
                    }
                }
            }
            _ => None,
        };
        // Экстент тома нужен и без ключа: у запертого тома нельзя спросить даже
        // длину через его устройство.
        let locked_extent = if locked && bitlocker.is_none() {
            let extents = platform.ciphertext_extents(&read_path)?;
            match extents.as_slice() {
                [e] => Some((e.device.clone(), e.offset, e.length)),
                _ => {
                    return Err(OwnBackupError::Device(
                        tr!(
                            "engine_win.locked_multi_extent",
                            volume = read_path,
                            count = extents.len()
                        )
                        .into_owned(),
                    ))
                }
            }
        } else {
            None
        };
        let device_len = match (&bitlocker, &locked_extent) {
            (Some(b), _) => b.length,
            (None, Some((_, _, len))) => *len,
            _ => probe.len(),
        };
        drop(probe);
        // The container's own header, kept verbatim so a restore can rebuild
        // it byte-for-byte (D19). A failure here costs the ability to restore
        // this partition encrypted, not the backup itself, so it only warns.
        let luks_header = luks.as_ref().and_then(|l| {
            match P::Dev::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, l))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(part = p.part_index, bytes = bytes.len(), "LUKS header captured");
                    Some(own_backup_common::image::LuksHeaderBackup {
                        part_index: p.part_index,
                        data_offset: l.data_offset,
                        sector_size: l.sector_size,
                        kind: l.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    })
                }
                Err(e) => {
                    warn!(part = p.part_index, error = %e,
                          "could not read the LUKS header — this partition will only restore decrypted");
                    None
                }
            }
        });
        let volume_len = match &luks {
            Some(l) => device_len.saturating_sub(l.data_offset),
            None => device_len,
        };
        // The volume must fit the GPT slot it fills.
        let slot_len = (entry.last_lba + 1 - entry.first_lba) * sector;
        if volume_len > slot_len {
            return Err(OwnBackupError::Config(format!(
                "partition {} volume ({volume_len} B) exceeds its GPT slot ({slot_len} B)",
                p.part_index
            )));
        }

        let prepare_tick = |entries: u64, total: u64| {
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
        let pm = match &bitlocker {
            // Запертый том, ключ у нас есть: карта и исключения снимаются с
            // расшифрованного окна, ровно как в копии одного тома.
            Some(b) => prepare_encrypted_partition_map(
                Box::new(crate::bitlocker::BitLockerVolume::new(
                    Box::new(b.window::<P::Dev>()?),
                    b.params.clone(),
                )),
                req.block_size,
                &req.exclusion_patterns_windows,
                // Своя обёртка: скану есть что сказать про общее число записей,
                // и полоса «N из M» честнее бегущего счётчика.
                |done, total| {
                    progress(&BackupProgress {
                        backup_id: "prepare".into(),
                        status: BackupStatus::Running,
                        blocks_done: done,
                        blocks_total: total,
                        bytes_written: 0,
                        speed_bps: 0,
                        categories: None,
                        excluded: None,
                    })
                },
            ),
            None if locked => Ok(PreparedMap {
                map: None, // ключа нет — копируем шифротекст целиком
                // Уходит в манифест образа (`DiskImageRef::filesystem`) — это
                // данные копии, не текст окна: не переводится.
                filesystem: "BitLocker (заперт)".into(),
                zero_ranges: ByteRanges::new(),
                patches: Vec::new(),
                removal_paths: Vec::new(),
                excluded_files: 0,
                excluded_bytes: 0,
                categories: None,
                skipped: Some(tr!("engine_win.skip_bitlocker_no_password").into_owned()),
            }),
            None => prepare_partition_map(
                platform,
                &read_path,
                req.block_size,
                &luks,
                &req.exclusion_patterns_windows,
                &req.exclusion_patterns_linux,
                // Снимок этого раздела взят, или мы на носителе и система
                // офлайн — только тогда сырые метаданные не движутся.
                session.frozen(),
                // Taken with the filesystem frozen (an LVM or dm snapshot): a
                // dirty but covered XFS log. Asked only where the platform's
                // parsers need a still source — never on Windows.
                P::METADATA_NEEDS_FROZEN && session.quiesced(),
                prepare_tick,
            ),
        }?;
        let used = pm
            .map
            .unwrap_or_else(|| Box::new(AllUsedMap::new(volume_len.div_ceil(bs))));
        excluded_files += pm.excluded_files;
        excluded_bytes += pm.excluded_bytes;
        if let Some(c) = &pm.categories {
            categories.get_or_insert_with(Default::default).merge(c);
        }
        if let Some(why) = &pm.skipped {
            scan_skips.push(
                tr!(
                    "engine_win.skip_partition_line",
                    index = p.part_index,
                    fs = pm.filesystem,
                    why = why
                )
                .into_owned(),
            );
        }
        info!(
            part = p.part_index,
            fs = %pm.filesystem,
            used_blocks = used.used_block_count(),
            excluded_files = pm.excluded_files,
            offset = part_offset,
            "whole-disk backup: partition prepared"
        );
        prepared.push(PreparedPartition {
            part_index: p.part_index,
            device: source,
            read_path,
            luks,
            bitlocker,
            locked_window: locked_extent,
            ro_guard: part_ro_guard,
            volume_len,
            part_offset,
            filesystem: pm.filesystem,
            luks_header,
            zero_ranges: pm.zero_ranges,
            patches: pm.patches,
            removal_paths: pm.removal_paths,
            container,
            used,
            session,
        });
    }

    // Разделы подготовлены — значит сводка по типам файлов посчитана по всем
    // сразу. Отправляем её одним сообщением, до первого скопированного блока.
    report_categories(
        &mut progress,
        categories.as_ref(),
        excluded_files,
        excluded_bytes,
    )?;

    // 3a. Parent chain (plan D5). A non-full kind writes a differencing CHILD
    // over the previous whole-disk image; a broken/incompatible parent falls
    // back to a full backup (reported in the summary) rather than failing.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} whole-disk backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(pp) => match validate_disk_parent(
                pp,
                req.block_size,
                req.image_format,
                req.encryption.as_ref(),
            ) {
                // The child inherits the parent's GPT: with partitions moved or
                // resized since (Windows Update servicing WinRE), today's data
                // would land in yesterday's slots. The image size alone cannot
                // tell — the last partition may still end in the same place.
                Ok(sc)
                    if sc
                        .disk_manifest
                        .as_ref()
                        .is_some_and(|dm| !dm.layout.same_partition_geometry(&layout)) =>
                {
                    warn!(parent = %pp.display(),
                        "the disk layout changed since the parent copy — creating a full backup instead");
                    kind = BackupType::Full;
                }
                Ok(sc) => match child_refusal(
                    req.image_format,
                    req.encryption.is_some(),
                    req.block_size,
                    &sc,
                    P::SOURCE_OS,
                    P::VHDX_CHAINS,
                ) {
                    None => parent = Some((pp.clone(), sc)),
                    Some(why) => {
                        warn!(parent = %pp.display(), reason = %why,
                            "not continuing this whole-disk chain — creating a full backup instead");
                        kind = BackupType::Full;
                    }
                },
                Err(e) => {
                    warn!(parent = %pp.display(), error = %e,
                        "whole-disk parent chain unusable — creating a full backup instead");
                    kind = BackupType::Full;
                }
            },
        }
    }

    // 3b. Size the image from the used-block total, plan the meta partition.
    // Build the whole-disk used-map FIRST (over the partitions region) so the
    // meta partition is sized from the actual disk-block count: a partition not
    // aligned to the block size straddles two disk-blocks per volume block, so
    // the count can exceed the sum of per-volume used counts.
    let part_end = prepared
        .iter()
        .map(|p| p.part_offset + p.volume_len)
        .max()
        .unwrap_or(0);
    let part_end_blocks = part_end.div_ceil(bs);
    let disk_used = DiskUsedMap::build(
        &prepared
            .iter()
            .map(|p| (p.part_offset, p.volume_len, p.used.as_ref()))
            .collect::<Vec<_>>(),
        bs,
        part_end_blocks,
    );
    // A plaintext child inherits the parent's GPT. New full images reserve for
    // every addressable source block, so ordinary growth of used space cannot
    // exhaust the metadata partition. Children read its actual size from GPT;
    // recomputing it from the parent's used-block count would lose that reserve.
    let boot_entries = boot_entries_for_image(platform, req.bootable);
    let own_reserve = crate::sidecar::luks_reserve_for(
        prepared
            .iter()
            .filter_map(|p| p.luks_header.as_ref().map(|h| h.data_offset)),
    ) + crate::sidecar::boot_entries_reserve(boot_entries.as_ref());
    let full_used = |used: u64, reserve: u64| {
        crate::sidecar::calculate_meta_partition_size(used.max(1), reserve)
    };
    let encrypted = req.encryption.is_some();
    let needed_meta = full_used(disk_used.count, own_reserve);
    let full_meta = crate::sidecar::whole_disk_full_metadata_size(
        encrypted,
        disk_used.count,
        part_end_blocks,
        own_reserve,
    );
    let parent_region = match (&parent, encrypted) {
        (Some((path, _)), false) => Some(crate::sidecar::embedded_metadata_region(path, "")?),
        _ => None,
    };
    let parent_meta = parent_region.map(|(_, size)| size);
    let metadata_size = crate::sidecar::whole_disk_metadata_size(
        encrypted,
        if parent.is_some() {
            needed_meta
        } else {
            full_meta
        },
        parent_meta,
    );
    let mut meta_size = metadata_size.unwrap_or(full_meta);
    // Old plaintext chains may still have a small partition. Keep the safe
    // fallback when their fixed room is insufficient for this run's metadata.
    // OBK has no guest metadata partition. Each child reserves its own room,
    // so crossing a metadata-size boundary must not start a new full chain.
    if metadata_size.is_none() {
        if let Some((pp, sc)) = &parent {
            warn!(
                parent = %pp.display(),
                parent_meta = parent_meta.unwrap_or(0),
                needed_meta,
                own_reserve,
                parent_blocks = sc.manifest.used_blocks,
                own_blocks = disk_used.count,
                "this copy's metadata does not fit the room the parent reserved \
                 — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
        }
    }
    // An encrypted image has no guest meta partition: its sidecar is sealed
    // inside the container (§7.3). `meta_size` stays the measure of how much
    // room that sidecar needs — it becomes the container's reservation instead
    // of a partition's size — but the image itself is planned without one.
    let planned_meta = |bytes: u64| if encrypted { 0 } else { bytes };
    let mut plan = own_backup_common::gpt::plan_whole_disk_image(&layout, planned_meta(meta_size));
    if let Some((pp, sc)) = &parent {
        if plan.image_size != sc.manifest.source_disk.size_bytes
            || parent_region.is_some_and(|(offset, _)| offset != plan.meta_first_lba * sector)
        {
            warn!(
                parent = %pp.display(),
                planned = plan.image_size,
                parent_size = sc.manifest.source_disk.size_bytes,
                "whole-disk parent geometry changed — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
            meta_size = full_meta;
            plan = own_backup_common::gpt::plan_whole_disk_image(&layout, planned_meta(meta_size));
        }
    }
    // The kind is settled — the image is written and named as what it is (see
    // the same step in `backup_volume_inner`).
    let published = settled_image_path(
        published_path,
        req.full_image_path.as_deref(),
        req.kind,
        kind,
    );
    let published_path: &Path = &published;
    let meta_offset = plan.meta_first_lba * sector;
    let meta_guid = (!encrypted).then(Uuid::new_v4);

    // 4. Create the image and (for a full) write its GPT. The image GPT gets
    //    FRESH random disk + partition GUIDs, NEVER the source disk's — a
    //    byte-clone of the live disk's GUIDs collides with it (Windows offlines
    //    one of the two, and if the destination lives on the source disk the
    //    whole disk, incl. the backup drive, vanishes: the 2026-07-11 E:
    //    incident). The REAL GUIDs are kept in the manifest below; restore
    //    writes them back (bootable).
    let mut safe_layout = layout.clone();
    safe_layout.disk_guid = Uuid::new_v4().to_string().to_uppercase();
    for p in &mut safe_layout.partitions {
        p.part_guid = Uuid::new_v4().to_string().to_uppercase();
    }
    // The GPT write-op grid (offsets + lengths) is deterministic from the plan
    // and partition count — the GUID VALUES only change the bytes. A child
    // reuses the PARENT's exact GPT bytes (read below) so any used block that
    // overlaps the GPT head (a partition starting at LBA 34) matches the parent
    // and is not needlessly rewritten; a full writes its own fresh GPT.
    let mut gpt_ops: Vec<(u64, Vec<u8>)> =
        crate::gpt_writer::whole_disk_gpt_ops(&safe_layout, &plan, meta_guid)?
            .into_iter()
            .map(|op| (op.offset, op.bytes))
            .collect();

    // Identity of this image, needed before it is created: the container seals
    // it into its own root record when encrypted.
    let backup_id = Uuid::new_v4();
    let (mut target, sector_size) = match &parent {
        None => create_disk_image_target(
            platform,
            &req.image_path,
            plan.image_size,
            req.image_format,
            req.encryption.as_ref(),
            &DiskImagePlan {
                backup_id,
                role: ChainRole::Full,
                parent: None,
                source_id: identity.source_id.clone(),
                zstd_level: crate::qcow2::zstd_level_or_default(req.zstd_level),
            },
            owned_file,
        )?,
        Some((pp, sc)) => {
            // Child inherits the parent's GPT through the chain — overwrite the
            // op bytes with the parent's actual GPT so the composite reader
            // reproduces them byte-for-byte. Drop the parent read handle before
            // creating the differencing child (VHDX CreateVirtualDisk opens the
            // parent itself).
            {
                // The chain key comes from the parent itself, which is where
                // the wrapped one travels.
                let parent_key = match &req.encryption {
                    Some(keys) => Some(keys.open_chain_of(pp)?.key),
                    None => None,
                };
                let mut pimg = ImageReadSource::open_with(
                    pp,
                    &container_ext(req.image_format, req.encryption.as_ref()),
                    parent_key.as_ref(),
                )?;
                for (off, bytes) in &mut gpt_ops {
                    pimg.read_at(*off, bytes)?;
                }
            }
            let parent_name = pp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            create_disk_child_target(
                platform,
                &req.image_path,
                pp,
                plan.image_size,
                req.image_format,
                req.encryption.as_ref(),
                &DiskImagePlan {
                    backup_id,
                    role: match kind {
                        BackupType::Differential => ChainRole::Differential,
                        _ => ChainRole::Incremental,
                    },
                    parent: Some((sc.manifest.backup_id, parent_name)),
                    source_id: identity.source_id.clone(),
                    zstd_level: crate::qcow2::zstd_level_or_default(req.zstd_level),
                },
                owned_file,
            )?
        }
    };
    if parent.is_none() {
        // Full backup: lay down the fresh GPT — but ONLY into blocks the copy
        // pass will NOT write. A partition that begins inside the first block
        // (LBA 34, offset 17408) makes block 0 "used"; the composite copy then
        // writes that block with the GPT bytes already baked in, so writing the
        // GPT op there too would hit the same host cluster twice — fatal for the
        // write-once qcow2 writer ("cluster 0 written twice") and merely
        // redundant for VHDX. Unused GPT regions (an aligned primary GPT, the
        // backup GPT at the image tail) still need this direct write.
        for (off, bytes) in &gpt_ops {
            let mut p = 0usize;
            while p < bytes.len() {
                let abs = off + p as u64;
                let block = abs / bs;
                let take = (((block + 1) * bs - abs) as usize).min(bytes.len() - p);
                if !disk_used.is_block_used(block) {
                    target.write_at(abs, &bytes[p..p + take])?;
                }
                p += take;
            }
        }
    }

    // 5. Copy the disk as whole-image blocks (block N = image offset N*bs). Each
    //    block is assembled from the GPT and the partitions at their real
    //    offsets, so partitions need no block alignment and the checksum grid is
    //    the image itself (block_number * bs = disk offset — verifiable).
    //    `disk_used` (built above) returns false past the partitions region, so
    //    the meta/tail blocks (written separately) are not copied.
    let mut regions: Vec<DiskRegion> = Vec::new();
    for prep in &prepared {
        let reader: Box<dyn crate::fat_map::RawVolume + Send> =
            match (&prep.bitlocker, &prep.locked_window) {
                // Расшифрованное окно на диск: том заперт, его устройство молчит.
                (Some(b), _) => Box::new(crate::bitlocker::BitLockerVolume::new(
                    Box::new(b.window::<P::Dev>()?),
                    b.params.clone(),
                )),
                // Заперт и без ключа — тот же путь, но без расшифровки.
                (None, Some((disk, off, len))) => {
                    Box::new(disk_window::<P::Dev>(disk, *off, *len)?)
                }
                (None, None) => {
                    let dev = P::Dev::open_read(&prep.read_path)?;
                    match &prep.luks {
                        Some(l) => Box::new(crate::decrypt::DecryptingVolume::new(
                            Box::new(dev),
                            l.clone(),
                        )),
                        None => Box::new(dev),
                    }
                }
            };
        regions.push(DiskRegion {
            part_offset: prep.part_offset,
            volume_len: prep.volume_len,
            reader,
            zero_ranges: prep.zero_ranges.clone(),
            patches: prep.patches.clone(),
        });
    }
    let mut composite = DiskCompositeReader {
        block_size: req.block_size,
        image_size: plan.image_size,
        gpt_ops,
        regions,
    };
    // For a child only blocks that differ from the parent version are written;
    // the rest resolve through the differencing chain. `outcome.checksums` still
    // covers EVERY used block, so the child's sidecar describes the full image.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sc)| {
        sc.checksums
            .iter()
            .map(|c| (c.block_number, c.checksum))
            .collect()
    });
    let outcome = copy_used_blocks(
        &identity.source_id,
        &mut composite,
        &disk_used,
        parent_hashes.as_ref(),
        |block, data| target.write_at(block * bs, data),
        &mut progress,
    )?;
    let mut checksums = outcome.checksums;
    let bytes_copied = outcome.bytes_copied;
    let blocks_copied = outcome.blocks_copied;
    let blocks_unchanged = outcome.blocks_unchanged;

    // 6. Reserve the meta partition region while the image is open so the VHDX
    //    BAT physically allocates EVERY block of it (an all-zero write is
    //    optimised away → the block stays ZERO/unallocated and the later
    //    guest-write of the real blob fails with "block state 2"). A non-zero
    //    fill across the whole region forces allocation; the guest-write below
    //    then overwrites it with the actual sidecar. Then finalize + release.
    //    An encrypted image has no such partition: the sidecar goes inside the
    //    container at finalize, with `meta_size` bytes of room reserved so the
    //    final one — the exclusion figures and the re-checksummed blocks are
    //    known only after the pass below — can replace it in place.
    if !encrypted {
        let placeholder = vec![0x01u8; meta_size as usize];
        target.write_at(meta_offset, &placeholder)?;
    }
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: bytes_copied,
        speed_bps: 0,
        categories: None,
        excluded: None,
    });
    // A provisional sidecar, so the container has something of the right shape
    // to reserve around. Nothing reads it: the run either replaces it below or
    // fails, and a failed run deletes its image.
    let provisional = if encrypted {
        let mut blob = Vec::new();
        let info = SidecarInfo::new_full("provisional", 0);
        crate::sidecar::write_sidecar_v2_to(
            &mut blob,
            &info,
            &whole_disk_image_manifest(
                &layout,
                req,
                P::SOURCE_OS,
                &identity.device_id,
                sector_size,
                plan.image_size,
                plan.image_size.div_ceil(bs),
                0,
                kind,
                None,
            ),
            &[],
        )?;
        blob
    } else {
        Vec::new()
    };
    target.finalize(&provisional, meta_size as usize)?;
    for prep in &mut prepared {
        if let Some(Err(e)) = prep.session.complete() {
            warn!(error = %e, "VSS release failed (data is already safe)");
        }
    }

    // 6b. True removal of excluded NTFS/ReFS files inside the finished image
    //     (plan D2, both formats, full AND incremental). Mount the whole-disk
    //     image once — an incremental is a differencing child, so the temp child
    //     the removal pass creates over it resolves the WHOLE chain and every
    //     partition's files are visible — let the FS driver delete each
    //     partition's recorded paths on its own volume, then merge into THIS
    //     image's leaf + re-checksum the touched blocks. So excluded files vanish
    //     from the catalog instead of remaining zero-filled ghosts. Same graceful
    //     contract as the per-volume path: a pre-merge failure keeps the ghosts
    //     (valid backup), only a post-merge failure fails the run — cannot break
    //     a working backup. The content is already zeroed either way, so no
    //     excluded data leaks even when this pass is skipped.
    let removal: Vec<(u64, Vec<crate::exclusions::ExcludedPath>)> = prepared
        .iter_mut()
        .filter(|p| !p.removal_paths.is_empty())
        .map(|p| (p.part_offset, std::mem::take(&mut p.removal_paths)))
        .collect();
    let mut excluded_entries_kept: Option<String> = None;
    if !removal.is_empty() {
        // Каждый проход удаления называет свой итог сам: удаление — пути,
        // зануление — куски, пере-хеширование — блоки. Ноль значит «итог
        // неизвестен», и полоса тогда остаётся бегущей, как раньше.
        let mut tick = |entries: u64, total: u64| {
            let _ = progress(&BackupProgress {
                backup_id: "exclusions".into(),
                status: BackupStatus::Running,
                blocks_done: entries,
                blocks_total: total,
                bytes_written: bytes_copied,
                speed_bps: 0,
                categories: None,
                excluded: None,
            });
        };
        let image = FinishedImage {
            path: &req.image_path,
            format: req.image_format,
            encryption: req.encryption.as_ref(),
            block_size: req.block_size,
            zstd_level: req.zstd_level,
        };
        let removed = platform.remove_from_disk_image(
            &image,
            plan.image_size,
            &removal,
            &mut checksums,
            &mut tick,
        )?;
        // The pass logged why in detail; until this line that log was the
        // only place it was said, and the run showed a plain success.
        excluded_entries_kept = match removed {
            None => Some(platform.entries_kept_reason()),
            Some(0) => None,
            Some(failed) => {
                Some(tr!("engine_win.entries_partly_kept", count = failed).into_owned())
            }
        };
    }

    // 7. Build the embedded manifest + sidecar and write it into the meta region.
    let image_file_name = published_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let images: Vec<own_backup_common::gpt::DiskImageRef> = prepared
        .iter()
        .map(|p| own_backup_common::gpt::DiskImageRef {
            part_index: p.part_index,
            token: disk_source_token(&p.device),
            image_file_name: image_file_name.clone(),
            filesystem: p.filesystem.clone(),
            // Opened → the image holds the volume in the clear. Not opened →
            // the container's own bytes went in untouched, and the restore
            // brings it back encrypted, password and all.
            encryption: p.container.clone().map(|kind| {
                use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
                let opened = p.luks.is_some();
                VolumeEncryptionState {
                    kind,
                    capture_mode: if opened {
                        VolumeCaptureMode::Plaintext
                    } else {
                        VolumeCaptureMode::SectorBySector
                    },
                    luks_header_backup_b64: None, // the header itself lives in `info.luks_headers`
                    note: None,
                }
            }),
        })
        .collect();
    let disk_manifest = own_backup_common::gpt::DiskBackupManifest {
        format_version: own_backup_common::gpt::DiskBackupManifest::FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        job_id: req.job_id.clone(),
        disk_number: req.disk_number,
        disk_model: req.disk_model.clone(),
        layout: layout.clone(),
        images,
        source_disk: req.source_disk.clone(),
    };

    let total_blocks = plan.image_size.div_ceil(bs);
    // `used_blocks` is the FULL image state (every checksummed block), not just
    // this run's writes — the next child sizes its meta partition from it, so it
    // must stay stable across the chain (checksums.len() == disk_used.count).
    let mut manifest = whole_disk_image_manifest(
        &layout,
        req,
        P::SOURCE_OS,
        &identity.device_id,
        sector_size,
        plan.image_size,
        total_blocks,
        checksums.len() as u64,
        kind,
        parent.as_ref().map(|(_, sc)| sc.manifest.backup_id),
    );
    // The container sealed this identity into its own root record before a
    // single cluster was written; the sidecar has to repeat it, or the two
    // halves of the image would name different backups.
    manifest.backup_id = backup_id;
    // A whole-disk image is disk-block absolute → volume_data_offset = 0.
    let mut info = match &parent {
        None => SidecarInfo::new_full(image_file_name, 0),
        Some((pp, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_name = pp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_name, 0)
        }
    };
    info.container = container_ext(req.image_format, req.encryption.as_ref());
    info.job_id = req.job_id.clone();
    info.source_disk_layout = req.source_disk.clone();
    info.categories = categories;
    info.excluded_files = excluded_files;
    info.excluded_bytes = excluded_bytes;
    // One captured container header per encrypted partition (D19): with them a
    // restore rebuilds the container byte-for-byte, so the restored system's
    // crypttab and initramfs keep working untouched.
    info.luks_headers = prepared
        .iter()
        .filter_map(|p| p.luks_header.clone())
        .collect();
    // The machine's boot entries — INSIDE the image, with everything else that
    // describes it. They used to be written to a `.boot.json` beside it: the
    // last piece of a backup that was not in the backup, and the one a user
    // would carry to another disk without noticing it had stayed behind.
    //
    // On 2026-09-18 this machine lost the entries for Acronis and R-Drive Image
    // while their loaders sat untouched on the ESP; no copy taken before that
    // would have brought them back. That is what they are for.
    info.boot_entries = boot_entries;

    let mut blob = Vec::new();
    crate::sidecar::write_sidecar_v2_full_to(
        &mut blob,
        &info,
        &manifest,
        &checksums,
        Some(&disk_manifest),
    )?;
    // The metadata partition was sized before any of this existed as bytes. If
    // the blob will not fit, the boot entries are the one part of it that can
    // be given up: they are the machine's, identical from run to run, and the
    // chain's other copies have them. Everything else in here describes THIS
    // image and has nowhere else to live.
    if blob.len() as u64 > meta_size && info.boot_entries.is_some() {
        warn!(
            blob = blob.len(),
            meta_size,
            "no room for the boot entries in this image's metadata — writing without them"
        );
        info.boot_entries = None;
        blob.clear();
        crate::sidecar::write_sidecar_v2_full_to(
            &mut blob,
            &info,
            &manifest,
            &checksums,
            Some(&disk_manifest),
        )?;
    }
    match (&req.encryption, req.image_format) {
        (Some(keys), _) => {
            // No guest meta partition to write into: the sidecar replaces the
            // reserved one inside the container, sealed under the same chain
            // key and with a fresh nonce.
            let chain = keys.open_chain_of(&req.image_path)?;
            crate::seal::obk::metadata::update_unpublished_sealed_metadata(
                &req.image_path,
                &chain.key,
                &blob,
            )?;
        }
        (None, ImageFormat::Vhdx) => {
            crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob)?
        }
        (None, ImageFormat::Qcow2) => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob)?
        }
    }

    info!(
        image = %req.image_path.display(),
        disk = %identity.device_id,
        kind = %kind,
        partitions = prepared.len(),
        blocks_written = blocks_copied,
        blocks_unchanged,
        bytes = bytes_copied,
        secs = started.elapsed().as_secs(),
        "Whole-disk backup complete"
    );
    Ok(DiskBackupSummary {
        image_path: req.image_path.clone(),
        bytes_copied,
        blocks_copied,
        partitions: prepared.len() as u32,
        duration: started.elapsed(),
        backup_kind: kind,
        blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
        file_scan_skipped: if scan_skips.is_empty() {
            None
        } else {
            Some(scan_skips.join("; "))
        },
        excluded_entries_kept,
    })
}

#[cfg(test)]
#[path = "disk_tests.rs"]
mod tests;
