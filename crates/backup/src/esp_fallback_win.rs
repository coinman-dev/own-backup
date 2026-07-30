//! Making a restored ESP bootable on a machine that has never seen it (R11).
//!
//! The reading half of this lives in [`crate::esp_fallback`]; here is the part
//! that touches a disk: find the ESP's volume, look at what is on it, work out
//! which bootloader directory belongs to the system that was just restored, and
//! copy it to `\EFI\BOOT\BOOTX64.EFI`.
//!
//! The ESP is FAT32, so Windows mounts it like any other volume — it simply has
//! no drive letter. A volume GUID path (`\\?\Volume{…}\`) opens it without one,
//! which is what the rest of the restore already uses.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use own_backup_common::restore_plan::PartRef;
use own_backup_common::{OwnBackupError, Result};

use crate::esp_fallback::{
    choose_loader, copies_for, parse_grub_search_uuid, VendorDir, FALLBACK_DIR, FALLBACK_NAME,
};

/// What the operation did, in the words the report shows.
pub struct FallbackOutcome {
    pub message: String,
    pub follow_ups: Vec<String>,
    /// A loader was actually copied — the only case worth telling the user
    /// about, since this step is otherwise invisible.
    pub copied: bool,
    /// Which `EFI\<vendor>` directory it came from, when it was.
    pub from: String,
}

/// Ensure `\EFI\BOOT\BOOTX64.EFI` exists on the ESP at `esp.offset`.
///
/// `restored` are the partitions this run wrote; their filesystem UUIDs are
/// what identifies the live bootloader directory among the leftovers of every
/// system the machine has ever had.
///
/// Never overwrites an existing fallback loader.
pub fn ensure_fallback_bootloader(
    disk_number: u32,
    esp: PartRef,
    restored: &[PartRef],
) -> Result<FallbackOutcome> {
    let root = crate::volume_win::volume_root_at_offset(disk_number, esp.offset).ok_or_else(|| {
        OwnBackupError::Device(format!(
            "Windows не показала том раздела EFI (смещение {}) — загрузчик скопировать некуда",
            esp.offset
        ))
    })?;
    let root = PathBuf::from(&root);
    let efi = root.join("EFI");
    if !efi.is_dir() {
        return Ok(FallbackOutcome {
            copied: false,
            from: String::new(),
            message: "На разделе EFI нет каталога \\EFI — копировать нечего".into(),
            follow_ups: vec![
                "Раздел EFI восстановлен, но каталога \\EFI в нём нет: загрузчика на диске \
                 не оказалось. Восстановленная система сама не загрузится."
                    .into(),
            ],
        });
    }

    // First question, before anything else: is there already a loader at the
    // path the firmware tries by itself? A disk that boots anywhere needs
    // nothing from us — and the one way this operation could do harm is by
    // replacing a working loader with one of our choosing. So it never does.
    let fallback = efi.join("BOOT").join(FALLBACK_NAME);
    if fallback.is_file() {
        let size = std::fs::metadata(&fallback).map(|m| m.len()).unwrap_or(0);
        let known = identify_loader(&fallback);
        info!(
            esp = %root.display(), bytes = size, kind = known,
            "the ESP already has a fallback loader — left alone"
        );
        return Ok(FallbackOutcome {
            message: format!(
                "{FALLBACK_DIR}\\{FALLBACK_NAME} уже есть ({known}, {}) — оставили как есть",
                own_backup_common::units::human_bytes(size)
            ),
            follow_ups: Vec::new(),
            copied: false,
            from: String::new(),
        });
    }

    let uuids = restored_filesystem_uuids(disk_number, restored);
    let vendors = read_vendor_dirs(&efi);
    info!(
        disk = disk_number,
        esp = %root.display(),
        restored_uuids = ?uuids,
        vendors = ?vendors.iter().map(|v| (v.path.as_str(), v.grub_uuid.as_deref())).collect::<Vec<_>>(),
        "inspecting the restored ESP"
    );

    let Some(choice) = choose_loader(&vendors, &uuids) else {
        return Ok(FallbackOutcome {
            copied: false,
            from: String::new(),
            message: "Загрузчика для UEFI на разделе EFI не нашлось".into(),
            follow_ups: vec![format!(
                "На разделе EFI нет ни {FALLBACK_DIR}\\{FALLBACK_NAME}, ни знакомого загрузчика \
                 (shim, GRUB, диспетчер загрузки Windows). Если система с этого диска не \
                 стартует, загрузочную запись придётся создать вручную."
            )],
        });
    };
    info!(
        dir = %choice.dir,
        loader = %choice.loader,
        confirmed_by_uuid = choice.confirmed_by_uuid,
        others = ?choice.other_candidates,
        "chose the bootloader to publish at the fallback path"
    );

    let dest_dir = efi.join("BOOT");
    std::fs::create_dir_all(&dest_dir).map_err(|e| {
        OwnBackupError::Device(format!(
            "не удалось создать {} на разделе EFI: {e}",
            dest_dir.display()
        ))
    })?;

    let copies = copies_for(&vendors, &choice);
    let mut copied = Vec::new();
    for c in &copies {
        let from = root.join(&c.from);
        let to = dest_dir.join(&c.to);
        match std::fs::copy(&from, &to) {
            Ok(bytes) => {
                info!(from = %from.display(), to = %to.display(), bytes, "copied to the fallback path");
                copied.push(c.to.clone());
            }
            Err(e) => {
                warn!(from = %from.display(), to = %to.display(), error = %e, "could not copy");
                // The loader itself is the one file without which the rest is
                // pointless, and a half-populated EFI\BOOT is worse than none.
                if c.to.eq_ignore_ascii_case(FALLBACK_NAME) {
                    return Err(OwnBackupError::Device(format!(
                        "не удалось скопировать загрузчик в {FALLBACK_DIR}\\{FALLBACK_NAME}: {e}"
                    )));
                }
            }
        }
    }

    let mut follow_ups = vec![format!(
        "Загрузчик EFI\\{}\\{} скопирован в {FALLBACK_DIR}\\{FALLBACK_NAME} — по этому пути UEFI \
         ищет загрузчик сама, без записи в NVRAM материнской платы. Именно эта запись не \
         переносится вместе с копией, поэтому на другой машине (и в виртуальной) диск иначе не \
         стартует. На прежней машине ничего не изменится: её собственная запись в NVRAM \
         по-прежнему имеет приоритет.",
        choice.dir, choice.loader
    )];
    if !choice.confirmed_by_uuid && !choice.other_candidates.is_empty() {
        follow_ups.push(format!(
            "На разделе EFI несколько загрузчиков ({}), и определить по UUID, какой из них от \
             восстановленной системы, не удалось — взят EFI\\{}. Если система не загрузится, \
             запустите из UEFI Shell загрузчик из другого каталога.",
            choice.other_candidates.join(", "),
            choice.dir
        ));
    }
    Ok(FallbackOutcome {
        message: format!("Из EFI\\{} скопировано в {FALLBACK_DIR}: {}", choice.dir, copied.join(", ")),
        follow_ups,
        copied: true,
        from: choice.dir.clone(),
    })
}

/// What the loader already at the fallback path appears to be.
///
/// EFI binaries are PE files and carry their origin in plain UTF-16 strings —
/// enough to tell a Windows boot manager from shim or GRUB, which is what a
/// user reading the report wants to know ("что там уже лежит?"). Nothing is
/// decided from this: the file is kept either way.
fn identify_loader(path: &Path) -> &'static str {
    let Ok(bytes) = std::fs::read(path) else { return "загрузчик" };
    // The strings are UTF-16LE inside the binary; matching on the ASCII bytes
    // interleaved with NULs finds them without decoding the whole file.
    let has = |needle: &str| -> bool {
        let wide: Vec<u8> = needle.bytes().flat_map(|b| [b, 0]).collect();
        bytes.windows(wide.len()).any(|w| w == wide)
            || bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    };
    if has("Windows Boot Manager") || has("bootmgfw") || has("BCD") {
        "диспетчер загрузки Windows"
    } else if has("shim") || has("MokManager") {
        "shim (Secure Boot)"
    } else if has("GRUB") || has("grub") {
        "GRUB"
    } else if has("systemd-boot") {
        "systemd-boot"
    } else {
        "загрузчик"
    }
}

/// Filesystem UUIDs of the partitions this restore wrote.
///
/// Read from the target disk rather than from the copy's manifest: on Windows
/// the inventory cannot read an ext4 or btrfs UUID at all, and the bytes are
/// right here.
fn restored_filesystem_uuids(disk_number: u32, restored: &[PartRef]) -> Vec<String> {
    let mut out = Vec::new();
    for p in restored {
        let path = format!("part:{disk_number}:{}:{}", p.offset, p.size);
        let mut dev = match crate::win_device::WinRawDevice::open_read(&path) {
            Ok(d) => d,
            Err(e) => {
                warn!(part = p.index, error = %e, "could not open a restored partition to read its filesystem UUID");
                continue;
            }
        };
        let want = crate::fs_uuid::PROBE_LEN.min(p.size as usize);
        let mut head = vec![0u8; want];
        if let Err(e) = dev.read_at(0, &mut head) {
            warn!(part = p.index, error = %e, "could not read the head of a restored partition");
            continue;
        }
        if let Some(uuid) = crate::fs_uuid::probe(&head) {
            info!(part = p.index, %uuid, "restored filesystem identified");
            out.push(uuid);
        }
    }
    out
}

/// Vendor directories under `EFI\`, with their file names and the UUID their
/// GRUB stub points at.
///
/// One level deep, plus `Microsoft\Boot`, which is where Windows keeps its boot
/// manager and the only nested case worth knowing about.
fn read_vendor_dirs(efi: &Path) -> Vec<VendorDir> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(efi) else { return out };
    for e in entries.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        out.push(vendor_dir(&name, &e.path()));
        let nested = e.path().join("Boot");
        if nested.is_dir() {
            out.push(vendor_dir(&format!("{name}\\Boot"), &nested));
        }
    }
    out
}

fn vendor_dir(path: &str, dir: &Path) -> VendorDir {
    let files: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    // The stub is three lines; reading it costs nothing and is the only thing
    // that says which system this directory belongs to.
    let grub_uuid = files
        .iter()
        .find(|f| f.eq_ignore_ascii_case("grub.cfg"))
        .and_then(|f| std::fs::read_to_string(dir.join(f)).ok())
        .and_then(|text| parse_grub_search_uuid(&text));
    VendorDir { path: path.to_string(), files, grub_uuid }
}
