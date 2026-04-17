use clap::Args;
use zeroize::Zeroizing;

#[derive(Args)]
pub struct BackupArgs {
    /// Source device (e.g., C: or \\.\PhysicalDrive0)
    pub source: String,

    /// Output file path (.obk)
    #[arg(short, long)]
    pub output: String,

    /// Backup type: full, incremental, differential
    #[arg(short = 't', long, default_value = "full")]
    pub backup_type: String,

    /// Compression level (0=none, 1-19=zstd)
    #[arg(short, long, default_value = "3")]
    pub compression: i32,

    /// Encrypt the backup (will prompt for password interactively)
    #[arg(short, long)]
    pub encrypt: bool,
}

pub async fn run(args: BackupArgs) -> own_backup_common::Result<()> {
    println!(
        "Starting {} backup of {} -> {}",
        args.backup_type, args.source, args.output
    );
    println!("Compression level: {}", args.compression);

    if args.encrypt {
        // Zeroizing<String> clears password memory on drop
        let password = Zeroizing::new(
            rpassword::prompt_password("Enter encryption password: ")
                .map_err(own_backup_common::OwnBackupError::Io)?,
        );
        let confirm = Zeroizing::new(
            rpassword::prompt_password("Confirm encryption password: ")
                .map_err(own_backup_common::OwnBackupError::Io)?,
        );
        if *password != *confirm {
            return Err(own_backup_common::OwnBackupError::Encryption(
                "Passwords do not match".into(),
            ));
        }
        println!("Encryption: enabled");
        // password and confirm are Zeroizing<String> — cleared on drop here
    }

    // TODO: Connect to gRPC service or run backup directly
    println!("\nBackup engine not yet wired up. Use gRPC service for full pipeline.");

    Ok(())
}
