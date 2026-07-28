//! Choosing what to put at `\EFI\BOOT\BOOTX64.EFI`.
//!
//! The pure half of [`crate::esp_fallback_win`]: given what an ESP contains,
//! decide which loader the firmware should be handed and what has to travel
//! with it. No I/O, so it can be tested anywhere.
//!
//! ## Why anything is copied at all
//!
//! UEFI does not look for a bootloader. It runs the one its **NVRAM** names,
//! and NVRAM belongs to the machine, not to the disk. Restore a disk onto a
//! different machine and every loader on it is still there, still signed, still
//! correct — and nothing asks for any of them. The single exception is the
//! *removable media path*, `\EFI\BOOT\BOOTX64.EFI`, which every firmware tries
//! on its own. That is why USB sticks boot anywhere, and why a restored disk
//! that lacks it drops into a UEFI shell.
//!
//! ## Why more than one file
//!
//! `shimx64.efi` is the signed first stage; it loads `grubx64.efi` **from its
//! own directory** and hands MOK enrolment to `mmx64.efi` beside it. GRUB in
//! turn reads `grub.cfg` from where it was loaded — a three-line stub that
//! names the real `/boot/grub/grub.cfg` by filesystem UUID. Copy shim alone and
//! it starts and finds no second stage; copy shim and GRUB without the stub and
//! GRUB starts and lands on its rescue prompt. Both look exactly like a broken
//! restore.
//!
//! ## Why the UUID decides which directory
//!
//! An ESP that has served a machine for years accumulates vendor directories —
//! the one in front of me had five, two of them complete shim+GRUB sets
//! (`\EFI\ubuntu` left over from a system that is no longer installed, and
//! `\EFI\kubuntu`, the one that actually boots). Nothing in the file names says
//! which is live: same shim byte-for-byte, similar dates, both plausible.
//!
//! The stub does say. It searches for a filesystem by UUID, and only one of
//! those UUIDs belongs to a partition that was just restored. So the right
//! directory is the one whose stub points at the copy — everything else is
//! guesswork, and guessing wrong means GRUB looking for a `/boot` that is not
//! on this disk.

/// The removable-media path every UEFI firmware tries without being told.
pub const FALLBACK_DIR: &str = "EFI\\BOOT";
pub const FALLBACK_NAME: &str = "BOOTX64.EFI";

/// One file to copy: `from` is relative to the ESP root, `to` is its name
/// inside `EFI\BOOT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub from: String,
    pub to: String,
}

/// What a vendor directory on the ESP holds, as far as this decision cares.
#[derive(Debug, Clone, Default)]
pub struct VendorDir {
    /// Directory name under `EFI\`, e.g. "ubuntu" or "Microsoft\\Boot".
    pub path: String,
    pub files: Vec<String>,
    /// Filesystem UUID its `grub.cfg` stub searches for, when it has one.
    pub grub_uuid: Option<String>,
}

impl VendorDir {
    fn has(&self, name: &str) -> bool {
        self.files.iter().any(|f| f.eq_ignore_ascii_case(name))
    }

    /// Whether this directory's stub points at one of the filesystems that were
    /// just restored.
    fn points_at(&self, known_uuids: &[String]) -> bool {
        let Some(u) = &self.grub_uuid else { return false };
        known_uuids.iter().any(|k| k.eq_ignore_ascii_case(u))
    }
}

/// Loaders we know how to make bootable, best first.
///
/// Shim before GRUB: on a Secure Boot machine only shim is signed by a key the
/// firmware trusts, and booting the same disk on a machine with Secure Boot off
/// works either way. Windows' `bootmgfw.efi` last — a Windows restore normally
/// gets its boot chain from `bcdboot`, and this is the fallback for the case
/// where that could not run.
const CANDIDATES: &[&str] = &["shimx64.efi", "grubx64.efi", "bootmgfw.efi", "systemd-bootx64.efi"];

/// Files that must accompany a chosen loader, kept under their own names.
fn companions(loader: &str) -> &'static [&'static str] {
    match loader.to_ascii_lowercase().as_str() {
        // grub.cfg is not optional: GRUB reads it from the directory it was
        // loaded out of, and without it stops at its rescue prompt.
        "shimx64.efi" => &["grubx64.efi", "grub.cfg", "mmx64.efi", "fbx64.efi", "BOOTX64.CSV"],
        "grubx64.efi" => &["grub.cfg"],
        _ => &[],
    }
}

/// Which vendor directory the loader should come from, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub dir: String,
    pub loader: String,
    /// Its `grub.cfg` names a filesystem that was restored — the strong signal.
    pub confirmed_by_uuid: bool,
    /// Other directories that also held a loader. Reported, because when the
    /// choice was not confirmed the user is the only one who knows which system
    /// they meant.
    pub other_candidates: Vec<String>,
}

/// Pick the loader directory. `known_uuids` are the filesystem UUIDs of the
/// partitions this restore just wrote.
pub fn choose_loader(vendors: &[VendorDir], known_uuids: &[String]) -> Option<Choice> {
    // A vendor directory named BOOT is the fallback path itself; taking a
    // loader from there to put it back would be circular.
    let usable: Vec<&VendorDir> =
        vendors.iter().filter(|d| !d.path.eq_ignore_ascii_case("BOOT")).collect();

    let mut all_with_loaders: Vec<String> = Vec::new();
    for d in &usable {
        if CANDIDATES.iter().any(|c| d.has(c)) {
            all_with_loaders.push(d.path.clone());
        }
    }

    // Two passes over the same preference order: first only directories whose
    // stub points at a restored filesystem, then any. The order inside each
    // pass is CANDIDATES, so shim still beats bare GRUB.
    for confirmed_only in [true, false] {
        for candidate in CANDIDATES {
            let Some(dir) = usable
                .iter()
                .filter(|d| !confirmed_only || d.points_at(known_uuids))
                .find(|d| d.has(candidate))
            else {
                continue;
            };
            let loader = dir
                .files
                .iter()
                .find(|f| f.eq_ignore_ascii_case(candidate))
                .cloned()
                .unwrap_or_else(|| (*candidate).to_string());
            let others: Vec<String> =
                all_with_loaders.iter().filter(|p| **p != dir.path).cloned().collect();
            return Some(Choice {
                dir: dir.path.clone(),
                loader,
                confirmed_by_uuid: confirmed_only,
                other_candidates: others,
            });
        }
    }
    None
}

/// The files to copy for `choice`, in order — the loader itself first.
pub fn copies_for(vendors: &[VendorDir], choice: &Choice) -> Vec<Copy> {
    let Some(dir) = vendors.iter().find(|d| d.path == choice.dir) else { return Vec::new() };
    let mut out = vec![Copy {
        from: format!("EFI\\{}\\{}", dir.path, choice.loader),
        to: FALLBACK_NAME.to_string(),
    }];
    for c in companions(&choice.loader) {
        if let Some(name) = dir.files.iter().find(|f| f.eq_ignore_ascii_case(c)) {
            out.push(Copy { from: format!("EFI\\{}\\{name}", dir.path), to: name.clone() });
        }
    }
    out
}

/// The filesystem UUID a GRUB stub searches for.
///
/// Ubuntu's stub is three lines and comes in two spellings depending on the
/// version:
///
/// ```text
/// search.fs_uuid 90a2b1c3-1111-2222-3333-444455556666 root hd0,gpt2
/// set prefix=($root)'/grub'
/// configfile $prefix/grub.cfg
/// ```
///
/// or `search --no-floppy --fs-uuid --set=root <uuid>`. Both are handled; a
/// stub written any other way simply yields `None`, and the directory then has
/// to win on its own.
pub fn parse_grub_search_uuid(cfg: &str) -> Option<String> {
    for line in cfg.lines() {
        let line = line.trim();
        if !line.starts_with("search") {
            continue;
        }
        let mut tokens = line.split_whitespace().skip(1).peekable();
        // `search.fs_uuid <uuid> …` puts it first; `search … --fs-uuid …`
        // puts it after the flags. Either way it is the first token that looks
        // like a UUID or a FAT serial.
        while let Some(tok) = tokens.next() {
            let _ = tokens.peek();
            if looks_like_uuid(tok) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

/// A filesystem UUID (ext/btrfs/xfs, 36 chars with dashes) or a FAT serial
/// (`XXXX-XXXX`). Deliberately narrow: everything else on that line is a flag,
/// a device name, or the word `root`.
fn looks_like_uuid(t: &str) -> bool {
    let hex_dash = |s: &str| s.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    (t.len() == 36 && t.as_bytes()[8] == b'-' && hex_dash(t))
        || (t.len() == 9 && t.as_bytes()[4] == b'-' && hex_dash(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str, files: &[&str], uuid: Option<&str>) -> VendorDir {
        VendorDir {
            path: path.into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            grub_uuid: uuid.map(|s| s.to_string()),
        }
    }

    const KUBUNTU_BOOT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const GONE: &str = "ffffffff-0000-1111-2222-333344445555";

    /// The ESP this was written against, read off the source machine: five
    /// vendor directories, two of them complete shim+GRUB sets, and only one of
    /// those two belonging to a system that is still installed.
    fn real_esp() -> Vec<VendorDir> {
        let grub_set = ["grubx64.efi", "shimx64.efi", "mmx64.efi", "BOOTX64.CSV", "grub.cfg"];
        vec![
            dir("ubuntu", &grub_set, Some(GONE)),
            dir("acronis", &["bootx64.efi", "bootwiz.efi"], None),
            dir("kubuntu", &grub_set, Some(KUBUNTU_BOOT)),
            dir("Microsoft\\Boot", &["bootmgfw.efi", "memtest.efi"], None),
            dir("r-drive", &["bootx64.efi"], None),
        ]
    }

    #[test]
    fn the_directory_that_points_at_the_restored_filesystem_wins() {
        // Both \EFI\ubuntu and \EFI\kubuntu hold a complete, valid, identical
        // shim. Only kubuntu's stub names a partition that this restore wrote,
        // and taking ubuntu's would leave GRUB searching for a /boot that is
        // not on the disk.
        let choice = choose_loader(&real_esp(), &[KUBUNTU_BOOT.into()]).unwrap();
        assert_eq!(choice.dir, "kubuntu");
        assert!(choice.confirmed_by_uuid);
        assert!(choice.other_candidates.contains(&"ubuntu".to_string()));
    }

    #[test]
    fn the_stub_travels_with_grub() {
        // GRUB reads grub.cfg from the directory it was loaded out of. Without
        // it the fallback boots as far as the rescue prompt — which reads as a
        // failed restore.
        let choice = choose_loader(&real_esp(), &[KUBUNTU_BOOT.into()]).unwrap();
        let copies = copies_for(&real_esp(), &choice);
        assert_eq!(copies[0], Copy {
            from: "EFI\\kubuntu\\shimx64.efi".into(),
            to: "BOOTX64.EFI".into(),
        });
        for wanted in ["grubx64.efi", "grub.cfg", "mmx64.efi"] {
            assert!(copies.iter().any(|c| c.to == wanted), "{wanted} missing from {copies:#?}");
        }
        // Not on this ESP, so nothing pretends it is.
        assert!(!copies.iter().any(|c| c.to == "fbx64.efi"));
    }

    #[test]
    fn without_a_matching_uuid_it_still_picks_one_and_says_it_guessed() {
        // A copy whose filesystems could not be read, or a stub written some
        // other way. Refusing outright would help nobody — but the report has
        // to say which directory was taken and what else was there.
        let choice = choose_loader(&real_esp(), &[]).unwrap();
        assert!(!choice.confirmed_by_uuid);
        assert!(!choice.other_candidates.is_empty());
    }

    #[test]
    fn a_confirmed_directory_beats_an_earlier_unconfirmed_one() {
        // Order on the ESP must not decide this: `ubuntu` is read first and has
        // an equally good shim.
        let vendors = real_esp();
        let choice = choose_loader(&vendors, &[KUBUNTU_BOOT.into()]).unwrap();
        assert_eq!(choice.dir, "kubuntu");
        assert_eq!(vendors[0].path, "ubuntu", "…and it really is listed first");
    }

    #[test]
    fn windows_falls_back_to_its_own_boot_manager() {
        let vendors = vec![dir("Microsoft\\Boot", &["bootmgfw.efi", "bootmgr.efi"], None)];
        let choice = choose_loader(&vendors, &[]).unwrap();
        assert_eq!(choice.dir, "Microsoft\\Boot");
        let copies = copies_for(&vendors, &choice);
        assert_eq!(copies.len(), 1, "the boot manager needs nothing beside it");
    }

    #[test]
    fn grub_without_shim_still_boots_and_takes_its_stub() {
        let vendors = vec![dir("debian", &["grubx64.efi", "grub.cfg"], None)];
        let choice = choose_loader(&vendors, &[]).unwrap();
        let copies = copies_for(&vendors, &choice);
        assert_eq!(copies[0].from, "EFI\\debian\\grubx64.efi");
        assert!(copies.iter().any(|c| c.to == "grub.cfg"));
    }

    #[test]
    fn names_are_matched_case_insensitively_and_kept_as_they_are() {
        // FAT is case-insensitive and distros disagree about capitalisation;
        // the copy must use the name that is actually on the disk.
        let vendors = vec![dir("Ubuntu", &["ShimX64.efi", "GrubX64.EFI"], None)];
        let choice = choose_loader(&vendors, &[]).unwrap();
        let copies = copies_for(&vendors, &choice);
        assert_eq!(copies[0].from, "EFI\\Ubuntu\\ShimX64.efi");
        assert_eq!(copies[1].to, "GrubX64.EFI");
    }

    #[test]
    fn an_esp_with_no_loader_at_all_asks_for_nothing() {
        assert!(choose_loader(&[dir("dell", &["diags.efi"], None)], &[]).is_none());
    }

    #[test]
    fn the_fallback_directory_is_never_its_own_source() {
        assert!(choose_loader(&[dir("BOOT", &["grubx64.efi"], None)], &[]).is_none());
    }

    #[test]
    fn both_spellings_of_the_grub_stub_are_read() {
        let modern = "search.fs_uuid 1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d root hd0,gpt2 \n\
                      set prefix=($root)'/grub'\nconfigfile $prefix/grub.cfg\n";
        assert_eq!(
            parse_grub_search_uuid(modern).as_deref(),
            Some("1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d")
        );
        let older = "search --no-floppy --fs-uuid --set=root 1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d\n\
                     configfile /grub/grub.cfg\n";
        assert_eq!(
            parse_grub_search_uuid(older).as_deref(),
            Some("1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d")
        );
        // A FAT /boot is named by its 8-hex serial instead.
        assert_eq!(
            parse_grub_search_uuid("search --fs-uuid --set=root 060B-E961\n").as_deref(),
            Some("060B-E961")
        );
        assert_eq!(parse_grub_search_uuid("set prefix=($root)'/grub'\n"), None);
    }
}
