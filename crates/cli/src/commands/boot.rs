//! The machine's boot configuration, as the firmware holds it.
//!
//! Stage E0 of `.ai/boot-entries-plan_2026-09-18.md`: read and show, nothing
//! more. The point of having it before anything can be changed is that the
//! answer to "why is this program not in my boot menu any more" is a list, and
//! the list has to be trustworthy before a single byte is written back.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct BootArgs {
    #[command(subcommand)]
    pub action: BootAction,
}

#[derive(Subcommand)]
pub enum BootAction {
    /// Показать записи загрузки UEFI и порядок, в котором прошивка их предлагает
    List(ListArgs),

    /// Сохранить записи загрузки в файл. Весит килобайты, снимается мгновенно
    Export(FileArgs),

    /// Вернуть записи из файла. По умолчанию только показывает, что изменится
    Import(ImportArgs),

    /// Проверить, принимает ли эта плата запись в память прошивки
    Probe,

    /// Переименовать запись
    Rename(RenameArgs),

    /// Показывать запись в меню
    Enable(OneArgs),

    /// Перестать показывать запись в меню (сама запись остаётся)
    Disable(OneArgs),

    /// Переставить запись в меню
    Move(MoveArgs),

    /// Удалить запись из прошивки
    Delete(OneArgs),

    /// Создать запись на загрузчик, который уже лежит на разделе
    Create(CreateArgs),

    /// Загрузиться с этой записи ОДИН следующий раз
    Next(NextArgs),

    /// Сколько секунд показывать меню
    Timeout(TimeoutArgs),

    /// Что лежит на разделах EFI и на что из этого никто не ссылается
    Loaders(LoadersArgs),
}

#[derive(Args)]
pub struct LoadersArgs {
    /// Выдать JSON вместо таблицы
    #[arg(long)]
    pub json: bool,

    /// Показать все файлы, а не только те, на которые нет записи
    #[arg(long)]
    pub all: bool,
}

#[derive(Args)]
pub struct OneArgs {
    /// Номер записи, как его печатает `boot list` (например 000F)
    pub entry: String,
    /// Согласиться тронуть ту запись, с которой машина загружена сейчас
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct RenameArgs {
    /// Номер записи, как его печатает `boot list`
    pub entry: String,
    /// Новое название — то, что будет видно в меню платы
    pub name: String,
}

#[derive(Args)]
pub struct MoveArgs {
    /// Номер записи
    pub entry: String,
    /// На какое место, считая с единицы
    #[arg(long, value_name = "МЕСТО")]
    pub to: Option<usize>,
    /// В начало меню
    #[arg(long)]
    pub first: bool,
    /// В конец меню
    #[arg(long)]
    pub last: bool,
}

#[derive(Args)]
pub struct CreateArgs {
    /// Название для меню
    #[arg(long)]
    pub name: String,
    /// Диск: в Windows — номер, как его печатает `list-disks`; в Linux —
    /// устройство целого диска, как его печатает `boot loaders` (/dev/nvme0n1)
    #[arg(long)]
    pub disk: String,
    /// Номер раздела в разметке
    #[arg(long)]
    pub partition: u32,
    /// Путь к загрузчику ВНУТРИ раздела, например \EFI\acronis\bootx64.efi
    #[arg(long)]
    pub file: String,
    /// На какое место в меню, считая с единицы. По умолчанию в конец
    #[arg(long, value_name = "МЕСТО")]
    pub position: Option<usize>,
}

#[derive(Args)]
pub struct NextArgs {
    /// Номер записи
    pub entry: Option<String>,
    /// Убрать однократную загрузку
    #[arg(long)]
    pub clear: bool,
}

#[derive(Args)]
pub struct TimeoutArgs {
    /// Секунды
    pub secs: u16,
}

#[derive(Args)]
pub struct FileArgs {
    /// Куда записать копию
    pub file: String,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Откуда брать
    pub file: String,

    /// Действительно записать. Без этого команда только показывает разницу
    #[arg(long)]
    pub apply: bool,

    /// Не снимать копию текущего состояния перед записью. Нужен, только если
    /// писать её некуда — и это плохая мысль
    #[arg(long)]
    pub no_safety_copy: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Выдать JSON вместо таблицы
    #[arg(long)]
    pub json: bool,

    /// Показать сырые байты каждой записи (для разбора и отчётов об ошибках)
    #[arg(long)]
    pub raw: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: BootArgs) -> own_backup_common::Result<()> {
    match args.action {
        BootAction::List(a) => list(a),
        BootAction::Export(a) => export(a),
        BootAction::Import(a) => import(a),
        BootAction::Probe => probe(),
        BootAction::Rename(a) => rename(a),
        BootAction::Enable(a) => set_shown(a, true),
        BootAction::Disable(a) => set_shown(a, false),
        BootAction::Move(a) => move_entry(a),
        BootAction::Delete(a) => delete(a),
        BootAction::Create(a) => create(a),
        BootAction::Next(a) => boot_next(a),
        BootAction::Timeout(a) => timeout(a),
        BootAction::Loaders(a) => loaders(a),
    }
}

#[cfg(target_os = "windows")]
fn loaders(args: LoadersArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::{efivars_win, esp_scan_win};

    let cfg = efivars_win::read_config()?;
    let esps = esp_scan_win::scan(&cfg.entries)?;
    let views: Vec<EspView> = esps
        .iter()
        .map(|e| EspView {
            disk: serde_json::json!(e.disk),
            partition: e.partition,
            guid: e.partition_guid.clone(),
            size_bytes: e.size_bytes,
            unreadable: e.unreadable.clone(),
            loaders: e
                .loaders
                .iter()
                .map(|l| LoaderView {
                    path: l.path.clone(),
                    size: l.size,
                    kind: l.kind,
                    fallback: l.fallback,
                    role: l.role,
                    named_by: l.named_by,
                })
                .collect(),
        })
        .collect();
    print_loaders(&views, &args)
}

/// One EFI System Partition as both systems print it. `disk` is a number on
/// Windows and a device path on Linux.
#[cfg(any(target_os = "windows", target_os = "linux"))]
struct EspView {
    disk: serde_json::Value,
    partition: u32,
    guid: String,
    size_bytes: u64,
    unreadable: Option<String>,
    loaders: Vec<LoaderView>,
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
struct LoaderView {
    path: String,
    size: u64,
    kind: &'static str,
    fallback: bool,
    role: own_backup_backup::esp_scan::Role,
    named_by: Option<u16>,
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn print_loaders(esps: &[EspView], args: &LoadersArgs) -> own_backup_common::Result<()> {
    let disk_text = |d: &serde_json::Value| match d {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if args.json {
        let out: Vec<_> = esps
            .iter()
            .map(|e| {
                serde_json::json!({
                    "disk": e.disk,
                    "partition": e.partition,
                    "guid": e.guid,
                    "size_bytes": e.size_bytes,
                    "unreadable": e.unreadable,
                    "loaders": e.loaders.iter().map(|l| serde_json::json!({
                        "path": l.path,
                        "size": l.size,
                        "kind": l.kind,
                        "fallback": l.fallback,
                        "named_by": l.named_by.map(|n| format!("{n:04X}")),
                        "second_stage": l.role == own_backup_backup::esp_scan::Role::SecondStage,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "esps": out }))?
        );
        return Ok(());
    }

    let mut orphans = 0usize;
    for esp in esps {
        println!(
            "Раздел EFI: диск {} раздел {} · {:.0} МиБ · {}",
            disk_text(&esp.disk),
            esp.partition,
            esp.size_bytes as f64 / 1048576.0,
            esp.guid
        );
        if let Some(why) = &esp.unreadable {
            println!("  {why}");
            println!();
            continue;
        }
        if esp.loaders.is_empty() {
            println!("  загрузчиков нет");
            println!();
            continue;
        }
        for l in &esp.loaders {
            // Показываем либо всё, либо только то, ради чего это писалось: файл,
            // который запускает прошивка, а записи на него нет. Вторые ступени
            // сюда не входят — их запускает другой загрузчик, и своей записи у
            // них не бывает.
            let interesting = l.named_by.is_none()
                && !l.fallback
                && l.role == own_backup_backup::esp_scan::Role::EntryPoint;
            if interesting {
                orphans += 1;
            }
            if !args.all && !interesting {
                continue;
            }
            let state = match (l.named_by, l.fallback, l.role) {
                (Some(n), _, _) => format!("запись {n:04X}"),
                (None, true, _) => "запасной путь съёмного носителя".into(),
                (None, _, own_backup_backup::esp_scan::Role::SecondStage) => {
                    "запускает другой загрузчик".into()
                }
                (None, false, _) => "ЗАПИСИ НЕТ".into(),
            };
            println!(
                "  {:<40} {:>8} КиБ  {:<32} {}",
                l.path,
                l.size / 1024,
                l.kind,
                state
            );
        }
        println!();
    }

    if orphans == 0 {
        println!(
            "Загрузчиков без записи нет: на всё, что лежит на разделах EFI, кто-то ссылается."
        );
    } else {
        println!(
            "Загрузчиков без записи: {orphans}. Файлы на месте, но ни одна запись на них не \
             указывает — в меню платы их нет."
        );
        println!("Добавить такой: own-backup-cli boot create --name НАЗВАНИЕ --disk ДИСК --partition M --file ПУТЬ");
        if !args.all {
            println!("Показать вообще все файлы: добавьте --all");
        }
    }
    Ok(())
}

/// `000F`, `f`, `0x000F` — all the same entry. The number is shown in hex, so
/// it is read in hex; a decimal reading would silently pick a different entry.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn parse_entry(s: &str) -> own_backup_common::Result<u16> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(t, 16).map_err(|_| {
        own_backup_common::OwnBackupError::Config(format!(
            "«{s}» не похоже на номер записи — ожидается шестнадцатеричный, как в `boot list` (например 000F)"
        ))
    })
}

/// Rule two of the plan: the entry this machine booted from is not deleted or
/// hidden by a single command. Not a prompt — a prompt inside a script is
/// answered by whoever wrote the script, and this is the one mistake that ends
/// with a machine that does not start.
#[cfg(target_os = "windows")]
fn guard_current(n: u16, force: bool, what: &str) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    if efivars_win::read_config()?.current != Some(n) {
        return Ok(());
    }
    if force {
        println!("Это запись, с которой машина загружена сейчас. Продолжаем — так было велено.");
        return Ok(());
    }
    Err(own_backup_common::OwnBackupError::Config(format!(
        "запись {n:04X} — та, с которой эта машина загружена сейчас. {what} её без другой рабочей \
         записи в меню означает машину, которая не стартует. Если это осознанно, повторите с --force"
    )))
}

/// Every write goes through here, so the copy is not something a caller can
/// forget.
#[cfg(target_os = "windows")]
fn with_safety_copy<T>(
    action: impl FnOnce() -> own_backup_common::Result<T>,
) -> own_backup_common::Result<T> {
    use own_backup_backup::efivars_win;
    match efivars_win::safety_copy() {
        Ok(c) => {
            println!("Прежнее состояние сохранено: {}", c.path.display());
            if !c.durable {
                println!(
                    "ВНИМАНИЕ: это временный диск носителя — копия не переживёт перезагрузку. \
                     Сохраните её сами, если она может понадобиться."
                );
            }
        }
        // Refusing to act because the copy could not be written is the right
        // way round: that copy is the only undo there is.
        Err(e) => {
            return Err(own_backup_common::OwnBackupError::Config(format!(
                "не удалось сохранить копию записей загрузки ({e}) — без неё менять их нельзя"
            )))
        }
    }
    action()
}

#[cfg(target_os = "windows")]
fn rename(args: RenameArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    let n = parse_entry(&args.entry)?;
    let before = efivars_win::read_entry(n)?
        .ok_or_else(|| own_backup_common::OwnBackupError::Config(format!("записи {n:04X} нет")))?;
    println!("{n:04X}: «{}» → «{}»", before.description, args.name);
    with_safety_copy(|| efivars_win::set_description(n, &args.name))?;
    println!("Готово.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn set_shown(args: OneArgs, shown: bool) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    let n = parse_entry(&args.entry)?;
    if !shown {
        guard_current(n, args.force, "Убрать из меню")?;
    }
    with_safety_copy(|| efivars_win::set_active(n, shown))?;
    println!(
        "{n:04X}: {}",
        if shown {
            "показывается в меню"
        } else {
            "убрана из меню"
        }
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn move_entry(args: MoveArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars::reorder;
    use own_backup_backup::efivars_win;
    let n = parse_entry(&args.entry)?;
    let cfg = efivars_win::read_config()?;
    let to = if args.first {
        0
    } else if args.last {
        cfg.order.len()
    } else if let Some(t) = args.to {
        t.saturating_sub(1)
    } else {
        return Err(own_backup_common::OwnBackupError::Config(
            "скажите куда: --first, --last или --to МЕСТО".into(),
        ));
    };
    let new_order = reorder(&cfg.order, n, to);
    println!(
        "Порядок: {} → {}",
        cfg.order
            .iter()
            .map(|x| format!("{x:04X}"))
            .collect::<Vec<_>>()
            .join(" "),
        new_order
            .iter()
            .map(|x| format!("{x:04X}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    with_safety_copy(|| efivars_win::set_order(&new_order))?;
    println!("Готово.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn delete(args: OneArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    let n = parse_entry(&args.entry)?;
    guard_current(n, args.force, "Удалить")?;
    let before = efivars_win::read_entry(n)?
        .ok_or_else(|| own_backup_common::OwnBackupError::Config(format!("записи {n:04X} нет")))?;
    println!("Удаляем {n:04X}: «{}»", before.description);
    if let Some(f) = before.file_path() {
        println!("  Сам загрузчик {f} остаётся на разделе — удаляется только запись о нём.");
    }
    with_safety_copy(|| efivars_win::delete_entry(n))?;
    println!("Готово.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn create(args: CreateArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars::HardDrive;
    use own_backup_backup::{efivars_win, gpt_reader};
    use own_backup_common::OwnBackupError;

    let disk: u32 = args.disk.trim().parse().map_err(|_| {
        OwnBackupError::Config(format!(
            "«{}» не похоже на номер диска — нужен номер, как его печатает `list-disks`",
            args.disk
        ))
    })?;
    let layout = gpt_reader::read_disk_layout(disk)?;
    let part = layout
        .partitions
        .iter()
        .find(|p| p.index == args.partition)
        .ok_or_else(|| {
            OwnBackupError::Config(format!(
                "на диске {} нет раздела {}",
                disk, args.partition
            ))
        })?;
    let hd = HardDrive::from_gpt(part.index, part.first_lba, part.last_lba, &part.part_guid)
        .ok_or_else(|| {
            OwnBackupError::Config(format!("не разобрать GUID раздела: {}", part.part_guid))
        })?;

    // Путь внутри раздела — с обратными косыми, как его пишет прошивка.
    let file = args.file.replace('/', "\\");
    let file = if file.starts_with('\\') {
        file
    } else {
        format!("\\{file}")
    };

    println!("Новая запись «{}»", args.name);
    println!(
        "  диск {} раздел {} · {}",
        disk, part.index, part.part_guid
    );
    println!("  загрузчик: {file}");
    println!("  есть ли он там на самом деле, эта команда не проверяет: раздел EFI Windows не показывает.");

    let n = with_safety_copy(|| {
        efivars_win::create_entry(
            &args.name,
            &hd,
            &file,
            args.position.map(|p| p.saturating_sub(1)),
        )
    })?;
    println!("Создана запись {n:04X}. Появится в меню платы после перезагрузки.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn boot_next(args: NextArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    if args.clear {
        with_safety_copy(|| efivars_win::set_boot_next(None))?;
        println!("Однократная загрузка убрана.");
        return Ok(());
    }
    let Some(e) = args.entry else {
        return Err(own_backup_common::OwnBackupError::Config(
            "скажите номер записи или --clear".into(),
        ));
    };
    let n = parse_entry(&e)?;
    with_safety_copy(|| efivars_win::set_boot_next(Some(n)))?;
    println!("Следующая загрузка — с записи {n:04X}, один раз. Дальше всё как обычно.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn timeout(args: TimeoutArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;
    with_safety_copy(|| efivars_win::set_timeout(args.secs))?;
    println!("Меню платы будет показываться {} с.", args.secs);
    Ok(())
}

/// Where the automatic copy goes before anything is written. Named by the
/// minute, so a second restore in the same session does not overwrite the state
/// the first one found.
#[cfg(target_os = "windows")]
fn safety_copy_path() -> std::path::PathBuf {
    let dir = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    std::path::Path::new(&dir)
        .join("OwnBackup")
        .join("boot")
        .join(format!(
            "boot-{}.json",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ))
}

#[cfg(target_os = "windows")]
fn export(args: FileArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;

    let backup = efivars_win::export()?;
    let path = std::path::PathBuf::from(&args.file);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&path, backup.to_json()?)?;
    println!("Копия записей загрузки: {}", path.display());
    println!("  машина:     {}", backup.machine);
    println!("  снято:      {}", backup.taken_at);
    println!("  переменных: {}", backup.variables.len());
    for (n, opt) in backup.entries() {
        println!(
            "    {n:04X}  {}{}",
            opt.description,
            opt.file_path()
                .map(|f| format!("  →  {f}"))
                .unwrap_or_default()
        );
    }
    println!();
    println!(
        "Записи загрузки живут в памяти материнской платы, а не на диске, и копия разделов их не спасает."
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn probe() -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win::{self, WriteProbe};

    if !efivars_win::is_uefi() {
        println!("Эта машина загружается через обычный BIOS — записей UEFI у неё нет.");
        return Ok(());
    }
    if !efivars_win::enable_privilege()? {
        println!("Нет привилегии SeSystemEnvironmentPrivilege — нужны права администратора.");
        return Ok(());
    }
    // Пишем Timeout тем же значением, что там уже стоит: если получится,
    // не изменилось ничего.
    match efivars_win::probe_write()? {
        WriteProbe::Works => println!("Прошивка принимает запись — править записи загрузки можно."),
        WriteProbe::Ignored => println!(
            "Прошивка приняла вызов, но значение не изменилось: эта плата запись молча \n             игнорирует. Читать записи можно, менять — нет."
        ),
        WriteProbe::Refused(why) => println!("Прошивка отказала в записи: {why}"),
        WriteProbe::NotTested(why) => println!("Проверить не на чем: {why}"),
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn import(args: ImportArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars::BootBackup;
    use own_backup_backup::efivars_win::{self, Change};

    let json = std::fs::read_to_string(&args.file)?;
    let backup = BootBackup::load(&json)?;

    let here = own_backup_common::media_mode::meaningful_machine_name().unwrap_or_default();
    println!(
        "Копия снята: {} на машине «{}»",
        backup.taken_at, backup.machine
    );
    if !here.is_empty() && !backup.machine.eq_ignore_ascii_case(&here) {
        // Запись указывает на раздел по GUID. С другой машины эти GUID чужие.
        println!(
            "ВНИМАНИЕ: копия с другой машины («{}», а здесь «{here}»). Записи указывают \n             на разделы по их GUID, и здесь таких разделов может не быть — пункты \n             появятся в меню и не заработают.",
            backup.machine
        );
    }

    let plan = efivars_win::plan_import(&backup)?;
    let differs = plan.iter().filter(|(_, c)| *c != Change::Same).count();
    println!();
    println!("Что сделает восстановление:");
    for (name, change) in &plan {
        let what = match change {
            Change::Same => "уже такое же, трогать не будем",
            Change::Differs => "БУДЕТ ПЕРЕЗАПИСАНО",
            Change::Absent => "будет создано заново",
        };
        println!("  {name:<12} {what}");
    }
    if differs == 0 {
        println!();
        println!("Разницы нет — восстанавливать нечего.");
        return Ok(());
    }

    if !args.apply {
        println!();
        println!("Это только показ. Чтобы записать, повторите с --apply.");
        return Ok(());
    }

    // Правило первое из плана: копия перед каждой записью, молча и всегда.
    if !args.no_safety_copy {
        let path = safety_copy_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, efivars_win::export()?.to_json()?)?;
        println!();
        println!("Прежнее состояние сохранено: {}", path.display());
    }

    let written = efivars_win::apply_import(&backup)?;
    println!("Записано переменных: {written}");
    println!("Проверьте меню платы при следующей загрузке.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn list(args: ListArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_win;

    let cfg = efivars_win::read_config()?;
    print_list(
        &ConfigView {
            entries: cfg
                .entries
                .iter()
                .map(|e| EntryView {
                    number: e.number,
                    option: &e.option,
                    order_index: e.order_index,
                    is_current: e.is_current,
                    is_next: e.is_next,
                })
                .collect(),
            order: &cfg.order,
            current: cfg.current,
            next: cfg.next,
            timeout: cfg.timeout,
            found_by: cfg.found_by.to_string(),
            dangling: &cfg.dangling,
        },
        &args,
    )
}

/// The firmware's boot configuration as both systems print it.
#[cfg(any(target_os = "windows", target_os = "linux"))]
struct ConfigView<'a> {
    entries: Vec<EntryView<'a>>,
    order: &'a [u16],
    current: Option<u16>,
    next: Option<u16>,
    timeout: Option<u16>,
    found_by: String,
    dangling: &'a [u16],
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
struct EntryView<'a> {
    number: u16,
    option: &'a own_backup_backup::efivars::LoadOption,
    order_index: Option<usize>,
    is_current: bool,
    is_next: bool,
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn print_list(cfg: &ConfigView, args: &ListArgs) -> own_backup_common::Result<()> {
    if args.json {
        let entries: Vec<_> = cfg
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "number": format!("{:04X}", e.number),
                    "name": e.option.description,
                    "in_menu": e.order_index.map(|i| i + 1),
                    "active": e.option.is_active(),
                    "hidden": e.option.is_hidden(),
                    "current": e.is_current,
                    "next": e.is_next,
                    "device_entry": e.option.file_path().is_none(),
                    "file": e.option.file_path(),
                    "partition": e.option.hard_drive().map(|h| serde_json::json!({
                        "number": h.partition_number,
                        "guid": h.partition_guid(),
                        "start_lba": h.start_lba,
                        "size_lba": h.size_lba,
                    })),
                    "editable": e.option.fully_understood(),
                    "raw": args.raw.then(|| hex(&e.option.to_bytes())),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "order": cfg.order.iter().map(|n| format!("{n:04X}")).collect::<Vec<_>>(),
                "current": cfg.current.map(|n| format!("{n:04X}")),
                "next": cfg.next.map(|n| format!("{n:04X}")),
                "timeout_secs": cfg.timeout,
                "found_by": cfg.found_by,
                "dangling": cfg.dangling.iter().map(|n| format!("{n:04X}")).collect::<Vec<_>>(),
                "entries": entries,
            }))?
        );
        return Ok(());
    }

    println!("Загрузка UEFI на этой машине");
    println!(
        "  Порядок в меню: {}",
        if cfg.order.is_empty() {
            "пусто".to_string()
        } else {
            cfg.order
                .iter()
                .map(|n| format!("{n:04X}"))
                .collect::<Vec<_>>()
                .join(" → ")
        }
    );
    println!(
        "  Загружены из: {}   Один раз: {}   Меню показывается: {}",
        cfg.current
            .map(|n| format!("{n:04X}"))
            .unwrap_or_else(|| "—".into()),
        cfg.next
            .map(|n| format!("{n:04X}"))
            .unwrap_or_else(|| "—".into()),
        cfg.timeout
            .map(|t| format!("{t} с"))
            .unwrap_or_else(|| "—".into()),
    );
    println!("  Записи найдены: {}", cfg.found_by);
    if !cfg.dangling.is_empty() {
        println!(
            "  В меню названы записи, которых нет: {} — прошивка предлагает то, чего загрузить не сможет",
            cfg.dangling.iter().map(|n| format!("{n:04X}")).collect::<Vec<_>>().join(", ")
        );
    }
    println!();

    if cfg.entries.is_empty() {
        println!(
            "Ни одной записи. Это не бывает на исправной машине — скорее всего, не хватило прав."
        );
        return Ok(());
    }

    println!("  №     Меню  Название                         Куда ведёт");
    println!(
        "  ----  ----  -------------------------------  --------------------------------------"
    );
    for e in &cfg.entries {
        let menu = match e.order_index {
            Some(i) => format!("{}", i + 1),
            None => "—".into(),
        };
        let target = match (e.option.file_path(), e.option.hard_drive()) {
            (Some(f), Some(h)) => format!("раздел {} · {f}", h.partition_number),
            (Some(f), None) => f,
            (None, _) => "устройство, а не файл".into(),
        };
        let mut marks = Vec::new();
        if e.is_current {
            marks.push("загружены отсюда");
        }
        if e.is_next {
            marks.push("следующая загрузка");
        }
        if !e.option.is_active() {
            marks.push("выключена");
        }
        if e.option.is_hidden() {
            marks.push("скрыта");
        }
        if !e.option.fully_understood() {
            marks.push("разобрана не полностью — править нельзя");
        }
        println!(
            "  {:04X}  {:>4}  {:<31}  {}",
            e.number,
            menu,
            trim(&e.option.description, 31),
            target
        );
        if !marks.is_empty() {
            println!("                      └─ {}", marks.join(", "));
        }
        if args.raw {
            println!("                      └─ {}", hex(&e.option.to_bytes()));
        }
    }

    let orphans = cfg
        .entries
        .iter()
        .filter(|e| e.order_index.is_none())
        .count();
    if orphans > 0 {
        println!();
        println!("Записей вне меню: {orphans}. Они существуют, но прошивка их не предлагает.");
    }
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- Linux: efivarfs ---------------------------------------------------------
//
// The same commands over `efivars_linux`. Every change goes through
// `Efivarfs::apply_action`, which takes the safety copy and guards the entry
// this machine booted from — the same door the service's boot page uses.
// Writing needs root; reading the entries does not.

#[cfg(target_os = "linux")]
pub async fn run(args: BootArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::efivars_linux::Action;
    match args.action {
        BootAction::List(a) => linux::list(a),
        BootAction::Export(a) => linux::export(a),
        BootAction::Import(a) => linux::import(a),
        BootAction::Probe => linux::probe(),
        BootAction::Loaders(a) => linux::loaders(a),
        BootAction::Rename(a) => linux::change(Action::Rename {
            entry: parse_entry(&a.entry)?,
            name: a.name,
        }),
        BootAction::Enable(a) => linux::change(Action::SetActive {
            entry: parse_entry(&a.entry)?,
            on: true,
            force: a.force,
        }),
        BootAction::Disable(a) => linux::change(Action::SetActive {
            entry: parse_entry(&a.entry)?,
            on: false,
            force: a.force,
        }),
        BootAction::Move(a) => linux::move_entry(a),
        BootAction::Delete(a) => linux::change(Action::Delete {
            entry: parse_entry(&a.entry)?,
            force: a.force,
        }),
        BootAction::Create(a) => linux::create(a),
        BootAction::Next(a) => linux::change(Action::BootNext {
            entry: if a.clear {
                None
            } else {
                Some(parse_entry(a.entry.as_deref().ok_or_else(|| {
                    own_backup_common::OwnBackupError::Config(
                        "скажите номер записи или --clear".into(),
                    )
                })?)?)
            },
        }),
        BootAction::Timeout(a) => linux::change(Action::Timeout { secs: a.secs }),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use own_backup_backup::efivars_linux::{safety_copy_dir, Action, Efivarfs};
    use own_backup_common::OwnBackupError;

    /// The service's data directory (its published one, when it runs with
    /// another `--data-dir`): the safety copies go where the boot page puts
    /// them.
    fn copy_dir() -> (std::path::PathBuf, bool) {
        let data = own_backup_common::service_data_dir::published()
            .unwrap_or_else(|| own_backup_common::config::AppConfig::default().data_dir);
        safety_copy_dir(&data)
    }

    fn not_uefi() -> own_backup_common::Result<()> {
        println!("Эта машина загружается через обычный BIOS — записей UEFI у неё нет.");
        Ok(())
    }

    pub fn list(args: ListArgs) -> own_backup_common::Result<()> {
        let efi = Efivarfs::default();
        if !efi.is_uefi() {
            return not_uefi();
        }
        let cfg = efi.read_config()?;
        print_list(
            &ConfigView {
                entries: cfg
                    .entries
                    .iter()
                    .map(|e| EntryView {
                        number: e.number,
                        option: &e.option,
                        order_index: e.order_index,
                        is_current: e.is_current,
                        is_next: e.is_next,
                    })
                    .collect(),
                order: &cfg.order,
                current: cfg.current,
                next: cfg.next,
                timeout: cfg.timeout,
                found_by: "efivarfs".into(),
                dangling: &cfg.dangling,
            },
            &args,
        )
    }

    pub fn loaders(args: LoadersArgs) -> own_backup_common::Result<()> {
        let efi = Efivarfs::default();
        if !efi.is_uefi() {
            return not_uefi();
        }
        let cfg = efi.read_config()?;
        let esps = own_backup_backup::esp_scan_linux::scan(&cfg.entries)?;
        let views: Vec<EspView> = esps
            .into_iter()
            .map(|e| EspView {
                disk: serde_json::json!(e.disk),
                partition: e.partition,
                guid: e.partition_guid,
                size_bytes: e.size_bytes,
                unreadable: e.unreadable,
                loaders: e
                    .loaders
                    .into_iter()
                    .map(|l| LoaderView {
                        path: l.found.path,
                        size: l.found.size,
                        kind: l.found.kind,
                        fallback: l.found.fallback,
                        role: l.found.role,
                        named_by: l.named_by,
                    })
                    .collect(),
            })
            .collect();
        print_loaders(&views, &args)
    }

    /// One change, with the safety copy and the guard of `apply_action`.
    pub fn change(action: Action) -> own_backup_common::Result<()> {
        let (dir, durable) = copy_dir();
        let done = Efivarfs::default().apply_action(action, &dir, durable)?;
        println!("Прежнее состояние сохранено: {}", done.safety_copy.path.display());
        if !done.safety_copy.durable {
            println!(
                "ВНИМАНИЕ: это временный диск носителя — копия не переживёт перезагрузку. \
                 Сохраните её сами, если она может понадобиться."
            );
        }
        println!("{}", done.message);
        Ok(())
    }

    pub fn move_entry(args: MoveArgs) -> own_backup_common::Result<()> {
        let entry = parse_entry(&args.entry)?;
        let to = if args.first {
            0
        } else if args.last {
            Efivarfs::default().read_config()?.order.len()
        } else if let Some(t) = args.to {
            t.saturating_sub(1)
        } else {
            return Err(OwnBackupError::Config(
                "скажите куда: --first, --last или --to МЕСТО".into(),
            ));
        };
        change(Action::Move { entry, to })
    }

    pub fn create(args: CreateArgs) -> own_backup_common::Result<()> {
        use own_backup_backup::efivars::HardDrive;
        let disk = args.disk.trim();
        let name = disk.strip_prefix("/dev/").unwrap_or_default();
        let sys = std::path::Path::new("/sys/class/block");
        if name.is_empty()
            || name.contains('/')
            || !sys.join(name).exists()
            || own_backup_common::linux_block::is_partition(sys, name)
        {
            return Err(OwnBackupError::Config(format!(
                "«{disk}» — не целый диск: нужно устройство диска, как его печатает \
                 `boot loaders` (например /dev/nvme0n1)"
            )));
        }
        let layout = own_backup_backup::esp_scan_linux::read_layout(disk)?;
        let part = layout
            .partitions
            .iter()
            .find(|p| p.index == args.partition)
            .ok_or_else(|| {
                OwnBackupError::Config(format!("на диске {disk} нет раздела {}", args.partition))
            })?;
        if !part
            .type_guid
            .eq_ignore_ascii_case(own_backup_common::linux_block::ESP_TYPE_GUID)
        {
            return Err(OwnBackupError::Config(format!(
                "раздел {} диска {disk} — не раздел EFI; прошивка с него не загрузит",
                args.partition
            )));
        }
        let hd = HardDrive::from_gpt(part.index, part.first_lba, part.last_lba, &part.part_guid)
            .ok_or_else(|| {
                OwnBackupError::Config(format!("не разобрать GUID раздела: {}", part.part_guid))
            })?;
        println!("Новая запись «{}»", args.name);
        println!("  диск {disk} раздел {} · {}", part.index, part.part_guid);
        change(Action::Create {
            name: args.name,
            hd,
            file: args.file,
            position: args.position.map(|p| p.saturating_sub(1)),
        })
    }

    pub fn export(args: FileArgs) -> own_backup_common::Result<()> {
        let machine =
            own_backup_common::media_mode::meaningful_machine_name().unwrap_or_default();
        let backup = Efivarfs::default().export(&machine)?;
        let path = std::path::PathBuf::from(&args.file);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&path, backup.to_json()?)?;
        println!("Копия записей загрузки: {}", path.display());
        println!("  машина:     {}", backup.machine);
        println!("  снято:      {}", backup.taken_at);
        println!("  переменных: {}", backup.variables.len());
        for (n, opt) in backup.entries() {
            println!(
                "    {n:04X}  {}{}",
                opt.description,
                opt.file_path()
                    .map(|f| format!("  →  {f}"))
                    .unwrap_or_default()
            );
        }
        println!();
        println!(
            "Записи загрузки живут в памяти материнской платы, а не на диске, и копия разделов их не спасает."
        );
        Ok(())
    }

    pub fn import(args: ImportArgs) -> own_backup_common::Result<()> {
        use own_backup_backup::efivars::BootBackup;
        use own_backup_backup::efivars_linux::{partition_guids_here, Change};

        let efi = Efivarfs::default();
        let backup = BootBackup::load(&std::fs::read_to_string(&args.file)?)?;
        let here = own_backup_common::media_mode::meaningful_machine_name().unwrap_or_default();
        println!(
            "Копия снята: {} на машине «{}»",
            backup.taken_at, backup.machine
        );
        if !here.is_empty() && !backup.machine.eq_ignore_ascii_case(&here) {
            println!(
                "ВНИМАНИЕ: копия с другой машины («{}», а здесь «{here}»). Записи на разделы, \
                 которых здесь нет, восстановлены не будут.",
                backup.machine
            );
        }
        let plan = efi.plan_import(&backup)?;
        let differs = plan.iter().filter(|(_, c)| *c != Change::Same).count();
        println!();
        println!("Что сделает восстановление:");
        for (name, change) in &plan {
            let what = match change {
                Change::Same => "уже такое же, трогать не будем",
                Change::Differs => "БУДЕТ ПЕРЕЗАПИСАНО",
                Change::Absent => "будет создано заново",
            };
            println!("  {name:<12} {what}");
        }
        if differs == 0 {
            println!();
            println!("Разницы нет — восстанавливать нечего.");
            return Ok(());
        }
        if !args.apply {
            println!();
            println!("Это только показ. Чтобы записать, повторите с --apply.");
            return Ok(());
        }
        if !args.no_safety_copy {
            let (dir, durable) = copy_dir();
            let copy = efi.safety_copy(&dir, durable).map_err(|e| {
                OwnBackupError::Config(format!(
                    "не удалось сохранить копию записей загрузки ({e}) — без неё менять их нельзя"
                ))
            })?;
            println!();
            println!("Прежнее состояние сохранено: {}", copy.path.display());
        }
        let parts = partition_guids_here(std::path::Path::new("/sys/class/block"));
        let done = efi.apply_import(&backup, &parts, true)?;
        println!("Записано переменных: {}", done.written);
        if done.skipped_removable > 0 {
            println!(
                "Пропущено записей съёмных устройств: {}",
                done.skipped_removable
            );
        }
        for name in &done.skipped_orphaned {
            println!("Пропущена {name}: её раздела на этой машине нет");
        }
        if let Some(n) = done.kept_current {
            println!("Запись {n:04X}, с которой машина загружена сейчас, оставлена как есть.");
        }
        println!("Проверьте меню платы при следующей загрузке.");
        Ok(())
    }

    /// Writes `Timeout` back with the value it has: nothing changes when the
    /// firmware takes it.
    pub fn probe() -> own_backup_common::Result<()> {
        let efi = Efivarfs::default();
        if !efi.is_uefi() {
            return not_uefi();
        }
        let Some(secs) = efi.read_config()?.timeout else {
            println!("Проверить не на чем: у прошивки нет переменной Timeout.");
            return Ok(());
        };
        match efi.set_timeout(secs) {
            Ok(()) if efi.read_config()?.timeout == Some(secs) => {
                println!("Прошивка принимает запись — править записи загрузки можно.")
            }
            Ok(()) => println!(
                "Прошивка приняла запись, но значение изменилось — так быть не должно."
            ),
            Err(e) => println!("Прошивка отказала в записи: {e}"),
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: BootArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Записи загрузки UEFI читаются только на Windows и в Linux.".into(),
    ))
}
