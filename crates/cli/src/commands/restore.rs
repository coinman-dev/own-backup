use clap::Args;

#[derive(Args)]
pub struct RestoreArgs {
    /// Backup image (.vhdx) to restore from (sidecar .obkm must sit next to it)
    pub input: String,

    /// Target volume/device to OVERWRITE (e.g. D: or \\.\PhysicalDrive1)
    #[arg(short, long)]
    pub target: String,

    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,

    /// Skip per-block checksum verification while restoring
    #[arg(long)]
    pub no_verify: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: RestoreArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::{restore_volume, VolumeRestoreRequest};
    use std::io::Write;

    println!("Restoring {} -> {}", args.input, args.target);
    println!("!! ALL DATA ON {} WILL BE OVERWRITTEN !!", args.target);

    if !args.yes {
        print!("Type YES (uppercase) to continue: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(own_backup_common::OwnBackupError::Io)?;
        if answer.trim() != "YES" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let request = VolumeRestoreRequest {
        image_path: std::path::PathBuf::from(&args.input),
        target: args.target.clone(),
        verify_checksums: !args.no_verify,
    };

    let summary = tokio::task::spawn_blocking(move || {
        let mut last_percent = u64::MAX;
        restore_volume(&request, |p| {
            let percent = if p.blocks_total > 0 {
                p.blocks_done * 100 / p.blocks_total
            } else {
                0
            };
            if percent != last_percent {
                last_percent = percent;
                print!(
                    "\r  {:3}%  {} / {} blocks  ({:.1} MB/s)   ",
                    percent,
                    p.blocks_done,
                    p.blocks_total,
                    p.speed_bps as f64 / 1_000_000.0
                );
                let _ = std::io::stdout().flush();
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| own_backup_common::OwnBackupError::Device(format!("restore task panicked: {e}")))??;

    println!();
    println!(
        "Restore complete: {} blocks, {:.2} GB in {:.1}s",
        summary.blocks_restored,
        summary.bytes_restored as f64 / 1e9,
        summary.duration.as_secs_f64()
    );

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: RestoreArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'restore' command currently requires Windows (VHDX engine). \
         Linux support is planned (plan v2, phase 8)."
            .into(),
    ))
}
