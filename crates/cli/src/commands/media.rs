//! `media` — собрать загрузочный носитель из командной строки.
//!
//! Ровно то же, что делает мастер в интерфейсе, и тем же путём: сборка живёт в
//! службе, а это её клиент. Появилось потому, что без этой команды носитель
//! нельзя пересобрать ни из скрипта, ни в автоматической проверке — только
//! руками, кликами, и каждая правка запускателя или загрузочных файлов стоила
//! отдельного захода человека (2026-09-20, пять пересборок за вечер).
//!
//! Прогресс приходит потоком событий службы, тем же, что слушает интерфейс.

use clap::{Args, Subcommand};
use own_backup_client::{BackupClient, ClientEvent};
use own_backup_common::{OwnBackupError, Result};

#[derive(Args)]
pub struct MediaArgs {
    /// Адрес службы (в бою — именованный канал; это для отладочного режима).
    #[arg(long, default_value = "http://127.0.0.1:50051", global = true)]
    pub endpoint: String,

    /// Токен. Пусто — прочитать из файла токена службы.
    #[arg(long, default_value = "", global = true)]
    pub token: String,

    #[command(subcommand)]
    pub command: MediaCommand,
}

#[derive(Subcommand)]
pub enum MediaCommand {
    /// Собрать носитель и записать ISO
    Build(BuildArgs),
    /// Сторонние средства этой машины, которые можно положить на носитель
    Tools,
    /// Драйверы этой машины, которые предлагает сборщик
    Drivers,
    /// Linux: флешки, на которые можно записать носитель
    Sticks,
    /// Linux: стереть флешку и записать на неё ISO носителя
    WriteStick(WriteStickArgs),
    /// Linux: собрать носитель из установленной программы (Debian 13) в папку
    BuildLinux(BuildLinuxArgs),
    /// Linux: где собирается носитель и что осталось от прерванных сборок
    /// (и кэш старых версий); --clear удаляет остатки
    LinuxCache(LinuxCacheArgs),
    /// Остановить то, что служба делает сейчас: сборку носителя, запись
    /// флешки (как кнопка «Отмена» в окне)
    Cancel,
}

#[derive(Args)]
pub struct BuildLinuxArgs {
    /// Папка для ISO — ваша, существующая. Файл получает имя
    /// `OwnBackup-Linux-<версия>.iso` и не заменяет существующий.
    #[arg(long)]
    pub out: String,

    /// Сразу записать носитель на эту флешку (`/dev/sdb`); список —
    /// `media sticks`. Всё на ней стирается.
    #[arg(long)]
    pub stick: Option<String>,

    /// Разработка: этот .deb вместо установленного пакета. Только под
    /// sudo (его сценарии выполняются при сборке от root) или со службой,
    /// запущенной с --foreground
    #[arg(long)]
    pub deb: Option<String>,

    /// Средства на всякий случай: krusader, featherpad (через запятую;
    /// список — `media tools`). Ставятся из пакетов Debian при сборке
    #[arg(long, value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Не печатать ход сборки, только итог
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Args)]
pub struct LinuxCacheArgs {
    /// Удалить остатки прошлых сборок
    #[arg(long)]
    pub clear: bool,
}

#[derive(Args)]
pub struct WriteStickArgs {
    /// ISO носителя (`scripts/build-linux-media.sh`)
    #[arg(long)]
    pub iso: String,

    /// Флешка целиком (`/dev/sdb`); список — `media sticks`. Всё на ней стирается.
    #[arg(long)]
    pub device: String,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Куда записать ISO
    #[arg(long)]
    pub out: String,

    /// Базовый образ (`winre.wim`). Пусто — найти самому, как делает мастер.
    #[arg(long, default_value = "")]
    pub base: String,

    /// Опубликованное имя драйвера (можно повторять: `--driver a --driver b`)
    #[arg(long = "driver")]
    pub drivers: Vec<String>,

    /// Папка с дополнительными `.inf`
    #[arg(long, default_value = "")]
    pub extra_drivers: String,

    /// Идентификатор стороннего средства (можно повторять); список — `media tools`
    #[arg(long = "tool")]
    pub tools: Vec<String>,

    /// Оставить `cmd` оболочкой носителя вместо нашего рабочего стола —
    /// запасной путь, если стол где-то не поднимется
    #[arg(long)]
    pub classic_console: bool,

    /// Сжать среду выполнения внутрь `boot.wim`: файл меньше, памяти при
    /// загрузке больше
    #[arg(long)]
    pub runtime_in_image: bool,

    /// Не печатать ход сборки, только итог
    #[arg(long)]
    pub quiet: bool,
}

pub async fn run(args: MediaArgs) -> Result<()> {
    let token = if args.token.is_empty() {
        let cfg = own_backup_common::config::AppConfig::default();
        std::fs::read_to_string(cfg.token_path())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        args.token.clone()
    };
    let mut client = BackupClient::connect(args.endpoint.clone(), token.clone())
        .await
        .map_err(|e| {
            OwnBackupError::Config(format!(
                "{e}\nЗапущена ли служба? Для локального теста: \
                 own-backup-service --foreground"
            ))
        })?;

    match args.command {
        MediaCommand::Tools => {
            let tools = client.list_media_tools().await.map_err(cfg)?;
            if tools.is_empty() {
                println!("Ни одного знакомого средства на этой машине не нашлось.");
            }
            for t in &tools {
                println!(
                    "{:<24} {:<28} {:>6} МБ  {} файлов  {}",
                    t.id,
                    t.name,
                    t.size_bytes / (1024 * 1024),
                    t.files,
                    t.source
                );
            }
        }
        MediaCommand::Drivers => {
            let list = client.list_drivers().await.map_err(cfg)?;
            if list.is_empty() {
                println!("Сборщик не предложил ни одного драйвера.");
            }
            for d in &list {
                println!(
                    "{:<14} {:<16} {:<34} {}",
                    d.inf,
                    d.class_name,
                    d.original_name,
                    if d.recommended {
                        "рекомендован"
                    } else {
                        ""
                    }
                );
            }
        }
        MediaCommand::Sticks => {
            let sticks = client.list_media_sticks().await.map_err(cfg)?;
            if sticks.is_empty() {
                println!("Ни одной флешки, карты памяти или съёмного диска.");
            }
            for d in &sticks {
                println!(
                    "{:<14} {:>8} МиБ  {:<10} {}{}",
                    d.device,
                    d.size_bytes >> 20,
                    d.transport,
                    d.model,
                    if d.mounted.is_empty() {
                        String::new()
                    } else {
                        format!("  (подключена: {})", d.mounted.join(", "))
                    }
                );
            }
        }
        MediaCommand::WriteStick(w) => {
            // Которую флешку стирать — по списку службы: имя `/dev/sdX`
            // переходит к другому диску, если флешки переставили. Служба
            // сверяет модель, серийный номер, объём и номер устройства с тем,
            // что откроет для записи.
            let stick = find_stick(&mut client, &w.device).await?;
            let report = client
                .write_media_stick(&own_backup_common::engine_dto::MediaStickWriteDto {
                    iso_path: w.iso.clone(),
                    device: stick.device.clone(),
                    expect: Some(stick.identity()),
                })
                .await
                .map_err(cfg)?;
            println!(
                "Записано и проверено: {} МиБ ({}) на {}",
                report.iso_bytes >> 20,
                report.volume_id,
                report.device
            );
            match (&report.log_partition, &report.log_skipped) {
                (Some(p), _) => println!("Раздел для журналов: {p}"),
                (None, Some(why)) => println!("Раздела для журналов нет: {why}"),
                (None, None) => {}
            }
        }
        MediaCommand::BuildLinux(b) => {
            let out = std::fs::canonicalize(&b.out)
                .map(|p| p.display().to_string())
                .map_err(|e| cfg(format!("{}: {e}", b.out)))?;
            let deb = match &b.deb {
                Some(d) => std::fs::canonicalize(d)
                    .map(|p| p.display().to_string())
                    .map_err(|e| cfg(format!("{d}: {e}")))?,
                None => String::new(),
            };
            let mut options = own_backup_common::engine_dto::LinuxMediaBuildDto {
                output_dir: out.clone(),
                deb_path: deb,
                tools: b.tools.iter().map(|t| t.trim().to_lowercase()).collect(),
                ..Default::default()
            };
            if let Some(wanted) = &b.stick {
                // Та же сверка, что у write-stick: флешка — по списку службы.
                let stick = find_stick(&mut client, wanted).await?;
                options.stick_device = stick.device.clone();
                options.stick_expect = Some(stick.identity());
            }
            let watcher = if b.quiet {
                None
            } else {
                watch_progress(&args.endpoint, &token).await
            };
            println!(
                "Собираю носитель Linux в {out} … (около 1 ГБ загрузки и 15 минут; временная папка сборки удаляется после неё)"
            );
            let report = client.build_linux_media(&options).await;
            if let Some(h) = watcher {
                h.abort();
            }
            let report = report.map_err(cfg)?;
            println!();
            println!("Готово: {}", report.iso_path);
            println!("  размер:  {} МиБ", report.iso_bytes >> 20);
            println!(
                "  версия:  {}{}",
                report.version,
                if report.from_installed {
                    " (установленный пакет)"
                } else {
                    " (указанный .deb)"
                }
            );
            if !report.tools.is_empty() {
                println!("  средства: {}", report.tools.join(", "));
            }
            if let Some(stick) = &report.stick {
                println!(
                    "Записано и проверено: {} МиБ ({}) на {}",
                    stick.iso_bytes >> 20,
                    stick.volume_id,
                    stick.device
                );
                match (&stick.log_partition, &stick.log_skipped) {
                    (Some(p), _) => println!("Раздел для журналов: {p}"),
                    (None, Some(why)) => println!("Раздела для журналов нет: {why}"),
                    (None, None) => {}
                }
            }
            for w in &report.warnings {
                println!("  [!] {w}");
            }
        }
        MediaCommand::Cancel => {
            client.cancel_engine_op().await.map_err(cfg)?;
            println!("Остановка запрошена; идущая операция завершится с отменой.");
        }
        MediaCommand::LinuxCache(c) => {
            if c.clear {
                let r = client.clear_linux_media_cache().await.map_err(cfg)?;
                println!("Остатки удалены: {} МиБ", r.freed_bytes >> 20);
            } else {
                let s = client.linux_media_status().await.map_err(cfg)?;
                println!("Временная папка сборки — в {}", s.cache_dir);
                println!("Остатки прошлых сборок: {} МиБ", s.cache_bytes >> 20);
                println!(
                    "Свободно: {} МиБ, для сборки нужно: {} МиБ",
                    s.free_bytes >> 20,
                    s.need_bytes >> 20
                );
                if s.installed_version.is_empty() {
                    println!("Установленный пакет: нет");
                } else {
                    println!("Установленный пакет: {}", s.installed_version);
                }
                for p in &s.problems {
                    println!("  [!] {p}");
                }
            }
        }
        MediaCommand::Build(b) => {
            let options = own_backup_common::engine_dto::MediaBuildOptionsDto {
                base_wim: b.base.clone(),
                output_iso: b.out.clone(),
                drivers: b.drivers.clone(),
                extra_drivers: b.extra_drivers.clone(),
                classic_console: b.classic_console,
                // Чьи настройки у сторонних средств. Служба работает от имени
                // системы, и её `%APPDATA%` — профиль SYSTEM, где ничьих
                // настроек нет; поэтому путь берётся здесь, у того, кто
                // запустил команду. Так же поступает интерфейс.
                user_appdata: std::env::var("APPDATA").unwrap_or_default(),
                tools: b.tools.clone(),
                runtime_in_image: b.runtime_in_image,
            };

            // Ход сборки — тем же потоком событий, что слушает интерфейс.
            // Сборка идёт минутами, и молчащая консоль всё это время выглядит
            // как зависшая.
            let watcher = if b.quiet {
                None
            } else {
                watch_progress(&args.endpoint, &token).await
            };

            println!("Собираю носитель в {} …", b.out);
            let report = client.build_media(&options).await;
            if let Some(h) = watcher {
                h.abort();
            }
            let report = report.map_err(cfg)?;

            println!();
            println!("Готово: {}", report.iso_path);
            println!("  размер:  {} МБ", report.iso_bytes / (1024 * 1024));
            println!(
                "  основа:  {} (сборка {}, {})",
                report.base_name, report.base_build, report.base_language
            );
            println!(
                "  драйверов: {}   средств: {}",
                report.drivers_added, report.tools_added
            );
            if report.unpacked {
                println!(
                    "  образ пришлось распаковать и собрать заново (монтирование WIM недоступно)"
                );
            }
            for w in &report.warnings {
                println!("  [!] {w}");
            }
        }
    }
    Ok(())
}

/// Флешка по имени — из списка службы, с тем, что о ней будет сказано перед
/// стиранием. Имя `/dev/sdX` переходит к другому диску, если флешки
/// переставили; служба сверяет модель, серийный номер, объём и номер
/// устройства с тем, что откроет для записи.
async fn find_stick(
    client: &mut BackupClient,
    device: &str,
) -> Result<own_backup_common::engine_dto::MediaStickDto> {
    let sticks = client.list_media_sticks().await.map_err(cfg)?;
    let wanted = std::fs::canonicalize(device)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| device.to_string());
    let Some(stick) = sticks.into_iter().find(|s| s.device == wanted) else {
        return Err(cfg(format!(
            "{device} нет в списке флешек (media sticks): записывается только USB-диск, SD-карта или съёмный диск"
        )));
    };
    println!(
        "Стирается: {} — {} {} МиБ{}",
        stick.device,
        if stick.model.is_empty() {
            "?"
        } else {
            &stick.model
        },
        stick.size_bytes >> 20,
        if stick.serial.is_empty() {
            String::new()
        } else {
            format!(", серийный номер {}", stick.serial)
        }
    );
    Ok(stick)
}

/// Печатать ход сборки из потока событий службы (вид `media`). Печатать
/// каждый процент — это сотни строк подряд, среди которых не видно ничего;
/// говорим, когда меняется ЭТАП или когда внутри этапа появляется новая
/// подробность (имя пакета, например).
async fn watch_progress(endpoint: &str, token: &str) -> Option<tokio::task::JoinHandle<()>> {
    let mut c = BackupClient::connect(endpoint.to_string(), token.to_string())
        .await
        .ok()?;
    Some(tokio::spawn(async move {
        use tokio_stream::StreamExt;
        let Ok(stream) = c.stream_events().await else {
            return;
        };
        let mut stream = Box::pin(stream);
        let mut last_stage = usize::MAX;
        let mut last_detail = String::new();
        while let Some(ev) = stream.next().await {
            let ClientEvent::Unknown { kind, json } = ev else {
                continue;
            };
            if kind != "media" {
                continue;
            }
            let Ok(p) =
                serde_json::from_str::<own_backup_common::engine_dto::MediaProgressDto>(&json)
            else {
                continue;
            };
            let detail = p.detail.trim().to_string();
            if p.stage_index == last_stage && detail == last_detail {
                continue;
            }
            let name = p
                .stages
                .get(p.stage_index)
                .map(|s| s.title.clone())
                .unwrap_or_default();
            if p.stage_index != last_stage {
                println!("  {name}");
            }
            if !detail.is_empty() && detail != last_detail {
                println!("      {detail}");
            }
            last_stage = p.stage_index;
            last_detail = detail;
        }
    }))
}

/// Ошибка клиента приходит строкой — у службы она уже с текстом для человека.
fn cfg(e: String) -> OwnBackupError {
    OwnBackupError::Config(e)
}
