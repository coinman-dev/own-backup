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
    /// Physical drive number, as `list-disks` prints it.
    #[arg(long, value_name = "N")]
    pub disk: u32,

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
    let tail: String = device_id.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
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
        .find(|d| disk_number_of(&d.device_id) == Some(args.disk))
        .ok_or_else(|| {
            OwnBackupError::Config(format!(
                "диск {} не найден — посмотрите own-backup-cli list-disks",
                args.disk
            ))
        })?;

    // What to copy. Microsoft Reserved has no filesystem and no device, and the
    // engine keeps its GPT entry regardless: naming it is not an error and not
    // a source either, so say that instead of failing or dropping it in silence.
    let msr = own_backup_common::gpt::GPT_TYPE_MSR;
    let mut chosen = Vec::new();
    if args.partitions.is_empty() {
        chosen.extend(disk.partitions.iter().filter(|p| !p.volume_device.is_empty()));
    } else {
        for want in &args.partitions {
            let p = disk.partitions.iter().find(|p| p.index == *want).ok_or_else(|| {
                OwnBackupError::Config(format!("на диске {} нет раздела {want}", args.disk))
            })?;
            if p.volume_device.is_empty() {
                if p.gpt_type_guid.eq_ignore_ascii_case(msr) {
                    println!(
                        "  #{}: Microsoft Reserved — остаётся в разметке, копировать в нём нечего",
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
        args.disk,
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
    let kept: Vec<u32> = chosen.iter().map(|p| p.index).collect();
    let left_out: Vec<_> = disk
        .partitions
        .iter()
        .filter(|p| !kept.contains(&p.index) && !p.gpt_type_guid.eq_ignore_ascii_case(msr))
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
        let mut p =
            if args.exclude_defaults { own_backup_core::default_exclusions() } else { Vec::new() };
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
        disk_number: args.disk,
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
        bootable: args.bootable,
        block_size: args.block_size_mib * 1024 * 1024,
        image_format: args.format,
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
                "finalize" => "  завершение: сброс, отсоединение, удаление исключённых…".to_string(),
                _ => format!(
                    "  {:3}%  {} / {} блоков  ({:.1} МБ/с)",
                    (p.blocks_done * 100).checked_div(p.blocks_total).unwrap_or(0),
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
    println!("  Заняло:      {:.1} с", summary.duration.as_secs_f64());
    if args.verify {
        println!();
        super::copy_options::verify_after(&summary.image_path)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: BackupDiskArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Копия целого диска собирается только на Windows.".into(),
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
