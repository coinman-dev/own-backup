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
    Install {
        /// Accept the third-party driver's terms without being asked (for
        /// scripts — the terms are printed either way).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Windows: remove the AIM driver. Linux: nothing to remove. (root/admin)
    Uninstall,
}

/// What is about to be installed, whose it is and on what terms — the same
/// facts the interface shows in its dialog, so a console install is not a
/// quieter one.
#[cfg(target_os = "windows")]
fn print_vendor_terms() {
    use own_backup_backup::aim_install::DRIVER_ZIP_URL;

    println!("Будет установлена ОТДЕЛЬНАЯ программа — не часть My Own Backup:");
    println!();
    println!("  Arsenal Image Mounter");
    println!("  © Arsenal Consulting, Inc. (d/b/a Arsenal Recon)");
    println!("  «mounts the contents of disk images as complete disks in Microsoft Windows»");
    println!();
    println!("Лицензия: GNU Affero General Public License v3 либо отдельная коммерческая");
    println!("лицензия Arsenal Recon. Без ключа драйвер работает в режиме Free Mode:");
    println!("«If Arsenal Image Mounter is run without a license, it will run in “Free Mode”");
    println!("and provide core functionality».");
    println!();
    println!("  https://arsenalrecon.com/weapons/image-mounter");
    println!("  https://github.com/ArsenalRecon/Arsenal-Image-Mounter");
    println!();
    println!("Пакет будет загружен напрямую от разработчика и установлен как драйвер режима");
    println!("ядра (SCSI-адаптер ROOT\\PhDskMnt). My Own Backup его не распространяет.");
    println!("  {DRIVER_ZIP_URL}");
    println!();
}

/// Ask, and treat anything that is not a clear yes as a no.
#[cfg(target_os = "windows")]
fn confirmed() -> bool {
    use std::io::Write;

    print!("Принять условия Arsenal Recon и установить драйвер? [y/N]: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false; // no console to ask on — `--yes` is the way then
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да")
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
        DriverAction::Install { yes } => {
            print_vendor_terms();
            if !yes && !confirmed() {
                println!("Отменено — ничего не установлено.");
                return Ok(());
            }
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
        // No third-party terms to accept here: the NBD module is part of the
        // running kernel, so `--yes` has nothing to agree to.
        DriverAction::Install { .. } => {
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
