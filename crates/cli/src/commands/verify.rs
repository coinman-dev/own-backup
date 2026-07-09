use clap::Args;

#[derive(Args)]
pub struct VerifyArgs {
    /// Backup image (.vhdx/.qcow2) to verify (using embedded metadata or sidecar .obkm)
    pub image: String,
}

#[cfg(target_os = "windows")]
pub async fn run(args: VerifyArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::verify_image;
    use std::io::Write;

    println!("Verifying {}...", args.image);

    let path = std::path::PathBuf::from(&args.image);
    let summary = tokio::task::spawn_blocking(move || {
        let mut last_percent = u64::MAX;
        verify_image(&path, |done, total| {
            let percent = if total > 0 { done * 100 / total } else { 0 };
            if percent != last_percent {
                last_percent = percent;
                print!("\r  {percent:3}%  {done} / {total} blocks   ");
                let _ = std::io::stdout().flush();
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| own_backup_common::OwnBackupError::Device(format!("verify task panicked: {e}")))??;

    println!();
    if summary.valid() {
        println!(
            "Result: VALID ({} blocks checked in {:.1}s)",
            summary.blocks_checked,
            summary.duration.as_secs_f64()
        );
    } else {
        println!(
            "Result: INVALID — {} corrupted blocks out of {}: {:?}",
            summary.mismatched_blocks.len(),
            summary.blocks_checked,
            &summary.mismatched_blocks[..summary.mismatched_blocks.len().min(16)]
        );
        return Err(own_backup_common::OwnBackupError::ImageFormat(
            "image failed checksum verification".into(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: VerifyArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'verify' command currently requires Windows (VHDX attach).".into(),
    ))
}
