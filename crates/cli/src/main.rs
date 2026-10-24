mod commands;
mod elevation;
mod logging;
mod secret_prompt;

use clap::{Parser, Subcommand};

/// Bump this on any build handed over for testing.
///
/// It exists because an old exe left in another folder is indistinguishable
/// from a bug: a run of this program answered with a message that had already
/// been replaced, and the option it was told to use "did not exist". `-V` and
/// every diagnostic command print it, so "which binary is this" is a question
/// with an answer.
pub const BUILD_TAG: &str = "2026-08-20q";
const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " (сборка 2026-08-05g)");

#[derive(Parser)]
#[command(
    name = "own-backup",
    about = "Own Backup — block-level backup (source-available, free for noncommercial use)",
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

    /// Copy a WHOLE disk (several partitions) into one bootable image --
    /// the same thing a job from the interface does
    BackupDisk(commands::backup_disk::BackupDiskArgs),

    /// Записи загрузки UEFI: что прошивка предлагает в меню и куда это ведёт
    Boot(commands::boot::BootArgs),
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

    /// Fold overflowed increments into their base (sliding window)
    Consolidate(commands::consolidate::ConsolidateArgs),

    /// Remove snapshots an interrupted backup left behind (Linux; the service
    /// also does this when it starts)
    CleanupSnapshots(commands::cleanup_snapshots::CleanupSnapshotsArgs),

    /// Turn archive encryption on for a job (password + recovery key)
    ArchiveKey(commands::archive_key::ArchiveKeyArgs),

    /// Check the service key store in a scratch data directory (Linux stand)
    KeyStore(commands::key_store::KeyStoreArgs),

    /// Mount a VHDX backup image read-only as a disk
    Mount(commands::mount::MountArgs),

    /// Serve a qcow2 backup as an iSCSI target for the OS's own initiator
    IscsiServe(commands::iscsi_serve::IscsiServeArgs),

    /// Mount prerequisites: the iSCSI initiator (Windows) / NBD & NTFS status (Linux)
    Driver(commands::driver::DriverArgs),

    /// Pick a screen mode fit for the interface (boot media)
    Display(commands::display::DisplayArgs),

    /// Собрать загрузочный носитель — то же, что делает мастер в интерфейсе
    Media(commands::media::MediaArgs),

    /// Check that this environment can host the interface (boot medium)
    MediaPreflight(commands::media_preflight::MediaPreflightArgs),

    /// Wait until the interface window is actually up (boot medium)
    MediaWait(commands::media_preflight::MediaWaitArgs),

    /// Сторонние средства этой машины, которые можно положить на носитель
    MediaTools(commands::media_tools::MediaToolsArgs),

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
/// Mount/Unmount use the LocalSystem service; Verify reads ordinary image
/// files in userspace. None of those commands requires elevation.
#[cfg(target_os = "windows")]
fn needs_admin(command: &Commands) -> bool {
    if let Commands::Consolidate(args) = command {
        // qcow2 and OBK merge in userspace. VHDX uses the Windows virtual-disk
        // driver, which needs elevation. An unreadable folder stays on the
        // conservative path; the command will report its own error later.
        let Ok(entries) = std::fs::read_dir(&args.folder) else {
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else { return true };
            let extension = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if matches!(extension.as_deref(), Some("vhdx" | "avhdx")) {
                return true;
            }
        }
        return false;
    }
    matches!(
        command,
        Commands::Preflight(_)
            | Commands::Backup(_)
            | Commands::BackupDisk(_)
            // Переменные прошивки читаются только с SeSystemEnvironmentPrivilege.
            | Commands::Boot(_)
            // Reads `\\.\PhysicalDriveN`, which is an administrator's privilege.
            | Commands::Bitlocker(_)
            | Commands::Restore(_)
            | Commands::RestoreDisk(_)
            | Commands::FinalizeBoot(_)
            | Commands::Service(_)
            | Commands::Driver(commands::driver::DriverArgs {
                action: commands::driver::DriverAction::Prepare,
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
        Commands::BackupDisk(args) => commands::backup_disk::run(args).await,
        Commands::Boot(args) => commands::boot::run(args).await,
        Commands::Bitlocker(args) => commands::bitlocker::run(args).await,
        Commands::ScanExclusions(args) => commands::scan_exclusions::run(args).await,
        Commands::Restore(args) => commands::restore::run(args).await,
        Commands::RestoreDisk(args) => commands::restore_disk::run(args).await,
        Commands::FinalizeBoot(args) => commands::finalize_boot::run(args).await,
        Commands::Verify(args) => commands::verify::run(args).await,
        Commands::Consolidate(args) => commands::consolidate::run(args).await,
        Commands::CleanupSnapshots(args) => commands::cleanup_snapshots::run(args).await,
        Commands::ArchiveKey(args) => commands::archive_key::run(args).await,
        Commands::KeyStore(args) => commands::key_store::run(args).await,
        Commands::Mount(args) => commands::mount::run(args).await,
        Commands::IscsiServe(args) => commands::iscsi_serve::run(args).await,
        Commands::Driver(args) => commands::driver::run(args).await,
        Commands::Display(args) => commands::display::run(args).await,
        Commands::Media(args) => commands::media::run(args).await,
        Commands::MediaPreflight(args) => commands::media_preflight::run(args).await,
        Commands::MediaWait(args) => commands::media_preflight::run_wait(args).await,
        Commands::MediaTools(args) => commands::media_tools::run(args).await,
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
