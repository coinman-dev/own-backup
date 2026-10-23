use clap::Args;

#[derive(Args)]
pub struct RestoreDiskArgs {
    /// Disk-backup manifest (.obdm); its .vhdx partition images sit next to it
    pub manifest: String,

    /// Disk to OVERWRITE: on Windows its NUMBER (1 for \\.\PhysicalDrive1);
    /// on Linux its device, preferably by its stable name
    /// (/dev/disk/by-id/…)
    #[arg(short, long)]
    pub target_disk: String,

    /// Linux: what to do when the copy's identities (filesystem, LUKS, LVM,
    /// md UUIDs and labels) are already on this machine — `keep` them or
    /// `change` them on the restored disk. Asked only when there is a clash.
    #[arg(long, value_parser = ["keep", "change"])]
    pub identities: Option<String>,

    /// Linux, with `--identities change`: also rewrite the references to the
    /// changed identities inside the restored system (fstab, crypttab, GRUB)
    #[arg(long)]
    pub fix_references: bool,

    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,

    /// Skip per-block checksum verification while restoring
    #[arg(long)]
    pub no_verify: bool,

    /// Accepted for compatibility; live system and boot partitions always stay protected
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
    let target_disk: u32 = args.target_disk.trim().parse().map_err(|_| {
        OwnBackupError::Config(format!(
            "«{}» не похоже на номер диска — нужен номер, как его печатает `list-disks`",
            args.target_disk
        ))
    })?;
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
                if p.name.is_empty() {
                    "(no name)"
                } else {
                    &p.name
                },
                p.size_bytes(sector) as f64 / 1e9,
                img.filesystem
            );
        }
    }
    println!("→ TARGET: \\\\.\\PhysicalDrive{}", target_disk);
    println!(
        "!! ALL DATA ON PhysicalDrive{} WILL BE DESTROYED !!",
        target_disk
    );

    // A flag from a client cannot override protection of the running system.
    let _ = args.force;
    own_backup_restore::guard_target_disk(target_disk)?;

    if !args.yes {
        print!("Type YES (uppercase) to wipe and restore: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(OwnBackupError::Io)?;
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
        target_disk,
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

/// Linux: through the service, the same way the window restores a disk —
/// the guard of the running system, the identity check, udev held off the
/// target, polkit (none needed as root), the busy slot. The engine is not run
/// here: those steps live in the service, and a second copy of them would
/// drift.
#[cfg(target_os = "linux")]
pub async fn run(args: RestoreDiskArgs) -> own_backup_common::Result<()> {
    use own_backup_client::{identity_clashes_in, BackupClient};
    use own_backup_common::engine_dto::{IdentityChoice, IdentityDecision};
    use own_backup_common::OwnBackupError;
    use std::io::Write;

    let target = args.target_disk.trim().to_string();
    let _ = args.force;
    if !args.yes {
        println!("Восстановление {} → {}", args.manifest, target);
        println!("!! ВСЕ ДАННЫЕ НА {target} БУДУТ УНИЧТОЖЕНЫ !!");
        print!("Введите YES (заглавными), чтобы продолжить: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(OwnBackupError::Io)?;
        if answer.trim() != "YES" {
            println!("Отменено.");
            return Ok(());
        }
    }
    let decision = match args.identities.as_deref() {
        Some("change") => IdentityDecision::change(args.fix_references),
        Some("keep") => Some(IdentityChoice::Keep).into(),
        _ => None::<IdentityChoice>.into(),
    };
    let token = crate::commands::mount::resolve_token("");
    let mut client = BackupClient::connect("http://127.0.0.1:50051", token)
        .await
        .map_err(OwnBackupError::Config)?;
    let secret = crate::commands::archive_key::ask_for_image(&mut client, &args.manifest).await?;
    println!("Восстанавливаю…");
    match client
        .restore_device_with_status(&args.manifest, &target, decision, &secret)
        .await
    {
        Ok(reply) => {
            println!("Готово.");
            if let Some(notes) = reply.get("notes").and_then(|n| n.as_array()) {
                for n in notes.iter().filter_map(|n| n.as_str()) {
                    println!("  {n}");
                }
            }
            println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_default());
            Ok(())
        }
        Err(s) => {
            let clashes = identity_clashes_in(&s);
            if !clashes.is_empty() {
                println!("На этой машине уже есть идентификаторы из копии:");
                for c in &clashes {
                    println!("  {} {} — уже на {}", c.kind, c.value, c.already_on.join(", "));
                }
                println!(
                    "Повторите с --identities keep (оставить как есть) или --identities change \
                     [--fix-references] (сменить на восстановленном диске)."
                );
            }
            Err(OwnBackupError::Config(s.message().to_string()))
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: RestoreDiskArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Whole-disk (bootable) restore is available on Windows and Linux.".into(),
    ))
}
