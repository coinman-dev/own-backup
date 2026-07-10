use clap::{Args, Subcommand};

#[derive(Args)]
pub struct DriverArgs {
    #[command(subcommand)]
    pub action: DriverAction,
}

#[derive(Subcommand)]
pub enum DriverAction {
    /// Show mount prerequisites (Windows: AIM driver; Linux: NBD/NTFS)
    Status,
    /// Windows: install the AIM driver. Linux: load the kernel NBD module. (root/admin)
    Install,
    /// Windows: remove the AIM driver. Linux: nothing to remove. (root/admin)
    Uninstall,
}

#[cfg(target_os = "windows")]
pub async fn run(args: DriverArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::aim_install;

    match args.action {
        DriverAction::Status => {
            let st = aim_install::status();
            println!(
                "Arsenal Image Mounter driver: {}",
                if st.installed { "УСТАНОВЛЕН" } else { "не установлен" }
            );
            if let Some(v) = st.version {
                println!("Версия: {v}");
            }
        }
        DriverAction::Install => {
            println!("Внимание: Arsenal Image Mounter — отдельная программа (© Arsenal Recon),");
            println!("распространяется по СВОЕЙ лицензии. Пакет будет загружен с официального");
            println!("репозитория GitHub и установлен в систему для работы с qcow2-образами:");
            println!("  {}", aim_install::DRIVER_ZIP_URL);
            println!();
            println!("Скачиваю и устанавливаю…");
            tokio::task::spawn_blocking(aim_install::install)
                .await
                .map_err(|e| own_backup_common::OwnBackupError::Device(format!("install task panicked: {e}")))??;
            println!("Готово. Драйвер Arsenal Image Mounter установлен.");
        }
        DriverAction::Uninstall => {
            println!("Удаляю драйвер Arsenal Image Mounter…");
            tokio::task::spawn_blocking(aim_install::uninstall)
                .await
                .map_err(|e| own_backup_common::OwnBackupError::Device(format!("uninstall task panicked: {e}")))??;
            println!("Готово. Драйвер удалён.");
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
        println!();
        println!("Arsenal Image Mounter не нужен на Linux — это компонент Windows.");
    }

    match args.action {
        DriverAction::Status => print_status(),
        DriverAction::Install => {
            println!("Загружаю модуль ядра NBD (нужны права root)…");
            ensure_nbd_loaded()?;
            println!("Готово.\n");
            print_status();
        }
        DriverAction::Uninstall => {
            println!("На Linux нечего удалять: модуль NBD встроен в ядро.");
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: DriverArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'driver' command manages platform mount prerequisites (Windows AIM / Linux NBD).".into(),
    ))
}
