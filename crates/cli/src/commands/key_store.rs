//! A check of the service key store on this machine, for the Linux stand
//! (`scripts/lab/linux-keys-scenario.sh`, stage k1): the store the installed
//! service uses, in a data directory of the caller's choosing, exercised the
//! way the service exercises it — never the service's own.
//!
//! `check` makes a throwaway job key, authorizes it (the machine key is made
//! through `systemd-creds` if there is none yet) and leaves beside the store
//! what a job would keep: the portable envelope and the reference. `reopen`,
//! run as ANOTHER process — after a reboot, too — opens it again with no
//! password. `revoke` removes it.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct KeyStoreArgs {
    #[command(subcommand)]
    pub action: KeyStoreAction,
}

#[derive(Subcommand)]
pub enum KeyStoreAction {
    /// Authorize a throwaway job key in the store under --data-dir.
    Check(KeyStoreDir),
    /// Open that authorization again, without a password.
    Reopen(KeyStoreDir),
    /// Remove that authorization.
    Revoke(KeyStoreDir),
}

#[derive(Args)]
pub struct KeyStoreDir {
    /// A root-owned data directory (not writable by others); the key
    /// directory is made inside it. Never point it at the service's own.
    #[arg(long)]
    pub data_dir: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
const JOB_ID: &str = "key-store-check";
#[cfg(target_os = "linux")]
const RECORD: &str = "key-store-check.json";

#[cfg(target_os = "linux")]
pub async fn run(args: KeyStoreArgs) -> own_backup_common::Result<()> {
    use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use own_backup_backup::seal::local::{LocalKeyRef, ServiceKeyStore};
    use own_backup_backup::seal::recovery::RecoveryKey;
    use own_backup_common::OwnBackupError;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Record {
        portable: SealedPrivateKey,
        local: LocalKeyRef,
    }
    let bad = |e: serde_json::Error| OwnBackupError::Config(e.to_string());

    let dir = match &args.action {
        KeyStoreAction::Check(d) | KeyStoreAction::Reopen(d) | KeyStoreAction::Revoke(d) => {
            d.data_dir.clone()
        }
    };
    let record = dir.join(RECORD);
    println!("protection: {}", ServiceKeyStore::protection(&dir)?);
    let keys = ServiceKeyStore::new(&dir)?;
    match args.action {
        KeyStoreAction::Check(_) => {
            let pair = JobKeyPair::generate()?;
            let recovery = RecoveryKey::generate()?;
            let portable = SealedPrivateKey::seal(
                JOB_ID,
                &pair,
                "key store check, not a real archive",
                &recovery,
                &KdfParams::strong(),
            )?;
            let local = keys.authorize_and_commit(JOB_ID, &portable, &pair.private, |local| {
                let json = serde_json::to_vec_pretty(&Record {
                    portable: portable.clone(),
                    local: local.clone(),
                })
                .map_err(bad)?;
                std::fs::write(&record, json)?;
                Ok(local)
            })?;
            println!("authorized: {}", local.id());
            println!("store: {}", keys.directory().display());
        }
        KeyStoreAction::Reopen(_) => {
            let saved: Record = serde_json::from_slice(&std::fs::read(&record)?).map_err(bad)?;
            let private = keys.open(JOB_ID, &saved.portable, &saved.local)?;
            if private.public() != saved.portable.public_key()? {
                return Err(OwnBackupError::Encryption(
                    "reopened key does not match".into(),
                ));
            }
            println!(
                "reopened: {} (fingerprint {})",
                saved.local.id(),
                saved.portable.public_key()?.fingerprint()
            );
        }
        KeyStoreAction::Revoke(_) => {
            let saved: Record = serde_json::from_slice(&std::fs::read(&record)?).map_err(bad)?;
            keys.revoke(&saved.local)?;
            println!("revoked: {}", saved.local.id());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_args: KeyStoreArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "The 'key-store' command is only available on Linux.".into(),
    ))
}
