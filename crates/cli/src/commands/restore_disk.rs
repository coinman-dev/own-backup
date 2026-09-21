use clap::Args;

#[derive(Args)]
pub struct RestoreDiskArgs {
    /// Disk-backup manifest (.obdm); its .vhdx partition images sit next to it
    pub manifest: String,

    /// Target physical drive NUMBER to OVERWRITE (e.g. 1 for \\.\PhysicalDrive1)
    #[arg(short, long)]
    pub target_disk: u32,

    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,

    /// Skip per-block checksum verification while restoring
    #[arg(long)]
    pub no_verify: bool,

    /// Ignore the "don't restore the live system disk" guard (DANGEROUS)
    #[arg(long)]
    pub force: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: RestoreDiskArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::{restore_disk, DiskRestoreRequest};
    use own_backup_common::gpt::DiskBackupManifest;
    use own_backup_common::OwnBackupError;
    use std::io::Write;

    // Show what will be restored, straight from the manifest.
    let raw = std::fs::read(&args.manifest).map_err(OwnBackupError::Io)?;
    let manifest: DiskBackupManifest = serde_json::from_slice(&raw)
        .map_err(|e| OwnBackupError::ImageFormat(format!("bad .obdm manifest: {e}")))?;

    let sector = manifest.layout.sector_size;
    println!(
        "Disk backup: {} — {} partition(s), disk GUID {}",
        manifest.disk_model,
        manifest.images.len(),
        manifest.layout.disk_guid
    );
    for img in &manifest.images {
        if let Some(p) = manifest
            .layout
            .partitions
            .iter()
            .find(|p| p.index == img.part_index)
        {
            println!(
                "  #{:<2} {:<26} {:>10.1} GB  {}",
                p.index,
                if p.name.is_empty() { "(no name)" } else { &p.name },
                p.size_bytes(sector) as f64 / 1e9,
                img.filesystem
            );
        }
    }
    println!("→ TARGET: \\\\.\\PhysicalDrive{}", args.target_disk);
    println!(
        "!! ALL DATA ON PhysicalDrive{} WILL BE DESTROYED !!",
        args.target_disk
    );

    // Safety guard: never wipe the live system disk unless on boot media (or forced).
    if !args.force {
        own_backup_restore::guard_target_disk(args.target_disk)?;
    }

    if !args.yes {
        print!("Type YES (uppercase) to wipe and restore: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map_err(OwnBackupError::Io)?;
        if answer.trim() != "YES" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let request = DiskRestoreRequest {
        manifest_path: std::path::PathBuf::from(&args.manifest),
        target_disk: args.target_disk,
        verify_checksums: !args.no_verify,
        // The command line has no way to ask for a password yet (E5), so an
        // encrypted image refuses here rather than restoring nothing.
        encryption: None,
    };

    let summary = tokio::task::spawn_blocking(move || {
        let mut last = String::new();
        restore_disk(&request, |p| {
            let pct = if p.blocks_total > 0 {
                p.blocks_done * 100 / p.blocks_total
            } else {
                0
            };
            let line = if p.phase == "gpt" {
                "  writing partition table…".to_string()
            } else {
                format!(
                    "  [{}/{}] {:<28} {pct:3}%",
                    p.partition_index + 1,
                    p.partition_count,
                    p.part_label
                )
            };
            if line != last {
                last = line.clone();
                print!("\r{line}   ");
                let _ = std::io::stdout().flush();
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| OwnBackupError::Device(format!("restore task panicked: {e}")))??;

    println!();
    println!(
        "Whole-disk restore complete: {} partition(s), {:.2} GB in {:.1}s",
        summary.partitions,
        summary.bytes_written as f64 / 1e9,
        summary.duration.as_secs_f64()
    );
    println!(
        "If restoring to a fresh disk or new hardware, run `bcdboot` from WinPE to finalize boot."
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: RestoreDiskArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Whole-disk (bootable) restore is Windows-only for now.".into(),
    ))
}
