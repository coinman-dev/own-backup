//! Acceptance from an unchanged product ISO and a separate read-only fixture CD.
#[cfg(windows)]
mod probe {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::win_device::WinRawDevice;
    use own_backup_backup::{engine_win, iscsi_win, volume_win};
    use sha2::{Digest, Sha256};

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn say(text: impl AsRef<str>) {
        let text = text.as_ref();
        println!("{text}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(r"X:\h03-product.log")
        {
            let _ = writeln!(file, "{text}");
        }
        if let Ok(mut port) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
            let _ = writeln!(port, "{text}");
        }
    }

    fn media(marker: &str) -> Result<PathBuf> {
        (b'C'..=b'Z')
            .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
            .find(|root| root.join(marker).exists())
            .ok_or_else(|| format!("fixture marker missing: {marker}").into())
    }

    fn files(root: &Path) -> Result<()> {
        for (name, expected) in [
            (
                "keep-1.bin",
                "6acfe96bb9801018a55e414171ce872610c9331727313723648c07734e69396a",
            ),
            (
                r"docs\keep-2.bin",
                "d8a5f91ac637fc1560efbfe98dc3787b660312b68060eac7c986816599eefd10",
            ),
            (
                "gen-1.bin",
                "9f5e78e53b07e50280c6036000671199304af70555b1388cf183bc683d3bbbb1",
            ),
            (
                "gen-2.bin",
                "cb4ddc09dd1cec36b671e302794e6c7ffb12060319a2b0ae92d279843857a662",
            ),
        ] {
            let data = std::fs::read(root.join(name))?;
            let actual = format!("{:x}", Sha256::digest(&data));
            say(format!(
                "PRODUCT-ISO-FILE root={} name={name} bytes={} sha256={actual}",
                root.display(),
                data.len()
            ));
            if actual != expected {
                return Err(format!("control file differs: {name}").into());
            }
        }
        if root.join("excluded-secret.bin").exists() {
            return Err("excluded file returned".into());
        }
        Ok(())
    }

    fn run() -> Result<()> {
        say("PRODUCT-ISO-BEGIN");
        if !own_backup_common::media_mode::is_windows_pe() {
            return Err("not real WinPE".into());
        }
        if !own_backup_backup::efivars_win::enable_privilege()? {
            return Err("firmware read privilege unavailable".into());
        }
        let secure_boot = own_backup_backup::efivars_win::read_var("SecureBoot")?;
        let setup_mode = own_backup_backup::efivars_win::read_var("SetupMode")?;
        say(format!(
            "PRODUCT-ISO-FIRMWARE secure_boot={secure_boot:?} setup_mode={setup_mode:?}"
        ));
        if secure_boot.as_deref() != Some(&[1]) || setup_mode.as_deref() != Some(&[0]) {
            return Err("fixture did not boot with Secure Boot enforced".into());
        }
        let fixture = media("h03-product-fixture.marker")?;
        let app = Path::new(r"X:\Program Files\OwnBackup");
        let runtime = media(r"OwnBackup\media.id")?.join(r"OwnBackup\WebView2");
        for name in ["vk_swiftshader.dll", "vulkan-1.dll"] {
            let bytes = std::fs::read(runtime.join(name))?;
            if bytes.is_empty() {
                return Err(format!("empty software renderer: {name}").into());
            }
            say(format!(
                "PRODUCT-ISO-SOFTWARE-RENDERER name={name} bytes={} sha256={:x}",
                bytes.len(),
                Sha256::digest(&bytes)
            ));
        }
        let expected: std::collections::BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(fixture.join("program-hashes.json"))?)?;
        for (name, hash) in expected {
            let actual = format!("{:x}", Sha256::digest(std::fs::read(app.join(&name))?));
            if actual != hash {
                return Err(format!("product binary differs: {name}").into());
            }
            say(format!("PRODUCT-ISO-BINARY name={name} sha256={actual}"));
        }
        // WinRE does not include tasklist. Ask the native process snapshot instead.
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut names = Vec::new();
        // SAFETY: the snapshot is owned, the entry has its required size, and
        // each successful call fills a terminated executable name in that entry.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = Process32FirstW(snapshot, &mut entry).is_ok();
            while found {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.push(String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase());
                found = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        std::fs::write(r"X:\h03-product-processes.csv", names.join("\n"))?;
        let processes = names.join("\n");
        for name in [
            "own-backup-shell.exe",
            "own-backup-gui.exe",
            "own-backup-service.exe",
            "msedgewebview2.exe",
        ] {
            if !processes.contains(name) {
                return Err(format!("product process absent: {name}").into());
            }
        }
        say("PRODUCT-ISO-PROCESSES shell=true gui=true service=true webview=true");
        // A console opened by the desktop does not inherit the boot script's
        // TEMP override. The frontend writes into that script's media data dir.
        let ready = own_backup_common::config::AppConfig::default()
            .data_dir
            .join("own-backup-gui-up");
        if !ready.exists() {
            return Err("frontend did not acknowledge rendering".into());
        }
        say("PRODUCT-ISO-RENDERED frontend_ready=true");
        let status = Command::new(app.join("own-backup-cli.exe"))
            .args(["jobs", "status"])
            .output()?;
        std::fs::write(
            r"X:\h03-product-status.log",
            [&status.stdout[..], &status.stderr[..]].concat(),
        )?;
        if !status.status.success() {
            return Err("production CLI cannot reach boot service".into());
        }
        say("PRODUCT-ISO-RPC status=true");
        let initiator = iscsi_win::status();
        say(format!(
            "PRODUCT-ISO-INITIATOR driver={} present={} running={} ready={}",
            initiator.driver_file, initiator.service_present, initiator.running, initiator.ready
        ));
        if !initiator.ready {
            return Err("product did not prepare iSCSI initiator".into());
        }

        let image = media("h03-media.marker")?.join("lab_disk0_full_1.obk");
        let keys = ArchiveKeys::from_image_password(&image, "лабораторный пароль архива")?;
        let manifest = engine_win::load_sidecar_with_keys(&image, Some(&keys))?
            .disk_manifest
            .ok_or("whole-disk manifest missing")?;
        if manifest.images.len() != 1 || !manifest.images[0].filesystem.eq_ignore_ascii_case("ntfs")
        {
            return Err("fixture must contain exactly one NTFS image".into());
        }
        let ntfs = manifest
            .layout
            .partitions
            .iter()
            .find(|part| part.index == manifest.images[0].part_index)
            .ok_or("NTFS partition missing from captured layout")?;
        let restore_partition = ntfs.index;
        say(format!(
            "PRODUCT-ISO-LAYOUT ntfs_partition={restore_partition} offset={} bytes={}",
            ntfs.offset_bytes(manifest.layout.sector_size),
            ntfs.size_bytes(manifest.layout.sector_size)
        ));
        let verified = engine_win::verify_image_with_keys(&image, Some(&keys), |_, _| Ok(()))?;
        say(format!(
            "PRODUCT-ISO-VERIFY blocks={} mismatched={}",
            verified.blocks_checked,
            verified.mismatched_blocks.len()
        ));
        if verified.blocks_checked == 0 || !verified.valid() {
            return Err("archive verification failed".into());
        }
        let physical = engine_win::mount_image_with_keys(&image, false, Some(&keys))?;
        let checked = (|| -> Result<()> {
            let disk: u32 = physical
                .rsplit("PhysicalDrive")
                .next()
                .ok_or("bad mounted path")?
                .parse()?;
            let root = volume_win::drive_letters_on_disk(disk)
                .into_iter()
                .map(PathBuf::from)
                .find(|root| root.join("keep-1.bin").exists())
                .ok_or("mounted NTFS missing")?;
            files(&root)
        })();
        let detached = engine_win::unmount_image(&image);
        checked?;
        detached?;
        say("PRODUCT-ISO-MOUNT files=4 excluded_absent=true detached=true");

        // This fixture runs only with a blank 1 GiB QEMU disk. Refuse any other target.
        let target = WinRawDevice::open_read(r"\\.\PhysicalDrive0")?;
        if target.length() != 1 << 30 {
            return Err("target is not the disposable 1 GiB disk".into());
        }
        let mut first = vec![0; 1024 * 1024];
        let mut target = target;
        target.read_at(0, &mut first)?;
        if first.iter().any(|byte| *byte != 0) {
            return Err("target disk is not blank".into());
        }
        drop(target);
        let restored = engine_win::restore_disk(
            &engine_win::DiskRestoreRequest {
                manifest_path: image,
                target_disk: 0,
                verify_checksums: true,
                encryption: Some(keys),
                passphrases: Default::default(),
            },
            |_| Ok(()),
        )?;
        if restored.partitions != 1 || restored.bytes_written == 0 {
            return Err("empty restore".into());
        }
        say(format!(
            "PRODUCT-ISO-RESTORE partitions={} bytes={}",
            restored.partitions, restored.bytes_written
        ));
        let device = WinRawDevice::open_write(r"\\.\PhysicalDrive0")?;
        device.clear_read_only_this_session()?;
        device.set_online_this_session()?;
        device.update_properties()?;
        drop(device);
        std::thread::sleep(Duration::from_secs(3));
        std::fs::write(
            r"X:\h03-assign.txt",
            format!("rescan\r\nselect disk 0\r\nonline disk noerr\r\nselect partition {restore_partition}\r\nassign letter=R\r\n"),
        )?;
        let assigned = Command::new("diskpart.exe")
            .args(["/s", r"X:\h03-assign.txt"])
            .output()?;
        std::fs::write(r"X:\h03-product-diskpart.log", &assigned.stdout)?;
        if !assigned.status.success() {
            return Err("assign restored NTFS failed".into());
        }
        files(Path::new(r"R:\"))?;
        let write = Path::new(r"R:\h03-product-write.txt");
        std::fs::write(write, b"written from unchanged Own Backup product ISO")?;
        if std::fs::read(write)? != b"written from unchanged Own Backup product ISO" {
            return Err("NTFS write failed".into());
        }
        let check = Command::new("chkdsk.exe").arg("R:").output()?;
        std::fs::write(
            r"X:\h03-product-chkdsk.log",
            [&check.stdout[..], &check.stderr[..]].concat(),
        )?;
        say(format!("PRODUCT-ISO-CHKDSK exit={:?}", check.status.code()));
        if !check.status.success() {
            return Err("restored NTFS is not chkdsk-clean".into());
        }
        say("PRODUCT-ISO-PASS files=4 excluded_absent=true ntfs_write=true chkdsk=0");
        Ok(())
    }

    pub fn main() {
        let result = run();
        if let Err(ref error) = result {
            say(format!("PRODUCT-ISO-FAIL {error}"));
        }
        say("PRODUCT-ISO-DONE");
        if let Ok(root) = media("h03-product-output.marker") {
            if let Ok(entries) = std::fs::read_dir(r"X:\") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("h03-product") {
                        let _ = std::fs::copy(entry.path(), root.join(name));
                    }
                }
            }
            let logs = Path::new(r"X:\Program Files\OwnBackup");
            for name in [
                "own-backup-gui.log",
                "own-backup-service.log",
                "own-backup-shell.log",
                "own-backup-cli.log",
            ] {
                let _ = std::fs::copy(logs.join(name), root.join(name));
            }
            if let Ok(entries) = std::fs::read_dir(logs.join("Logs")) {
                for entry in entries.flatten() {
                    if entry.path().extension().and_then(|e| e.to_str()) == Some("log") {
                        let _ = std::fs::copy(entry.path(), root.join(entry.file_name()));
                    }
                }
            }
        }
        if result.is_err() {
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn main() {
    probe::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This acceptance requires a disposable product WinPE VM.");
}
