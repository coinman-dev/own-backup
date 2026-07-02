use clap::Args;

#[derive(Args)]
pub struct MountArgs {
    /// Backup image (.vhdx) to mount read-only
    pub image: String,
}

#[cfg(target_os = "windows")]
pub async fn run(args: MountArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::mount_image;

    let path = std::path::PathBuf::from(&args.image);
    if !path.exists() {
        return Err(own_backup_common::OwnBackupError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Image not found: {}", args.image),
        )));
    }

    let physical = mount_image(&path)?;
    println!("Mounted read-only: {}", args.image);
    println!("Virtual disk: {physical}");
    println!("Windows will assign drive letters to the volumes inside (check Explorer).");
    println!("Detach with: own-backup-cli unmount \"{}\"", args.image);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: MountArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'mount' command requires Windows. On Linux, use:\n  \
         qemu-nbd --connect=/dev/nbd0 --read-only <image.vhdx> && mount -o ro /dev/nbd0p1 /mnt"
            .into(),
    ))
}
