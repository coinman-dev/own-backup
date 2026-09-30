//! Read a new dual-header .obk from portable media in a disposable WinPE VM.
#[cfg(target_os = "windows")]
fn say(message: &str) {
    use std::io::Write;

    println!("{message}");
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"X:\audit-h03.log")
    {
        let _ = writeln!(log, "{message}");
    }
    if let Ok(mut port) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
        let _ = writeln!(port, "{message}");
    }
}

#[cfg(target_os = "windows")]
fn main() {
    if let Err(error) = run() {
        say(&format!("WINPE-H03-FAIL {error}"));
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_backup::engine_win::verify_image_with_keys;
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::obk::read_open_header;
    use own_backup_backup::sidecar::read_sealed_sidecar;

    say("WINPE-H03-BEGIN");
    if !own_backup_common::media_mode::is_windows_pe() {
        return Err("test did not boot WinPE".into());
    }
    let image = (b'C'..=b'Z')
        .map(|letter| std::path::PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|root| root.join("h03-media.marker").exists())
        .map(|root| root.join("lab_disk0_full_1.obk"))
        .find(|path| path.exists())
        .ok_or("portable H03 archive ISO not found")?;
    say(&format!("WINPE-H03-IMAGE path={}", image.display()));

    let open = read_open_header(&image)?;
    if !open.dual_header {
        return Err("portable archive is not the new dual-header format".into());
    }
    let keys = ArchiveKeys::from_image_password(&image, "лабораторный пароль архива")?;
    let key = keys.open_chain_of(&image)?.key;
    let sidecar = read_sealed_sidecar(&image, &key)?;
    let partitions = sidecar
        .disk_manifest
        .as_ref()
        .ok_or("sealed whole-disk manifest missing")?
        .images
        .len();
    say(&format!(
        "WINPE-H03-HEADER generation={} partitions={partitions}",
        open.header_generation
    ));
    let report = verify_image_with_keys(&image, Some(&keys), |_, _| Ok(()))?;
    say(&format!(
        "WINPE-H03-VERIFY blocks={} mismatched={}",
        report.blocks_checked,
        report.mismatched_blocks.len()
    ));
    if partitions != 1 || report.blocks_checked == 0 || !report.mismatched_blocks.is_empty() {
        return Err("portable archive validation failed".into());
    }
    if image
        .parent()
        .ok_or("portable archive has no parent directory")?
        .join("h03-restore-allowed.marker")
        .exists()
    {
        use own_backup_backup::engine_win::{restore_disk, DiskRestoreRequest};

        // The disposable VM for this phase has exactly one blank 1 GiB disk,
        // PhysicalDrive0, and boots WinPE from CD. The marker is present only
        // on that phase's archive CD.
        let restored = restore_disk(
            &DiskRestoreRequest {
                manifest_path: image,
                target_disk: 0,
                verify_checksums: true,
                encryption: Some(keys),
            },
            |_| Ok(()),
        )?;
        say(&format!(
            "WINPE-H03-RESTORE partitions={} bytes={}",
            restored.partitions, restored.bytes_written
        ));
        if restored.partitions != 1 || restored.bytes_written == 0 {
            return Err("WinPE restored an empty disk".into());
        }
    }
    say("WINPE-H03-PASS");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("audit_h03_winpe is Windows-only");
}
