use clap::Args;

#[derive(Args)]
pub struct ConsolidateArgs {
    /// Folder holding the backup chains
    pub folder: String,

    /// Job id whose chains to consolidate (sidecars carrying another job's id
    /// are never touched)
    #[arg(long)]
    pub job: String,

    /// Keep at most this many increments behind each base
    #[arg(long, default_value_t = 3)]
    pub keep: u32,

    /// Say what would be folded, change nothing
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: ConsolidateArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::chains::{list_job_chains, plan_consolidation};
    use own_backup_backup::consolidate::{consolidate_step, resume_pending};
    use std::path::PathBuf;

    let folder = PathBuf::from(&args.folder);

    // Even a dry run reports interrupted steps, because a folder with a marker
    // in it is not describing a state anyone should plan against.
    if args.dry_run {
        println!("Dry run — nothing will be changed.\n");
    } else {
        let report = resume_pending(&folder)?;
        if !report.finished.is_empty() {
            println!("Finished {} interrupted consolidation step(s).\n", report.finished.len());
        }
        // A marker that could not be finished means the chain it names is in
        // an unknown state. Consolidating further would plan against that.
        if !report.all_finished() {
            for (marker, why) in &report.unfinished {
                eprintln!("НЕ ЗАВЕРШЕНО: {} — {why}", marker.display());
            }
            return Err(own_backup_common::OwnBackupError::ChainBroken(
                "прерванная консолидация не доведена до конца — цепочку нельзя ни \
                 использовать, ни удалять, пока это не решено"
                    .into(),
            )
            .into());
        }
    }

    let mut steps = 0u32;
    loop {
        let chains = list_job_chains(&folder, &args.job)?;
        if chains.is_empty() {
            println!("No chains for job {} in {}", args.job, folder.display());
            return Ok(());
        }

        let Some(step) = chains.iter().find_map(|c| plan_consolidation(c, args.keep)) else {
            break;
        };

        println!(
            "{} + {}  ->  fold into the base{}",
            step.base
                .image_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            step.victim
                .image_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            match &step.next {
                Some(next) => format!(
                    ", repoint {}",
                    next.image_path.file_name().unwrap_or_default().to_string_lossy()
                ),
                None => String::new(),
            }
        );

        if args.dry_run {
            // A dry run cannot walk the chain forward — the next step depends
            // on this one having happened — so it reports the first step only.
            println!("\n(dry run: only the next step is shown)");
            return Ok(());
        }

        let outcome = consolidate_step(&step)?;
        steps += 1;
        println!(
            "  done in {:.1}s{}",
            outcome.duration.as_secs_f64(),
            if outcome.bytes_orphaned > 0 {
                format!(
                    ", {:.1} MiB of the base is now dead space",
                    outcome.bytes_orphaned as f64 / (1024.0 * 1024.0)
                )
            } else {
                String::new()
            }
        );
    }

    match steps {
        0 => println!("Nothing to consolidate — every chain is within {} increments.", args.keep),
        n => println!("\n{n} increment(s) folded in."),
    }
    Ok(())
}
