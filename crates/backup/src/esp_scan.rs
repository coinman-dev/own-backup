//! What a file on an EFI System Partition is, by its name.
//!
//! The pure half of [`crate::esp_scan_win`]. No I/O, so the rules can be tested
//! against the real partition of a real machine without that machine.
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
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase()
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
    // Launched by shim, by GRUB, or by the Windows boot manager — never by the
    // firmware, and never worth an entry of its own.
    const SECOND_STAGE: [&str; 7] = [
        "grubx64.efi",    // shim's second stage
        "mmx64.efi",      // MokManager, launched by shim
        "fbx64.efi",      // shim's fallback, rebuilds entries by itself
        "bootmgr.efi",    // the inner half of the Windows boot manager
        "memtest.efi",    // a tool Windows offers from its own menu
        "securebootrecovery.efi",
        "shimx64.efi.signed",
    ];
    if SECOND_STAGE.contains(&file_name(path).as_str()) {
        Role::SecondStage
    } else {
        Role::EntryPoint
    }
}

/// What this file is, in words, from its name and the directory above it.
///
/// `None` when the name says nothing and the caller should look inside.
pub fn kind_by_name(path: &str) -> Option<&'static str> {
    let file = file_name(path);
    let vendor = vendor(path);

    // The vendor directory is the stronger signal when it is one we know: a
    // loader in `EFI\acronis` is Acronis' whatever it is built from.
    let by_vendor = match vendor.as_str() {
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

    let by_file = match file.as_str() {
        "shimx64.efi" => Some("shim, первая ступень Secure Boot"),
        "grubx64.efi" => Some("GRUB"),
        "mmx64.efi" => Some("MokManager, ключи Secure Boot"),
        "fbx64.efi" => Some("fallback, восстанавливает запись сам"),
        "bootmgfw.efi" => Some("диспетчер загрузки"),
        "bootmgr.efi" => Some("диспетчер загрузки, внутренняя часть"),
        "memtest.efi" => Some("проверка памяти"),
        "securebootrecovery.efi" => Some("восстановление Secure Boot"),
        "systemd-bootx64.efi" => Some("systemd-boot"),
        _ => None,
    };

    match (by_vendor, by_file) {
        (Some(v), Some(f)) => Some(join(v, f)),
        (Some(v), None) => Some(v),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    }
}

/// `"Ubuntu"` + `"GRUB"` → `"Ubuntu · GRUB"`, without allocating at runtime.
///
/// The pairs are few and known, so they are spelled out: a `String` here would
/// have to be leaked or the return type widened, and neither is worth it for a
/// column in a list.
fn join(vendor: &'static str, file: &'static str) -> &'static str {
    match (vendor, file) {
        ("Ubuntu", "shim, первая ступень Secure Boot") => "Ubuntu · shim",
        ("Ubuntu", "GRUB") => "Ubuntu · GRUB",
        ("Ubuntu", "MokManager, ключи Secure Boot") => "Ubuntu · MokManager",
        ("Kubuntu", "shim, первая ступень Secure Boot") => "Kubuntu · shim",
        ("Kubuntu", "GRUB") => "Kubuntu · GRUB",
        ("Kubuntu", "MokManager, ключи Secure Boot") => "Kubuntu · MokManager",
        ("Windows", "диспетчер загрузки") => "диспетчер загрузки Windows",
        ("Windows", "диспетчер загрузки, внутренняя часть") => "Windows, внутренняя часть",
        ("Windows", "проверка памяти") => "проверка памяти Windows",
        ("Windows", "восстановление Secure Boot") => "восстановление Secure Boot",
        (v, _) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three files the string-matching version got wrong, and the one it
    /// got right. All four are on this machine's ESP.
    #[test]
    fn the_files_that_lie_about_themselves_are_named_correctly() {
        assert_eq!(kind_by_name(r"\EFI\kubuntu\shimx64.efi"), Some("Kubuntu · shim"));
        assert_eq!(kind_by_name(r"\EFI\kubuntu\grubx64.efi"), Some("Kubuntu · GRUB"));
        assert_eq!(kind_by_name(r"\EFI\r-drive\bootx64.efi"), Some("R-Drive Image"));
        assert_eq!(kind_by_name(r"\EFI\acronis\bootx64.efi"), Some("Acronis"));
        assert_eq!(
            kind_by_name(r"\EFI\Microsoft\Boot\bootmgfw.efi"),
            Some("диспетчер загрузки Windows")
        );
        assert_eq!(kind_by_name(r"\EFI\ubuntu\shimx64.efi"), Some("Ubuntu · shim"));
    }

    /// A vendor nobody has heard of still gets its name through, and a file
    /// with nothing to go on says so instead of guessing.
    #[test]
    fn an_unknown_file_admits_it_rather_than_guessing() {
        assert_eq!(kind_by_name(r"\EFI\whatever\thing.efi"), None);
        assert_eq!(kind_by_name(r"\EFI\vendor\grubx64.efi"), Some("GRUB"));
        assert_eq!(kind_by_name(r"\EFI\fedora\odd.efi"), Some("Fedora"));
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
        ] {
            assert_eq!(role(second), Role::SecondStage, "{second}");
        }
        for first in [
            r"\EFI\ubuntu\shimx64.efi",
            r"\EFI\kubuntu\shimx64.efi",
            r"\EFI\acronis\bootx64.efi",
            r"\EFI\acronis\bootwiz.efi",
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
        assert_eq!(kind_by_name(r"\efi\UBUNTU\ShimX64.EFI"), Some("Ubuntu · shim"));
        assert_eq!(kind_by_name("/EFI/ubuntu/shimx64.efi"), Some("Ubuntu · shim"));
        assert_eq!(role(r"\EFI\UBUNTU\GRUBX64.EFI"), Role::SecondStage);
    }
}
