//! Finding a base image for the boot medium on the machine we are running on.
//!
//! We do not ship WinPE — the medium is built from what Windows already carries.
//! That image is `Winre.wim`, and where it lives depends on the installation:
//! registered with the recovery environment on its own partition, or sitting in
//! `System32\Recovery` with the environment switched off, or nowhere at all
//! (verified on the author's machine 2026-07-30: WinRE `Disabled`, only
//! `ReAgent.xml` left behind).
//!
//! So every place is looked at, best first, and each candidate is read before it
//! is offered — a file called `Winre.wim` is not necessarily one, and an
//! installation image is a different thing wearing the same extension. The user
//! chooses from what was actually found; nothing is picked silently, because
//! building a medium on the wrong base fails at boot, on their hardware, in the
//! middle of a rescue.

use crate::wim_info::{read_wim_info, WimInfo};
use own_backup_common::tr;
use own_backup_common::types::PartitionTable;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Where a candidate came from. Shown to the user, because "found on D:" and
/// "registered with the recovery environment" mean different degrees of
/// confidence, and because a candidate inside an installation image needs
/// extracting before it can be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseOrigin {
    /// The image the recovery environment itself points at (`ReAgent.xml`).
    Registered,
    /// `%SystemRoot%\System32\Recovery` — where it waits while WinRE is off.
    SystemRecovery,
    /// `\Recovery\WindowsRE` on a volume, by its letter or its volume GUID path.
    RecoveryFolder,
    /// Chosen by the user in a file dialog.
    Manual,
}

impl BaseOrigin {
    /// In the window's language.
    pub fn describe(&self) -> Cow<'static, str> {
        match self {
            BaseOrigin::Registered => tr!("winre_find.origin_registered"),
            BaseOrigin::SystemRecovery => tr!("winre_find.origin_system_recovery"),
            BaseOrigin::RecoveryFolder => tr!("winre_find.origin_recovery_folder"),
            BaseOrigin::Manual => tr!("winre_find.origin_manual"),
        }
    }
}

/// A usable base image, already read and vouched for.
#[derive(Debug, Clone)]
pub struct BaseImage {
    pub path: PathBuf,
    pub origin: BaseOrigin,
    pub info: WimInfo,
    /// Reasons to hesitate — not refusals. A medium built on a different
    /// servicing branch than the running Windows usually works, and the user is
    /// entitled to build it; they are just told first.
    pub warnings: Vec<String>,
}

/// What `ReAgent.xml` says. Read instead of running `reagentc /info`, whose
/// output is localised — the XML is not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReAgentInfo {
    /// The recovery environment is enabled (`InstallState state="1"`).
    pub enabled: bool,
    /// Directory the image is registered in, relative to its volume
    /// (`\Recovery\WindowsRE`). Empty when nothing is registered.
    pub winre_dir: String,
    /// The partition holding it is named by its DISK and its place on it, not
    /// by a volume: `guid` is the GPT disk GUID (all zeros on MBR), `id` the
    /// MBR disk signature in decimal (0 on GPT), `offset` the partition's start
    /// in bytes. The first version of this read `guid` as a volume GUID, so the
    /// registered image was never found — `\\?\Volume{disk GUID}\` names no
    /// volume — and the search always fell through to the slower sweeps.
    pub winre_disk_guid: String,
    pub winre_disk_signature: u32,
    pub winre_offset: u64,
    /// `10.0.19041.1` — the build the environment belongs to.
    pub os_build: String,
}

impl ReAgentInfo {
    /// Something is registered and it says where: a directory, and a disk to
    /// look for it on.
    pub fn has_location(&self) -> bool {
        !self.winre_dir.is_empty()
            && (!self.winre_disk_guid.is_empty() || self.winre_disk_signature != 0)
    }

    /// Whether a disk with this scheme and id (as the inventory reports it:
    /// GPT GUID without braces, MBR signature as hex) is the one registered.
    pub fn is_registered_disk(&self, table: PartitionTable, disk_id: &str) -> bool {
        match table {
            PartitionTable::Gpt => {
                !self.winre_disk_guid.is_empty()
                    && bare_guid(&self.winre_disk_guid) == bare_guid(disk_id)
            }
            PartitionTable::Mbr => {
                self.winre_disk_signature != 0
                    && u32::from_str_radix(disk_id, 16).ok() == Some(self.winre_disk_signature)
            }
        }
    }
}

/// `{8f9e951e-…}` → `8F9E951E-…`, for comparison.
fn bare_guid(g: &str) -> String {
    g.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_uppercase()
}

/// Parse `ReAgent.xml`. Attribute soup, no schema: the file is small, flat and
/// stable, and pulling a few attributes out of it does not justify a dependency.
pub fn parse_reagent_xml(xml: &str) -> ReAgentInfo {
    let disk_guid = attr(xml, "WinreLocation", "guid").unwrap_or_default();
    let disk_guid_is_zero = bare_guid(&disk_guid).chars().all(|c| c == '0' || c == '-');
    ReAgentInfo {
        enabled: attr(xml, "InstallState", "state").as_deref() == Some("1"),
        winre_dir: attr(xml, "WinreLocation", "path").unwrap_or_default(),
        winre_disk_guid: if disk_guid_is_zero { String::new() } else { disk_guid },
        winre_disk_signature: attr(xml, "WinreLocation", "id")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        winre_offset: attr(xml, "WinreLocation", "offset")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        os_build: attr(xml, "OsBuildVersion", "path").unwrap_or_default(),
    }
}

/// Value of `attribute` on the first `<element …>` in the document.
fn attr(xml: &str, element: &str, attribute: &str) -> Option<String> {
    let start = xml.find(&format!("<{element}"))?;
    let rest = &xml[start..];
    let end = rest.find('>')?;
    let tag = &rest[..end];
    // Whole attribute names only: `id="` is also the tail of `guid="`.
    let key = format!("{attribute}=\"");
    let vs = tag
        .match_indices(&key)
        .find(|(i, _)| tag[..*i].ends_with(char::is_whitespace))
        .map(|(i, _)| i + key.len())?;
    // A string, not the char literal: the catalog test (`i18n_catalogs.rs`)
    // reads this file without knowing char literals, and a lone quote in one
    // hides every `tr!` below it.
    let ve = tag[vs..].find("\"")? + vs;
    let v = tag[vs..ve].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Read and vouch for one candidate path, or say why it is not usable.
///
/// `running_build` is the build of the Windows we are on, for the branch
/// comparison; pass 0 when it is unknown and the check is skipped.
pub fn inspect_candidate(
    path: &Path,
    origin: BaseOrigin,
    running_build: u32,
) -> std::result::Result<BaseImage, String> {
    let info = read_wim_info(path).map_err(|e| e.to_string())?;

    if !info.is_windows_pe() {
        let edition = if info.edition_id.is_empty() {
            tr!("winre_find.edition_unknown").into_owned()
        } else {
            info.edition_id.clone()
        };
        return Err(tr!("winre_find.not_recovery_image", path = path.display(), edition = edition)
            .into_owned());
    }
    if !info.is_x64() {
        let arch = if info.architecture.is_empty() {
            tr!("winre_find.arch_unknown").into_owned()
        } else {
            info.architecture.clone()
        };
        return Err(tr!("winre_find.not_x64", path = path.display(), arch = arch).into_owned());
    }
    if info.total_parts > 1 {
        return Err(tr!(
            "winre_find.split_image",
            path = path.display(),
            part = info.part_number,
            total = info.total_parts
        )
        .into_owned());
    }

    let mut warnings = Vec::new();
    if running_build > 0 && !info.same_branch_as(running_build) {
        warnings.push(
            tr!(
                "winre_find.other_branch",
                image_build = info.build,
                running_build = running_build
            )
            .into_owned(),
        );
    }
    Ok(BaseImage { path: path.to_path_buf(), origin, info, warnings })
}

/// Advice for when nothing was found. Deliberately instructions rather than an
/// error: the user's machine is missing a file, and every line here is a way to
/// get it.
pub fn nothing_found_advice() -> Vec<String> {
    vec![
        tr!("winre_find.advice_enable").into_owned(),
        tr!("winre_find.advice_extract").into_owned(),
        tr!("winre_find.advice_manual").into_owned(),
        tr!("winre_find.advice_adk").into_owned(),
    ]
}

/// Every place worth looking, best first. Windows only — on Linux there is no
/// WinRE to find, and the media builder is a Windows feature.
#[cfg(target_os = "windows")]
pub fn find_base_images() -> Vec<BaseImage> {
    use std::collections::BTreeSet;

    let running_build = running_windows_build();
    let mut found: Vec<BaseImage> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    let mut consider = |path: PathBuf, origin: BaseOrigin, found: &mut Vec<BaseImage>| {
        if !path.exists() || !seen.insert(path.clone()) {
            return;
        }
        match inspect_candidate(&path, origin, running_build) {
            Ok(img) => {
                tracing::info!(path = %img.path.display(), build = img.info.build,
                               lang = %img.info.language, "base image found");
                found.push(img);
            }
            Err(why) => tracing::debug!(%why, "candidate rejected"),
        }
    };

    // Every path handed out must still open when the medium is BUILT, which is
    // another request, minutes later. So a letterless volume is named by its
    // volume GUID path (`\\?\Volume{…}\Recovery\…`), which the file API opens
    // directly — never through a letter lent for the search. That is what this
    // did at first, and the build then failed with "path not found" on a letter
    // given back the moment the search ended.
    let volume_root = |volume: &str| -> String {
        // `"X:"` joined as-is would be drive-RELATIVE (`X:Recovery\…`).
        match crate::volume_win::volume_drive_letters(volume).first() {
            Some(letter) => format!("{letter}\\"),
            None => volume.to_string(),
        }
    };

    // 1. What the recovery environment itself points at: a partition named by
    //    its disk and offset, found among the volumes by exactly that.
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let reagent = PathBuf::from(&system_root).join(r"System32\Recovery\ReAgent.xml");
    if let Ok(text) = std::fs::read_to_string(&reagent) {
        let ra = parse_reagent_xml(&text);
        tracing::info!(enabled = ra.enabled, dir = %ra.winre_dir, build = %ra.os_build,
                       disk_guid = %ra.winre_disk_guid, disk_signature = ra.winre_disk_signature,
                       offset = ra.winre_offset, "ReAgent.xml read");
        if ra.has_location() {
            let dir = ra.winre_dir.trim_start_matches('\\');
            let registered = crate::volume_win::all_volume_roots().into_iter().find(|v| {
                matches!(crate::volume_win::volume_start_on_disk(v),
                         Some((disk, offset)) if offset == ra.winre_offset
                             && own_backup_inventory::win_disks::disk_identity(disk)
                                 .is_some_and(|(t, id)| ra.is_registered_disk(t, &id)))
            });
            match registered {
                Some(volume) => consider(
                    PathBuf::from(volume_root(&volume)).join(dir).join("Winre.wim"),
                    BaseOrigin::Registered,
                    &mut found,
                ),
                None => tracing::info!("the registered recovery partition is not among the volumes"),
            }
        }
    }

    // 2. Where the image waits while the environment is switched off.
    consider(
        PathBuf::from(&system_root).join(r"System32\Recovery\Winre.wim"),
        BaseOrigin::SystemRecovery,
        &mut found,
    );

    // 3. Every volume that has a letter: the recovery folder, and a second
    //    Windows' own System32\Recovery. Two file opens per volume, not a walk.
    for letter in 'A'..='Z' {
        let root = format!("{letter}:\\");
        if std::fs::metadata(&root).is_err() {
            continue;
        }
        consider(
            PathBuf::from(&root).join(r"Recovery\WindowsRE\Winre.wim"),
            BaseOrigin::RecoveryFolder,
            &mut found,
        );
        consider(
            PathBuf::from(&root).join(r"Windows\System32\Recovery\Winre.wim"),
            BaseOrigin::SystemRecovery,
            &mut found,
        );
    }

    // 4. Recovery partitions with no letter — the usual case on a factory
    //    installation. Looked at through the volume GUID path; nothing is
    //    mounted, so nothing flashes up in Explorer either.
    for volume in crate::volume_win::all_volume_roots() {
        // A volume that already answers to a letter was covered by the sweep
        // above, under the name the user knows it by.
        if !crate::volume_win::volume_drive_letters(&volume).is_empty() {
            continue;
        }
        if std::fs::metadata(&volume).is_err() {
            continue; // not readable at all (offline disk, empty reader)
        }
        consider(
            PathBuf::from(&volume).join(r"Recovery\WindowsRE\Winre.wim"),
            BaseOrigin::RecoveryFolder,
            &mut found,
        );
    }

    // Best base first: the registered one, then the rest, newest build first so
    // a machine with two Windows offers the current one at the top.
    found.sort_by(|a, b| {
        (a.origin != BaseOrigin::Registered)
            .cmp(&(b.origin != BaseOrigin::Registered))
            .then(b.info.build.cmp(&a.info.build))
    });
    found
}

#[cfg(not(target_os = "windows"))]
pub fn find_base_images() -> Vec<BaseImage> {
    Vec::new()
}

/// Build number of the running Windows, 0 when it cannot be read.
#[cfg(target_os = "windows")]
fn running_windows_build() -> u32 {
    use windows::Win32::System::SystemInformation::GetVersionExW;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut vi = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: the struct's size field is set as the API requires.
    if unsafe { GetVersionExW(&mut vi) }.is_ok() {
        vi.dwBuildNumber
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ReAgent.xml from an installation with the environment enabled.
    const REAGENT_ENABLED: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<WindowsRE version="2.0">
  <WinreBCD id="{00000000-0000-0000-0000-000000000000}"/>
  <WinreLocation path="\Recovery\WindowsRE" id="0" offset="1048576" guid="{8f9e951e-0000-0000-0000-000000000000}"/>
  <ImageLocation path="" id="0" offset="0" guid="{00000000-0000-0000-0000-000000000000}"/>
  <InstallState state="1"/>
  <OsInstallAvailable state="0"/>
  <IsAutoRepairOn state="1"/>
  <OsBuildVersion path="10.0.19041.1"/>
</WindowsRE>"#;

    #[test]
    fn reads_where_the_recovery_environment_keeps_its_image() {
        let ra = parse_reagent_xml(REAGENT_ENABLED);
        assert!(ra.enabled);
        assert_eq!(ra.winre_dir, r"\Recovery\WindowsRE");
        assert_eq!(ra.winre_disk_guid, "{8f9e951e-0000-0000-0000-000000000000}");
        assert_eq!(ra.winre_disk_signature, 0);
        assert_eq!(ra.winre_offset, 1048576);
        assert_eq!(ra.os_build, "10.0.19041.1");
        assert!(ra.has_location());
    }

    /// `guid` names the DISK, compared with what the inventory reports for it:
    /// uppercase, no braces. Right offset on the wrong disk is not the image.
    #[test]
    fn a_gpt_location_matches_its_disk_and_no_other() {
        let ra = parse_reagent_xml(REAGENT_ENABLED);
        assert!(ra.is_registered_disk(PartitionTable::Gpt, "8F9E951E-0000-0000-0000-000000000000"));
        assert!(!ra.is_registered_disk(PartitionTable::Gpt, "8F9E951E-0000-0000-0000-000000000001"));
        assert!(!ra.is_registered_disk(PartitionTable::Mbr, "00000000"));
    }

    /// On MBR the disk is named by its signature, in decimal, and `guid` is all
    /// zeros — which must not be taken for a GPT disk with a zero GUID.
    #[test]
    fn an_mbr_location_is_named_by_the_decimal_signature() {
        let xml = r#"<WindowsRE version="2.0"><WinreLocation path="\Recovery\WindowsRE" id="1560802729" offset="115153920" guid="{00000000-0000-0000-0000-000000000000}"/></WindowsRE>"#;
        let ra = parse_reagent_xml(xml);
        assert!(ra.winre_disk_guid.is_empty());
        assert_eq!(ra.winre_disk_signature, 1560802729);
        assert_eq!(ra.winre_offset, 115153920);
        assert!(ra.has_location());
        assert!(ra.is_registered_disk(PartitionTable::Mbr, "5D07F5A9"));
        assert!(!ra.is_registered_disk(PartitionTable::Mbr, "5D07F5AA"));
        assert!(!ra.is_registered_disk(PartitionTable::Gpt, ""));
    }

    /// `id="` is also the tail of `guid="`; the order of attributes must not
    /// decide which one is read.
    #[test]
    fn attributes_are_matched_by_whole_name() {
        let xml = r#"<WinreLocation guid="{00000000-0000-0000-0000-000000000000}" path="\R" offset="1" id="42"/>"#;
        assert_eq!(parse_reagent_xml(xml).winre_disk_signature, 42);
    }

    /// The case that started this: WinRE switched off, so the XML registers
    /// nothing — and the image is elsewhere, if it is anywhere.
    #[test]
    fn a_disabled_environment_registers_nothing() {
        let xml = REAGENT_ENABLED
            .replace(r#"<InstallState state="1"/>"#, r#"<InstallState state="0"/>"#)
            .replace(r#"path="\Recovery\WindowsRE""#, r#"path="""#);
        let ra = parse_reagent_xml(&xml);
        assert!(!ra.enabled);
        assert!(ra.winre_dir.is_empty());
        assert!(!ra.has_location());
    }

    #[test]
    fn missing_file_yields_defaults_not_a_panic() {
        let ra = parse_reagent_xml("<WindowsRE version=\"2.0\"/>");
        assert_eq!(ra, ReAgentInfo::default());
    }

    /// An installation image passes every structural test and is still the wrong
    /// file — the rejection has to say what it actually is.
    #[test]
    fn an_installation_image_is_refused_with_its_own_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("install.wim");
        write_wim(&p, "Professional", "9", 19041, 1);
        let e = inspect_candidate(&p, BaseOrigin::Manual, 19041).unwrap_err();
        assert!(e.contains("установочный образ"), "{e}");
    }

    #[test]
    fn a_split_image_is_refused_before_anything_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("boot.swm");
        write_wim(&p, "WindowsPE", "9", 19041, 3);
        let e = inspect_candidate(&p, BaseOrigin::Manual, 19041).unwrap_err();
        assert!(e.contains(".swm"), "{e}");
    }

    /// A different branch is a warning, not a refusal: the user may well have
    /// good reason, and the medium usually works.
    #[test]
    fn a_different_windows_branch_warns_but_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("winre.wim");
        write_wim(&p, "WindowsPE", "9", 22621, 1);
        let img = inspect_candidate(&p, BaseOrigin::Manual, 19045).unwrap();
        assert_eq!(img.warnings.len(), 1, "{:?}", img.warnings);
        assert!(img.warnings[0].contains("22621"));

        let p2 = dir.path().join("same.wim");
        write_wim(&p2, "WindowsPE", "9", 19041, 1);
        assert!(inspect_candidate(&p2, BaseOrigin::Manual, 19045).unwrap().warnings.is_empty());
    }

    #[test]
    fn advice_is_offered_when_the_machine_has_no_image() {
        let advice = nothing_found_advice();
        assert!(advice.iter().any(|a| a.contains("reagentc /enable")));
        assert!(advice.len() >= 3);
    }

    fn write_wim(path: &Path, edition: &str, arch: &str, build: u32, parts: u16) {
        use std::io::Write;
        let xml = format!(
            "<WIM><IMAGE INDEX=\"1\"><NAME>t</NAME><WINDOWS><ARCH>{arch}</ARCH>\
             <EDITIONID>{edition}</EDITIONID><VERSION><BUILD>{build}</BUILD></VERSION>\
             <LANGUAGE>ru-RU</LANGUAGE></WINDOWS></IMAGE></WIM>"
        );
        let mut xml_bytes: Vec<u8> = vec![0xFF, 0xFE];
        for u in xml.encode_utf16() {
            xml_bytes.extend_from_slice(&u.to_le_bytes());
        }
        let mut header = vec![0u8; 0x60];
        header[..8].copy_from_slice(crate::wim_info::WIM_MAGIC);
        header[0x28..0x2A].copy_from_slice(&1u16.to_le_bytes());
        header[0x2A..0x2C].copy_from_slice(&parts.to_le_bytes());
        header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        header[0x48..0x50].copy_from_slice(&(xml_bytes.len() as u64).to_le_bytes());
        header[0x50..0x58].copy_from_slice(&0x60u64.to_le_bytes());
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&header).unwrap();
        f.write_all(&xml_bytes).unwrap();
    }
}
