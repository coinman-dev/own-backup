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

    // Two shapes arrive here, and only one of them is JSON: the legacy `.obdm`
    // manifest with its images beside it, and a single whole-disk image that
    // carries the manifest inside. Reading the argument as JSON whatever it is
    // could never restore the second — it answered "bad .obdm manifest" for
    // every image, encrypted or not, after reading the whole file into memory
    // to say so.
    let image_path = std::path::PathBuf::from(&args.manifest);
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let whole_disk_image = matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2")
        || ext == own_backup_backup::seal::obk::OBK_EXTENSION;

    // The password is asked for HERE, before anything is shown or confirmed:
    // the summary below comes out of the sealed metadata, so for an encrypted
    // copy there is nothing to print until the key is open.
    let mut encryption = None;
    let manifest: DiskBackupManifest = if whole_disk_image {
        let path = image_path.clone();
        let (keys, sidecar) = tokio::task::spawn_blocking(move || {
            let keys = crate::commands::archive_key::open_keys_for_image(&path)?;
            let sidecar =
                own_backup_backup::engine_win::load_sidecar_with_keys(&path, keys.as_ref())?;
            Ok::<_, OwnBackupError>((keys, sidecar))
        })
        .await
        .map_err(|e| OwnBackupError::Device(format!("reading the image panicked: {e}")))??;
        encryption = keys;
        sidecar.disk_manifest.ok_or_else(|| {
            OwnBackupError::ImageFormat(
                "у образа нет встроенного описания диска — это копия тома, а не диска".into(),
            )
        })?
    } else {
        let raw = std::fs::read(&args.manifest).map_err(OwnBackupError::Io)?;
        serde_json::from_slice(&raw)
            .map_err(|e| OwnBackupError::ImageFormat(format!("bad .obdm manifest: {e}")))?
    };

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

    // Already open: asked for above, before the summary was printed and long
    // before the target disk is touched. Asking again here would be a second
    // prompt for the same secret — and for a `.obdm` it asked a file that is
    // not an image at all what container it is, and failed.
    let request = DiskRestoreRequest {
        manifest_path: image_path,
        target_disk: args.target_disk,
        verify_checksums: !args.no_verify,
        encryption,
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
