//! Whole-disk (bootable) backup — what a real job actually does.
//!
//! [`super::backup`] copies ONE filesystem. A job copies a DISK: several
//! partitions into a single image carrying the disk's own GPT and an embedded
//! restore manifest, and that is where the interesting machinery lives — the
//! per-partition exclusion scan, the GPT the image is built around, and the
//! read-write mount that afterwards deletes the excluded entries from the
//! image's own catalog.
//!
//! None of it could be reached without the wizard until 2026-09-18, so
//! reproducing a whole-disk fault meant clicking through the interface and
//! waiting for a scheduled run, with the service owning the log, the
//! destination and the selection. This is the same engine call the service
//! makes, with the selection spelled out as partition numbers and the run in
//! the foreground where its output can be read.

use clap::Args;

#[derive(Args)]
pub struct BackupDiskArgs {
    /// Physical drive number, as `list-disks` prints it (Windows).
    #[arg(long, value_name = "N", required_unless_present = "device")]
    pub disk: Option<u32>,

    /// The disk's device (Linux): `/dev/disk/by-id/…` — the name that stays
    /// the same after a reboot — or any path to the whole disk.
    #[arg(long, value_name = "УСТРОЙСТВО", conflicts_with = "disk")]
    pub device: Option<String>,

    /// GPT partition numbers to copy, e.g. 1,2,4. Left out: every partition the
    /// inventory found something openable on, which is the wizard with all
    /// boxes ticked. A partition that is NOT selected is not in the copy at
    /// all — on restore its space comes back free.
    #[arg(long, value_delimiter = ',', value_name = "СПИСОК")]
    pub partitions: Vec<u32>,

    /// Output image path.
    #[arg(short, long)]
    pub output: String,

    /// Загрузочная копия: положить в образ ещё и записи загрузки UEFI этой
    /// машины. Они живут в памяти материнской платы, а не на диске, поэтому
    /// копия диска с данными их не несёт — и восстанавливать их на другой
    /// машине было бы прямо неверно.
    #[arg(long)]
    pub bootable: bool,

    /// Image container format: vhdx (Windows-native mounting, no compression)
    /// or qcow2 (zstd-compressed, cross-platform).
    #[arg(long, default_value = "vhdx")]
    pub format: own_backup_common::types::ImageFormat,

    /// Backup block size in MiB.
    #[arg(long, default_value_t = 1)]
    pub block_size_mib: u32,

    /// full | incremental | differential. A child is written over --parent;
    /// without a usable parent the engine falls back to a full copy and says so.
    #[arg(long, default_value = "full")]
    pub kind: String,

    /// Parent whole-disk image of the SAME disk: the previous version for an
    /// incremental, the chain's full for a differential.
    #[arg(long, value_name = "ОБРАЗ")]
    pub parent: Option<String>,

    /// Найти родителя самому: новейшая цепочка образов рядом с --output из
    /// тех, что собраны не заданием, а из командной строки. Наследовать нечего
    /// — пишется полная копия, и об этом говорится.
    #[arg(long)]
    pub parent_auto: bool,

    /// Проверить готовый образ по его контрольным суммам — то же, что
    /// «проверять после создания» в задании.
    #[arg(long)]
    pub verify: bool,

    /// Маска исключения; можно повторять. Обычно правила приходят из задания
    /// GUI — здесь они нужны, чтобы проверять исключения без мастера.
    #[arg(long, value_name = "МАСКА")]
    pub exclude: Vec<String>,

    /// Взять те же двенадцать правил, что мастер подставляет новому заданию
    /// (pagefile.sys, временные папки, *.tmp и прочие).
    #[arg(long)]
    pub exclude_defaults: bool,

    /// Passphrase for encrypted (LUKS/BitLocker) partitions of this disk.
    #[arg(long)]
    pub password: Option<String>,

    /// Show the selection and what it would copy, then stop.
    #[arg(long)]
    pub dry_run: bool,
}

/// Trailing digits of a `PhysicalDrive4`-shaped name — how a disk names itself
/// in the inventory, and the only part of it that is a number.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn disk_number_of(device_id: &str) -> Option<u32> {
    let tail: String = device_id
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    tail.chars().rev().collect::<String>().parse().ok()
}

#[cfg(target_os = "windows")]
pub async fn run(args: BackupDiskArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::{backup_disk, DiskBackupRequest, DiskPartitionBackup};
    use own_backup_common::OwnBackupError;
    use own_backup_inventory::DiskInventory;
    use std::io::Write;

    if args.block_size_mib == 0 || args.block_size_mib > 64 {
        return Err(OwnBackupError::Config("block size must be 1-64 MiB".into()));
    }
    let disk_number = args.disk.ok_or_else(|| {
        OwnBackupError::Config("на Windows диск называется номером: --disk N".into())
    })?;
    let image_path = std::path::PathBuf::from(&args.output);
    // Which version this is and what it is written over — the same rules the
    // single-volume copy follows (`super::copy_options`).
    let (kind, parent_image) = super::copy_options::resolve_parent(
        super::copy_options::backup_kind(&args.kind)?,
        args.parent.as_deref(),
        args.parent_auto,
        &image_path,
        args.format,
    )?;

    let inventory = own_backup_inventory::WindowsInventory::new()?;
    let disks = inventory.enumerate_disks().await?;
    let disk = disks
        .iter()
        .find(|d| disk_number_of(&d.device_id) == Some(disk_number))
        .ok_or_else(|| {
            OwnBackupError::Config(format!(
                "диск {} не найден — посмотрите own-backup-cli list-disks",
                disk_number
            ))
        })?;

    // What to copy. Microsoft Reserved has no filesystem and no device, and the
    // engine keeps its GPT entry whenever the copy carries Windows: naming it
    // is not an error and not a source either, so say that instead of failing
    // or dropping it in silence.
    let msr = own_backup_common::gpt::GPT_TYPE_MSR;
    let mut chosen = Vec::new();
    if args.partitions.is_empty() {
        chosen.extend(
            disk.partitions
                .iter()
                .filter(|p| !p.volume_device.is_empty()),
        );
    } else {
        for want in &args.partitions {
            let p = disk
                .partitions
                .iter()
                .find(|p| p.index == *want)
                .ok_or_else(|| {
                    OwnBackupError::Config(format!("на диске {} нет раздела {want}", disk_number))
                })?;
            if p.volume_device.is_empty() {
                if p.gpt_type_guid.eq_ignore_ascii_case(msr) {
                    println!(
                        "  #{}: Microsoft Reserved — копировать в нём нечего; в разметке он \
                         останется, если в копию входит Windows",
                        p.index
                    );
                    continue;
                }
                return Err(OwnBackupError::Config(format!(
                    "раздел {want} нечем открыть: файловая система не опознана ({:?})",
                    p.filesystem
                )));
            }
            chosen.push(p);
        }
    }
    if chosen.is_empty() {
        return Err(OwnBackupError::Config(
            "копировать нечего: ни один выбранный раздел не открывается".into(),
        ));
    }

    // Named before anything runs. A whole-disk copy quietly leaving a partition
    // behind is the one mistake here that stays invisible until a restore, so
    // both halves of the selection go on screen.
    println!(
        "Диск {} — {} ({:.1} ГБ, разделов: {})",
        disk_number,
        disk.model,
        disk.size_bytes as f64 / 1e9,
        disk.partitions.len()
    );
    println!("В копию войдут:");
    for p in &chosen {
        println!(
            "  #{:<2} {:<9} {:>8.1} ГБ  {}",
            p.index,
            format!("{:?}", p.filesystem),
            p.size_bytes as f64 / 1e9,
            p.volume_device
        );
    }
    let kept: std::collections::HashSet<u32> = chosen.iter().map(|p| p.index).collect();
    // The engine's own rule, asked of the same GPT it will read.
    let msr_kept = own_backup_backup::gpt_reader::read_disk_layout(disk_number)?
        .copy_carries_windows(&kept, Some(disk));
    let is_msr =
        |p: &&own_backup_common::disk::PartitionInfo| p.gpt_type_guid.eq_ignore_ascii_case(msr);
    if msr_kept {
        for p in disk.partitions.iter().filter(is_msr) {
            println!(
                "  #{:<2} Microsoft Reserved — только запись в таблице разделов",
                p.index
            );
        }
    }
    let left_out: Vec<_> = disk
        .partitions
        .iter()
        .filter(|p| !kept.contains(&p.index) && !(msr_kept && is_msr(p)))
        .collect();
    if !left_out.is_empty() {
        println!("НЕ войдут (при восстановлении их место будет свободно):");
        for p in &left_out {
            println!(
                "  #{:<2} {:<9} {:>8.1} ГБ  {}",
                p.index,
                format!("{:?}", p.filesystem),
                p.size_bytes as f64 / 1e9,
                p.volume_label
            );
        }
    }

    let exclusions = {
        let mut p = if args.exclude_defaults {
            own_backup_core::default_exclusions()
        } else {
            Vec::new()
        };
        p.extend(args.exclude.iter().cloned());
        p
    };
    if !exclusions.is_empty() {
        println!("Правил исключения: {}", exclusions.len());
    }
    if args.dry_run {
        println!("--dry-run: ничего не копируем.");
        return Ok(());
    }

    let request = DiskBackupRequest {
        disk_number,
        disk_device: None,
        disk_model: disk.model.clone(),
        partitions: chosen
            .iter()
            .map(|p| DiskPartitionBackup {
                device: p.volume_device.clone(),
                part_index: p.index,
                passphrase: args.password.clone().map(zeroize::Zeroizing::new),
            })
            .collect(),
        image_path: image_path.clone(),
        full_image_path: None,
        bootable: args.bootable,
        block_size: args.block_size_mib * 1024 * 1024,
        image_format: args.format,
        // The command line does not open encrypted archives yet (E5).
        encryption: None,
        // The settings' default — what a job that has not chosen its own gets.
        zstd_level: own_backup_backup::qcow2::zstd_level_or_default(
            own_backup_common::config::AppConfig::load_or_default().default_compression_level,
        ),
        job_id: None,
        exclusion_patterns_windows: exclusions.clone(),
        exclusion_patterns_linux: exclusions,
        kind,
        parent_image,
        source_disk: Some(disk.clone()),
    };

    println!();
    println!("Пишем {} ({}, {kind:?})", image_path.display(), args.format);
    let summary = tokio::task::spawn_blocking(move || {
        let mut last = String::new();
        backup_disk(&request, |p| {
            // The engine tags the phases that are not the block copy through
            // backup_id: the exclusion scan before it, and the flush / detach /
            // true-removal pass after. Either can take minutes with the block
            // counter standing still, which reads as a hang unless it is named.
            let line = match p.backup_id.as_str() {
                "prepare" => "  поиск исключаемых файлов…".to_string(),
                "finalize" | "publish" => {
                    "  завершение: сброс, отсоединение, удаление исключённых…".to_string()
                }
                _ => format!(
                    "  {:3}%  {} / {} блоков  ({:.1} МБ/с)",
                    (p.blocks_done * 100)
                        .checked_div(p.blocks_total)
                        .unwrap_or(0),
                    p.blocks_done,
                    p.blocks_total,
                    p.speed_bps as f64 / 1_000_000.0
                ),
            };
            if line != last {
                last.clone_from(&line);
                print!("\r{line:<72}");
                let _ = std::io::stdout().flush();
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| OwnBackupError::Device(format!("backup task panicked: {e}")))??;

    println!();
    println!("Готово:");
    println!("  Образ:       {}", summary.image_path.display());
    println!("  Вид:         {:?}", summary.backup_kind);
    println!("  Разделов:    {}", summary.partitions);
    println!(
        "  Скопировано: {:.2} ГБ ({} блоков, {} совпало с родителем)",
        summary.bytes_copied as f64 / 1e9,
        summary.blocks_copied,
        summary.blocks_unchanged
    );
    println!(
        "  Исключено:   {} файлов, {:.2} ГБ",
        summary.excluded_files,
        summary.excluded_bytes as f64 / 1e9
    );
    if let Some(skipped) = &summary.file_scan_skipped {
        println!("  Без разбора файлов: {skipped}");
    }
    if let Some(kept) = &summary.excluded_entries_kept {
        println!("  Исключённые остались в каталоге копии: {kept}");
    }
    println!("  Заняло:      {:.1} с", summary.duration.as_secs_f64());
    if args.verify {
        println!();
        super::copy_options::verify_after(&summary.image_path)?;
    }
    Ok(())
}

/// The device of partition `index` of `disk`: its stable
/// `/dev/disk/by-partuuid/…` name when udev made one, the kernel's name
/// otherwise (`sdb2`, `nvme0n1p2`, `loop0p2`).
#[cfg(target_os = "linux")]
fn partition_device(disk: &str, index: u32, part_guid: &str) -> Option<String> {
    use std::os::unix::fs::FileTypeExt;
    let is_block = |p: &str| {
        std::fs::metadata(p)
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false)
    };
    let stable = format!("/dev/disk/by-partuuid/{}", part_guid.to_ascii_lowercase());
    if is_block(&stable) {
        return Some(stable);
    }
    let canon = std::fs::canonicalize(disk).ok()?;
    let canon = canon.to_str()?;
    let name = if canon.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{canon}p{index}")
    } else {
        format!("{canon}{index}")
    };
    is_block(&name).then_some(name)
}

#[cfg(target_os = "linux")]
pub async fn run(args: BackupDiskArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_linux::{
        backup_disk, container_key_from, verify_image, DiskBackupRequest, DiskPartitionBackup,
    };
    use own_backup_common::OwnBackupError;
    use own_backup_snapshot::SnapshotProvider;

    if args.block_size_mib == 0 || args.block_size_mib > 64 {
        return Err(OwnBackupError::Config("block size must be 1-64 MiB".into()));
    }
    let device = args.device.clone().ok_or_else(|| {
        OwnBackupError::Config(
            "на Linux диск называется устройством: --device /dev/disk/by-id/…".into(),
        )
    })?;
    let kind = super::copy_options::backup_kind(&args.kind)?;
    if kind != own_backup_common::types::BackupType::Full
        && args.format == own_backup_common::types::ImageFormat::Vhdx
    {
        return Err(OwnBackupError::Config(
            "цепочки VHDX (incremental/differential) на Linux пока не пишутся — \
             используйте --format qcow2"
                .into(),
        ));
    }
    let image_path = std::path::PathBuf::from(&args.output);
    let (kind, parent_image) = super::copy_options::resolve_parent(
        kind,
        args.parent.as_deref(),
        args.parent_auto,
        &image_path,
        args.format,
    )?;

    // The disk's own GPT, read off the disk.
    let layout = {
        let dev = own_backup_backup::linux_device::LinuxRawDevice::open_read(&device)?;
        let sector = dev.sector_size();
        let want = (34 * sector as u64).min(dev.length()) as usize;
        let mut head = vec![0u8; want];
        dev.read_at(0, &mut head)?;
        own_backup_backup::gpt_reader::parse_disk_layout(&head, sector, dev.length())?
    };
    let chosen: Vec<&own_backup_common::gpt::GptPartitionEntry> = if args.partitions.is_empty() {
        layout.partitions.iter().collect()
    } else {
        args.partitions
            .iter()
            .map(|want| {
                layout
                    .partitions
                    .iter()
                    .find(|p| p.index == *want)
                    .ok_or_else(|| {
                        OwnBackupError::Config(format!("на диске {device} нет раздела {want}"))
                    })
            })
            .collect::<Result<_, _>>()?
    };
    let mut parts = Vec::new();
    for p in &chosen {
        let dev = partition_device(&device, p.index, &p.part_guid).ok_or_else(|| {
            OwnBackupError::Config(format!(
                "раздел {} диска {device}: устройства раздела нет (ядро не перечитало таблицу?)",
                p.index
            ))
        })?;
        parts.push((p.index, dev, p.size_bytes(layout.sector_size)));
    }
    println!(
        "Диск {device} ({:.1} ГБ, разделов: {})",
        layout.size_bytes as f64 / 1e9,
        layout.partitions.len()
    );
    println!("В копию войдут:");
    for (index, dev, size) in &parts {
        println!("  #{index:<2} {:>8.1} ГБ  {dev}", *size as f64 / 1e9);
    }
    if args.dry_run {
        return Ok(());
    }

    // Every partition held before a block is copied: a snapshot, a short
    // hold, or the idle device; a partition in use without one is refused.
    let out_dir = match image_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let provider = own_backup_snapshot::LinuxSnapshotProvider::with_cow_dir(&out_dir)
        .container_key(container_key_from(
            args.password.clone().map(zeroize::Zeroizing::new),
        ));
    let mut snapshots = Vec::new();
    for (_, dev, _) in &parts {
        match provider.create_snapshot(dev).await {
            Ok(s) => snapshots.push(s),
            Err(e) => {
                for s in &snapshots {
                    let _ = provider.release_snapshot(s).await;
                }
                return Err(e);
            }
        }
    }

    // A mask typed here is meant for whatever the partition holds: both
    // families; the defaults are each family's own.
    let rules = |defaults: Vec<String>| {
        let mut p = if args.exclude_defaults {
            defaults
        } else {
            Vec::new()
        };
        p.extend(args.exclude.iter().cloned());
        p
    };
    let request = DiskBackupRequest {
        disk_number: 0,
        disk_device: Some(device.clone()),
        disk_model: String::new(),
        partitions: parts
            .iter()
            .map(|(index, dev, _)| DiskPartitionBackup {
                device: dev.clone(),
                part_index: *index,
                passphrase: args.password.clone().map(zeroize::Zeroizing::new),
            })
            .collect(),
        image_path: image_path.clone(),
        full_image_path: None,
        block_size: args.block_size_mib * 1024 * 1024,
        image_format: args.format,
        job_id: None,
        encryption: None,
        exclusion_patterns_windows: rules(own_backup_core::default_exclusions()),
        exclusion_patterns_linux: rules(own_backup_core::default_linux_exclusions()),
        kind,
        parent_image,
        source_disk: None,
        bootable: args.bootable,
        zstd_level: own_backup_backup::qcow2::zstd_level_or_default(
            own_backup_common::config::AppConfig::load_or_default().default_compression_level,
        ),
    };

    // Ctrl+C, SIGTERM or a closed terminal: the copy stops at its next block
    // and every hold is let go below, as after any other failure.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            eprintln!("stopping: the copy ends at its next block and the snapshots are released");
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let copied = {
        let stop = stop.clone();
        let snapshots = snapshots.clone();
        tokio::task::spawn_blocking(move || {
            backup_disk(&request, &snapshots, |_p| {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    Err(OwnBackupError::Cancelled)
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| OwnBackupError::Config(format!("backup task: {e}")))?
    };
    for s in &snapshots {
        if let Err(e) = provider.release_snapshot(s).await {
            eprintln!("warning: snapshot release failed: {e}");
        }
    }
    let summary = copied?;
    println!(
        "Done: {} partitions, {:.1} MiB copied in {:.1}s ({:?})",
        summary.partitions,
        summary.bytes_copied as f64 / 1_048_576.0,
        summary.duration.as_secs_f64(),
        summary.backup_kind
    );
    if summary.excluded_files > 0 {
        println!(
            "Excluded: {} files, {:.1} MiB",
            summary.excluded_files,
            summary.excluded_bytes as f64 / 1_048_576.0
        );
    }
    if let Some(why) = &summary.file_scan_skipped {
        println!("Exclusions not applied: {why}");
    }
    if let Some(why) = &summary.excluded_entries_kept {
        println!("Excluded names kept: {why}");
    }
    if summary.backup_kind != kind {
        println!(
            "The parent could not be continued — a {:?} copy was written instead (see the log).",
            summary.backup_kind
        );
    }
    println!("Image: {}", summary.image_path.display());
    if args.verify {
        let v = verify_image(&summary.image_path, |_, _| Ok(()))?;
        if !v.valid() {
            return Err(OwnBackupError::ImageFormat(format!(
                "{}: {} blocks do not match their checksums",
                summary.image_path.display(),
                v.mismatched_blocks.len()
            )));
        }
        println!("Verify: OK ({} blocks)", v.blocks_checked);
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: BackupDiskArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Копия целого диска собирается только на Windows и Linux.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inventory names a disk `PhysicalDrive4`; the selection is by number,
    /// and the two have to meet somewhere.
    #[test]
    fn a_disk_is_found_by_the_number_at_the_end_of_its_name() {
        assert_eq!(disk_number_of(r"\\.\PhysicalDrive4"), Some(4));
        assert_eq!(disk_number_of(r"\\.\PhysicalDrive11"), Some(11));
        assert_eq!(disk_number_of("PhysicalDrive0"), Some(0));
        assert_eq!(disk_number_of(r"\\.\PhysicalDrive"), None);
        assert_eq!(disk_number_of(""), None);
    }
}
