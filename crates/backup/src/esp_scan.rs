//! What a file on an EFI System Partition is, by its name.
//!
//! The shared half of [`crate::esp_scan_win`] and `esp_scan_linux`. The rules
//! do no I/O, so they can be tested against the real partition of a real
//! machine without that machine; [`collect_loaders`] lists a partition both
//! systems have already made readable as a directory.
//!
//! ## Why not by what is inside the file
//!
//! The first version of this read the binary and matched the strings in it,
//! the way [`crate::esp_fallback_win`] does for its one narrow question. On a
//! partition with five vendor directories it was wrong about most files
//! (2026-09-18, this machine):
//!
//! | file | strings said | it is |
//! |---|---|---|
//! | `kubuntu\shimx64.efi` | MokManager | shim |
//! | `kubuntu\grubx64.efi` | shim | GRUB |
//! | `r-drive\bootx64.efi` | Windows boot manager | R-Drive Image |
//!
//! None of that is surprising once seen: shim carries the name of the manager
//! it launches, GRUB carries the name of the shim that launches it, and a
//! loader built to chain into Windows carries Windows' own file names. The
//! strings are all really there. They just answer a different question.
//!
//! The file name does not have that problem. `shimx64.efi` is shim everywhere,
//! by convention older than any of these tools, and the directory above it
//! names the vendor. The bytes are consulted only when the name says nothing.

use std::borrow::Cow;

use own_backup_common::tr;

/// A catalog text as `&'static str`, for the names that have always been
/// handed out that way — [`kind_by_name`], `Loader::kind`, the driver roles of
/// [`crate::media_layout::rescue_role`] — and are compared as such by their
/// tests.
///
/// A lookup without parameters hands out the catalog's own static text, so in
/// practice this only unwraps it. Should it ever come back owned, the text is
/// kept once and reused: a dozen short names per language, not one leak per
/// call.
pub(crate) fn static_text(text: Cow<'static, str>) -> &'static str {
    use std::sync::Mutex;
    match text {
        Cow::Borrowed(s) => s,
        Cow::Owned(s) => {
            static KEPT: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
            let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(&same) = kept.iter().find(|k| **k == s) {
                return same;
            }
            let leaked: &'static str = Box::leak(s.into_boxed_str());
            kept.push(leaked);
            leaked
        }
    }
}

/// Is this a file a boot entry would sensibly name?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Something the firmware can be pointed at: the first thing that runs.
    EntryPoint,
    /// Something another loader launches. Real, needed, and never the target of
    /// a boot entry — listing these as "loaders nobody points at" buries the
    /// ones that are (on this machine: nine of twelve).
    SecondStage,
}

/// The last component of an EFI path, lower-cased.
fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .to_lowercase()
}

/// The directory holding it, lower-cased — the vendor, by convention.
fn vendor(path: &str) -> String {
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    // `\EFI\<vendor>\file` and `\EFI\<vendor>\Boot\file` both name the vendor
    // second.
    parts.get(1).map(|s| s.to_lowercase()).unwrap_or_default()
}

/// What launches this, and therefore whether it belongs in a boot menu.
pub fn role(path: &str) -> Role {
    // Acronis' boot agent is put on the firmware's "next boot" by Acronis
    // itself, for one boot, and taken off again by its own `bootwiz.xml`
    // (`RemoveFakeBootNext`). A permanent entry for it is nothing Acronis
    // would make; offering one listed it as an orphan beside the real menu
    // (2026-10-15, this machine).
    if vendor(path) == "acronis" && file_name(path) == "bootwiz.efi" {
        return Role::SecondStage;
    }
    // Launched by shim, by GRUB, or by the Windows boot manager — never by the
    // firmware, and never worth an entry of its own.
    const SECOND_STAGE: [&str; 7] = [
        "grubx64.efi", // shim's second stage
        "mmx64.efi",   // MokManager, launched by shim
        "fbx64.efi",   // shim's fallback, rebuilds entries by itself
        "bootmgr.efi", // the inner half of the Windows boot manager
        "memtest.efi", // a tool Windows offers from its own menu
        "securebootrecovery.efi",
        "shimx64.efi.signed",
    ];
    if SECOND_STAGE.contains(&file_name(path).as_str()) {
        Role::SecondStage
    } else {
        Role::EntryPoint
    }
}

/// What this file is, in words (the window's language), from its name and the
/// directory above it.
///
/// `None` when the name says nothing and the caller should look inside.
pub fn kind_by_name(path: &str) -> Option<&'static str> {
    let file = file_name(path);
    let vendor = vendor(path);

    // The vendor directory is the stronger signal when it is one we know: a
    // loader in `EFI\acronis` is Acronis' whatever it is built from.
    let by_vendor = match vendor.as_str() {
        "ownbackup" => Some("Own Backup"),
        "acronis" => Some("Acronis"),
        "r-drive" | "rdrive" => Some("R-Drive Image"),
        "microsoft" => Some("Windows"),
        "ubuntu" => Some("Ubuntu"),
        "kubuntu" => Some("Kubuntu"),
        "debian" => Some("Debian"),
        "fedora" => Some("Fedora"),
        "opensuse" => Some("openSUSE"),
        "arch" => Some("Arch"),
        "systemd" => Some("systemd-boot"),
        _ => None,
    };

    // Files a vendor ships under a generic name, told apart by which one it is.
    if let Some(v) = by_vendor {
        if let Some(named) = vendor_file(v, &file) {
            return Some(named);
        }
    }
    match (by_vendor, file_kind(&file)) {
        (Some(v), Some(_)) => Some(join(v, &file)),
        (Some(v), None) => Some(v),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    }
}

/// What a file is by its name alone. `file` is lower-cased.
fn file_kind(file: &str) -> Option<&'static str> {
    Some(match file {
        "shimx64.efi" => static_text(tr!("esp_scan.shim")),
        "grubx64.efi" => "GRUB",
        "mmx64.efi" => static_text(tr!("esp_scan.mok_manager")),
        "fbx64.efi" => static_text(tr!("esp_scan.fallback")),
        "bootmgfw.efi" => static_text(tr!("esp_scan.boot_manager")),
        "bootmgr.efi" => static_text(tr!("esp_scan.boot_manager_inner")),
        "memtest.efi" => static_text(tr!("esp_scan.memtest")),
        "securebootrecovery.efi" => static_text(tr!("esp_scan.secure_boot_recovery")),
        "systemd-bootx64.efi" => "systemd-boot",
        _ => return None,
    })
}

/// A vendor's own loaders that all carry a generic name. Acronis keeps three
/// different programs in one directory; our own boot environment is the
/// Windows boot manager under our directory name.
fn vendor_file(vendor: &'static str, file: &str) -> Option<&'static str> {
    Some(match (vendor, file) {
        ("Acronis", "bootx64.efi") => static_text(tr!("esp_scan.acronis_recovery_menu")),
        ("Acronis", "bootia32.efi") => static_text(tr!("esp_scan.acronis_recovery_menu_32")),
        ("Acronis", "bootwiz.efi") => static_text(tr!("esp_scan.acronis_boot_agent")),
        ("Own Backup", "bootx64.efi") => static_text(tr!("esp_scan.own_backup_boot")),
        _ => return None,
    })
}

/// `"Ubuntu"` + `grubx64.efi` → `"Ubuntu · GRUB"`, without allocating at
/// runtime.
///
/// The pairs are few and known, so they are spelled out. Matched on the file's
/// NAME, not on what [`file_kind`] calls it: that is in the window's language.
/// A known vendor with a file not listed here is named by the vendor alone.
fn join(vendor: &'static str, file: &str) -> &'static str {
    match (vendor, file) {
        ("Ubuntu", "shimx64.efi") => "Ubuntu · shim",
        ("Ubuntu", "grubx64.efi") => "Ubuntu · GRUB",
        ("Ubuntu", "mmx64.efi") => "Ubuntu · MokManager",
        ("Kubuntu", "shimx64.efi") => "Kubuntu · shim",
        ("Kubuntu", "grubx64.efi") => "Kubuntu · GRUB",
        ("Kubuntu", "mmx64.efi") => "Kubuntu · MokManager",
        ("Windows", "bootmgfw.efi") => static_text(tr!("esp_scan.windows_boot_manager")),
        ("Windows", "bootmgr.efi") => static_text(tr!("esp_scan.windows_inner")),
        ("Windows", "memtest.efi") => static_text(tr!("esp_scan.windows_memtest")),
        ("Windows", "securebootrecovery.efi") => static_text(tr!("esp_scan.secure_boot_recovery")),
        (v, _) => v,
    }
}

// ---- the walk over a readable partition -------------------------------------
//
// The one part of this module that touches files: both systems reach the
// partition as an ordinary directory — Windows through a volume GUID path,
// Linux through a mount — and list it the same way.

/// One `.efi` found on a partition, before anyone has said which entry names
/// it.
#[derive(Debug, Clone)]
pub struct FoundLoader {
    /// Inside the partition, the way a boot entry would spell it.
    pub path: String,
    pub size: u64,
    /// What it is, in words — in the window's language.
    pub kind: &'static str,
    /// Neither its name nor its contents said what it is — `kind` is the
    /// generic «загрузчик». The boot page then offers the vendor folder's name
    /// for a new entry instead of that word; a flag, because the word itself is
    /// shown in the window's language.
    pub generic: bool,
    /// Whether a boot entry would sensibly name it, or another loader launches
    /// it. See [`role`].
    pub role: Role,
    /// The removable-media path every firmware tries by itself.
    pub fallback: bool,
}

/// Every `.efi` under `EFI\` of the partition at `root`, two levels deep plus
/// `<vendor>\Boot`.
///
/// Deep enough for every vendor directory that exists in practice, and shallow
/// enough that a partition holding a whole installer tree does not turn the
/// list into a file dump. FAT is case-insensitive on both systems, so `EFI`
/// and `Boot` find `efi` and `BOOT` too.
pub fn collect_loaders(root: &std::path::Path) -> Vec<FoundLoader> {
    let efi = root.join("EFI");
    let mut out = Vec::new();
    let Ok(vendors) = std::fs::read_dir(&efi) else {
        // No `EFI\` at all: a formatted but unused ESP.
        return out;
    };
    for vendor in vendors.flatten() {
        if !vendor.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let vendor_name = vendor.file_name().to_string_lossy().to_string();
        let mut dirs = vec![(vendor.path(), format!("EFI\\{vendor_name}"))];
        // Windows keeps its loader one level further down, and it is the one
        // entry on most machines.
        let nested = vendor.path().join("Boot");
        if nested.is_dir() {
            dirs.push((nested, format!("EFI\\{vendor_name}\\Boot")));
        }
        for (dir, prefix) in dirs {
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                if !name.to_lowercase().ends_with(".efi") {
                    continue;
                }
                let path = format!("\\{prefix}\\{name}");
                let known = kind_by_name(&path).or_else(|| identify(&f.path()));
                out.push(FoundLoader {
                    kind: known.unwrap_or_else(|| static_text(tr!("esp_scan_win.generic"))),
                    generic: known.is_none(),
                    role: role(&path),
                    size: f.metadata().map(|m| m.len()).unwrap_or(0),
                    fallback: crate::efivars::is_fallback_path(&path),
                    path,
                });
            }
        }
    }
    out.sort_by_key(|a| a.path.to_lowercase());
    out
}

/// A last resort for a file whose name says nothing.
///
/// An EFI binary is a PE file and carries strings in plain UTF-16. Those
/// strings are a poor witness — see the table at the top of this module for
/// the three files on this machine they got wrong — so this runs only after the
/// name and the vendor directory have both come up empty, and its answers stay
/// vague on purpose.
fn identify(path: &std::path::Path) -> Option<&'static str> {
    let bytes = std::fs::read(path).ok()?;
    let has = |needle: &str| -> bool {
        let wide: Vec<u8> = needle.bytes().flat_map(|b| [b, 0]).collect();
        bytes.windows(wide.len()).any(|w| w == wide)
            || bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    };
    // Only names that cannot be mistaken for a string another loader carries
    // about its neighbours.
    if has("Acronis") {
        Some(static_text(tr!("esp_scan_win.looks_like_acronis")))
    } else if has("R-Drive") || has("RDrive") {
        Some(static_text(tr!("esp_scan_win.looks_like_rdrive")))
    } else if has("systemd-boot") {
        Some("systemd-boot")
    } else {
        None
    }
}

/// Which entry, if any, names `path` on the partition `partition_guid`.
/// `entries` are the firmware's entries as (number, load option).
pub fn naming_entry<'a>(
    entries: impl IntoIterator<Item = (u16, &'a crate::efivars::LoadOption)>,
    partition_guid: &str,
    path: &str,
) -> Option<u16> {
    entries.into_iter().find_map(|(number, option)| {
        let hd = option.hard_drive()?;
        let guid = hd.partition_guid()?;
        let file = option.file_path()?;
        // The partition is matched by GUID, never by its number: two disks each
        // have a partition 1 here, and both hold a loader.
        (guid.eq_ignore_ascii_case(partition_guid) && crate::efivars::same_efi_path(&file, path))
            .then_some(number)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three files the string-matching version got wrong, and the one it
    /// got right. All four are on this machine's ESP.
    #[test]
    fn the_files_that_lie_about_themselves_are_named_correctly() {
        assert_eq!(
            kind_by_name(r"\EFI\kubuntu\shimx64.efi"),
            Some("Kubuntu · shim")
        );
        assert_eq!(
            kind_by_name(r"\EFI\kubuntu\grubx64.efi"),
            Some("Kubuntu · GRUB")
        );
        assert_eq!(
            kind_by_name(r"\EFI\r-drive\bootx64.efi"),
            Some("R-Drive Image")
        );
        assert_eq!(
            kind_by_name(r"\EFI\acronis\bootx64.efi"),
            Some("Acronis · меню восстановления")
        );
        assert_eq!(
            kind_by_name(r"\EFI\Microsoft\Boot\bootmgfw.efi"),
            Some("диспетчер загрузки Windows")
        );
        assert_eq!(
            kind_by_name(r"\EFI\ubuntu\shimx64.efi"),
            Some("Ubuntu · shim")
        );
    }

    /// A vendor nobody has heard of still gets its name through, and a file
    /// with nothing to go on says so instead of guessing.
    #[test]
    fn an_unknown_file_admits_it_rather_than_guessing() {
        assert_eq!(kind_by_name(r"\EFI\whatever\thing.efi"), None);
        assert_eq!(kind_by_name(r"\EFI\vendor\grubx64.efi"), Some("GRUB"));
        assert_eq!(kind_by_name(r"\EFI\fedora\odd.efi"), Some("Fedora"));
        assert_eq!(kind_by_name(r"\EFI\acronis\other.efi"), Some("Acronis"));
    }

    /// The two Acronis programs in one directory are two different things,
    /// and our own boot environment is ours, not "a loader" (2026-10-15).
    #[test]
    fn acronis_files_and_our_own_boot_environment_are_told_apart() {
        assert_eq!(
            kind_by_name(r"\EFI\acronis\bootwiz.efi"),
            Some("Acronis · загрузочный агент")
        );
        assert_eq!(
            kind_by_name(r"\EFI\OwnBackup\bootx64.efi"),
            Some("Own Backup · загрузочная среда")
        );
        assert_eq!(role(r"\EFI\OwnBackup\bootx64.efi"), Role::EntryPoint);
    }

    /// Nine of the twelve files on this machine's partitions are launched by
    /// another loader. Counting them as "nobody points at this" is what made
    /// the first run of the list useless.
    #[test]
    fn what_another_loader_launches_is_not_something_to_point_at() {
        for second in [
            r"\EFI\ubuntu\grubx64.efi",
            r"\EFI\ubuntu\mmx64.efi",
            r"\EFI\kubuntu\mmx64.efi",
            r"\EFI\Microsoft\Boot\bootmgr.efi",
            r"\EFI\Microsoft\Boot\memtest.efi",
            r"\EFI\Microsoft\Boot\SecureBootRecovery.efi",
            // Acronis puts it on "next boot" itself, once.
            r"\EFI\acronis\bootwiz.efi",
        ] {
            assert_eq!(role(second), Role::SecondStage, "{second}");
        }
        for first in [
            r"\EFI\ubuntu\shimx64.efi",
            r"\EFI\kubuntu\shimx64.efi",
            r"\EFI\acronis\bootx64.efi",
            r"\EFI\r-drive\bootx64.efi",
            r"\EFI\Microsoft\Boot\bootmgfw.efi",
            r"\EFI\Boot\bootx64.efi",
        ] {
            assert_eq!(role(first), Role::EntryPoint, "{first}");
        }
    }

    /// Case and separators vary between what the firmware writes and what the
    /// filesystem holds; neither should change the answer.
    #[test]
    fn spelling_does_not_change_what_a_file_is() {
        assert_eq!(
            kind_by_name(r"\efi\UBUNTU\ShimX64.EFI"),
            Some("Ubuntu · shim")
        );
        assert_eq!(
            kind_by_name("/EFI/ubuntu/shimx64.efi"),
            Some("Ubuntu · shim")
        );
        assert_eq!(role(r"\EFI\UBUNTU\GRUBX64.EFI"), Role::SecondStage);
    }
}
