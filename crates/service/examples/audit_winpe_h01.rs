//! Run from a disposable WinPE boot.wim to check its service transport and H01
//! storage boundary. The installed Windows disk is read only in this probe.
#[cfg(target_os = "windows")]
fn say(message: &str) {
    use std::io::Write;
    println!("{message}");
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"X:\audit-h01.log")
    {
        let _ = writeln!(log, "{message}");
    }
    if let Ok(mut port) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
        let _ = writeln!(port, "{message}");
    }
}

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        say(&format!("WINPE-H01-FAIL {error}"));
        for dir in [r"X:\Windows\System32\Logs", r"X:\Windows\Temp\OwnBackup\Logs"] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("own-backup-service-") {
                        if let Ok(log) = std::fs::read_to_string(entry.path()) {
                            for line in log.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                                say(&format!("WINPE-H01-SERVICE-LINE {line}"));
                            }
                        }
                    }
                }
            }
        }
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_client::BackupClient;
    use own_backup_common::config::AppConfig;
    use own_backup_common::media_mode;

    say("WINPE-H01-BEGIN");
    let winpe = media_mode::is_windows_pe();
    let media = media_mode::is_media_mode();
    let config = AppConfig::default();
    say(&format!("WINPE-H01-MODE winpe={winpe} media={media} data_dir={}", config.data_dir.display()));
    if !winpe || !media || !config.data_dir.to_string_lossy().starts_with("X:\\") {
        return Err("media mode did not select the WinPE RAM disk".into());
    }
    let installed_private = || {
        (b'C'..=b'Z')
            .filter(|&letter| letter != b'X')
            .map(|letter| {
                std::path::PathBuf::from(format!(
                    "{}:\\ProgramData\\OwnBackup\\.own-backup-service-keys\\volume-secrets.db",
                    letter as char
                ))
            })
            .filter(|path| path.exists())
            .collect::<Vec<_>>()
    };
    let before = installed_private();
    say(&format!("WINPE-H01-INSTALLED-PRIVATE-BEFORE count={}", before.len()));

    let exe = std::env::current_exe()?;
    let service = exe.parent().ok_or("no executable directory")?.join("own-backup-service.exe");
    let mut child = std::process::Command::new(service).arg("--run").spawn()?;
    let result = async {
        let mut connected = None;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if let Some(status) = child.try_wait()? {
                say(&format!("WINPE-H01-SERVICE-EXIT code={:?}", status.code()));
                break;
            }
            let Ok(token) = std::fs::read_to_string(config.token_path()) else { continue };
            if let Ok(mut client) = BackupClient::connect(
                config.grpc_address.clone(), token.trim().to_string()
            ).await {
                if let Ok(status) = client.service_status_json().await {
                    connected = Some(status);
                    break;
                }
            }
        }
        let status = connected.ok_or("WinPE client could not reach the foreground service")?;
        say(&format!("WINPE-H01-PIPE connected=true uptime={}", status.contains("uptime_secs")));
        if !status.contains("uptime_secs") { return Err("service status missing uptime".into()); }

        let passwords = own_backup_core::PasswordStore::new(config.db_path());
        let blocked = passwords.remember("WINPE-H01", "disposable".into(), true).is_err();
        let after = installed_private();
        say(&format!("WINPE-H01-SECRET remember_blocked={blocked} installed_private_after={}", after.len()));
        if !blocked || before != after { return Err("WinPE wrote a persistent installed-disk secret".into()); }
        Ok::<(), Box<dyn std::error::Error>>(())
    }.await;
    let _ = child.kill();
    let _ = child.wait();
    result?;
    say("WINPE-H01-PASS");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("audit_winpe_h01 is Windows-only");
}
