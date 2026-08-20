mod commands;
mod elevation;
mod logging;

use clap::{Parser, Subcommand};

/// Bump this on any build handed over for testing.
///
/// It exists because an old exe left in another folder is indistinguishable
/// from a bug: a run of this program answered with a message that had already
/// been replaced, and the option it was told to use "did not exist". `-V` and
/// every diagnostic command print it, so "which binary is this" is a question
/// with an answer.
pub const BUILD_TAG: &str = "2026-08-20o";
const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " (сборка 2026-08-05g)");

#[derive(Parser)]
#[command(
    name = "own-backup",
    about = "My Own Backup - Open-source block-level backup system",
    version = VERSION_LINE
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Set by the self-elevation relaunch (see `elevation.rs`): this process
    /// owns a fresh console window, so hold it open on exit.
    #[arg(long, hide = true, global = true)]
    elevated: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// List available disks and partitions
    ListDisks(commands::list_disks::ListDisksArgs),

    /// Run preflight analysis on a device
    Preflight(commands::preflight::PreflightArgs),

    /// Create a backup
    Backup(commands::backup::BackupArgs),
    /// What a BitLocker volume says about itself (read-only, no password)
    Bitlocker(commands::bitlocker::BitlockerArgs),

    /// Прогнать сканер исключений по тому и сказать, сколько он занял
    /// (стенд этапов B0/B1 плана исключений-до-копии; ничего не меняет).
    ScanExclusions(commands::scan_exclusions::ScanExclusionsArgs),

    /// Restore from a backup
    Restore(commands::restore::RestoreArgs),

    /// Restore a WHOLE disk (bootable) from a .obdm manifest — DESTRUCTIVE
    RestoreDisk(commands::restore_disk::RestoreDiskArgs),

    /// Recreate Windows boot files (bcdboot) after a whole-disk restore
    FinalizeBoot(commands::finalize_boot::FinalizeBootArgs),

    /// Verify a VHDX backup image against its sidecar checksums
    Verify(commands::verify::VerifyArgs),

    /// Mount a VHDX backup image read-only as a disk
    Mount(commands::mount::MountArgs),

    /// Serve a qcow2 backup to Arsenal Image Mounter over devio TCP (read-only)
    DevioServe(commands::devio_serve::DevioServeArgs),

    /// Mount prerequisites: AIM driver (Windows) / NBD & NTFS status (Linux)
    Driver(commands::driver::DriverArgs),

    /// Pick a screen mode fit for the interface (boot media)
    Display(commands::display::DisplayArgs),

    /// Check that this environment can host the interface (boot medium)
    MediaPreflight(commands::media_preflight::MediaPreflightArgs),

    /// Wait until the interface window is actually up (boot medium)
    MediaWait(commands::media_preflight::MediaWaitArgs),

    /// Detach a mounted VHDX backup image
    Unmount(commands::unmount::UnmountArgs),

    /// List backup jobs and sets
    ListBackups(commands::list_backups::ListBackupsArgs),

    /// Manage backup jobs via the running service (gRPC client)
    Jobs(commands::jobs::JobsArgs),

    /// Manage the background service
    Service(commands::service::ServiceArgs),
}

/// Commands that touch raw devices, VSS, virtual disks or the SCM directly —
/// they fail with ERROR_ACCESS_DENIED in a non-elevated console, so the CLI
/// requests elevation up front instead of surfacing 0x80070005 mid-run.
/// (Mount/Unmount are NOT here: they are thin clients of the LocalSystem
/// service, which does the privileged work.)
#[cfg(target_os = "windows")]
fn needs_admin(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Preflight(_)
            | Commands::Backup(_)
            // Reads `\\.\PhysicalDriveN`, which is an administrator's privilege.
            | Commands::Bitlocker(_)
            | Commands::Restore(_)
            | Commands::RestoreDisk(_)
            | Commands::FinalizeBoot(_)
            | Commands::Verify(_)
            | Commands::Service(_)
            | Commands::Driver(commands::driver::DriverArgs {
                action: commands::driver::DriverAction::Install { .. }
                    | commands::driver::DriverAction::Uninstall,
            })
    )
}

#[tokio::main]
async fn main() {
    // Console stays quiet (warn); the full info log goes to own-backup-cli.log
    // next to the exe — it survives the elevated console window closing.
    let log_path = logging::init();

    let cli = Cli::parse();

    // Admin-requiring command in a non-elevated console: relaunch through the
    // UAC prompt, mirror the child's exit code. The child prints into its own
    // console window and holds it open (see the `cli.elevated` block below).
    #[cfg(target_os = "windows")]
    if needs_admin(&cli.command) && !elevation::is_elevated() {
        println!("Требуются права администратора — запрашиваю повышение (UAC)…");
        println!("Результат появится в новом окне консоли.");
        match elevation::relaunch_elevated() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Не удалось получить права администратора: {e}");
                std::process::exit(1);
            }
        }
    }

    let result = match cli.command {
        Commands::ListDisks(args) => commands::list_disks::run(args).await,
        Commands::Preflight(args) => commands::preflight::run(args).await,
        Commands::Backup(args) => commands::backup::run(args).await,
        Commands::Bitlocker(args) => commands::bitlocker::run(args).await,
        Commands::ScanExclusions(args) => commands::scan_exclusions::run(args).await,
        Commands::Restore(args) => commands::restore::run(args).await,
        Commands::RestoreDisk(args) => commands::restore_disk::run(args).await,
        Commands::FinalizeBoot(args) => commands::finalize_boot::run(args).await,
        Commands::Verify(args) => commands::verify::run(args).await,
        Commands::Mount(args) => commands::mount::run(args).await,
        Commands::DevioServe(args) => commands::devio_serve::run(args).await,
        Commands::Driver(args) => commands::driver::run(args).await,
        Commands::Display(args) => commands::display::run(args).await,
        Commands::MediaPreflight(args) => commands::media_preflight::run(args).await,
        Commands::MediaWait(args) => commands::media_preflight::run_wait(args).await,
        Commands::Unmount(args) => commands::unmount::run(args).await,
        Commands::ListBackups(args) => commands::list_backups::run(args).await,
        Commands::Jobs(args) => commands::jobs::run(args).await,
        Commands::Service(args) => commands::service::run(args).await,
    };

    let code = match result {
        Ok(()) => {
            tracing::info!("command completed");
            0
        }
        Err(e) => {
            tracing::error!(error = %e, "command failed");
            eprintln!("Error: {}", e);
            if let Some(p) = &log_path {
                eprintln!("Подробный лог: {}", p.display());
            }
            1
        }
    };

    // An elevated relaunch runs in its own console window that would vanish
    // the instant the process exits — keep it open until the user has read
    // the output.
    if cli.elevated {
        use std::io::{BufRead, Write};
        print!("\nНажмите Enter для выхода…");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().lock().read_line(&mut String::new());
    }

    std::process::exit(code);
}
