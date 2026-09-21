use clap::Args;

#[derive(Args)]
pub struct RestoreArgs {
    /// Backup image (.vhdx/.qcow2) to restore from (with embedded metadata or sidecar .obkm)
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
        // Asking for the password of an encrypted copy is E5; until then
        // such a restore fails saying exactly that.
        encryption: None,
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

#[cfg(target_os = "linux")]
pub async fn run(args: RestoreArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_linux::{restore_volume, LinuxRestoreRequest};
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

    let request = LinuxRestoreRequest {
        image_path: std::path::PathBuf::from(&args.input),
        target: args.target.clone(),
        verify_checksums: !args.no_verify,
        encryption: None,
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
                print!("\r  {percent:3}%  {} / {} blocks   ", p.blocks_done, p.blocks_total);
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

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: RestoreArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'restore' command is only available on Windows and Linux.".into(),
    ))
}
