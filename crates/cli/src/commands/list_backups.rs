use clap::Args;
use own_backup_common::config::AppConfig;

#[derive(Args)]
pub struct ListBackupsArgs {
    /// Filter by job ID
    #[arg(long)]
    pub job_id: Option<String>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: ListBackupsArgs) -> own_backup_common::Result<()> {
    let config = AppConfig::default();
    let conn = own_backup_metadata::open_database(&config.db_path())
        .map_err(|e| own_backup_common::OwnBackupError::Database(e.to_string()))?;

    let jobs = own_backup_metadata::list_jobs(&conn)
        .map_err(|e| own_backup_common::OwnBackupError::Database(e.to_string()))?;

    if jobs.is_empty() {
        println!("No backup jobs found.");
        return Ok(());
    }

    for job in &jobs {
        if let Some(ref filter_id) = args.job_id {
            if job.id.to_string() != *filter_id {
                continue;
            }
        }

        println!("Job: {} ({})", job.name, &job.id.to_string()[..8]);
        println!("  Source: {}", job.source_device);
        println!("  Target: {}", job.target_path);

        let sets = own_backup_metadata::list_sets_for_job(&conn, &job.id)
            .map_err(|e| own_backup_common::OwnBackupError::Database(e.to_string()))?;

        if sets.is_empty() {
            println!("  No backup sets.\n");
        } else {
            for set in &sets {
                println!(
                    "    {} | {:?} | {:?} | {}",
                    &set.id.to_string()[..8],
                    set.backup_type,
                    set.status,
                    set.started_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into()),
                );
            }
            println!();
        }
    }

    Ok(())
}
