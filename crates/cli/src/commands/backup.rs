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

    /// Passphrase for an encrypted (LUKS) source. When given and the source is
    /// a LUKS container, its decrypted data is backed up (used blocks only).
    /// Visible to every user in the process list: prefer --password-stdin or
    /// the prompt the command shows when the source needs a passphrase.
    #[arg(long, conflicts_with = "password_stdin")]
    pub password: Option<String>,

    /// Read the source's passphrase as one line from standard input (a pipe,
    /// a file) instead of the command line or a prompt.
    #[arg(long)]
    pub password_stdin: bool,

    /// Image container format: vhdx (Windows-native mounting, no compression)
    /// or qcow2 (zstd-compressed, cross-platform)
    #[arg(long, default_value = "vhdx")]
    pub format: own_backup_common::types::ImageFormat,

    /// Маска исключения; можно повторять. Обычно правила приходят из задания
    /// GUI — здесь они нужны, чтобы проверять исключения без мастера.
    #[arg(long, value_name = "МАСКА")]
    pub exclude: Vec<String>,

    /// Взять те же двенадцать правил, что мастер подставляет новому заданию
    /// (pagefile.sys, **\Temp\**, *.tmp и прочие).
    #[arg(long)]
    pub exclude_defaults: bool,

    /// full | incremental | differential. Ребёнок пишется поверх --parent или
    /// того, что найдёт --parent-auto. На Linux цепочки — только qcow2.
    #[arg(long, default_value = "full")]
    pub kind: String,

    /// Родительский образ ТОГО ЖЕ тома: предыдущая версия для инкрементальной
    /// копии, полная копия цепочки — для дифференциальной.
    #[arg(long, value_name = "ОБРАЗ")]
    pub parent: Option<String>,

    /// Найти родителя самому: новейшая цепочка образов рядом с --output из
    /// тех, что собраны не заданием, а из командной строки. Наследовать нечего
    /// — пишется полная копия, и об этом говорится.
    #[arg(long)]
    pub parent_auto: bool,

    /// Проверить готовый образ по его контрольным суммам — то же, что
    /// «проверять после создания» в задании.
    #[arg(long)]
    pub verify: bool,

    /// Linux: write an encrypted copy (`.obk`, --output must end in `.obk`).
    /// A full copy gets a new key: a password asked twice and a recovery key
    /// shown once and typed back. A child of an encrypted parent always
    /// continues with the parent's key (its password or recovery key is
    /// asked), with or without this flag.
    #[arg(long)]
    pub encrypt: bool,
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

    let image_path = std::path::PathBuf::from(&args.output);
    // Which version this is and what it is written over. Retention and
    // scheduling stay with the job model in the service; the copy itself is
    // all here — see [`super::copy_options`].
    let (kind, parent_image) = super::copy_options::resolve_parent(
        super::copy_options::backup_kind(&args.kind)?,
        args.parent.as_deref(),
        args.parent_auto,
        &image_path,
        args.format,
    )?;
    let request = VolumeBackupRequest {
        source: args.source.clone(),
        image_path: image_path.clone(),
        full_image_path: None,
        block_size: args.block_size_mib * 1024 * 1024,
        all_sectors: args.all_sectors,
        use_vss: !args.no_vss,
        kind,
        parent_image,
        exclusion_patterns: {
            let mut p = if args.exclude_defaults {
                own_backup_core::default_exclusions()
            } else {
                Vec::new()
            };
            p.extend(args.exclude.iter().cloned());
            p
        },
        // Windows applies no Linux rules to a volume (decision R6).
        exclusion_patterns_linux: Vec::new(),
        job_id: None,
        passphrase: args.password.clone().map(zeroize::Zeroizing::new),
        image_format: args.format,
        // The command line does not open encrypted archives yet (E5).
        encryption: None,
        // The settings' default — what a job that has not chosen its own gets.
        zstd_level: own_backup_backup::qcow2::zstd_level_or_default(
            own_backup_common::config::AppConfig::load_or_default().default_compression_level,
        ),
        // CLI has no live disk-layout snapshot to embed (that comes from the GUI).
        source_disk: None,
    };

    println!(
        "Backing up {} -> {} ({}, {kind:?})",
        args.source, args.output, args.format
    );
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
            let percent = (p.blocks_done * 100)
                .checked_div(p.blocks_total)
                .unwrap_or(0);
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
    .map_err(|e| {
        own_backup_common::OwnBackupError::Device(format!("backup task panicked: {e}"))
    })??;

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
        if summary.vss_used {
            "shadow copy"
        } else {
            "live volume (crash-consistent)"
        }
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
    if args.verify {
        println!();
        super::copy_options::verify_after(&summary.image_path)?;
    }
    println!(
        "\nMount it with: own-backup-cli mount \"{}\"",
        summary.image_path.display()
    );

    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn run(args: BackupArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_linux::{
        backup_volume, verify_image, VolumeBackupRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_common::OwnBackupError;

    // Chains are qcow2 here: a VHDX child made on Linux waits for its Windows
    // check (CHECKS 12). Refusing is the honest answer — quietly writing a full
    // copy where an increment was asked for is how a backup plan turns out to
    // be one file.
    let kind = super::copy_options::backup_kind(&args.kind)?;
    // An encrypted chain is `.obk` whatever --format says; its parent decides
    // that below.
    let parent_sealed = args.parent.as_deref().is_some_and(|p| {
        own_backup_backup::sidecar::detect_container_format(std::path::Path::new(p))
            .is_ok_and(|c| c == own_backup_backup::seal::obk::OBK_EXTENSION)
    });
    if kind != own_backup_common::types::BackupType::Full
        && args.format == own_backup_common::types::ImageFormat::Vhdx
        && !args.encrypt
        && !parent_sealed
    {
        return Err(OwnBackupError::Config(
            "цепочки VHDX (incremental/differential) на Linux пока не пишутся — \
             используйте --format qcow2"
                .into(),
        ));
    }
    let block_size = if args.block_size_mib == 0 {
        DEFAULT_BLOCK_SIZE
    } else {
        args.block_size_mib * 1024 * 1024
    };
    let image_path = std::path::PathBuf::from(&args.output);
    // The source's passphrase: given, piped, asked now (a container the engine
    // decrypts itself) or asked only if a snapshot of an opened mapping
    // turns out to need it — never required on the command line.
    let passphrase = std::sync::Arc::new(SourcePassphrase::resolve(&args)?);
    // Which version this is and what it is written over: --parent, or the
    // newest command-line chain beside the output (`copy_options`).
    let (kind, parent_image) = super::copy_options::resolve_parent_in(
        kind,
        args.parent.as_deref(),
        args.parent_auto,
        &image_path,
        if args.encrypt {
            own_backup_backup::seal::obk::OBK_EXTENSION
        } else {
            args.format.extension()
        },
    )?;
    // The keys of an encrypted copy, asked for before anything is held.
    let encryption = archive_keys(&args, parent_image.as_deref())?;
    // A point in time to read: an LVM or device-mapper snapshot (its change
    // area beside the output; an opened LUKS container may need --password
    // for that), a short partition held still, or the device itself when
    // nothing writes to it; a volume in use without one is refused.
    use own_backup_snapshot::SnapshotProvider;
    let out_dir = match image_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let provider =
        own_backup_snapshot::LinuxSnapshotProvider::with_cow_dir(out_dir).container_key({
            let passphrase = passphrase.clone();
            move |device: &std::path::Path| match passphrase.get_or_ask()? {
                Some(p) => own_backup_backup::engine_linux::luks_volume_key(device, &p).map(Some),
                None => Ok(None),
            }
        });
    let snapshot = provider.create_snapshot(&args.source).await?;
    let request = VolumeBackupRequest {
        // The device itself; the engine reads the snapshot of it.
        source: args.source.clone(),
        image_path: image_path.clone(),
        full_image_path: None,
        block_size,
        all_sectors: args.all_sectors,
        use_vss: true,
        kind,
        parent_image,
        // A mask typed here is meant for whatever the volume holds: it goes
        // to both families. The defaults are each family's own, as a new job
        // gets them.
        exclusion_patterns: {
            let mut p = if args.exclude_defaults {
                own_backup_core::default_exclusions()
            } else {
                Vec::new()
            };
            p.extend(args.exclude.iter().cloned());
            p
        },
        exclusion_patterns_linux: {
            let mut p = if args.exclude_defaults {
                own_backup_core::default_linux_exclusions()
            } else {
                Vec::new()
            };
            p.extend(args.exclude.iter().cloned());
            p
        },
        job_id: None,
        passphrase: passphrase.known(),
        image_format: if encryption.is_some() {
            own_backup_common::types::ImageFormat::Qcow2
        } else {
            args.format
        },
        encryption: encryption.clone(),
        // The settings' default — what a job that has not chosen its own gets.
        zstd_level: own_backup_backup::qcow2::zstd_level_or_default(
            own_backup_common::config::AppConfig::load_or_default().default_compression_level,
        ),
        // No live disk-layout snapshot to embed (that comes from the GUI).
        source_disk: None,
    };

    println!(
        "Backing up {} -> {} ({}, {kind:?})",
        args.source, args.output, args.format
    );
    // Ctrl+C, SIGTERM or a closed terminal while a partition is frozen or a
    // snapshot is held must not leave it that way until the next cleanup: the
    // copy stops at its next block and the snapshot is released below, as
    // after any other failure.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            eprintln!("stopping: the copy ends at its next block and the snapshot is released");
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    // The engine checks, right before it publishes the image, that the
    // snapshot held to the end of the copy; one that did not leaves no image.
    let copied = {
        let stop = stop.clone();
        let snapshot = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            backup_volume(&request, &snapshot, |_p| {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    Err(OwnBackupError::Cancelled)
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| OwnBackupError::Config(format!("backup task: {e}")))?
    };
    // Given back whatever came of the copy, before the image is read back.
    if let Err(e) = provider.release_snapshot(&snapshot).await {
        eprintln!("warning: snapshot release failed: {e}");
    }
    let summary = copied?;
    println!(
        "Done: {} of {} blocks used, {:.1} MiB copied in {:.1}s ({})",
        summary.used_blocks,
        summary.total_blocks,
        summary.bytes_copied as f64 / 1_048_576.0,
        summary.duration.as_secs_f64(),
        if summary.smart_mode {
            "used-block"
        } else {
            "sector-by-sector"
        },
    );
    if summary.excluded_files > 0 {
        println!(
            "Excluded: {} files, {:.1} MiB",
            summary.excluded_files,
            summary.excluded_bytes as f64 / 1_048_576.0
        );
    }
    if let Some(why) = &summary.file_scan_skipped {
        println!("Exclusions not applied: {why}");
    }
    if let Some(why) = &summary.excluded_entries_kept {
        println!("Excluded names kept: {why}");
    }
    if summary.backup_kind != kind {
        println!(
            "The parent could not be continued — a {:?} copy was written instead (see the log).",
            summary.backup_kind
        );
    } else if kind != own_backup_common::types::BackupType::Full {
        println!(
            "{:?}: {} blocks written, {} unchanged from the parent",
            kind, summary.blocks_copied, summary.blocks_unchanged
        );
    }
    let v = match &encryption {
        Some(keys) => own_backup_backup::engine_linux::verify_image_with_keys(
            &summary.image_path,
            Some(keys),
            |_, _| Ok(()),
        )?,
        None => verify_image(&summary.image_path, |_, _| Ok(()))?,
    };
    println!(
        "Verify: {}",
        if v.valid() {
            "OK".to_string()
        } else {
            format!("{} mismatched blocks", v.mismatched_blocks.len())
        }
    );
    Ok(())
}

/// The keys an encrypted copy is written with (`--encrypt`, or a parent that
/// is encrypted), or `None` for a plaintext copy.
#[cfg(target_os = "linux")]
fn archive_keys(
    args: &BackupArgs,
    parent: Option<&std::path::Path>,
) -> own_backup_common::Result<Option<own_backup_backup::seal::context::ArchiveKeys>> {
    use own_backup_backup::seal::obk::OBK_EXTENSION;
    use own_backup_common::OwnBackupError;
    let sealed_parent = match parent {
        Some(p) => own_backup_backup::sidecar::detect_container_format(p)? == OBK_EXTENSION,
        None => false,
    };
    if let (true, Some(p)) = (sealed_parent, parent) {
        println!("Родитель зашифрован: копия продолжает цепочку его ключом.");
        return crate::commands::archive_key::open_keys_for_image(p);
    }
    if !args.encrypt {
        return Ok(None);
    }
    if parent.is_some() {
        return Err(OwnBackupError::Config(
            "зашифрованная копия не может продолжить незашифрованную цепочку: \
             начните её с полной копии (--kind full --encrypt)"
                .into(),
        ));
    }
    // `.obk` is qcow2 underneath whatever --format says (it defaults to vhdx).
    new_archive_keys().map(Some)
}

/// A new key for a copy made from the command line: a password asked twice,
/// a recovery key shown once and typed back — the same two steps as turning
/// encryption on for a job (`archive-key`), with nothing stored anywhere.
#[cfg(target_os = "linux")]
fn new_archive_keys() -> own_backup_common::Result<own_backup_backup::seal::context::ArchiveKeys> {
    use crate::secret_prompt::{ask_secret, ask_secret_twice};
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use own_backup_backup::seal::recovery::RecoveryKey;
    use own_backup_common::OwnBackupError;
    let err = |e: std::io::Error| OwnBackupError::Config(e.to_string());

    println!(
        "Копия будет зашифрована. Прочитать её можно будет только этим паролем \
         или ключом восстановления."
    );
    let password = zeroize::Zeroizing::new(
        ask_secret_twice("  Пароль архива (не отображается): ", "  Ещё раз: ").map_err(err)?,
    );
    let min = own_backup_core::archive_setup::MIN_PASSWORD_CHARS;
    if password.chars().count() < min {
        return Err(OwnBackupError::Config(format!(
            "пароль архива короче {min} символов"
        )));
    }
    let job_id = format!("cli-{}", uuid::Uuid::new_v4());
    println!("  Готовлю ключ (это несколько секунд)…");
    let pair = JobKeyPair::generate()?;
    let recovery = RecoveryKey::generate()?;
    let portable =
        SealedPrivateKey::seal(&job_id, &pair, &password, &recovery, &KdfParams::strong())?;
    let shown = recovery.to_string_grouped();
    println!();
    println!("  Ключ восстановления — запишите его сейчас:");
    println!();
    println!("      {}", *shown);
    println!();
    println!("  Он показывается ОДИН раз и нигде не сохраняется.");
    let echo = zeroize::Zeroizing::new(
        ask_secret("  Введите ключ восстановления для подтверждения: ").map_err(err)?,
    );
    let typed = RecoveryKey::parse(&echo)?;
    if typed.to_string_grouped() != shown {
        return Err(OwnBackupError::Config(
            "введённый ключ восстановления не совпадает с показанным — копия не сделана".into(),
        ));
    }
    ArchiveKeys::from_private(portable, pair.private)
}

/// The passphrase of an encrypted Linux source, obtained the least visible way
/// the invocation allows: `--password` (warned about), `--password-stdin`, a
/// prompt at once when the engine will decrypt the device itself, or a prompt
/// only when a device-mapper snapshot of an opened LUKS mapping asks for its
/// container key. Asked at most once.
#[cfg(target_os = "linux")]
struct SourcePassphrase {
    value: std::sync::Mutex<Option<zeroize::Zeroizing<String>>>,
    /// May still be asked for on the terminal.
    may_ask: bool,
}

#[cfg(target_os = "linux")]
impl SourcePassphrase {
    fn resolve(args: &BackupArgs) -> own_backup_common::Result<Self> {
        use own_backup_backup::engine_linux::{has_encryption_header, is_opened_luks_mapping};
        let err = |e: std::io::Error| own_backup_common::OwnBackupError::Config(e.to_string());
        let known = if let Some(p) = &args.password {
            eprintln!(
                "warning: --password is visible to every user in the process list; \
                 use --password-stdin or the prompt instead"
            );
            Some(zeroize::Zeroizing::new(p.clone()))
        } else if args.password_stdin {
            let mut line = zeroize::Zeroizing::new(String::new());
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).map_err(err)?;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                return Err(own_backup_common::OwnBackupError::Config(
                    "--password-stdin: standard input held no passphrase".into(),
                ));
            }
            Some(zeroize::Zeroizing::new(trimmed.to_string()))
        } else if has_encryption_header(&args.source) {
            Some(Self::ask(&args.source)?)
        } else {
            None
        };
        Ok(Self {
            may_ask: known.is_none() && is_opened_luks_mapping(&args.source),
            value: std::sync::Mutex::new(known),
        })
    }

    fn ask(source: &str) -> own_backup_common::Result<zeroize::Zeroizing<String>> {
        let typed = zeroize::Zeroizing::new(
            crate::secret_prompt::ask_secret(&format!(
                "Passphrase of the encrypted source {source} (not shown): "
            ))
            .map_err(|e| own_backup_common::OwnBackupError::Config(e.to_string()))?,
        );
        if typed.is_empty() {
            return Err(own_backup_common::OwnBackupError::Config(
                "no passphrase was typed".into(),
            ));
        }
        Ok(typed)
    }

    fn known(&self) -> Option<zeroize::Zeroizing<String>> {
        self.value.lock().unwrap().clone()
    }

    /// For the snapshot's container key: what is known, else one prompt.
    fn get_or_ask(&self) -> own_backup_common::Result<Option<zeroize::Zeroizing<String>>> {
        let mut value = self.value.lock().unwrap();
        if value.is_none() && self.may_ask {
            *value = Some(Self::ask("(opened LUKS mapping)")?);
        }
        Ok(value.clone())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: BackupArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'backup' command is only available on Windows and Linux.".into(),
    ))
}
