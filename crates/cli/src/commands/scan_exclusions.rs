//! Стенд для этапов B0/B1 плана `.ai/exclusions-before-copy-plan_2026-08-19.md`:
//! прогнать сканер исключений по одному тому и сказать, сколько он занял.
//!
//! Зачем отдельная команда. Прогон 2026-08-19 на носителе показал скан длиной
//! 15 мин 59 с (703 317 записей внутри готового образа на SMR-диске), и по
//! логу нельзя отличить цену МЕТОДА (обход дерева каталогами) от цены СРЕДЫ
//! (случайные чтения через разностный VHDX на SMR). Тот же обход по живому
//! тому на SSD отвечает на этот вопрос одним числом — и без него оптимизировать
//! нечего: урок пересчёта контрольных сумм (CLAUDE.md) стоил трёх неверных
//! гипотез ровно потому, что виноватым назначили код, не измерив диск.
//!
//! Ничего не пишет и не меняет: открывает том на чтение, обходит дерево,
//! печатает числа. Список найденных путей (`--list`) — эталон для сверки с
//! MFT-перечислением, которое придёт в B1/B2 (`scripts/ntfs-mft-probe.py`
//! печатает такой же).

use clap::Args;
use own_backup_common::Result;

#[derive(Args)]
pub struct ScanExclusionsArgs {
    /// Том: буква (`C:`), путь тома или устройство VSS-снимка.
    #[arg(long)]
    pub volume: String,

    /// Правила исключений. Без них берутся те же двенадцать, что подставляет
    /// мастер новому заданию.
    #[arg(long, value_name = "МАСКА")]
    pub pattern: Vec<String>,

    /// Размер кластера тома в байтах (для перевода экстентов в байты).
    #[arg(long, default_value = "4096")]
    pub cluster_size: u32,

    /// Выписать найденные пути сюда (по одному на строку) — для сверки.
    #[arg(long, value_name = "ФАЙЛ")]
    pub list: Option<String>,

    /// Чем искать: `walk` — обход дерева каталогами (как было), `mft` —
    /// чтение $MFT (как теперь), `both` — оба подряд со сверкой.
    #[arg(long, default_value = "both")]
    pub method: String,
}

#[cfg(target_os = "windows")]
pub async fn run(args: ScanExclusionsArgs) -> Result<()> {
    use std::time::Instant;

    let patterns = if args.pattern.is_empty() {
        own_backup_core::default_exclusions()
    } else {
        args.pattern.clone()
    };

    // Без SeBackupPrivilege обход спотыкается о защищённые каталоги и меряет
    // не то, что мерит движок.
    own_backup_backup::privileges_win::enable_backup_privileges();

    let volume = args.volume.trim_end_matches('\\').to_string();
    println!("Том:      {volume}");
    println!("Правил:   {}", patterns.len());
    println!("Кластер:  {} Б", args.cluster_size);
    println!("Метод:    {}", args.method);
    println!();

    // --- чтение $MFT -------------------------------------------------------
    // Тот же сканер, что работает над расшифрованным окном BitLocker: ни одного
    // открытия файла, последовательное чтение вместо обхода дерева.
    if args.method != "walk" {
        let t = Instant::now();
        let mut dev = own_backup_backup::win_device::WinRawDevice::open_read(&volume)?;
        let cur = own_backup_backup::ntfs_scan::RawVolumeCursor::new(&mut dev);
        let mut buffered = own_backup_backup::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
        let mut last = Instant::now();
        match own_backup_backup::ntfs_scan::scan_mft_with(&mut buffered, &patterns, |done, total| {
            if last.elapsed().as_secs() >= 2 {
                println!("  … {done} из {total} записей MFT, {:.0} с", t.elapsed().as_secs_f64());
                last = Instant::now();
            }
        }) {
            Ok(scan) => {
                let secs = t.elapsed().as_secs_f64();
                println!("=== чтение $MFT ===");
                println!("Записей MFT:      {}", scan.records_scanned);
                println!("Вычтено файлов:   {}", scan.files_excluded);
                println!(
                    "Вычтено байт:     {} ({:.0} МиБ)",
                    scan.bytes_excluded,
                    scan.bytes_excluded as f64 / (1024.0 * 1024.0)
                );
                println!("Оставлено в копии: {} (резидентные, reparse и пр.)", scan.kept.len());
                println!("Путей к удалению: {}", scan.paths.len());
                println!("ВРЕМЯ:            {secs:.1} с");
                if let Some(path) = &args.list {
                    use std::io::Write;
                    let mut f = std::fs::File::create(format!("{path}.mft"))?;
                    let mut by = scan.file_bytes.clone();
                    by.sort_by(|a, b| a.0.cmp(&b.0));
                    for (rel, bytes) in &by {
                        writeln!(f, "{bytes}\t{rel}")?;
                    }
                    println!("Байты по файлам:  {path}.mft ({} строк)", by.len());
                }
            }
            Err(e) => println!("=== чтение $MFT НЕ УДАЛОСЬ: {e} ==="),
        }
        println!();
        if args.method == "mft" {
            return Ok(());
        }
    }

    println!("=== обход дерева каталогами ===");
    let started = Instant::now();
    let mut last_report = Instant::now();
    let scan = own_backup_backup::exclusions_win::collect_excluded_ranges_verbose(
        &volume,
        args.cluster_size,
        &patterns,
        |entries| {
            // Раз в две секунды — чтобы на многоминутном проходе было видно,
            // что он жив, и с какой скоростью идёт.
            if last_report.elapsed().as_secs() >= 2 {
                let secs = started.elapsed().as_secs_f64();
                println!(
                    "  … {entries} записей, {secs:.0} с ({:.0} записей/с)",
                    entries as f64 / secs.max(0.001)
                );
                last_report = Instant::now();
            }
            Ok(())
        },
    )?;
    let elapsed = started.elapsed();

    let secs = elapsed.as_secs_f64();
    println!();
    println!("Записей обойдено: {}", scan.entries_scanned);
    println!("Совпало файлов:   {}", scan.files_excluded);
    println!(
        "Исключено байт:   {} ({:.0} МиБ)",
        scan.bytes_excluded,
        scan.bytes_excluded as f64 / (1024.0 * 1024.0)
    );
    println!("Путей к удалению: {}", scan.paths.len());
    println!(
        "ВРЕМЯ:            {secs:.1} с ({:.0} записей/с)",
        scan.entries_scanned as f64 / secs.max(0.001)
    );

    if let Some(path) = &args.list {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        let mut sorted: Vec<&own_backup_backup::exclusions_win::ExcludedPath> =
            scan.paths.iter().collect();
        sorted.sort_by(|a, b| a.rel.cmp(&b.rel));
        for p in sorted {
            writeln!(f, "{}\t{}", if p.is_dir { "d" } else { "f" }, p.rel)?;
        }
        println!("Список:           {path}");

        // Пофайловые байты — рядом, с суффиксом `.bytes`. Ими закрывается
        // единственное расхождение этапа B0: набор файлов у обхода и у чтения
        // MFT совпал, а сумма байт разошлась на треть, и пофайловая разница —
        // единственное, что скажет, кто из двоих неправ.
        let bytes_path = format!("{path}.bytes");
        let mut bf = std::fs::File::create(&bytes_path)?;
        let mut by = scan.file_bytes.clone();
        by.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, bytes) in &by {
            writeln!(bf, "{bytes}\t{rel}")?;
        }
        println!("Байты по файлам:  {bytes_path} ({} строк)", by.len());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: ScanExclusionsArgs) -> Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "scan-exclusions работает только на Windows".into(),
    ))
}
