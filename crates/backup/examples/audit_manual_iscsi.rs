//! Disposable Windows VM check of `own-backup-cli iscsi-serve` with Microsoft
//! Initiator: wrong CHAP fails, correct CHAP attaches the disk and serves the
//! image's real bytes, then logout removes the session.
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_backup::iscsi::{iqn_for, serial_for_image};
    use own_backup_backup::iscsi_win::login;
    use own_backup_backup::qcow2::Qcow2RandomReader;
    use own_backup_backup::win_device::WinRawDevice;

    let path = std::env::args()
        .nth(1)
        .ok_or("usage: audit_manual_iscsi.exe <image>")?;
    let path = std::path::PathBuf::from(path);
    let mut image = Qcow2RandomReader::open(&path)?;
    let size = image.virtual_size();
    let iqn = iqn_for(&serial_for_image(&path));
    let wrong = *b"wrong-wrong-1234";
    let correct = *b"0123456789abcdef";
    if login("127.0.0.1", 3260, &iqn, &wrong, size).is_ok() {
        return Err("manual target accepted the wrong CHAP secret".into());
    }
    println!("A04-MANUAL-WRONG-CHAP refused=true");

    let attached = login("127.0.0.1", 3260, &iqn, &correct, size)?;
    let disk = attached.disk_number;
    let read = (|| -> Result<bool, Box<dyn std::error::Error>> {
        let mut expected = [0u8; 512];
        image.read_at(0, &mut expected)?;
        let mut actual = [0u8; 512];
        let device_path = format!(r"\\.\PhysicalDrive{disk}");
        let mut device = WinRawDevice::open_read(&device_path)?;
        device.read_at(0, &mut actual)?;
        Ok(actual == expected && expected.starts_with(b"OBK-MANUAL-CHAP-CONTROL"))
    })();
    let logged_out = attached.logout();
    let same_bytes = read?;
    println!("A04-MANUAL-CORRECT-CHAP disk={disk} same_bytes={same_bytes} logout={logged_out}");
    if !same_bytes || !logged_out {
        return Err("manual iSCSI data or logout check failed".into());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("audit_manual_iscsi is Windows-only");
}
