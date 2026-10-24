use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Install the Windows Service
    Install,
    /// Uninstall the Windows Service
    Uninstall,
    /// Start the service
    Start,
    /// Stop the service
    Stop,
    /// Show service status
    Status,
}

pub async fn run(args: ServiceArgs) -> own_backup_common::Result<()> {
    // The service manages itself; the CLI just points at the right command.
    // (`own-backup-service /install` self-elevates via UAC on Windows.)
    let name = own_backup_common::WINDOWS_SERVICE_DISPLAY_NAME;
    match args.command {
        ServiceCommand::Install => {
            println!("Установка службы «{name}»:");
            println!(
                "  own-backup-service.exe /install   (Windows — запросит права администратора)"
            );
            println!(
                "  sudo apt install ./own-backup_*.deb  (Linux — пакет ставит и запускает службу)"
            );
        }
        ServiceCommand::Uninstall => {
            println!("own-backup-service.exe /uninstall");
        }
        ServiceCommand::Start => {
            println!("own-backup-service.exe /start   (или: net start \"{name}\")");
        }
        ServiceCommand::Stop => {
            println!("own-backup-service.exe /stop    (или: net stop \"{name}\")");
        }
        ServiceCommand::Status => {
            println!(
                "own-backup-service.exe /status  (или: sc query {})",
                own_backup_common::WINDOWS_SERVICE_NAME
            );
        }
    }
    Ok(())
}
