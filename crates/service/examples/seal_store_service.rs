//! Disposable SCM/LocalSystem probe for E2. Never backs up or mounts a disk.
//! Install ONLY on a Windows test VM with scripts/test-seal-service.ps1.
//! First start creates random keys and discards password/recovery material.
//! Subsequent starts can succeed only through the local service authorization.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This probe runs as a Windows service on a disposable test VM.");
}

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe::dispatch()
}

#[cfg(target_os = "windows")]
mod probe {
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::Path;
    use std::time::Duration;

    use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use own_backup_backup::seal::local::ServiceKeyStore;
    use own_backup_backup::seal::recovery::RecoveryKey;
    use own_backup_common::{OwnBackupError, Result};
    use own_backup_core::{Job, JobStore};
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    pub const NAME: &str = "OwnBackupSealProbe";
    const MARKER: &str = "OwnBackup E2 disposable laboratory probe";
    windows_service::define_windows_service!(ffi_main, service_main);

    pub fn dispatch() -> std::result::Result<(), Box<dyn std::error::Error>> {
        windows_service::service_dispatcher::start(NAME, ffi_main)?;
        Ok(())
    }

    fn service_main(_arguments: Vec<OsString>) {
        let _ = run_service();
    }

    fn run_service() -> Result<()> {
        let executable = std::env::current_exe()?;
        let root = executable
            .parent()
            .ok_or_else(|| OwnBackupError::Config("no probe directory".into()))?;
        if std::fs::read_to_string(root.join("lab-probe.marker"))? != MARKER {
            return Err(OwnBackupError::Config(
                "not an initialized probe directory".into(),
            ));
        }
        let handle = service_control_handler::register(NAME, |control| match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })
        .map_err(|e| OwnBackupError::Device(e.to_string()))?;
        let status = |state, exit_code| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        handle
            .set_service_status(status(ServiceState::Running, 0))
            .map_err(|e| OwnBackupError::Device(e.to_string()))?;

        let result = run_probe(&root.join("state"));
        let report = serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "uptime_ms": unsafe { GetTickCount64() },
            "ok": result.is_ok(),
            "phase": result.as_ref().ok(),
            "error": result.as_ref().err().map(ToString::to_string),
        });
        let written = (|| -> Result<()> {
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("reports.jsonl"))?;
            writeln!(log, "{}", serde_json::to_string(&report)?)?;
            log.sync_all()?;
            Ok(())
        })();
        handle
            .set_service_status(status(
                ServiceState::Stopped,
                if result.is_ok() && written.is_ok() {
                    0
                } else {
                    1
                },
            ))
            .map_err(|e| OwnBackupError::Device(e.to_string()))?;
        result.and(written).map(|_| ())
    }

    fn run_probe(data_dir: &Path) -> Result<&'static str> {
        std::fs::create_dir_all(data_dir)?;
        let keys = ServiceKeyStore::new(data_dir)?; // Requires actual LocalSystem.
        let jobs = JobStore::load(data_dir);
        if let Some(job) = jobs.get("seal-probe") {
            let encryption = job
                .archive_encryption
                .as_ref()
                .ok_or_else(|| OwnBackupError::Config("probe job has no authorization".into()))?;
            let private = keys.open(&job.id, &encryption.portable_key, &encryption.local_key)?;
            if private.public() != encryption.portable_key.public_key()? {
                return Err(OwnBackupError::Encryption("unexpected reopened key".into()));
            }
            return Ok("reopened-without-password");
        }
        // A corrupt/missing jobs.json must not quietly start a new fixture.
        if data_dir.join("initialized.marker").exists() {
            return Err(OwnBackupError::Config(
                "initialized probe lost its job document".into(),
            ));
        }
        let pair = JobKeyPair::generate()?;
        let recovery = RecoveryKey::generate()?;
        let password = RecoveryKey::generate()?.to_string_plain();
        let portable = SealedPrivateKey::seal(
            "seal-probe",
            &pair,
            &password,
            &recovery,
            &KdfParams::strong(),
        )?;
        let job = Job {
            id: "seal-probe".into(),
            name: "Disposable key-store probe".into(),
            ..Default::default()
        };
        jobs.upsert_with_archive_key(job, portable, &pair.private, &keys)?;
        // No password, recovery secret or clear private key is stored for boot 2.
        drop(password);
        drop(recovery);
        drop(pair);
        let mut marker = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(data_dir.join("initialized.marker"))?;
        marker.write_all(b"Do not recreate missing keys")?;
        marker.sync_all()?;
        Ok("created-password-discarded")
    }
}
