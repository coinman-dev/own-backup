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
    match args.command {
        ServiceCommand::Install => {
            #[cfg(target_os = "windows")]
            {
                let exe = std::env::current_exe()
                    .map_err(|e| own_backup_common::OwnBackupError::Io(e))?;
                println!("Installing service...");
                println!(
                    "Run: sc create MyOwnBackup binPath= \"{}\" start= auto",
                    exe.display()
                );
                println!(
                    "Run: sc description MyOwnBackup \"My Own Backup - Block-level backup service\""
                );
            }
            #[cfg(not(target_os = "windows"))]
            {
                println!("Service installation is only supported on Windows.");
            }
        }
        ServiceCommand::Uninstall => {
            println!("Run: sc delete MyOwnBackup");
        }
        ServiceCommand::Start => {
            println!("Run: sc start MyOwnBackup");
        }
        ServiceCommand::Stop => {
            println!("Run: sc stop MyOwnBackup");
        }
        ServiceCommand::Status => {
            println!("Run: sc query MyOwnBackup");
        }
    }
    Ok(())
}
