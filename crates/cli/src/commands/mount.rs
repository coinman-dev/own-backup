use clap::Args;

use own_backup_client::BackupClient;
use own_backup_common::{OwnBackupError, Result};

#[derive(Args)]
pub struct MountArgs {
    /// Backup image (.vhdx / .qcow2) to mount
    pub image: String,

    /// Mount in read-write mode
    #[arg(long, default_value_t = false)]
    pub rw: bool,

    /// Service endpoint (foreground/dev TCP; the installed service is reached
    /// over the named pipe automatically).
    #[arg(long, default_value = "http://127.0.0.1:50051")]
    pub endpoint: String,

    /// Bearer token (defaults to the service's token file).
    #[arg(long, default_value = "")]
    pub token: String,
}

/// Resolve the bearer token: explicit flag, else the service's per-boot file.
pub(crate) fn resolve_token(explicit: &str) -> String {
    if explicit.is_empty() {
        let cfg = own_backup_common::config::AppConfig::default();
        std::fs::read_to_string(cfg.token_path())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        explicit.to_string()
    }
}

/// Mount is done by the **service** (a long-running process), not in this CLI:
/// a qcow2 mount is served by an in-process iSCSI target that must outlive the
/// command, so the service holds it. The CLI just asks the service and exits —
/// the disk stays mounted in the background until `unmount`.
pub async fn run(args: MountArgs) -> Result<()> {
    let path = std::path::Path::new(&args.image);
    if !path.exists() {
        return Err(OwnBackupError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Image not found: {}", args.image),
        )));
    }
    let token = resolve_token(&args.token);
    let mut client = BackupClient::connect(args.endpoint.clone(), token)
        .await
        .map_err(|e| OwnBackupError::Device(format!(
            "cannot reach the Own Backup service ({e}). Is it running?"
        )))?;

    let secret = crate::commands::archive_key::ask_for_image(&mut client, &args.image).await?;
    let physical = client
        .mount_image(&args.image, args.rw, &secret)
        .await
        .map_err(|e| OwnBackupError::Device(e.to_string()))?;

    let mode_str = if args.rw { "чтение и запись" } else { "только чтение" };
    println!("Подключено ({}): {}", mode_str, args.image);
    println!("Виртуальный диск: {physical}");
    println!("Windows назначит буквы томам внутри — смотрите проводник.");
    println!("Диск смонтирован в фоне (службой). Отключить: own-backup-cli unmount \"{}\"", args.image);
    Ok(())
}
