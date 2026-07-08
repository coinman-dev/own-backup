use clap::{Args, Subcommand};

#[derive(Args)]
pub struct DriverArgs {
    #[command(subcommand)]
    pub action: DriverAction,
}

#[derive(Subcommand)]
pub enum DriverAction {
    /// Show whether the Arsenal Image Mounter driver is installed
    Status,
    /// Download the official AIM driver package and install it (needs admin)
    Install,
    /// Remove the Arsenal Image Mounter driver (needs admin)
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

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: DriverArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'driver' command manages the Windows Arsenal Image Mounter driver.".into(),
    ))
}
