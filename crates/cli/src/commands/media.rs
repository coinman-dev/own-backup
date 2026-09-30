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
                let mut side = BackupClient::connect(args.endpoint.clone(), token)
                    .await
                    .ok();
                side.take().map(|mut c| {
                    tokio::spawn(async move {
                        use tokio_stream::StreamExt;
                        let Ok(stream) = c.stream_events().await else {
                            return;
                        };
                        let mut stream = Box::pin(stream);
                        // Печатать каждый процент — это триста строк «фиксация
                        // изменений» подряд, среди которых не видно ничего.
                        // Говорим, когда меняется ЭТАП или когда внутри этапа
                        // появляется новая подробность (имя файла, например).
                        let mut last_stage = usize::MAX;
                        let mut last_detail = String::new();
                        while let Some(ev) = stream.next().await {
                            let ClientEvent::Unknown { kind, json } = ev else {
                                continue;
                            };
                            if kind != "media" {
                                continue;
                            }
                            let Ok(p) = serde_json::from_str::<
                                own_backup_common::engine_dto::MediaProgressDto,
                            >(&json) else {
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
                    })
                })
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

/// Ошибка клиента приходит строкой — у службы она уже с текстом для человека.
fn cfg(e: String) -> OwnBackupError {
    OwnBackupError::Config(e)
}
