use clap::Args;
use std::path::Path;
use zeroize::Zeroizing;

use own_backup_backup::encryption;
use own_backup_backup::ImageReader;
use own_backup_restore::validator;

#[derive(Args)]
pub struct ValidateArgs {
    /// Backup file (.obk) to validate
    pub file: String,

    /// Run full validation (check every block checksum)
    #[arg(long)]
    pub full: bool,
}

pub async fn run(args: ValidateArgs) -> own_backup_common::Result<()> {
    let path = Path::new(&args.file);

    if !path.exists() {
        return Err(own_backup_common::OwnBackupError::Io(
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("File not found: {}", args.file),
            ),
        ));
    }

    println!("Validating {}...", args.file);

    if args.full {
        let file = std::fs::File::open(path)?;
        let reader = ImageReader::open(std::io::BufReader::new(file))?;
        let is_encrypted = reader.manifest().encryption_algo.is_some();

        let key = if is_encrypted {
            // Zeroizing<String> clears password from memory on drop
            let password = Zeroizing::new(
                rpassword::prompt_password("Backup is encrypted. Enter password: ")
                    .map_err(own_backup_common::OwnBackupError::Io)?,
            );
            let salt = reader.manifest().encryption_salt.as_ref().ok_or_else(|| {
                own_backup_common::OwnBackupError::Encryption(
                    "Encrypted backup has no salt in manifest".into(),
                )
            })?;
            let key = encryption::derive_key(&password, salt)?;
            // password dropped and zeroized here
            Some(key)
        } else {
            None
        };

        let result = validator::full_validate(path, key.as_ref())?;
        println!(
            "Result: {} ({}/{} blocks checked)",
            if result.valid { "VALID" } else { "INVALID" },
            result.blocks_checked,
            result.blocks_total
        );
        println!("{}", result.message);
    } else {
        let result = validator::quick_validate(path)?;
        println!(
            "Result: {}",
            if result.valid { "VALID" } else { "INVALID" }
        );
        println!("{}", result.message);
    }

    Ok(())
}
