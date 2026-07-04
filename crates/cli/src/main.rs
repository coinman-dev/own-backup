mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "own-backup",
    about = "My Own Backup - Open-source block-level backup system",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List available disks and partitions
    ListDisks(commands::list_disks::ListDisksArgs),

    /// Run preflight analysis on a device
    Preflight(commands::preflight::PreflightArgs),

    /// Create a backup
    Backup(commands::backup::BackupArgs),

    /// Restore from a backup
    Restore(commands::restore::RestoreArgs),

    /// Verify a VHDX backup image against its sidecar checksums
    Verify(commands::verify::VerifyArgs),

    /// Mount a VHDX backup image read-only as a disk
    Mount(commands::mount::MountArgs),

    /// Detach a mounted VHDX backup image
    Unmount(commands::unmount::UnmountArgs),

    /// Validate a legacy .obk backup file
    Validate(commands::validate::ValidateArgs),

    /// List backup jobs and sets
    ListBackups(commands::list_backups::ListBackupsArgs),

    /// Manage backup jobs via the running service (gRPC client)
    Jobs(commands::jobs::JobsArgs),

    /// Manage the background service
    Service(commands::service::ServiceArgs),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::ListDisks(args) => commands::list_disks::run(args).await,
        Commands::Preflight(args) => commands::preflight::run(args).await,
        Commands::Backup(args) => commands::backup::run(args).await,
        Commands::Restore(args) => commands::restore::run(args).await,
        Commands::Verify(args) => commands::verify::run(args).await,
        Commands::Mount(args) => commands::mount::run(args).await,
        Commands::Unmount(args) => commands::unmount::run(args).await,
        Commands::Validate(args) => commands::validate::run(args).await,
        Commands::ListBackups(args) => commands::list_backups::run(args).await,
        Commands::Jobs(args) => commands::jobs::run(args).await,
        Commands::Service(args) => commands::service::run(args).await,
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
