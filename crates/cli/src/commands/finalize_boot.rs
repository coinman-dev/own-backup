use clap::Args;

#[derive(Args)]
pub struct FinalizeBootArgs {
    /// The restored Windows volume (drive letter), e.g. C:
    #[arg(short, long)]
    pub windows: String,

    /// EFI System Partition mount for UEFI, e.g. S: (assign one in WinPE with
    /// `diskpart` → select disk/partition → assign letter=S). Omit to let
    /// bcdboot use the current system partition.
    #[arg(short, long)]
    pub esp: Option<String>,

    /// Target BIOS/MBR boot instead of UEFI.
    #[arg(long)]
    pub bios: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: FinalizeBootArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::engine_win::finalize_boot;

    println!(
        "Recreating boot files for {}\\Windows via bcdboot ({})…",
        args.windows,
        if args.bios { "BIOS" } else { "UEFI" }
    );
    tokio::task::spawn_blocking(move || finalize_boot(&args.windows, args.esp.as_deref(), !args.bios))
        .await
        .map_err(|e| own_backup_common::OwnBackupError::Device(format!("bcdboot task panicked: {e}")))??;

    println!("Boot files created — the disk should now boot.");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: FinalizeBootArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "finalize-boot (bcdboot) is Windows-only.".into(),
    ))
}
