//! A06 fixture and strict IPC acceptance on explicitly identified disposable disks.
#[cfg(windows)]
mod probe {
    use own_backup_backup::{efivars, efivars_win as efi, gpt_reader};
    use own_backup_restore as guard;
    use std::path::{Path, PathBuf};
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn say(text: impl AsRef<str>) {
        use std::io::Write;
        println!("{}", text.as_ref());
        if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
            let _ = writeln!(file, "{}", text.as_ref());
        }
    }

    fn authorized() -> Result<(PathBuf, serde_json::Value)> {
        let path = (b'C'..=b'Z')
            .map(|l| PathBuf::from(format!("{}:\\a06-auth.json", l as char)))
            .find(|p| p.exists())
            .ok_or("A06 authorization fixture not found")?;
        let body: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        for (disk, key, size) in [
            (0, "windows_guid", 40u64 << 30),
            (1, "esp_guid", 512u64 << 20),
        ] {
            let actual = gpt_reader::read_disk_layout(disk)?;
            if actual.size_bytes != size
                || !actual
                    .disk_guid
                    .eq_ignore_ascii_case(body[key].as_str().ok_or("missing authorized GUID")?)
            {
                return Err(format!("disk {disk} is not this run's disposable fixture").into());
            }
        }
        Ok((path, body))
    }

    fn command(program: &str, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new(program).args(args).output()?;
        say(format!(
            "A06-COMMAND program={program} exit={}",
            output.status
        ));
        if !output.status.success() {
            return Err(format!(
                "{program} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }

    fn setup_pe() -> Result<()> {
        if !guard::is_boot_media() || !efi::is_uefi() {
            return Err("A06 setup must boot UEFI WinPE".into());
        }
        command("wpeinit.exe", &[])?;
        let (authorization, _) = authorized()?;
        let windows = gpt_reader::read_disk_layout(0)?;
        let windows_part = windows
            .partitions
            .iter()
            .find(|p| {
                p.type_guid
                    .eq_ignore_ascii_case("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7")
            })
            .ok_or("Windows fixture has no basic-data partition")?;
        let script = format!("san policy=onlineall\r\nselect disk 0\r\nonline disk noerr\r\nselect partition {}\r\nassign letter=C noerr\r\nselect disk 1\r\nonline disk noerr\r\nselect partition 1\r\nassign letter=S noerr\r\nexit\r\n", windows_part.index);
        std::fs::write(r"X:\a06-diskpart.txt", script)?;
        command("diskpart.exe", &["/s", r"X:\a06-diskpart.txt"])?;
        for (letter, disk) in [("C:\\", 0), ("S:\\", 1)] {
            let root = own_backup_backup::volume_win::volume_root_for_path(letter)
                .ok_or("assigned volume root missing")?;
            if own_backup_backup::volume_win::volume_start_on_disk(&root).map(|p| p.0) != Some(disk)
            {
                return Err(format!("{letter} is not on the authorized disk {disk}").into());
            }
        }
        if !Path::new(r"C:\Windows\System32\kernel32.dll").exists() {
            return Err("Windows fixture is not C:".into());
        }
        command("bcdboot.exe", &[r"C:\Windows", "/s", "S:", "/f", "UEFI"])?;
        if !Path::new(r"S:\EFI\Microsoft\Boot\bootmgfw.efi").exists() {
            return Err("ESP boot loader missing".into());
        }
        let esp = gpt_reader::read_disk_layout(1)?;
        let part = esp
            .partitions
            .iter()
            .find(|p| {
                p.type_guid
                    .eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM)
            })
            .ok_or("ESP fixture has no EFI partition")?;
        let hd = efivars::HardDrive::from_gpt(
            part.index,
            part.first_lba,
            part.last_lba,
            &part.part_guid,
        )
        .ok_or("invalid ESP GUID")?;
        let entry = efi::create_entry(
            "A06 Windows on separate ESP",
            &hd,
            r"\EFI\Microsoft\Boot\bootmgfw.efi",
            Some(0),
        )?;
        efi::set_boot_next(Some(entry))?;
        std::fs::create_dir_all(r"C:\Lab\a06")?;
        std::fs::copy(authorization, r"C:\Lab\a06-auth.json")?;
        let report = serde_json::json!({"pass":true,"windows_disk":0,"esp_disk":1,"entry":entry,"esp_partition_guid":part.part_guid});
        std::fs::write(
            r"C:\Lab\a06\setup-pe.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        say(format!("A06-PE-SETUP-PASS {report}"));
        Ok(())
    }

    fn live(path: &str) -> Result<()> {
        let (_, _) = authorized()?;
        if guard::is_boot_media() || !efi::is_uefi() {
            return Err("live matrix did not boot installed UEFI Windows".into());
        }
        if guard::system_disk_number() != Some(0) {
            return Err("Windows is not on disposable disk 0".into());
        }
        let cfg = efi::read_config()?;
        let entry = cfg
            .entries
            .iter()
            .find(|e| e.is_current)
            .ok_or("BootCurrent entry missing")?;
        let guid = entry
            .option
            .hard_drive()
            .and_then(|h| h.partition_guid())
            .ok_or("BootCurrent has no GPT hard-drive GUID")?;
        let esp = gpt_reader::read_disk_layout(1)?;
        let active = esp
            .partitions
            .iter()
            .find(|p| p.part_guid.eq_ignore_ascii_case(&guid))
            .ok_or("BootCurrent did not use disk 1 ESP")?;
        let mut disks = Vec::new();
        for disk in 0..=2 {
            let p = guard::protection(disk);
            if p.cannot_tell.is_some() {
                return Err(format!("guard cannot determine disk {disk}: {p:?}").into());
            }
            if (disk < 2) == p.parts.is_empty() {
                return Err(format!("unexpected protected partitions on {disk}: {p:?}").into());
            }
            let refusal = guard::guard_target_disk(disk).err().map(|e| e.to_string());
            if (disk < 2) != refusal.is_some() {
                return Err("whole-disk guard decision differs from fixture".into());
            }
            disks.push(serde_json::json!({"disk":disk,"protected":p.parts,"guard_error":refusal}));
        }
        let mut volumes = Vec::new();
        for target in ["C:", "S:", "T:"] {
            let root = own_backup_backup::volume_win::volume_root_for_path(&format!("{target}\\"))
                .ok_or("live fixture volume is not mounted")?;
            let disks: Vec<_> = own_backup_backup::volume_win::volume_extents(&root)
                .into_iter()
                .map(|e| e.disk_number)
                .collect();
            let expected = match target {
                "C:" => 0,
                "S:" => 1,
                _ => 2,
            };
            if disks != vec![expected] {
                return Err(
                    format!("{target} does not map to fixture disk {expected}: {disks:?}").into(),
                );
            }
            let refusal = guard::guard_target_volume(target)
                .err()
                .map(|e| e.to_string());
            if (target != "T:") != refusal.is_some() {
                return Err(format!("unexpected volume guard decision {target}").into());
            }
            volumes.push(serde_json::json!({"target":target,"guard_error":refusal}));
        }
        let report = serde_json::json!({"pass":true,"boot_current":cfg.current,"boot_description":entry.option.description,
            "boot_esp_guid":guid,"boot_esp_partition":active.index,"system_disk":0,"esp_disk":1,"disks":disks,"volumes":volumes});
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
        say(format!("A06-LIVE-GUARD-PASS {report}"));
        Ok(())
    }

    async fn rpc(path: &str) -> Result<()> {
        use own_backup_client::{ArchiveSecret, BackupClient};
        let config = own_backup_common::config::AppConfig::default();
        let token = std::fs::read_to_string(config.token_path())?;
        let mut client = BackupClient::connect(config.grpc_address, token.trim()).await?;
        let mut reports = Vec::new();
        let secret = ArchiveSecret::default();
        let expected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r"C:\Lab\a06\rpc-live-guard.json")?)?;
        for target in ["C:", "S:"] {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                client.restore_volume_with_status(r"C:\Lab\a06\volume.qcow2", target, &secret),
            )
            .await?;
            let error = result.err().ok_or("service restored a live volume")?;
            if error.code() != tonic::Code::FailedPrecondition {
                return Err(format!("volume refusal not guard: {error}").into());
            }
            let wanted = expected["volumes"]
                .as_array()
                .ok_or("expected volumes missing")?
                .iter()
                .find(|r| r["target"] == target)
                .and_then(|r| r["guard_error"].as_str())
                .ok_or("expected live volume guard error missing")?;
            if error.message() != wanted {
                return Err(
                    format!("wrong volume refusal for {target}: {}", error.message()).into(),
                );
            }
            reports.push(serde_json::json!({"operation":"volume","target":target,"code":format!("{:?}",error.code()),"message":error.message()}));
        }
        for disk in [0, 1] {
            for force in [false, true] {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    client.restore_disk_with_status(r"C:\Lab\a06\disk.qcow2", disk, force, &secret),
                )
                .await?;
                let error = result.err().ok_or("service restored a live disk")?;
                if error.code() != tonic::Code::FailedPrecondition {
                    return Err(format!("disk refusal not guard: {error}").into());
                }
                let wanted = expected["disks"]
                    .as_array()
                    .ok_or("expected disks missing")?
                    .iter()
                    .find(|r| r["disk"] == disk)
                    .and_then(|r| r["guard_error"].as_str())
                    .ok_or("expected live disk guard error missing")?;
                if error.message() != wanted {
                    return Err(
                        format!("wrong disk refusal for {disk}: {}", error.message()).into(),
                    );
                }
                reports.push(serde_json::json!({"operation":"disk","target":disk,"force":force,"code":format!("{:?}",error.code()),"message":error.message()}));
            }
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({"pass":true,"refusals":reports}))?,
        )?;
        say("A06-RPC-REFUSALS-PASS count=6");
        Ok(())
    }

    async fn positive(kind: &str, path: &str) -> Result<()> {
        use own_backup_client::{ArchiveSecret, BackupClient};
        let config = own_backup_common::config::AppConfig::default();
        let token = std::fs::read_to_string(config.token_path())?;
        let mut client = BackupClient::connect(config.grpc_address, token.trim()).await?;
        let result = if kind == "volume" {
            serde_json::to_value(
                client
                    .restore_volume_with_status(
                        r"C:\Lab\a06\volume.qcow2",
                        "T:",
                        &ArchiveSecret::default(),
                    )
                    .await?,
            )?
        } else {
            serde_json::to_value(
                client
                    .restore_disk_with_status(
                        r"C:\Lab\a06\disk.qcow2",
                        2,
                        true,
                        &ArchiveSecret::default(),
                    )
                    .await?,
            )?
        };
        std::fs::write(
            path,
            serde_json::to_vec_pretty(
                &serde_json::json!({"pass":true,"kind":kind,"summary":result}),
            )?,
        )?;
        say(format!("A06-RPC-POSITIVE-PASS kind={kind}"));
        Ok(())
    }

    fn pe_restore() -> Result<()> {
        if !guard::is_boot_media() {
            return Err("destructive final phase must boot PE".into());
        }
        command("wpeinit.exe", &[])?;
        authorized()?;
        std::fs::write(r"X:\a06-pe-mount.txt", "san policy=onlineall\r\nselect disk 1\r\nonline disk noerr\r\nselect partition 1\r\nassign letter=S noerr\r\nexit\r\n")?;
        command("diskpart.exe", &["/s", r"X:\a06-pe-mount.txt"])?;
        guard::guard_target_disk(0)?;
        guard::guard_target_volume("C:")?;
        let result = own_backup_backup::engine_win::restore_disk(
            &own_backup_backup::engine_win::DiskRestoreRequest {
                manifest_path: PathBuf::from(r"S:\a06-fixture\disk.qcow2"),
                target_disk: 0,
                verify_checksums: true,
                encryption: None,
            },
            |_| Ok(()),
        )?;
        // After writing the system disk, write proof only on the separate ESP.
        let report = serde_json::json!({"pass":true,"media":true,"target":0,"partitions":result.partitions,"bytes":result.bytes_written});
        std::fs::write(
            r"S:\a06-pe-restore.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        say(format!("A06-PE-RESTORE-PASS {report}"));
        Ok(())
    }

    pub async fn main() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("setup-pe")=>setup_pe(),
            Some("live")=>live(args.get(2).ok_or("result path required")?),
            Some("rpc")=>rpc(args.get(2).ok_or("result path required")?).await,
            Some("positive-volume")=>positive("volume",args.get(2).ok_or("result path required")?).await,
            Some("positive-disk")=>positive("disk",args.get(2).ok_or("result path required")?).await,
            Some("pe-restore")=>pe_restore(),
            _=>Err("usage: audit_a06_guard setup-pe|live|rpc|positive-volume|positive-disk|pe-restore [result.json]".into()),
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    let pe = args.iter().any(|a| a == "setup-pe" || a == "pe-restore");
    if let Err(error) = probe::main().await {
        let message = format!("A06-FAIL {error}");
        println!("{message}");
        if pe {
            let _ = std::fs::write(r"X:\a06-failure.txt", &message);
        }
        std::process::exit(1);
    }
    if pe {
        let _ = std::process::Command::new("wpeutil.exe")
            .arg("shutdown")
            .status();
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("audit_a06_guard is Windows-only");
}
