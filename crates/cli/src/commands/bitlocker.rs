//! What a BitLocker volume says about itself — read-only, no password.
//!
//! The first step of making BitLocker work on Windows, and deliberately the
//! harmless one. It answers the questions everything else depends on:
//!
//!   * is the ciphertext reachable at all on this machine? (it is read from
//!     `\\.\PhysicalDriveN` at the partition's offset, because a locked volume
//!     refuses reads through its own device);
//!   * which cipher — we implement AES-XTS 128/256 and nothing else;
//!   * which ways in does the volume have? A passphrase is the only one we can
//!     use today, and a volume protected by TPM alone has none — the user
//!     deserves to be told that instead of "wrong password".
//!
//! It also prints the two metadata headers as hex. The fields we do not claim
//! to understand — among them whatever marks an encryption still in progress —
//! must be learned from real volumes before any code decides anything on them.

use clap::Args;
use own_backup_common::Result;

#[derive(Args)]
pub struct BitlockerArgs {
    /// One volume: a letter (`G:`), a volume path, or on Linux a device.
    /// Without it every partition of every disk is examined.
    #[arg(long)]
    pub volume: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Write the first bytes of the volume to a file, for reading the layout
    /// somewhere else. Only headers and encrypted bytes — no readable data.
    #[arg(long, value_name = "ФАЙЛ")]
    pub dump: Option<String>,

    /// How much `--dump` writes, in KiB.
    #[arg(long, default_value = "64")]
    pub dump_kib: u64,
}

#[cfg(target_os = "windows")]
pub async fn run(args: BitlockerArgs) -> Result<()> {
    use own_backup_inventory::DiskInventory;

    let mut found = Vec::new();
    let inventory = own_backup_inventory::WindowsInventory::new()?;
    let disks = inventory.enumerate_disks().await?;

    for disk in &disks {
        let Some(number) = disk.device_id.rsplit("PhysicalDrive").next().and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        for part in &disk.partitions {
            if let Some(want) = &args.volume {
                let letter = part.volume_letter.map(|c| format!("{c}:")).unwrap_or_default();
                let dev = part.volume_device.clone();
                let want_norm = want.trim_end_matches('\\').to_lowercase();
                if letter.to_lowercase() != want_norm && dev.to_lowercase() != want_norm {
                    continue;
                }
            }
            let label = part
                .volume_letter
                .map(|c| format!("{c}:"))
                .unwrap_or_else(|| format!("{} раздел {}", disk.device_id, part.index));
            found.push(examine(
                number,
                part.offset_bytes,
                part.size_bytes,
                &label,
                &disk.device_id,
                args.dump.as_deref(),
                args.dump_kib,
            ));
        }
    }
    report(found, args.json)
}

/// Read one partition's BitLocker metadata off the physical disk.
#[cfg(target_os = "windows")]
fn examine(
    disk: u32,
    offset: u64,
    size: u64,
    label: &str,
    disk_id: &str,
    dump: Option<&str>,
    dump_kib: u64,
) -> (String, String, std::result::Result<own_backup_backup::bitlocker::BitLockerInfo, String>) {
    use own_backup_backup::fat_map::RawVolume;
    use own_backup_backup::volume_window::VolumeWindow;
    use own_backup_backup::win_device::WinRawDevice;

    let where_ = format!("{disk_id}, смещение {offset}");
    let path = format!(r"\\.\PhysicalDrive{disk}");
    let outcome = (|| -> std::result::Result<_, String> {
        let dev = WinRawDevice::open_read(&path).map_err(|e| e.to_string())?;
        let mut window =
            VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?;
        if let Some(dump) = dump {
            let want = (dump_kib * 1024).min(window.len()) as usize;
            let mut buf = vec![0u8; want];
            window.read_at(0, &mut buf).map_err(|e| e.to_string())?;
            let file = format!("{dump}.{}.bin", label.trim_end_matches(':').replace([' ', '\\'], "_"));
            std::fs::write(&file, &buf).map_err(|e| e.to_string())?;
            println!("  записано {} КиБ в {file}", want / 1024);
        }
        own_backup_backup::bitlocker::inspect_bitlocker(&mut window).map_err(|e| e.to_string())
    })();
    (label.to_string(), where_, outcome)
}

#[cfg(target_os = "linux")]
pub async fn run(args: BitlockerArgs) -> Result<()> {
    use own_backup_common::OwnBackupError;

    let Some(path) = args.volume.clone() else {
        return Err(OwnBackupError::Config(
            "укажите устройство: --volume /dev/sdXN".into(),
        ));
    };
    let outcome = (|| -> std::result::Result<_, String> {
        let mut dev = own_backup_backup::linux_device::LinuxRawDevice::open_read(&path)
            .map_err(|e| e.to_string())?;
        own_backup_backup::bitlocker::inspect_bitlocker(&mut dev).map_err(|e| e.to_string())
    })();
    report(vec![(path.clone(), path, outcome)], args.json)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn run(_args: BitlockerArgs) -> Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Чтение метаданных BitLocker поддерживается на Windows и Linux.".into(),
    ))
}

#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
type Examined = (
    String,
    String,
    std::result::Result<own_backup_backup::bitlocker::BitLockerInfo, String>,
);

#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn report(found: Vec<Examined>, json: bool) -> Result<()> {
    let encrypted: Vec<&Examined> = found.iter().filter(|(_, _, r)| r.is_ok()).collect();

    if !json {
        // Which binary answered. An old exe in another folder looks exactly
        // like a bug in this one — it has already cost a round trip.
        println!(
            "own-backup-cli {} (сборка {})",
            env!("CARGO_PKG_VERSION"),
            crate::BUILD_TAG
        );
    }

    if json {
        let items: Vec<serde_json::Value> = encrypted
            .iter()
            .map(|(label, where_, r)| {
                serde_json::json!({
                    "volume": label,
                    "where": where_,
                    "info": r.as_ref().ok(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if encrypted.is_empty() {
        println!("Томов, зашифрованных BitLocker, не найдено.");
        // A volume we could not even read is worth a line: it is the difference
        // between "no BitLocker here" and "we never got to look".
        for (label, where_, r) in found.iter() {
            if let Err(e) = r {
                if !e.contains("not a BitLocker volume") {
                    println!("  {label} ({where_}): прочитать не удалось — {e}");
                }
            }
        }
        return Ok(());
    }

    for (label, where_, r) in encrypted {
        let info = r.as_ref().unwrap();
        println!("\n=== {label} — BitLocker ({where_})");
        println!(
            "  Шифр:            {} [{:#06x}]{}",
            info.cipher_name,
            info.cipher,
            if info.cipher_supported { "" } else { "  ← не поддерживается" }
        );
        println!(
            "  Размер шифр. области: {} байт ({:.1} GiB)",
            info.encrypted_size,
            info.encrypted_size as f64 / 1_073_741_824.0
        );
        println!("  Сектор:          {} байт", info.sector_size);
        println!(
            "  Перенесённые загрузочные сектора: {} шт., по адресу {}",
            info.nb_backup_sectors, info.boot_backup_addr
        );
        println!("  Копия метаданных: №{}", info.metadata_copy + 1);
        println!("  Способы разблокировки:");
        if info.protectors.is_empty() {
            println!("    (ни одного — метаданные без VMK-датумов)");
        }
        for p in &info.protectors {
            println!(
                "    [{:#06x}] {}{}",
                p.kind,
                p.name,
                if p.usable { "  ← умеем" } else { "" }
            );
        }
        println!(
            "  Итог: {}",
            if !info.cipher_supported {
                "шифр не поддерживается — открыть не сможем".to_string()
            } else if info.has_password() {
                "можно открыть паролем".to_string()
            } else if info.openable() {
                "можно открыть".to_string()
            } else {
                "парольной защиты нет — понадобится ключ восстановления (пока не умеем)".to_string()
            }
        );
        // For checking our table of codes against a real volume.
        println!("  Заголовок метаданных: {}", info.header_hex);
        println!("  Заголовок набора:     {}", info.dataset_hex);
    }
    println!();
    Ok(())
}
