use clap::Args;

#[derive(Args)]
pub struct UnmountArgs {
    /// Previously mounted backup image (.vhdx)
    pub image: String,
}

#[cfg(target_os = "windows")]
pub async fn run(args: UnmountArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::unmount_image;

    unmount_image(std::path::Path::new(&args.image))?;
    println!("Detached: {}", args.image);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: UnmountArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'unmount' command requires Windows. On Linux: qemu-nbd --disconnect /dev/nbd0"
            .into(),
    ))
}
