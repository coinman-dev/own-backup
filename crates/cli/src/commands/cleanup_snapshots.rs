use clap::Args;

#[derive(Args)]
pub struct CleanupSnapshotsArgs {}

/// Undo the snapshots a killed backup left behind — the same cleanup the
/// service does when it starts. A snapshot a running backup still holds is
/// left alone.
#[cfg(target_os = "linux")]
pub async fn run(_args: CleanupSnapshotsArgs) -> own_backup_common::Result<()> {
    let removed = own_backup_snapshot::linux::remove_orphaned_snapshots().await;
    println!("Snapshots left by interrupted runs removed: {removed}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_args: CleanupSnapshotsArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'cleanup-snapshots' command is only available on Linux.".into(),
    ))
}
