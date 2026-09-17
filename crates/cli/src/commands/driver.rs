use clap::{Args, Subcommand};

#[derive(Args)]
pub struct DriverArgs {
    #[command(subcommand)]
    pub action: DriverAction,
}

#[derive(Subcommand)]
pub enum DriverAction {
    /// What mounting an image needs here (Windows: the iSCSI initiator;
    /// Linux: NBD and NTFS) and whether it is in place.
    Status,
    /// Put it in place: Windows starts the iSCSI initiator service, Linux
    /// loads the kernel NBD module. Needs administrator/root.
    Prepare,
}

/// Mounting a backup image needs no driver of ours on either platform.
///
/// Windows: an iSCSI target of ours plus the initiator Windows already has
/// (`MSiSCSI`). This replaced Arsenal Image Mounter, whose driver was
/// third-party, AGPL-derived and had to be installed before anything worked.
/// Linux: the in-tree NBD module.
#[cfg(target_os = "windows")]
pub async fn run(args: DriverArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::iscsi_win;

    fn print_status() {
        let s = iscsi_win::status();
        println!("Подключение образов (Windows) — через встроенный iSCSI-инициатор");
        println!("  Режим носителя (WinPE):  {}", if s.media_mode { "да" } else { "нет" });
        println!(
            "  Драйвер msiscsi.sys:     {}",
            if s.driver_file { "есть" } else { "НЕ НАЙДЕН" }
        );
        println!(
            "  Служба {}:           {}",
            iscsi_win::INITIATOR_SERVICE,
            match (s.service_present, s.disabled, s.running) {
                (false, _, _) => "НЕТ",
                (true, true, _) => "ОТКЛЮЧЕНА политикой",
                (true, false, true) => "работает",
                (true, false, false) => "остановлена (запустится по требованию)",
            }
        );
        println!();
        println!("{}", s.message);
        if s.ready {
            println!();
            println!("Проверить целиком:  own-backup-cli iscsi-serve <образ.qcow2>");
        }
    }

    match args.action {
        DriverAction::Status => print_status(),
        DriverAction::Prepare => {
            println!("Запускаю службу {}…", iscsi_win::INITIATOR_SERVICE);
            tokio::task::spawn_blocking(iscsi_win::ensure_initiator_service)
                .await
                .map_err(|e| {
                    own_backup_common::OwnBackupError::Device(format!("task panicked: {e}"))
                })??;
            println!("Готово.\n");
            print_status();
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn run(args: DriverArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_linux::{ensure_nbd_loaded, mount_support_status};

    // On Linux there is NO driver to install: mounting uses the in-tree kernel
    // NBD module (the service serves the blocks itself — no qemu), and NTFS
    // backups need the kernel `ntfs3` driver or the `ntfs-3g` package.
    fn print_status() {
        let s = mount_support_status();
        println!("Поддержка монтирования (Linux) — ядро {}", s.kernel);
        println!(
            "  Модуль NBD (/dev/nbd0): {}",
            if s.nbd_ok { "ДОСТУПЕН" } else { "не загружен — `sudo modprobe nbd`" }
        );
        match &s.ntfs_backend {
            Some(b) => println!("  NTFS: ДОСТУПЕН ({b})"),
            None => println!("  NTFS: нет — установите `ntfs-3g` (напр. `sudo apt install ntfs-3g`)"),
        }
    }

    match args.action {
        DriverAction::Status => print_status(),
        DriverAction::Prepare => {
            println!("Загружаю модуль ядра NBD (нужны права root)…");
            ensure_nbd_loaded()?;
            println!("Готово.\n");
            print_status();
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: DriverArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'driver' command reports what mounting an image needs on this platform.".into(),
    ))
}
