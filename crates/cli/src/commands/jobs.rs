//! `jobs` subcommand — talk to the running service over gRPC (plan v2, phase 4).
//!
//! Unlike `backup`/`restore` (which drive the engine directly for low-level
//! testing), these commands are pure clients: they connect to the service and
//! operate on the shared job model. This is the CLI half of "thin client".

use clap::{Args, Subcommand};

use own_backup_client::{BackupClient, ClientEvent};
use own_backup_common::{OwnBackupError, Result};

#[derive(Args)]
pub struct JobsArgs {
    /// Service endpoint (TCP/foreground mode; UDS/named-pipe is a TODO).
    #[arg(long, default_value = "http://127.0.0.1:50051", global = true)]
    pub endpoint: String,

    /// Bearer token. When omitted, read from the service's token file
    /// (`<data_dir>/service.token` on Windows) so interactive use just works.
    #[arg(long, default_value = "", global = true)]
    pub token: String,

    #[command(subcommand)]
    pub command: JobsCommand,
}

#[derive(Subcommand)]
pub enum JobsCommand {
    /// List configured backup jobs
    List,
    /// Show the run queue (current + pending)
    Queue,
    /// Recent run history
    Activity {
        /// Max entries to show
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Enqueue a job for backup
    Run {
        /// Job id
        id: String,
    },
    /// Cancel the currently running backup
    Cancel,
    /// Service version / uptime
    Status,
    /// Follow the live event stream (Ctrl+C to stop)
    Watch,

    /// Печатать задание как JSON — чтобы поправить и вернуть через `save`
    Show {
        /// Job id
        id: String,
    },
    /// Заготовка задания (JSON) с теми значениями по умолчанию, что подставляет
    /// служба новому заданию в интерфейсе
    Template,
    /// Создать или изменить задание целиком из JSON (файл, либо `-` — ввод)
    Save {
        #[arg(long, value_name = "ФАЙЛ")]
        file: String,
    },
    /// Создать задание флагами, без JSON
    New(NewJobArgs),
    /// Изменить политику существующего задания — те же флаги, что у `new`
    Set {
        /// Job id
        id: String,
        #[command(flatten)]
        policy: JobPolicyArgs,
    },
    /// Удалить задание, а по требованию — и его копии
    Delete {
        /// Job id
        id: String,
        /// Удалить ещё и файлы копий этого задания
        #[arg(long)]
        delete_files: bool,
    },
}

#[derive(Args)]
pub struct NewJobArgs {
    /// Имя задания, как оно будет видно в интерфейсе
    #[arg(long)]
    pub name: String,

    /// Том-источник; флаг можно повторить — каждый том получит свою цепочку
    #[arg(long, value_name = "ТОМ", required = true)]
    pub source: Vec<String>,

    /// Папка, куда пишутся образы (имя каждого прогона строит служба)
    #[arg(long, value_name = "ПАПКА")]
    pub dest: String,

    #[command(flatten)]
    pub policy: JobPolicyArgs,
}

/// Политика задания — всё, что служба считает сама: цепочки, хранение,
/// проверка, расписание. Ровно эти поля правит вкладками мастер.
///
/// Каждое поле необязательное: не указанное остаётся как было (у `new` — как в
/// заготовке службы). Переключатели принимают и `--флаг`, и `--флаг=false`.
#[derive(Args)]
pub struct JobPolicyArgs {
    /// Контейнер образов: vhdx или qcow2. У задания он не меняется по живым
    /// архивам — задавайте до первого прогона.
    #[arg(long)]
    pub format: Option<own_backup_common::types::ImageFormat>,

    /// full | incremental | differential — какие версии делает задание
    #[arg(long)]
    pub method: Option<String>,

    /// Полная версия после каждых N версий (0 — не начинать новую цепочку)
    #[arg(long, value_name = "N")]
    pub full_every_n: Option<u32>,

    /// Только инкрементальные, без полной версии по счёту
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub only_incremental: Option<bool>,

    /// Скользящее окно: держать не больше N инкрементов за полной копией,
    /// складывая самый старый в неё. 0 — выключить.
    #[arg(long, value_name = "N")]
    pub rolling_increments: Option<u32>,

    /// Хранить не больше N цепочек (самые старые удаляются). 0 — без предела.
    #[arg(long, value_name = "N")]
    pub keep_chains: Option<u32>,

    /// Удалять цепочки старше N дней. 0 — не удалять по возрасту.
    #[arg(long, value_name = "ДНЕЙ")]
    pub cleanup_older_than_days: Option<u32>,

    /// Первую цепочку не удалять никогда
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub keep_first: Option<bool>,

    /// Проверять образ по контрольным суммам сразу после создания
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub validate_after: Option<bool>,

    /// Копировать все секторы, а не только занятые
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub sector_by_sector: Option<bool>,

    /// Маска исключения; флаг можно повторять. Заменяет весь список.
    #[arg(long, value_name = "МАСКА")]
    pub exclude: Vec<String>,

    /// Вернуть список исключений к тем правилам, что подставляет мастер
    #[arg(long)]
    pub default_exclusions: bool,

    /// Выключить исключения целиком
    #[arg(long)]
    pub no_exclusions: bool,

    /// none | daily | weekly | monthly | event | nonstop
    #[arg(long, value_name = "ВИД")]
    pub schedule: Option<String>,

    /// Время запуска, ЧЧ:ММ
    #[arg(long, value_name = "ЧЧ:ММ")]
    pub at: Option<String>,

    /// Дни недели для weekly: mon,tue,wed,thu,fri,sat,sun
    #[arg(long, value_delimiter = ',', value_name = "ДНИ")]
    pub days: Vec<String>,

    /// День месяца для monthly, 1–28
    #[arg(long, value_name = "N")]
    pub day_of_month: Option<u32>,

    /// Событие для schedule=event: startup, logon, logoff, shutdown,
    /// dest_connected (подключён диск, куда пишется копия)
    #[arg(long, value_name = "СОБЫТИЕ")]
    pub event: Option<String>,
}

pub async fn run(args: JobsArgs) -> Result<()> {
    // Use the explicit token, else read the service's per-boot token file.
    let token = if args.token.is_empty() {
        let cfg = own_backup_common::config::AppConfig::default();
        std::fs::read_to_string(cfg.token_path())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        args.token.clone()
    };
    let mut client = BackupClient::connect(args.endpoint.clone(), token)
        .await
        .map_err(|e| {
            OwnBackupError::Config(format!(
                "{e}\nЗапущена ли служба? Для локального теста: \
                 own-backup-service --foreground"
            ))
        })?;

    match args.command {
        JobsCommand::List => {
            let jobs = client.list_jobs().await.map_err(cfg)?;
            if jobs.is_empty() {
                println!("Заданий нет.");
            }
            for j in jobs {
                let last = j
                    .last_run
                    .as_ref()
                    .map(|r| format!("{} {}", r.status, r.finished_at))
                    .unwrap_or_else(|| "ещё не запускалось".into());
                println!("{:<38} {:<20} {} → {}", j.id, j.name, j.source, j.dest_folder);
                println!("    последний запуск: {last}");
            }
        }
        JobsCommand::Queue => {
            let q = client.get_queue().await.map_err(cfg)?;
            println!("Выполняется: {}", q.current.as_deref().unwrap_or("—"));
            println!("В очереди:    {}", if q.pending.is_empty() { "—".into() } else { q.pending.join(", ") });
        }
        JobsCommand::Activity { limit } => {
            let runs = client.list_activity().await.map_err(cfg)?;
            for r in runs.into_iter().take(limit) {
                println!(
                    "{:<10} {:<20} {:<12} {} байт  {}",
                    r.status, r.job_name, r.backup_kind, r.bytes_copied, r.finished_at
                );
                if !r.error.is_empty() {
                    println!("    ошибка: {}", r.error);
                }
            }
        }
        JobsCommand::Run { id } => {
            client.run_job(&id).await.map_err(cfg)?;
            println!("Задание {id} поставлено в очередь.");
        }
        JobsCommand::Cancel => {
            client.cancel_running().await.map_err(cfg)?;
            println!("Сигнал отмены отправлен.");
        }
        JobsCommand::Status => {
            println!("{}", client.service_status_json().await.map_err(cfg)?);
        }
        JobsCommand::Watch => {
            use tokio_stream::StreamExt;
            let mut stream = Box::pin(client.stream_events().await.map_err(cfg)?);
            println!("Слушаю события службы (Ctrl+C — выход)…");
            while let Some(ev) = stream.next().await {
                match ev {
                    ClientEvent::Progress(p) => println!(
                        "[прогресс] {} {} {}% ({}/{})",
                        p.job_id, p.phase, p.percent, p.blocks_done, p.blocks_total
                    ),
                    ClientEvent::Queue(q) => println!(
                        "[очередь] текущая={:?} ожидают={}",
                        q.current,
                        q.pending.len()
                    ),
                    ClientEvent::Finished(r) => {
                        println!("[готово] {} — {} ({})", r.job_name, r.status, r.error)
                    }
                    ClientEvent::Unknown { kind, .. } => println!("[?] {kind}"),
                }
            }
        }
        JobsCommand::Show { id } => {
            let job = client
                .get_job(&id)
                .await
                .map_err(cfg)?
                .ok_or_else(|| OwnBackupError::Config(format!("задания {id} нет")))?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        JobsCommand::Template => {
            let job = client.new_job_template().await.map_err(cfg)?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        JobsCommand::Save { file } => {
            let text = if file == "-" {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(&file)?
            };
            let job: own_backup_core::Job = serde_json::from_str(&text).map_err(|e| {
                OwnBackupError::Config(format!("{file}: это не описание задания — {e}"))
            })?;
            let saved = client.save_job(&job).await.map_err(cfg)?;
            println!("Сохранено: {} — {}", saved.id, saved.name);
        }
        JobsCommand::New(new) => {
            // The template is the service's own defaults -- the same ones the
            // wizard starts a new job with, so a job made here behaves like a
            // job made there.
            let mut job = client.new_job_template().await.map_err(cfg)?;
            job.name = new.name.clone();
            job.sources = new.source.clone();
            job.source_labels = new.source.clone();
            job.source = new.source.first().cloned().unwrap_or_default();
            job.source_label = job.source.clone();
            job.dest_folder = new.dest.clone();
            apply_policy(&mut job, &new.policy)?;
            let saved = client.save_job(&job).await.map_err(cfg)?;
            println!("Создано задание {} — {}", saved.id, saved.name);
            println!("Запустить: own-backup-cli jobs run {}", saved.id);
        }
        JobsCommand::Set { id, policy } => {
            let mut job = client
                .get_job(&id)
                .await
                .map_err(cfg)?
                .ok_or_else(|| OwnBackupError::Config(format!("задания {id} нет")))?;
            apply_policy(&mut job, &policy)?;
            let saved = client.save_job(&job).await.map_err(cfg)?;
            println!("Изменено: {} — {}", saved.id, saved.name);
        }
        JobsCommand::Delete { id, delete_files } => {
            let report = client.delete_job(&id, delete_files).await.map_err(cfg)?;
            println!(
                "Задание {id} удалено{}.",
                if delete_files { " вместе с копиями" } else { ", копии остались на месте" }
            );
            if delete_files {
                println!("Удалено файлов: {}", report.deleted.len());
                for (path, why) in &report.kept {
                    println!("  ОСТАЛОСЬ: {path} — {why}");
                }
                if report.history_truncated {
                    println!(
                        "  Журнал запусков переполнен, поэтому папки, которыми задание \
                         пользовалось раньше, могли остаться не просмотренными."
                    );
                }
            }
        }
    }
    Ok(())
}

/// Put the flags onto a job. Absent flags change nothing, which is what makes
/// `set` an edit and not a rewrite.
///
/// `0` is the off switch for the three limits that mean "no limit" as `None`
/// (window, chains kept, age) — a command line has no way to type `None`, and
/// "keep at most zero increments" is not a thing anyone wants.
fn apply_policy(job: &mut own_backup_core::Job, p: &JobPolicyArgs) -> Result<()> {
    if let Some(f) = p.format {
        job.image_format = f;
    }
    if let Some(m) = &p.method {
        let m = m.to_ascii_lowercase();
        if !["full", "incremental", "differential"].contains(&m.as_str()) {
            return Err(OwnBackupError::Config(format!(
                "неизвестный метод «{m}» — ожидается full, incremental или differential"
            )));
        }
        job.scheme.method = m;
    }
    if let Some(n) = p.full_every_n {
        job.scheme.full_every_n = n;
    }
    if let Some(v) = p.only_incremental {
        job.scheme.only_incremental = v;
    }
    if let Some(n) = p.rolling_increments {
        job.scheme.rolling_incrementals = (n > 0).then_some(n);
    }
    if let Some(n) = p.keep_chains {
        job.scheme.keep_chains = (n > 0).then_some(n);
    }
    if let Some(n) = p.cleanup_older_than_days {
        job.scheme.cleanup_older_than_days = (n > 0).then_some(n);
    }
    if let Some(v) = p.keep_first {
        job.scheme.keep_first = v;
    }
    if let Some(v) = p.validate_after {
        job.advanced.validate_after = v;
    }
    if let Some(v) = p.sector_by_sector {
        job.advanced.sector_by_sector = v;
    }
    if p.no_exclusions {
        job.exclusions.enabled = false;
    }
    if p.default_exclusions {
        job.exclusions.enabled = true;
        job.exclusions.patterns = own_backup_core::default_exclusions();
        job.exclusions.patterns_linux = own_backup_core::default_linux_exclusions();
    }
    if !p.exclude.is_empty() {
        job.exclusions.enabled = true;
        job.exclusions.patterns = p.exclude.clone();
    }
    if let Some(kind) = &p.schedule {
        let kind = kind.to_ascii_lowercase();
        const KINDS: [&str; 6] = ["none", "daily", "weekly", "monthly", "event", "nonstop"];
        if !KINDS.contains(&kind.as_str()) {
            return Err(OwnBackupError::Config(format!(
                "неизвестное расписание «{kind}» — ожидается одно из: {}",
                KINDS.join(", ")
            )));
        }
        job.schedule.kind = kind;
    }
    if let Some(at) = &p.at {
        let ok = at.len() == 5
            && at.as_bytes()[2] == b':'
            && at[..2].parse::<u32>().is_ok_and(|h| h < 24)
            && at[3..].parse::<u32>().is_ok_and(|m| m < 60);
        if !ok {
            return Err(OwnBackupError::Config(format!("время «{at}» — ожидается ЧЧ:ММ")));
        }
        job.schedule.time = at.clone();
    }
    if !p.days.is_empty() {
        const WEEK: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        let days: Vec<String> = p.days.iter().map(|d| d.trim().to_ascii_lowercase()).collect();
        if let Some(bad) = days.iter().find(|d| !WEEK.contains(&d.as_str())) {
            return Err(OwnBackupError::Config(format!(
                "неизвестный день «{bad}» — ожидаются: {}",
                WEEK.join(", ")
            )));
        }
        job.schedule.days = days;
    }
    if let Some(d) = p.day_of_month {
        // 1..=28 only, so the run exists in February too -- the service's own
        // rule, and the wizard offers nothing else.
        if !(1..=28).contains(&d) {
            return Err(OwnBackupError::Config(
                "день месяца — от 1 до 28, чтобы запуск случался в любом месяце".into(),
            ));
        }
        job.schedule.day_of_month = d;
    }
    if let Some(e) = &p.event {
        let e = e.to_ascii_lowercase();
        const EVENTS: [&str; 5] = ["dest_connected", "logon", "logoff", "shutdown", "startup"];
        if !EVENTS.contains(&e.as_str()) {
            return Err(OwnBackupError::Config(format!(
                "неизвестное событие «{e}» — ожидается одно из: {}",
                EVENTS.join(", ")
            )));
        }
        job.schedule.event_trigger = e;
    }
    Ok(())
}

/// Client string errors → the CLI's error type.
fn cfg(e: String) -> OwnBackupError {
    OwnBackupError::Config(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_core::Job;

    fn policy() -> JobPolicyArgs {
        JobPolicyArgs {
            format: None,
            method: None,
            full_every_n: None,
            only_incremental: None,
            rolling_increments: None,
            keep_chains: None,
            cleanup_older_than_days: None,
            keep_first: None,
            validate_after: None,
            sector_by_sector: None,
            exclude: Vec::new(),
            default_exclusions: false,
            no_exclusions: false,
            schedule: None,
            at: None,
            days: Vec::new(),
            day_of_month: None,
            event: None,
        }
    }

    /// An edit changes what was named and nothing else — that is the whole
    /// difference between `set` and writing the job again.
    #[test]
    fn what_was_not_named_stays_as_it_was() {
        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        job.scheme.full_every_n = 7;
        job.advanced.validate_after = true;
        let before = job.clone();

        apply_policy(&mut job, &policy()).unwrap();
        assert_eq!(job.scheme.method, before.scheme.method);
        assert_eq!(job.scheme.full_every_n, before.scheme.full_every_n);
        assert!(job.advanced.validate_after);
    }

    /// Zero is how a command line says "no limit": the three limits that are
    /// `None` inside become `None`, not zero.
    #[test]
    fn zero_turns_a_limit_off_rather_than_setting_it_to_nothing() {
        let mut job = Job::default();
        let mut p = policy();
        p.rolling_increments = Some(3);
        p.keep_chains = Some(5);
        p.cleanup_older_than_days = Some(30);
        apply_policy(&mut job, &p).unwrap();
        assert_eq!(job.scheme.rolling_incrementals, Some(3));
        assert_eq!(job.scheme.keep_chains, Some(5));
        assert_eq!(job.scheme.cleanup_older_than_days, Some(30));

        let mut off = policy();
        off.rolling_increments = Some(0);
        off.keep_chains = Some(0);
        off.cleanup_older_than_days = Some(0);
        apply_policy(&mut job, &off).unwrap();
        assert_eq!(job.scheme.rolling_incrementals, None);
        assert_eq!(job.scheme.keep_chains, None);
        assert_eq!(job.scheme.cleanup_older_than_days, None);
    }

    /// A word the service does not know would be stored and then silently
    /// ignored by the scheduler, so it is refused here instead.
    #[test]
    fn a_policy_the_service_cannot_read_is_refused() {
        let mut job = Job::default();
        for bad in [
            ("method", "weekly"),
            ("schedule", "hourly"),
            ("event", "coffee"),
        ] {
            let mut p = policy();
            match bad.0 {
                "method" => p.method = Some(bad.1.into()),
                "schedule" => p.schedule = Some(bad.1.into()),
                _ => p.event = Some(bad.1.into()),
            }
            assert!(apply_policy(&mut job, &p).is_err(), "{bad:?} must be refused");
        }
        let mut when = policy();
        when.at = Some("25:00".into());
        assert!(apply_policy(&mut job, &when).is_err(), "there is no 25th hour");
        let mut dom = policy();
        dom.day_of_month = Some(31);
        assert!(apply_policy(&mut job, &dom).is_err(), "the 31st does not exist in February");
        let mut day = policy();
        day.days = vec!["mon".into(), "funday".into()];
        assert!(apply_policy(&mut job, &day).is_err());
    }

    /// Masks given on the command line are the whole list, and giving any
    /// turns exclusions on — a list nobody applies is a trap.
    #[test]
    fn masks_replace_the_list_and_switch_it_on() {
        let mut job = Job::default();
        job.exclusions.enabled = false;
        let mut p = policy();
        p.exclude = vec!["*.iso".into(), "Downloads\\**".into()];
        apply_policy(&mut job, &p).unwrap();
        assert!(job.exclusions.enabled);
        assert_eq!(job.exclusions.patterns, vec!["*.iso", "Downloads\\**"]);

        let mut back = policy();
        back.default_exclusions = true;
        apply_policy(&mut job, &back).unwrap();
        assert_eq!(job.exclusions.patterns, own_backup_core::default_exclusions());
    }
}
