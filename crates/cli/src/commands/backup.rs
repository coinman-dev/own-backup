use clap::Args;

#[derive(Args)]
pub struct BackupArgs {
    /// Source volume (e.g. C: or \\.\C:)
    pub source: String,

    /// Output image path (.vhdx)
    #[arg(short, long)]
    pub output: String,

    /// Copy every sector instead of only NTFS-used clusters
    #[arg(long)]
    pub all_sectors: bool,

    /// Backup block size in MiB (default 1)
    #[arg(long, default_value_t = 1)]
    pub block_size_mib: u32,

    /// Do not take a VSS shadow copy (crash-consistent copy of a live volume)
    #[arg(long)]
    pub no_vss: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: BackupArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::{backup_volume, VolumeBackupRequest};
    use std::io::Write;

    if args.block_size_mib == 0 || args.block_size_mib > 64 {
        return Err(own_backup_common::OwnBackupError::Config(
            "block size must be 1-64 MiB".into(),
        ));
    }

    let request = VolumeBackupRequest {
        source: args.source.clone(),
        image_path: std::path::PathBuf::from(&args.output),
        block_size: args.block_size_mib * 1024 * 1024,
        all_sectors: args.all_sectors,
        use_vss: !args.no_vss,
    };

    println!("Backing up {} -> {}", args.source, args.output);
    println!(
        "Mode: {}",
        if args.all_sectors {
            "sector-by-sector"
        } else {
            "used sectors only (NTFS bitmap)"
        }
    );

    // The engine is synchronous; run it on a blocking thread with progress
    // printed from the callback.
    let summary = tokio::task::spawn_blocking(move || {
        let mut last_percent = u64::MAX;
        backup_volume(&request, |p| {
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
    .map_err(|e| own_backup_common::OwnBackupError::Device(format!("backup task panicked: {e}")))??;

    println!();
    println!("Backup complete:");
    println!(
        "  Mode:        {}",
        if summary.smart_mode {
            "used sectors (NTFS bitmap)"
        } else {
            "sector-by-sector (requested or non-NTFS volume)"
        }
    );
    println!(
        "  VSS:         {}",
        if summary.vss_used { "shadow copy" } else { "live volume (crash-consistent)" }
    );
    println!("  Image:       {}", summary.image_path.display());
    println!("  Sidecar:     {}", summary.sidecar_path.display());
    println!(
        "  Source:      {:.2} GB ({} blocks, {} used)",
        summary.source_bytes as f64 / 1e9,
        summary.total_blocks,
        summary.used_blocks
    );
    println!(
        "  Copied:      {:.2} GB in {:.1}s",
        summary.bytes_copied as f64 / 1e9,
        summary.duration.as_secs_f64()
    );
    println!("\nMount it with: own-backup-cli mount \"{}\"", summary.image_path.display());

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: BackupArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'backup' command currently requires Windows (VHDX engine). \
         Linux support is planned (plan v2, phase 8)."
            .into(),
    ))
}
