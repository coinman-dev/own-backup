use clap::Args;

use own_backup_client::BackupClient;
use own_backup_common::{OwnBackupError, Result};

#[derive(Args)]
pub struct UnmountArgs {
    /// Previously mounted backup image (.vhdx / .qcow2)
    pub image: String,

    #[arg(long, default_value = "http://127.0.0.1:50051")]
    pub endpoint: String,

    #[arg(long, default_value = "")]
    pub token: String,
}

/// Unmount is handled by the service (it owns the mount). The CLI just asks it.
pub async fn run(args: UnmountArgs) -> Result<()> {
    let token = crate::commands::mount::resolve_token(&args.token);
    let mut client = BackupClient::connect(args.endpoint.clone(), token)
        .await
        .map_err(|e| OwnBackupError::Device(format!(
            "cannot reach the Own Backup service ({e}). Is it running?"
        )))?;

    client
        .unmount_image(&args.image)
        .await
        .map_err(|e| OwnBackupError::Device(e.to_string()))?;
    println!("Отключено: {}", args.image);
    Ok(())
}
