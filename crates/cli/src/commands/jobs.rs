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
    }
    Ok(())
}

/// Client string errors → the CLI's error type.
fn cfg(e: String) -> OwnBackupError {
    OwnBackupError::Config(e)
}
