use clap::Args;

#[derive(Args)]
pub struct RestoreArgs {
    /// Backup file (.obk) to restore from
    pub input: String,

    /// Target device to restore to
    #[arg(short, long)]
    pub target: String,
}

pub async fn run(args: RestoreArgs) -> own_backup_common::Result<()> {
    println!("Restoring {} -> {}", args.input, args.target);

    // Check if the backup is encrypted — if so, prompt for password
    // (will be implemented when restore pipeline is wired up)
    println!("If the backup is encrypted, you will be prompted for the password.");

    // TODO: wire up to restore pipeline
    println!("\nRestore engine not yet wired up.");

    Ok(())
}
