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

/// The first encrypted copy in the folder, or nothing when none is.
///
/// Its only job is to decide whether to ask for a password at all, and which
/// file to unseal the envelope from. Every image of a chain carries the same
/// portable key, so any of them will do.
fn first_encrypted_image(folder: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(folder).ok()?;
    let mut found: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref()
                == Some(own_backup_backup::seal::obk::OBK_EXTENSION)
        })
        .collect();
    // Same order every run, so the prompt names the same file twice in a row.
    found.sort();
    found.into_iter().next()
}

pub async fn run(args: ConsolidateArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::chains::{list_job_chains_strict, plan_consolidation};
    use own_backup_backup::consolidate::{consolidate_step_with_keys, resume_pending_with_keys};
    use std::path::PathBuf;

    let folder = PathBuf::from(&args.folder);

    // An encrypted chain cannot be folded without its key, and this command had
    // no way to be given one: it refused by name and stopped there. The key is
    // asked for from the first encrypted copy in the folder — one chain, one
    // key — before anything is read or written.
    let keys = match first_encrypted_image(&folder) {
        Some(image) => crate::commands::archive_key::open_keys_for_image(&image)?,
        None => None,
    };

    // Even a dry run reports interrupted steps, because a folder with a marker
    // in it is not describing a state anyone should plan against.
    if args.dry_run {
        println!("Dry run — nothing will be changed.\n");
    } else {
        let report = resume_pending_with_keys(&folder, keys.as_ref())?;
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
        let chains = list_job_chains_strict(&folder, &args.job)?;
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

        let outcome = consolidate_step_with_keys(&step, keys.as_ref())?;
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
