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

    /// Ask for the passphrase and try to unlock — read-only, and the volume is
    /// left exactly as it was. Says whether the key came out and whether what
    /// it decrypts is a filesystem; prints no key material.
    #[arg(long)]
    pub unlock: bool,

    /// Отдать разблокировку самой Windows (`manage-bde`), а не читать FVE
    /// самим: только так виден ReFS внутри контейнера и шифры вне AES-XTS.
    /// Диск на время закрывается на запись — смонтированная ФС пишет сама.
    #[arg(long)]
    pub windows_unlock: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: BitlockerArgs) -> Result<()> {
    use own_backup_inventory::DiskInventory;

    if args.windows_unlock {
        return windows_unlock(&args).await;
    }

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
            if args.unlock {
                if let Some((_, _, Ok(info))) = found.last() {
                    if info.has_password() {
                        try_unlock(number, part.offset_bytes, part.size_bytes, &label);
                    }
                }
            }
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
        // Windows opens a raw device with FILE_FLAG_NO_BUFFERING, so every read
        // must start on a sector boundary — and the FVE dataset begins 0x40
        // bytes into its block, which is not one. Same wrapper the ext scanner
        // needed for the same reason, and it caches the last window, which the
        // metadata walk reads over and over.
        let mut window = own_backup_backup::aligned_volume::AlignedVolume::new(
            VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?,
        );
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

/// Ask for the passphrase and see whether it opens the volume. Nothing is
/// written, nothing is kept: this answers "does our own crypto work on a real
/// disk", which no test in the tree can answer.
#[cfg(target_os = "windows")]
fn try_unlock(disk: u32, offset: u64, size: u64, label: &str) {
    use own_backup_backup::volume_window::VolumeWindow;
    use own_backup_backup::win_device::WinRawDevice;

    let pass = match ask_passphrase(&format!("  Пароль или ключ восстановления для {label} (не отображается): ")) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            println!("  Пароль не введён — пропускаем.");
            return;
        }
        Err(e) => {
            println!("  Не удалось прочитать пароль: {e}");
            return;
        }
    };

    let path = format!(r"\\.\PhysicalDrive{disk}");
    let outcome = (|| -> std::result::Result<String, String> {
        let dev = WinRawDevice::open_read(&path).map_err(|e| e.to_string())?;
        let mut window = own_backup_backup::aligned_volume::AlignedVolume::new(
            VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?,
        );
        let params = own_backup_backup::bitlocker::open_bitlocker(&mut window, &pass)
            .map_err(|e| e.to_string())?;

        // The key is only useful if what it decrypts is a filesystem, so say
        // which one — that is the end-to-end proof, and it costs one read.
        let dev = WinRawDevice::open_read(&path).map_err(|e| e.to_string())?;
        let window = own_backup_backup::aligned_volume::AlignedVolume::new(
            VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?,
        );
        let mut view =
            own_backup_backup::bitlocker::BitLockerVolume::new(Box::new(window), params);
        use own_backup_backup::fat_map::RawVolume;
        let want = (own_backup_inventory::fs_probe::PROBE_LEN as u64).min(view.len()) as usize;
        let mut buf = vec![0u8; want];
        view.read_at(0, &mut buf).map_err(|e| e.to_string())?;
        Ok(match own_backup_inventory::fs_probe::identify(&buf) {
            Some(p) => format!(
                "{:?}{}",
                p.fs,
                p.label.map(|l| format!(" «{l}»")).unwrap_or_default()
            ),
            None => "файловая система не опознана".into(),
        })
    })();

    match outcome {
        Ok(fs) => println!("  ✓ Пароль подошёл, ключ тома получен. Внутри: {fs}"),
        Err(e) => {
            println!("  ✗ Открыть не удалось: {e}");
            // A passphrase Windows accepts and we do not means the fault is
            // ours, and the shape of the datums is where to look. Types and
            // sizes only — nothing here is worth keeping secret.
            // The key unwrapped but what it decrypts is unrecognisable? Then
            // the passphrase was right and the LAYOUT is wrong — and with the
            // key in hand the layout can simply be asked.
            if e.contains("not an NTFS boot sector") {
                println!("  Ключ получен — значит пароль верный. Ищем, где лежит первый сектор:");
                let path = format!(r"\\.\PhysicalDrive{disk}");
                let probed = (|| -> std::result::Result<Vec<String>, String> {
                    let dev = WinRawDevice::open_read(&path).map_err(|e| e.to_string())?;
                    let mut window = own_backup_backup::aligned_volume::AlignedVolume::new(
                        VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?,
                    );
                    let params =
                        own_backup_backup::bitlocker::open_bitlocker_unverified(&mut window, &pass)
                            .map_err(|e| e.to_string())?;
                    Ok(own_backup_backup::bitlocker::diagnose_first_sector(&mut window, &params))
                })();
                match probed {
                    Ok(lines) => lines.iter().for_each(|l| println!("    {l}")),
                    Err(e) => println!("    не удалось: {e}"),
                }
            }
            println!("  Структура датумов (типы и размеры, без ключей):");
            let path = format!(r"\\.\PhysicalDrive{disk}");
            let described = (|| -> std::result::Result<Vec<String>, String> {
                let dev = WinRawDevice::open_read(&path).map_err(|e| e.to_string())?;
                let mut window = own_backup_backup::aligned_volume::AlignedVolume::new(
                    VolumeWindow::new(Box::new(dev), offset, size).map_err(|e| e.to_string())?,
                );
                own_backup_backup::bitlocker::describe_vmks(&mut window).map_err(|e| e.to_string())
            })();
            match described {
                Ok(lines) => lines.iter().for_each(|l| println!("    {l}")),
                Err(e) => println!("    не прочитать: {e}"),
            }
        }
    }
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
/// Read a passphrase from the console without echoing it.
#[cfg(target_os = "windows")]
fn ask_passphrase(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
        STD_INPUT_HANDLE,
    };
    print!("{prompt}");
    std::io::stdout().flush()?;

    // SAFETY: console-mode query and set on our own standard handle; the mode
    // is put back below whatever the read does.
    let restore = unsafe {
        GetStdHandle(STD_INPUT_HANDLE).ok().and_then(|h| {
            let mut mode = CONSOLE_MODE(0);
            GetConsoleMode(h, &mut mode).ok()?;
            SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT).ok()?;
            Some((h, mode))
        })
    };
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if let Some((h, mode)) = restore {
        // SAFETY: same handle, the mode it had before.
        let _ = unsafe { SetConsoleMode(h, mode) };
    }
    println!();
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}


/// Стенд этапа B5: проверить, умеет ли ЭТА система открыть том сама.
///
/// Проверяется по одному: есть ли `manage-bde` (он приходит с компонентом
/// WinPE-SecureStartup, а носитель у нас на базе WinRE — там он, скорее всего,
/// уже есть); встаёт ли запрет записи на диск; открывается ли том; что
/// оказалось внутри. Запрет снимается всегда — страж делает это в `Drop`.
#[cfg(target_os = "windows")]
async fn windows_unlock(args: &BitlockerArgs) -> Result<()> {
    use own_backup_backup::windows_unlock_win as wu;

    let Some(volume) = args.volume.clone() else {
        return Err(own_backup_common::OwnBackupError::Config(
            "укажите том: --volume G:".into(),
        ));
    };
    let volume = volume.trim_end_matches('\\').to_string();

    match wu::manage_bde_path() {
        Some(p) => println!("manage-bde:   {}", p.display()),
        None => {
            println!("manage-bde:   НЕТ В ЭТОЙ СИСТЕМЕ");
            println!();
            println!("Значит носитель собран без компонента WinPE-SecureStartup, и");
            println!("разблокировка силами Windows тут невозможна. Своё чтение FVE");
            println!("продолжает работать — оно и открывает NTFS внутри BitLocker.");
            return Ok(());
        }
    }

    // Диск, на котором лежит том: нужен, чтобы закрыть его на запись.
    let extents = own_backup_backup::volume_win::volume_extents(&volume);
    let [extent] = extents.as_slice() else {
        return Err(own_backup_common::OwnBackupError::Device(format!(
            "{volume}: том занимает {} экстентов — такой стенд их не поддерживает",
            extents.len()
        )));
    };
    println!("Том:          {volume} (диск {}, смещение {})", extent.disk_number, extent.offset);

    // Системный диск не трогаем даже так: запрет записи на него — не то, что
    // можно ставить между делом.
    if own_backup_restore::guard::system_disk_number() == Some(extent.disk_number) {
        return Err(own_backup_common::OwnBackupError::Config(
            "это системный диск — запрет записи на него ставить нельзя".into(),
        ));
    }

    // Тот же ввод без эха, что у обычной разблокировки, — своя реализация уже
    // есть, и она умеет молчать в консоли.
    let secret = ask_passphrase("Ключ восстановления или пароль тома: ")
        .map_err(|e| own_backup_common::OwnBackupError::Config(format!("ввод: {e}")))?;
    let secret = secret.trim().to_string();
    let digits: String = secret.chars().filter(|c| c.is_ascii_digit()).collect();
    let looks_like_key = digits.len() == 48 && secret.chars().all(|c| c.is_ascii_digit() || c == '-' || c == ' ');

    let guard = wu::ReadOnlyDisk::arm(extent.disk_number)?;
    println!("Диск закрыт на запись — монтирование ничего в нём не изменит.");

    let dashed = digits_with_dashes(&digits);
    let outcome = wu::unlock_with_windows(
        &volume,
        if looks_like_key {
            wu::Secret::RecoveryPassword(&dashed)
        } else {
            wu::Secret::Passphrase(&secret)
        },
    );
    match &outcome {
        Ok(()) => println!("\n✓ Windows открыла том."),
        Err(e) => println!("\n✗ {e}"),
    }

    // Что теперь видно на этом томе.
    if outcome.is_ok() {
        match own_backup_backup::win_device::WinRawDevice::open_read(&format!("\\\\.\\{volume}")) {
            Ok(mut dev) => {
                let mut boot = vec![0u8; 4096];
                match own_backup_backup::fat_map::RawVolume::read_at(&mut dev, 0, &mut boot) {
                    Ok(()) => {
                        let fs = if &boot[3..11] == b"NTFS    " {
                            "NTFS"
                        } else if &boot[3..11] == b"ReFS\0\0\0\0" || &boot[0..8] == b"\0\0\0ReFS\0" {
                            "ReFS"
                        } else if &boot[3..11] == b"EXFAT   " {
                            "exFAT"
                        } else {
                            "неизвестна"
                        };
                        println!("Файловая система внутри: {fs}");
                    }
                    Err(e) => println!("Том открылся, но читать его не даёт: {e}"),
                }
            }
            Err(e) => println!("Устройство тома не открылось: {e}"),
        }
    }

    guard.disarm()?;
    println!("Запрет записи снят. Том остался открытым до перезагрузки.");
    outcome
}

/// `123456789012…` → `123456-789012-…`, как ждёт manage-bde.
#[cfg(target_os = "windows")]
fn digits_with_dashes(digits: &str) -> String {
    digits
        .as_bytes()
        .chunks(6)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(not(target_os = "windows"))]
fn ask_passphrase(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

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
                "    [{:#06x}] {}{}{}",
                p.kind,
                p.name,
                if p.count > 1 { format!(" ×{}", p.count) } else { String::new() },
                if p.usable { "  ← умеем" } else { "" }
            );
        }
        println!(
            "  Итог: {}",
            if !info.cipher_supported {
                "шифр не поддерживается — открыть не сможем".to_string()
            } else if info.has_password() {
                "можно открыть паролем тома".to_string()
            } else if info.openable() {
                // A system volume: Windows allows no passphrase protector there,
                // so the 48-digit recovery key is the way in.
                "парольной защиты нет — открывается 48-значным ключом восстановления".to_string()
            } else {
                "нечем открыть: ни пароля, ни ключа восстановления среди защит нет".to_string()
            }
        );
        // For checking our table of codes against a real volume.
        println!("  Заголовок метаданных: {}", info.header_hex);
        println!("  Заголовок набора:     {}", info.dataset_hex);
    }
    println!();
    Ok(())
}
