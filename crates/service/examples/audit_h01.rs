//! Disposable Windows VM probe for migration of legacy saved volume passwords.
//! Run `seed` as SYSTEM before updating the service, then `check` after startup.
use own_backup_backup::secrets::default_store;
use own_backup_common::config::AppConfig;
use own_backup_core::PasswordResolver;

const TOKEN: &str = "audit-h01-volume";
const PASSWORD: &[u8] = b"Audit-H01-disposable-volume-password";
const NEW_TOKEN: &str = "audit-h01-new-volume";
const NEW_PASSWORD: &str = "Audit-H01-new-disposable-password";
const FAULT_BLOB: &str = r"C:\Lab\audit-h01-fault-blob.bin";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::default();
    let public_path = config.db_path();
    let key_path = config.data_dir.join("secret.key");
    let private_path = config
        .data_dir
        .join(own_backup_backup::seal::local::KEY_DIRECTORY_NAME)
        .join("volume-secrets.db");
    match std::env::args().nth(1).as_deref() {
        Some("seed") => {
            let blob = default_store(&key_path).protect(PASSWORD)?;
            let public = own_backup_metadata::open_database(&public_path)?;
            own_backup_metadata::upsert_volume_secret(&public, TOKEN, &blob)?;
            println!("H01-SEED public_blob_bytes={}", blob.len());
        }
        #[cfg(target_os = "windows")]
        Some("seed-dual" | "seed-deleted-pending") => {
            let delete_public = std::env::args().nth(1).as_deref() == Some("seed-deleted-pending");
            let guard = own_backup_backup::seal::local::PrivateSecretDatabase::new(&config.data_dir)?;
            let blob = default_store(&key_path).protect(PASSWORD)?;
            let public = own_backup_metadata::open_database(&public_path)?;
            own_backup_metadata::upsert_volume_secret(&public, TOKEN, &blob)?;
            let private = own_backup_metadata::open_volume_secret_database(guard.path())?;
            own_backup_metadata::upsert_volume_secret(&private, TOKEN, &blob)?;
            own_backup_metadata::mark_volume_secret_migration_pending(&private)?;
            std::fs::write(FAULT_BLOB, &blob)?;
            if delete_public {
                public.execute_batch("PRAGMA secure_delete=OFF;")?;
                own_backup_metadata::delete_volume_secret(&public, TOKEN)?;
            }
            guard.validate_files()?;
            println!("H01-FAULT-SEED private_committed=true public_deleted={delete_public} pending=true");
        }
        Some("check") => {
            let public = own_backup_metadata::open_database(&public_path)?;
            let public_absent = own_backup_metadata::get_volume_secret(&public, TOKEN)?.is_none();
            let private = own_backup_metadata::open_volume_secret_database(&private_path)?;
            let blob = own_backup_metadata::get_volume_secret(&private, TOKEN)?
                .ok_or("private secret missing")?;
            let password_ok = default_store(&key_path).unprotect(&blob)?.as_slice() == PASSWORD;
            let pending = own_backup_metadata::volume_secret_migration_pending(&private)?;
            let done = own_backup_metadata::volume_secret_migration_done(&private)?;
            let resolver = own_backup_core::PasswordStore::new_private(
                public_path.clone(),
                private_path.clone(),
            );
            let resolver_ok = resolver
                .resolve(TOKEN)
                .is_some_and(|password| password.as_bytes() == PASSWORD);
            println!(
                "H01-CHECK public_absent={public_absent} private_password_ok={password_ok} resolver_ok={resolver_ok} pending={pending} done={done}"
            );
            if !public_absent || !password_ok || !resolver_ok || pending || !done {
                return Err("migration verification failed".into());
            }
        }
        #[cfg(target_os = "windows")]
        Some("acl-check") => {
            // Keep a writer connection open so SQLite's WAL and SHM files are
            // present while the second user's access is tested.
            let private = own_backup_metadata::open_volume_secret_database(&private_path)?;
            private.execute_batch("PRAGMA user_version=1;")?;
            check_second_user_access(&public_path, &private_path)?;
        }
        Some("write-check") => {
            let store = own_backup_core::PasswordStore::new_private(
                public_path.clone(),
                private_path.clone(),
            );
            store.remember(NEW_TOKEN, NEW_PASSWORD.into(), true)?;
            let fresh = own_backup_core::PasswordStore::new_private(
                public_path.clone(),
                private_path.clone(),
            );
            let resolves = fresh
                .resolve(NEW_TOKEN)
                .is_some_and(|password| password.as_str() == NEW_PASSWORD);
            let public = own_backup_metadata::open_database(&public_path)?;
            let public_absent = own_backup_metadata::get_volume_secret(&public, NEW_TOKEN)?.is_none();
            store.forget(NEW_TOKEN)?;
            let forgotten = own_backup_core::PasswordStore::new_private(
                public_path.clone(),
                private_path.clone(),
            )
            .resolve(NEW_TOKEN)
            .is_none();
            println!("H01-WRITE resolves={resolves} public_absent={public_absent} forgotten={forgotten}");
            if !resolves || !public_absent || !forgotten {
                return Err("new volume password was not confined to the private database".into());
            }
        }
        Some("scan") => {
            let blob = std::fs::read(FAULT_BLOB)?;
            let mut found = Vec::new();
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let path = public_path.with_file_name(format!("metadata.db{suffix}"));
                if path.exists() && std::fs::read(&path)?.windows(blob.len()).any(|bytes| bytes == blob.as_slice()) {
                    found.push(suffix);
                }
            }
            println!("H01-FAULT-SCAN public_blob_found={}", !found.is_empty());
            if !found.is_empty() { return Err("legacy blob survived in current public SQLite files".into()); }
        }
        _ => return Err("usage: audit_h01.exe seed|check".into()),
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn check_second_user_access(
    public_path: &std::path::Path,
    private_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        ImpersonateLoggedOnUser, LogonUserW, RevertToSelf, LOGON32_LOGON_NETWORK,
        LOGON32_PROVIDER_DEFAULT,
    };

    struct Impersonation(HANDLE);
    impl Drop for Impersonation {
        fn drop(&mut self) {
            unsafe {
                let _ = RevertToSelf();
                let _ = CloseHandle(self.0);
            }
        }
    }

    let companions: Vec<_> = ["-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let path = private_path.with_file_name(format!("volume-secrets.db{suffix}"));
            (suffix, path.exists(), path)
        })
        .collect();
    let mut token = HANDLE::default();
    // This account and password exist only in the disposable VM's answer file.
    unsafe {
        LogonUserW(
            w!("lab2"),
            w!("."),
            w!("ObkLab!2026"),
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )?;
        ImpersonateLoggedOnUser(token)?;
    }
    let _guard = Impersonation(token);
    let public_readable = std::fs::File::open(public_path).is_ok();
    let private_readable = std::fs::File::open(private_path).is_ok();
    println!("H01-ACL public_readable={public_readable} private_readable={private_readable}");
    for (suffix, exists, path) in companions {
        if exists {
            let readable = std::fs::File::open(&path).is_ok();
            println!("H01-ACL file={suffix} readable={readable}");
            if readable {
                return Err("second user read a private SQLite companion file".into());
            }
        } else {
            println!("H01-ACL file={suffix} absent");
        }
    }
    if !public_readable || private_readable {
        return Err("second-user database access did not match expected ACL".into());
    }
    Ok(())
}
