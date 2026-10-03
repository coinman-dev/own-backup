//! Read-only reproduction of media/environment overrides on installed Windows.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_restore::guard;
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?;
    let path = args.get(2).ok_or("result path required")?;
    let real_pe = own_backup_common::media_mode::is_windows_pe();
    if real_pe {
        return Err("override probe must run on installed Windows".into());
    }
    match mode.as_str() {
        "media" => own_backup_common::media_mode::set_media_mode(true),
        "environment" => {
            std::env::set_var("SystemDrive", "X:");
            std::env::set_var("SystemRoot", r"T:\fake-windows");
        }
        _ => return Err("mode must be media or environment".into()),
    }
    let disk = guard::guard_target_disk(0).err().map(|e| e.to_string());
    let volume = guard::guard_target_volume("C:")
        .err()
        .map(|e| e.to_string());
    let report = serde_json::json!({"mode":mode,"real_windows_pe":real_pe,"guard_media":guard::is_boot_media(),
        "system_disk":guard::system_disk_number(),"disk_refused":disk.is_some(),"volume_refused":volume.is_some(),
        "disk_error":disk,"volume_error":volume,"writes_performed":false});
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    println!("A06-OVERRIDE {report}");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("audit_a06_media_override is Windows-only");
}
