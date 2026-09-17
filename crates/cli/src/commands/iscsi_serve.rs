use clap::Args;

#[derive(Args)]
pub struct IscsiServeArgs {
    /// qcow2 backup image to serve (its backing chain resolves automatically)
    pub image: String,

    /// Listen address. Loopback only — this hands out the contents of a backup.
    #[arg(long, default_value = "127.0.0.1:3260")]
    pub listen: String,

    /// Serve read-write. Writes land in a scratch file next to the image and
    /// never in the image itself.
    #[arg(long)]
    pub rw: bool,
}

/// Serve one backup image as an iSCSI target, by hand.
///
/// The service does this for itself when it mounts an image; this command is
/// how a person checks the same road without a backup job — and how the whole
/// path was accepted after it was written:
///
/// ```text
/// own-backup-cli iscsi-serve D:\backups\c.qcow2
/// # then, in another window (or Windows' own iSCSI Initiator applet):
/// iscsicli QAddTargetPortal 127.0.0.1 3260
/// ```
///
/// Blocks are decompressed on the fly and served from this process, so it has
/// to keep running while the disk is attached. Ctrl+C stops it — log the
/// session out first, or Windows is left with a disk whose target is gone.
pub async fn run(args: IscsiServeArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::block_source::BlockSource;
    use own_backup_backup::iscsi::{self, Lun};
    use own_backup_backup::overlay::WritableOverlaySource;
    use own_backup_backup::qcow2::Qcow2RandomReader;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let path = std::path::PathBuf::from(&args.image);
    let reader = Qcow2RandomReader::open(&path)?;
    let (virtual_size, cluster_size, chain) =
        (reader.virtual_size(), reader.cluster_size(), reader.chain_len());

    let serial = iscsi::serial_for_image(&path);
    let iqn = iscsi::iqn_for(&serial);

    let lun = if args.rw {
        let scratch = path.with_extension("iscsi-scratch");
        let _ = std::fs::remove_file(&scratch);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch)?;
        println!(
            "Записи идут в файл {} — сам образ не меняется.",
            scratch.display()
        );
        let base: Box<dyn BlockSource> = Box::new(reader);
        Lun::read_write(Box::new(Arc::new(Mutex::new(WritableOverlaySource::new(
            base,
            cluster_size,
            file,
        )))))
    } else {
        Lun::read_only(Box::new(reader))
    }
    .with_serial(&serial);

    println!(
        "Отдаю {} ({} МиБ, цепочка из {}) на {} — {}",
        args.image,
        virtual_size / (1024 * 1024),
        chain,
        args.listen,
        if args.rw { "чтение и запись" } else { "только чтение" }
    );
    println!("Имя цели: {iqn}");
    println!("Остановить — Ctrl+C (сначала отключите сессию в инициаторе).");

    let listener = std::net::TcpListener::bind(&args.listen).map_err(|e| {
        own_backup_common::OwnBackupError::Device(format!("cannot listen on {}: {e}", args.listen))
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let lun = Arc::new(Mutex::new(lun));
    let server =
        tokio::task::spawn_blocking(move || iscsi::serve_listener(listener, iqn, lun, stop_bg));

    let _ = tokio::signal::ctrl_c().await;
    println!("\nОстанавливаю цель…");
    stop.store(true, Ordering::SeqCst);
    server.await.map_err(|e| {
        own_backup_common::OwnBackupError::Device(format!("iSCSI task panicked: {e}"))
    })??;
    Ok(())
}
