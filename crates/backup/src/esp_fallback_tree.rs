//! Making a restored ESP bootable, given its mounted root — the part of
//! [`crate::esp_fallback_win`] that is about files, shared with Linux.
//!
//! The decision is [`crate::esp_fallback`]; this looks at what is on the ESP,
//! applies the decision and copies the files. Finding and mounting the ESP is
//! the caller's: a volume GUID path on Windows, a temporary mount on Linux.
//! ESP-relative paths are spelled the firmware's way (`EFI\ubuntu\…`) and
//! split into components here, so the same code walks both systems.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

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
    /// The copy took the place of a GRUB already at the path, rather than
    /// filling an empty one.
    pub replaced: bool,
}

/// `rel` (firmware spelling, backslashes) under `root`.
fn under(root: &Path, rel: &str) -> PathBuf {
    rel.split('\\')
        .filter(|c| !c.is_empty())
        .fold(root.to_path_buf(), |p, c| p.join(c))
}

/// Ensure `\EFI\BOOT\BOOTX64.EFI` exists on the ESP mounted at `root`.
///
/// `uuids` are the filesystem UUIDs of the partitions this run wrote; they are
/// what identifies the live bootloader directory among the leftovers of every
/// system the machine has ever had. `label` names the ESP in the log.
///
/// Never overwrites an existing fallback loader, with one exception: a plain
/// copy of GRUB whose own directory has shim beside it ([`vendors_behind`]).
pub fn ensure_at_root(root: &Path, uuids: &[String], label: &str) -> Result<FallbackOutcome> {
    let efi = root.join("EFI");
    if !efi.is_dir() {
        return Ok(FallbackOutcome {
            copied: false,
            from: String::new(),
            replaced: false,
            message: tr!("esp_fallback_win.no_efi_dir").into_owned(),
            follow_ups: vec![tr!("esp_fallback_win.no_efi_dir_follow_up").into_owned()],
        });
    }
    let fallback_path = format!("{FALLBACK_DIR}\\{FALLBACK_NAME}");

    // First question, before anything else: is there already a loader at the
    // path the firmware tries by itself? A disk that boots anywhere needs
    // nothing from us — and the one way this operation could do harm is by
    // replacing a working loader with one of our choosing. So it does not,
    // unless the file there is a plain copy of a GRUB that has its shim beside
    // it: that one starts only with Secure Boot off (a Kubuntu in a Hyper-V
    // machine, 30.10), and shim starts both ways and leads to the same GRUB.
    let fallback = efi.join("BOOT").join(FALLBACK_NAME);
    let mut vendors = read_vendor_dirs(&efi);
    let exists = fallback.is_file();
    let behind = if exists {
        vendors_behind(root, &fallback, &vendors)
    } else {
        Vec::new()
    };
    let replacing = !behind.is_empty();
    if replacing {
        vendors = behind;
    }
    if exists && !replacing {
        let size = std::fs::metadata(&fallback).map(|m| m.len()).unwrap_or(0);
        let known = identify_loader(&fallback);
        info!(
            esp = %root.display(), bytes = size, kind = %known,
            "the ESP already has a fallback loader — left alone"
        );
        return Ok(FallbackOutcome {
            message: tr!(
                "esp_fallback_win.already_there",
                fallback = fallback_path,
                kind = known,
                size = own_backup_common::units::human_bytes(size)
            )
            .into_owned(),
            follow_ups: Vec::new(),
            copied: false,
            from: String::new(),
            replaced: false,
        });
    }

    info!(
        esp = %label,
        root = %root.display(),
        restored_uuids = ?uuids,
        vendors = ?vendors.iter().map(|v| (v.path.as_str(), v.grub_uuid.as_deref())).collect::<Vec<_>>(),
        "inspecting the restored ESP"
    );

    let Some(choice) = choose_loader(&vendors, uuids) else {
        return Ok(FallbackOutcome {
            copied: false,
            from: String::new(),
            replaced: false,
            message: tr!("esp_fallback_win.no_loader").into_owned(),
            follow_ups: vec![tr!(
                "esp_fallback_win.no_loader_follow_up",
                fallback = fallback_path
            )
            .into_owned()],
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
        OwnBackupError::Device(
            tr!(
                "esp_fallback_win.mkdir_failed",
                dir = dest_dir.display(),
                error = e
            )
            .into_owned(),
        )
    })?;

    let mut copies = copies_for(&vendors, &choice);
    if replacing {
        replace_loader(root, &dest_dir, &mut copies, &fallback_path)?;
    }
    let mut copied = Vec::new();
    for c in &copies {
        let from = under(root, &c.from);
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
                    return Err(OwnBackupError::Device(
                        tr!(
                            "esp_fallback_win.copy_failed",
                            fallback = fallback_path,
                            error = e
                        )
                        .into_owned(),
                    ));
                }
            }
        }
    }

    let mut follow_ups = vec![tr!(
        "esp_fallback_win.copied_follow_up",
        dir = choice.dir,
        loader = choice.loader,
        fallback = fallback_path
    )
    .into_owned()];
    if !choice.confirmed_by_uuid && !choice.other_candidates.is_empty() {
        follow_ups.push(
            tr!(
                "esp_fallback_win.several_loaders",
                candidates = choice.other_candidates.join(", "),
                dir = choice.dir
            )
            .into_owned(),
        );
    }
    if replacing {
        info!(dir = %choice.dir, "shim took the place of a plain copy of GRUB at the fallback path");
        return Ok(FallbackOutcome {
            message: tr!(
                "esp_fallback_win.grub_replaced",
                fallback = fallback_path.as_str(),
                dir = choice.dir.as_str()
            )
            .into_owned(),
            follow_ups: Vec::new(),
            copied: true,
            from: choice.dir,
            replaced: true,
        });
    }
    Ok(FallbackOutcome {
        message: tr!(
            "esp_fallback_win.copied",
            dir = choice.dir,
            fallback_dir = FALLBACK_DIR,
            files = copied.join(", ")
        )
        .into_owned(),
        follow_ups,
        copied: true,
        from: choice.dir.clone(),
        replaced: false,
    })
}

/// The vendor directories whose `grubx64.efi` is byte for byte the file at the
/// fallback path and which have `shimx64.efi` too. Empty for anything else: a
/// shim, Windows' boot manager or a GRUB of unknown origin is left alone.
fn vendors_behind(root: &Path, fallback: &Path, vendors: &[VendorDir]) -> Vec<VendorDir> {
    let Ok(current) = std::fs::read(fallback) else {
        return Vec::new();
    };
    let named = |d: &VendorDir, name: &str| {
        d.files
            .iter()
            .find(|f| f.eq_ignore_ascii_case(name))
            .cloned()
    };
    vendors
        .iter()
        .filter(|d| !d.path.eq_ignore_ascii_case("BOOT"))
        .filter(|d| named(d, "shimx64.efi").is_some())
        .filter(|d| {
            named(d, "grubx64.efi").is_some_and(|grub| {
                std::fs::read(under(root, &format!("EFI\\{}\\{grub}", d.path)))
                    .is_ok_and(|bytes| bytes == current)
            })
        })
        .cloned()
        .collect()
}

/// Put everything shim needs in place first and shim itself last, in one
/// rename, so that a failure at any point leaves the GRUB that was there, in
/// working order. The loader is taken off `copies`: nothing is left to copy.
fn replace_loader(
    root: &Path,
    dest_dir: &Path,
    copies: &mut Vec<crate::esp_fallback::Copy>,
    fallback_path: &str,
) -> Result<()> {
    let failed = |e: std::io::Error| {
        OwnBackupError::Device(
            tr!(
                "esp_fallback_win.copy_failed",
                fallback = fallback_path,
                error = e
            )
            .into_owned(),
        )
    };
    let (loader, companions): (Vec<_>, Vec<_>) = copies
        .drain(..)
        .partition(|c| c.to.eq_ignore_ascii_case(FALLBACK_NAME));
    for c in &companions {
        std::fs::copy(under(root, &c.from), dest_dir.join(&c.to)).map_err(failed)?;
    }
    let Some(loader) = loader.first() else {
        return Ok(());
    };
    let staged = dest_dir.join("BOOTX64.NEW");
    std::fs::copy(under(root, &loader.from), &staged).map_err(failed)?;
    std::fs::rename(&staged, dest_dir.join(FALLBACK_NAME)).map_err(failed)?;
    Ok(())
}

/// What the loader already at the fallback path appears to be.
///
/// EFI binaries are PE files and carry their origin in plain UTF-16 strings —
/// enough to tell a Windows boot manager from shim or GRUB, which is what a
/// user reading the report wants to know ("что там уже лежит?"). Nothing is
/// decided from this: the file is kept either way.
fn identify_loader(path: &Path) -> Cow<'static, str> {
    let Ok(bytes) = std::fs::read(path) else {
        return tr!("esp_fallback_win.loader");
    };
    // The strings are UTF-16LE inside the binary; matching on the ASCII bytes
    // interleaved with NULs finds them without decoding the whole file.
    let has = |needle: &str| -> bool {
        let wide: Vec<u8> = needle.bytes().flat_map(|b| [b, 0]).collect();
        bytes.windows(wide.len()).any(|w| w == wide)
            || bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    };
    if has("Windows Boot Manager") || has("bootmgfw") || has("BCD") {
        tr!("esp_fallback_win.windows_boot_manager")
    } else if has("shim") || has("MokManager") {
        Cow::Borrowed("shim (Secure Boot)")
    } else if has("GRUB") || has("grub") {
        Cow::Borrowed("GRUB")
    } else if has("systemd-boot") {
        Cow::Borrowed("systemd-boot")
    } else {
        tr!("esp_fallback_win.loader")
    }
}

/// Vendor directories under `EFI\`, with their file names and the UUID their
/// GRUB stub points at.
///
/// One level deep, plus `Microsoft\Boot`, which is where Windows keeps its boot
/// manager and the only nested case worth knowing about.
fn read_vendor_dirs(efi: &Path) -> Vec<VendorDir> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(efi) else {
        return out;
    };
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
    VendorDir {
        path: path.to_string(),
        files,
        grub_uuid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "90a2b1c3-1111-2222-3333-444455556666";

    fn esp_with_two_vendors() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (vendor, uuid) in [
            ("ubuntu", "00000000-0000-0000-0000-000000000000"),
            ("kubuntu", UUID),
        ] {
            let d = dir.path().join("EFI").join(vendor);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("shimx64.efi"), format!("shim of {vendor}")).unwrap();
            std::fs::write(d.join("grubx64.efi"), b"grub").unwrap();
            std::fs::write(d.join("mmx64.efi"), b"mm").unwrap();
            std::fs::write(
                d.join("grub.cfg"),
                format!("search.fs_uuid {uuid} root hd0,gpt2\nset prefix=($root)'/grub'\n"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn the_directory_whose_stub_names_the_restored_filesystem_is_published() {
        let esp = esp_with_two_vendors();
        let out = ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        assert!(out.copied);
        assert_eq!(out.from, "kubuntu");
        let boot = esp.path().join("EFI").join("BOOT");
        assert_eq!(
            std::fs::read_to_string(boot.join(FALLBACK_NAME)).unwrap(),
            "shim of kubuntu"
        );
        assert!(boot.join("grubx64.efi").is_file());
        assert!(boot.join("mmx64.efi").is_file());
    }

    #[test]
    fn an_existing_fallback_loader_is_left_alone() {
        let esp = esp_with_two_vendors();
        let boot = esp.path().join("EFI").join("BOOT");
        std::fs::create_dir_all(&boot).unwrap();
        std::fs::write(boot.join(FALLBACK_NAME), b"theirs").unwrap();
        let out = ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        assert!(!out.copied);
        assert_eq!(std::fs::read(boot.join(FALLBACK_NAME)).unwrap(), b"theirs");
    }

    /// The ESP of a Kubuntu: GRUB at the fallback path, shim only in its own
    /// directory. The firmware's Secure Boot refuses that GRUB.
    fn esp_with_grub_at_the_fallback_path() -> (tempfile::TempDir, PathBuf) {
        let esp = esp_with_two_vendors();
        let boot = esp.path().join("EFI").join("BOOT");
        std::fs::create_dir_all(&boot).unwrap();
        std::fs::write(boot.join(FALLBACK_NAME), b"grub").unwrap();
        (esp, boot)
    }

    #[test]
    fn a_copy_of_grub_gives_way_to_its_shim() {
        let (esp, boot) = esp_with_grub_at_the_fallback_path();
        let out = ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        assert!(out.copied && out.replaced);
        assert_eq!(out.from, "kubuntu");
        assert_eq!(
            std::fs::read_to_string(boot.join(FALLBACK_NAME)).unwrap(),
            "shim of kubuntu"
        );
        assert_eq!(std::fs::read(boot.join("grubx64.efi")).unwrap(), b"grub");
        assert!(boot.join("grub.cfg").is_file());
        assert!(boot.join("mmx64.efi").is_file());
        assert!(!boot.join("BOOTX64.NEW").exists());
    }

    #[test]
    fn a_second_run_finds_shim_and_leaves_it() {
        let (esp, boot) = esp_with_grub_at_the_fallback_path();
        ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        let out = ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        assert!(!out.copied);
        assert_eq!(
            std::fs::read_to_string(boot.join(FALLBACK_NAME)).unwrap(),
            "shim of kubuntu"
        );
    }

    #[test]
    fn a_grub_with_no_shim_beside_it_stays() {
        let (esp, boot) = esp_with_grub_at_the_fallback_path();
        for vendor in ["ubuntu", "kubuntu"] {
            std::fs::remove_file(esp.path().join("EFI").join(vendor).join("shimx64.efi")).unwrap();
        }
        let out = ensure_at_root(esp.path(), &[UUID.to_string()], "test").unwrap();
        assert!(!out.copied);
        assert_eq!(std::fs::read(boot.join(FALLBACK_NAME)).unwrap(), b"grub");
    }

    #[test]
    fn a_failure_leaves_the_grub_that_was_there() {
        let (esp, boot) = esp_with_grub_at_the_fallback_path();
        // A directory where a companion goes: its copy fails.
        std::fs::create_dir(boot.join("mmx64.efi")).unwrap();
        assert!(ensure_at_root(esp.path(), &[UUID.to_string()], "test").is_err());
        assert_eq!(std::fs::read(boot.join(FALLBACK_NAME)).unwrap(), b"grub");
    }

    #[test]
    fn firmware_paths_are_split_into_components() {
        assert_eq!(
            under(Path::new("/esp"), "EFI\\Microsoft\\Boot\\bootmgfw.efi"),
            Path::new("/esp/EFI/Microsoft/Boot/bootmgfw.efi")
        );
    }
}
