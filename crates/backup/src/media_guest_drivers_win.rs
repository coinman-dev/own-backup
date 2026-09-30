//! Guest drivers for the boot medium: the ones a virtual machine needs and no
//! machine has in its own driver store.
//!
//! The medium's drivers come from the driver store of the machine that builds
//! it (see [`crate::media_build_win::list_driver_packages`]), and for real
//! hardware that is exactly right. For a virtual machine it is exactly wrong:
//! the host runs VMware Workstation, so its driver store holds the *host's*
//! drivers, while the driver the guest needs sits unopened in the tools image
//! next to the program — `windows.iso`.
//!
//! Without it a VM booted from our medium is stuck on the Microsoft Basic
//! Display Adapter, which under VMware offers 1024x768 and nothing else: the
//! interface asks for 1280x720 and cannot get it.
//!
//! What this does: find `windows.iso`, mount it, and take the drivers off it —
//! the ones lying there as files (there is exactly one, `pvscsi`) and the ones
//! inside `setup.exe`, which is where the display driver lives. Only the
//! classes a rescue medium can use are kept: display, disk controller, network,
//! mouse. The rest of the tools package (file sharing, drag-and-drop, the tray
//! application) has no place on a rescue medium.
//!
//! **Hyper-V has no equivalent and needs none:** its display, disk and network
//! drivers are inbox in Windows and therefore already in WinPE. Its resolution
//! is decided by the host and the firmware, not by a driver we could add —
//! see `.ai/boot-media-plan_2026-07-30.md`.

use own_backup_common::Result;
use std::path::{Path, PathBuf};
use tracing::info;
// Only the parts that run processes report failures of their own.
#[cfg(target_os = "windows")]
use own_backup_common::{tr, OwnBackupError};
#[cfg(target_os = "windows")]
use tracing::warn;

/// Published "INF name" of the VMware pseudo-package.
///
/// The driver list the user picks from is a list of INF names, and this is not
/// one — the package is not installed on this machine and has no name in its
/// driver store. Giving it an id of the same shape means the whole choosing,
/// counting and reporting path works unchanged; the build splits it back out
/// before anything tries to hand it to `pnputil`.
pub const VMWARE_TOOLS_ID: &str = "guest:vmware-tools";

/// Prefix marking every such pseudo-package.
pub const PSEUDO_PREFIX: &str = "guest:";

/// Driver classes worth taking out of a guest-tools package. A rescue medium
/// needs to see the screen, the disks, the network and the pointer; the rest of
/// what VMware Tools installs is for a running desktop.
const WANTED_CLASSES: [&str; 5] = ["display", "scsiadapter", "net", "mouse", "hidclass"];

/// Which Windows variant of a driver to take, best first. The medium is built
/// from WinRE 19041 — a Windows 10 image — so the Windows 10 package is the one
/// that belongs in it; the others are there for the guests VMware also supports.
const OS_PREFERENCE: [&str; 3] = ["Win10", "Win11", "Win8"];

/// The suffixes VMware's build puts on a file name inside its cabinets.
const OS_TAGS: [&str; 5] = ["Win11", "Win10", "Win8", "Win7", "Vista"];

// ---------------------------------------------------------------------------
// The installer's cabinets
//
// Only ONE driver is on the tools image as files (pvscsi, so Windows Setup can
// load it); everything else — the display driver included — is inside
// `setup.exe`, in cabinets appended to it. Checked against a real image
// (VMware Tools, 2026-08-01): 26 cabinets, one of them 72 MB holding `vm3d`,
// the WDDM display driver, in four Windows variants.
//
// The names inside those cabinets are MSI file keys, not file names:
// `_vm3d.inf_Win10.4C60EBF3_7457_440E_A848_1BB27D201892`. They have to be put
// back before the package means anything to Windows.
//
// We only PARSE the cabinet headers, to decide which cabinet is worth
// unpacking. The unpacking itself is done by `expand.exe`, which is part of
// Windows and speaks the LZX these cabinets use.
// ---------------------------------------------------------------------------

/// A cabinet found inside a self-extracting installer.
#[derive(Debug, Clone)]
pub struct Cabinet {
    pub offset: usize,
    pub size: usize,
    /// The names it holds, as far as they could be read. A short list is not a
    /// problem: it is only used to decide whether to unpack this cabinet.
    pub names: Vec<String>,
}

/// Every cabinet embedded in `data`.
///
/// `MSCF` also occurs by accident inside compressed data, so a candidate is
/// only accepted when its header is self-consistent: the reserved word is zero,
/// the declared size fits inside what we have, and the file table starts inside
/// the cabinet.
pub fn cabinets_in(data: &[u8]) -> Vec<Cabinet> {
    let u16at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);

    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(found) = find(data, b"MSCF", i) {
        i = found + 4;
        if found + 36 > data.len() {
            break;
        }
        let reserved1 = u32at(found + 4);
        let size = u32at(found + 8) as usize;
        let coff_files = u32at(found + 16) as usize;
        let flags = u16at(found + 30);
        let count = u16at(found + 28) as usize;
        if reserved1 != 0
            || size < 36
            || found + size > data.len()
            || coff_files < 36
            || coff_files >= size
            || count == 0
            // A cabinet that continues in another file cannot be carved out of
            // this one, and the tools installer has none.
            || flags & 0x0003 != 0
        {
            continue;
        }

        // File entries: cbFile(4) offset(4) folder(2) date(2) time(2) attr(2),
        // then a NUL-terminated name. The count in the header has been seen to
        // outrun the table, so the walk stops at the first entry that is not a
        // plausible name rather than trusting it.
        let mut names = Vec::new();
        let mut p = found + coff_files;
        for _ in 0..count {
            if p + 17 > found + size {
                break;
            }
            let Some(end) = find(&data[..found + size], b"\0", p + 16) else {
                break;
            };
            let raw = &data[p + 16..end];
            let Ok(name) = std::str::from_utf8(raw) else {
                break;
            };
            if name.is_empty()
                || name.len() > 255
                || !name.chars().all(|c| c.is_ascii_graphic() || c == ' ')
            {
                break;
            }
            names.push(name.to_string());
            p = end + 1;
        }
        if names.is_empty() {
            continue;
        }
        out.push(Cabinet {
            offset: found,
            size,
            names,
        });
        i = found + size;
    }
    out
}

/// Does every data block of this cabinet match its own checksum?
///
/// The cheap, complete answer to "did we reassemble this correctly". A cabinet
/// that fails here is one `expand` would open, chew on and reject with
/// `0x8007000D` — better to know before it costs a minute and a confusing
/// message in the report.
pub fn cab_data_intact(blob: &[u8]) -> bool {
    if blob.len() < 36 || !blob.starts_with(b"MSCF") {
        return false;
    }
    let u16at = |o: usize| u16::from_le_bytes([blob[o], blob[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]]);

    let folders = u16at(26) as usize;
    let flags = u16at(30);
    let mut p = 36usize;
    // With the reserve flag there are extra fields in the header, in each
    // folder entry and in front of each data block.
    let (mut res_folder, mut res_data) = (0usize, 0usize);
    if flags & 0x0004 != 0 {
        if blob.len() < p + 4 {
            return false;
        }
        let cb_header = u16at(p) as usize;
        res_folder = blob[p + 2] as usize;
        res_data = blob[p + 3] as usize;
        p += 4 + cb_header;
    }

    for f in 0..folders {
        let fo = p + f * (8 + res_folder);
        if fo + 8 > blob.len() {
            return false;
        }
        let mut d = u32at(fo) as usize;
        let blocks = u16at(fo + 4) as usize;
        for _ in 0..blocks {
            if d + 8 + res_data > blob.len() {
                return false;
            }
            let csum = u32at(d);
            let cb_data = u16at(d + 4) as usize;
            let body = d + 8 + res_data;
            if cb_data == 0 || body + cb_data > blob.len() {
                return false;
            }
            // The seed is the checksum of the two size fields, then the block.
            let seed = cab_checksum(&blob[d + 4..d + 8], 0);
            if cab_checksum(&blob[body..body + cb_data], seed) != csum {
                return false;
            }
            d = body + cb_data;
        }
    }
    true
}

/// The cabinet format's own checksum: XOR of the data as little-endian words,
/// with the trailing bytes packed big-endian.
fn cab_checksum(buf: &[u8], seed: u32) -> u32 {
    let mut ul = seed;
    let whole = buf.len() / 4 * 4;
    for c in buf[..whole].chunks_exact(4) {
        ul ^= u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    let tail = &buf[whole..];
    let mut tmp = 0u32;
    for &b in tail {
        tmp = (tmp << 8) | b as u32;
    }
    ul ^ tmp
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Turn an MSI file key back into a file name and the Windows variant it is for.
///
/// `_vm3d.inf_Win10.4C60EBF3_7457_440E_A848_1BB27D201892` → `vm3d.inf`, Win10.
/// A name with no such decoration is returned as it is.
pub fn unmangle(raw: &str) -> (String, Option<String>) {
    // The tail after the last dot is the component GUID with dashes turned into
    // underscores. Nothing else in these packages looks like that.
    let Some((base, tail)) = raw.rsplit_once('.') else {
        return (raw.to_string(), None);
    };
    let guid_like = tail.len() >= 8
        && tail.contains('_')
        && tail.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    if !guid_like {
        return (raw.to_string(), None);
    }
    // `_vm3d.inf_Win10` → name `vm3d.inf`, tag Win10.
    let (name, os) = match OS_TAGS.iter().find(|t| base.ends_with(&format!("_{t}"))) {
        Some(t) => (&base[..base.len() - t.len() - 1], Some(t.to_string())),
        None => (base, None),
    };
    (name.trim_start_matches('_').to_string(), os)
}

/// VMware Workstation's guest-tools image on this machine, if it has one.
///
/// There is regularly more than one. Workstation 17 moved from `Program Files
/// (x86)` to `Program Files`, and an upgrade leaves the old folder — with the
/// old `windows.iso` — exactly where it was. The two files are the same thing
/// at different versions, so the path says nothing about which to take and the
/// newest one is taken: the tools ISO is the driver package, and a newer one
/// knows about newer virtual hardware.
#[cfg(target_os = "windows")]
pub fn vmware_tools_iso() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(p) = std::env::var(var) {
            roots.push(PathBuf::from(p).join("VMware").join("VMware Workstation"));
        }
    }
    for key in [
        r"HKLM\SOFTWARE\VMware, Inc.\VMware Workstation",
        r"HKLM\SOFTWARE\WOW6432Node\VMware, Inc.\VMware Workstation",
    ] {
        if let Some(p) = reg_value(key, "InstallPath") {
            roots.push(PathBuf::from(p));
        }
    }

    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for iso in roots.into_iter().map(|r| r.join("windows.iso")) {
        if found.iter().any(|(_, p)| p == &iso) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&iso) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        found.push((meta.modified().unwrap_or(std::time::UNIX_EPOCH), iso));
    }
    if found.len() > 1 {
        info!(
            count = found.len(),
            "more than one windows.iso — taking the newest"
        );
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().next().map(|(_, p)| p)
}

/// Read one registry string with `reg.exe` (no PowerShell: this runs while the
/// user is waiting, and starting PowerShell costs more than the answer).
#[cfg(target_os = "windows")]
fn reg_value(key: &str, value: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(["query", key, "/v", value])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // "    InstallPath    REG_SZ    C:\Program Files (x86)\VMware\VMware Workstation\"
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.trim_start().starts_with(value))?;
    let rest = line.split("REG_SZ").nth(1)?.trim();
    (!rest.is_empty()).then(|| rest.trim_end_matches('\\').to_string())
}

/// Take the guest drivers out of `iso` and hand them to `add_driver`, one
/// package at a time. Returns how many went in.
///
/// The drivers are **files on the image**, not something to be installed: the
/// tools ISO carries `\Program Files\VMware\VMware Tools\Drivers\<name>\…` with
/// the `.inf`, `.sys` and `.cat` of each. So the image is walked for INFs and
/// the folders around the ones worth having are copied out — no installer is
/// run, and nothing is added to the machine doing the building.
///
/// Only if an image turns out to carry no drivers at all (older releases ship
/// just the MSI) is the installer unpacked, and even then by an administrative
/// install, which unpacks without installing.
///
/// `add_driver` is the caller's DISM call — this module finds and stages the
/// files, the builder owns how the image is serviced. It is called per package
/// so that one driver DISM refuses does not take the rest with it.
#[cfg(target_os = "windows")]
pub fn add_vmware_tools_drivers(
    iso: &Path,
    work: &Path,
    mut progress: impl FnMut(u64, &str),
    mut add_driver: impl FnMut(&Path) -> Result<()>,
) -> Result<(u32, Vec<String>)> {
    progress(5, &tr!("media_guest_drivers_win.mounting_iso"));
    let letter = mount_iso(iso)?;
    // From here on the ISO must be released whatever happens, so the work is
    // done in a closure and the result inspected after dismounting.
    // Packages that came out of the installer with a file missing: worth a line
    // in the report, since one of them may be the display driver the whole
    // feature is about.
    let mut incomplete: Vec<(String, Vec<String>)> = Vec::new();
    let outcome = (|| -> Result<u32> {
        let root = PathBuf::from(format!("{letter}:\\"));
        let staged = work.join("vmtools-drivers");
        let extract = work.join("vmtools");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&extract);
        std::fs::create_dir_all(&staged)?;

        progress(15, &tr!("media_guest_drivers_win.drivers_on_image"));
        let mut packages = stage_wanted_drivers(&root, &staged)?;

        // The image carries one driver as files (pvscsi) and keeps the rest —
        // the display driver above all — inside the installer.
        let setup = ["setup.exe", "setup64.exe"]
            .iter()
            .map(|n| root.join(n))
            .find(|p| p.is_file());
        if let Some(setup) = setup {
            match mine_setup_exe(&setup, work, &mut progress, &mut incomplete) {
                Ok(found) => {
                    for (name, dir) in found {
                        // What is on the image as files wins: it is already in
                        // the shape Windows expects.
                        if packages.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                            let _ = std::fs::remove_dir_all(&dir);
                            continue;
                        }
                        packages.push((name, dir));
                    }
                }
                Err(e) => warn!(error = %e, "the installer's cabinets could not be read"),
            }
        }

        if packages.is_empty() {
            // An older image, where everything is inside an MSI instead.
            if let Some(msi) = find_tools_msi(&root) {
                progress(45, &tr!("media_guest_drivers_win.unpacking_installer"));
                std::fs::create_dir_all(&extract)?;
                run(
                    "msiexec",
                    &[
                        "/a",
                        &msi.display().to_string(),
                        "/qn",
                        &format!("TARGETDIR={}", extract.display()),
                    ],
                )?;
                packages = stage_wanted_drivers(&extract, &staged)?;
            }
        }
        if packages.is_empty() {
            return Err(OwnBackupError::ImageFormat(
                tr!("media_guest_drivers_win.no_drivers", root = root.display()).into_owned(),
            ));
        }

        // One DISM call per package: a package DISM will not take (wrong
        // architecture, unsigned) must not cost us the display driver.
        let mut added = 0u32;
        let total = packages.len().max(1);
        for (i, (name, dir)) in packages.iter().enumerate() {
            progress((60 + i * 35 / total) as u64, name);
            match add_driver(dir) {
                Ok(()) => added += 1,
                Err(e) => warn!(driver = %name, error = %e, "guest driver refused by the image"),
            }
        }
        if added == 0 {
            return Err(OwnBackupError::ImageFormat(
                tr!("media_guest_drivers_win.none_accepted").into_owned(),
            ));
        }
        info!(packages = added, "VMware guest drivers added to the medium");
        Ok(added)
    })();

    progress(95, &tr!("media_guest_drivers_win.unmounting_iso"));
    dismount_iso(iso);
    let _ = std::fs::remove_dir_all(work.join("vmtools"));
    progress(100, "");

    let notes = incomplete
        .into_iter()
        .map(|(driver, missing)| {
            let names: Vec<String> = missing.iter().take(3).cloned().collect();
            tr!(
                "media_guest_drivers_win.incomplete",
                driver = driver,
                count = missing.len(),
                files = names.join(", "),
                more = if missing.len() > names.len() {
                    ", …"
                } else {
                    ""
                }
            )
            .into_owned()
        })
        .collect();
    outcome.map(|n| (n, notes))
}

/// Pull the driver packages out of the installer's cabinets into `staged`.
///
/// One cabinet at a time, and only the ones that hold an INF: the rest of the
/// installer is the tray application, the OpenGL runtime and the file-sharing
/// service, none of which belong on a rescue medium.
#[cfg(target_os = "windows")]
fn mine_setup_exe(
    setup: &Path,
    work: &Path,
    progress: &mut dyn FnMut(u64, &str),
    incomplete: &mut Vec<(String, Vec<String>)>,
) -> Result<Vec<(String, PathBuf)>> {
    let data = std::fs::read(setup)
        .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", setup.display())))?;

    // The cabinets are STREAMS in the MSI appended to the installer, not ranges
    // of it: their sectors are scattered through the file, so they have to be
    // put back together before they are anything but a valid-looking header.
    let compound = crate::ole_compound::Compound::find(&data).ok_or_else(|| {
        OwnBackupError::ImageFormat(
            tr!(
                "media_guest_drivers_win.not_an_installer",
                file = setup.display()
            )
            .into_owned(),
        )
    })?;
    let streams = compound.streams();
    let cabs: Vec<Vec<u8>> = streams
        .iter()
        .map(|s| compound.read(*s))
        .filter(|blob| blob.starts_with(b"MSCF"))
        .collect();
    info!(
        streams = streams.len(),
        cabinets = cabs.len(),
        file = %setup.display(),
        "installer opened"
    );

    // Cabinets are carved here and thrown away; what comes out of them lives
    // in a folder of its own until DISM has taken it.
    let carve_dir = work.join("vmtools-cabs");
    let out_dir = work.join("vmtools-from-setup");
    let _ = std::fs::remove_dir_all(&carve_dir);
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&carve_dir)?;
    std::fs::create_dir_all(&out_dir)?;

    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let total = cabs.len().max(1);
    for (i, blob) in cabs.iter().enumerate() {
        progress(
            (20 + i * 40 / total) as u64,
            &tr!("media_guest_drivers_win.reading_installer"),
        );

        // Which Windows variants of an INF does this cabinet hold? The names
        // are read out of the reassembled stream, so a cabinet whose header
        // survived carving but whose data did not cannot get this far.
        let Some(cab) = cabinets_in(blob).into_iter().next() else {
            continue;
        };
        let infs: Vec<(String, Option<String>)> = cab
            .names
            .iter()
            .map(|n| unmangle(n))
            .filter(|(name, _)| name.to_lowercase().ends_with(".inf"))
            .collect();
        if infs.is_empty() {
            continue;
        }
        let want_os = OS_PREFERENCE
            .iter()
            .find(|t| infs.iter().any(|(_, os)| os.as_deref() == Some(**t)))
            .map(|t| t.to_string());
        // A package with no variant we would use (Windows 7 only, say) is left
        // where it is — an image of one Windows does not take another's driver.
        if want_os.is_none() && infs.iter().any(|(_, os)| os.is_some()) {
            continue;
        }
        let driver = infs
            .iter()
            .find(|(_, os)| os.as_deref() == want_os.as_deref())
            .map(|(name, _)| {
                name.trim_end_matches(".inf")
                    .trim_end_matches(".INF")
                    .to_string()
            })
            .unwrap_or_default();
        if driver.is_empty() {
            continue;
        }

        // Every data block is checked against its own checksum before the
        // cabinet is handed on: a cabinet that does not verify here is a
        // cabinet DISM would reject after minutes of work.
        if !cab_data_intact(blob) {
            warn!(driver = %driver, "cabinet does not check out — skipped");
            continue;
        }
        let cab_path = carve_dir.join(format!("{driver}.cab"));
        std::fs::write(&cab_path, blob)?;
        // Into a folder of our own, never into one another source already
        // filled: a cabinet that fails to expand must not take the copy that
        // came off the image as files with it.
        let dst = out_dir.join(&driver);
        std::fs::create_dir_all(&dst)?;

        // `expand` is part of Windows and reads the LZX these cabinets use.
        //
        // The option comes AFTER the cabinet — `expand <cab> -F:<files> <dir>`
        // — and everything is taken rather than filtered by Windows variant:
        // guessing at how expand matches wildcards against names like
        // `_vm3d.inf_Win10.4C60EBF3_…` is what made every one of these calls
        // fail with exit code 1. The variants we do not want are dropped below,
        // where the rule is ours and is tested.
        let expanded = run(
            "expand",
            &[
                &cab_path.display().to_string(),
                "-F:*",
                &dst.display().to_string(),
            ],
        );
        let _ = std::fs::remove_file(&cab_path);
        if let Err(e) = expanded {
            warn!(driver = %driver, error = %e, "cabinet did not expand");
            let _ = std::fs::remove_dir_all(&dst);
            continue;
        }

        let kept = match rename_and_prune(&dst, want_os.as_deref()) {
            Ok(0) | Err(_) => {
                let _ = std::fs::remove_dir_all(&dst);
                continue;
            }
            Ok(n) => n,
        };
        // Now that the INF has its own name back, it can say what it is — and
        // whether a rescue medium has any use for it.
        let inf = dst.join(format!("{driver}.inf"));
        let class = inf_class(&inf).unwrap_or_default();
        if !WANTED_CLASSES.iter().any(|c| *c == class) {
            info!(driver = %driver, class = %class, "guest driver not for a rescue medium");
            let _ = std::fs::remove_dir_all(&dst);
            continue;
        }
        // A package is the INF AND every file it names. Handing DISM one with a
        // file missing costs a minute and comes back as `0x80070002` with
        // nothing said about which file — so the INF is asked here instead.
        let missing = reconcile_with_inf(&dst, &inf);
        if !missing.is_empty() {
            warn!(
                driver = %driver,
                files = %missing.len(),
                first = %missing.first().cloned().unwrap_or_default(),
                "driver package is incomplete — skipped"
            );
            let _ = std::fs::remove_dir_all(&dst);
            incomplete.push((driver, missing));
            continue;
        }
        info!(
            driver = %driver, class = %class, os = ?want_os, files = kept,
            "guest driver taken from the installer"
        );
        out.push((driver, dst));
    }
    let _ = std::fs::remove_dir_all(&carve_dir);
    Ok(out)
}

/// Put the real file names back on everything in `dir`, keeping only the
/// Windows variant asked for, and return how many files are left.
///
/// A cabinet holds every variant of its driver — four of them for the display
/// driver, 90 MB each — and they all carry the same real names, so they cannot
/// share a folder. Files with no variant in their key are shared between the
/// variants and are always kept.
fn rename_and_prune(dir: &Path, want_os: Option<&str>) -> Result<u32> {
    let mut kept = 0u32;
    for e in std::fs::read_dir(dir)?.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let raw = e.file_name().to_string_lossy().to_string();
        let (real, os) = unmangle(&raw);
        if let (Some(os), Some(want)) = (os.as_deref(), want_os) {
            if os != want {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        if real == raw {
            kept += 1;
            continue;
        }
        let target = dir.join(&real);
        if target.exists() && target != path {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        match std::fs::rename(&path, &target) {
            Ok(()) => kept += 1,
            Err(e) => tracing::warn!(file = %raw, error = %e, "could not put the real name back"),
        }
    }
    Ok(kept)
}

/// Mount the ISO and return its drive letter (`"E"`).
#[cfg(target_os = "windows")]
fn mount_iso(iso: &Path) -> Result<String> {
    // Mounting an image is the one thing here with no command-line equivalent;
    // `reg`, `msiexec` and DISM are all plain executables, this is not.
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         $d = Mount-DiskImage -ImagePath '{}' -PassThru; \
         ($d | Get-Volume).DriveLetter",
        iso.display()
    );
    let out = ps(&script)?;
    let letter = out.trim().trim_matches(['\r', '\n', ' ']).to_string();
    if letter.len() != 1 {
        return Err(OwnBackupError::ImageFormat(
            tr!(
                "media_guest_drivers_win.iso_not_mounted",
                answer = format!("{out:?}")
            )
            .into_owned(),
        ));
    }
    Ok(letter)
}

#[cfg(target_os = "windows")]
fn dismount_iso(iso: &Path) {
    let _ = ps(&format!(
        "Dismount-DiskImage -ImagePath '{}' | Out-Null",
        iso.display()
    ));
}

/// The tools installer on the image, when there is one at all. Newer images
/// carry the drivers as files and no MSI; older ones the other way round.
#[cfg(target_os = "windows")]
fn find_tools_msi(root: &Path) -> Option<PathBuf> {
    let mut msis: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("msi")))
        .collect();
    // "VMware Tools64.msi" over "VMware Tools.msi": the medium is x64.
    msis.sort_by_key(|p| {
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        usize::from(!name.contains("64"))
    });
    msis.into_iter().next()
}

/// Copy the driver packages worth having out of `from` into `staged`, and
/// return `(name, folder)` for each.
///
/// One package per driver, not per INF. The tools image keeps a driver's
/// variants under its own folder — `Drivers\pvscsi\Win8\amd64\pvscsi.inf` —
/// so the INFs are grouped by the driver folder and the x64 variant of each is
/// the one taken. Handing DISM an x86 package would only earn a refusal, and
/// handing it both would earn one per driver.
fn stage_wanted_drivers(from: &Path, staged: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut infs: Vec<PathBuf> = Vec::new();
    collect_infs(from, 0, &mut infs);

    // driver name -> (score, inf) — lower score is the better variant.
    let mut best: std::collections::BTreeMap<String, (u8, PathBuf)> = Default::default();
    for inf in infs {
        let Some(class) = inf_class(&inf) else {
            continue;
        };
        if !WANTED_CLASSES.iter().any(|c| *c == class) {
            continue;
        }
        let Some(arch) = arch_score(&inf) else {
            continue;
        };
        // Architecture first, then the Windows variant: the image is Windows 10
        // x64, and a driver folder holds one of each.
        let score = arch * 10 + os_score(&inf);
        let name = driver_name(&inf);
        match best.get(&name) {
            Some((s, _)) if *s <= score => {}
            _ => {
                best.insert(name, (score, inf));
            }
        }
    }

    let mut out = Vec::new();
    for (name, (_, inf)) in best {
        let Some(dir) = inf.parent() else { continue };
        let dst = staged.join(&name);
        std::fs::create_dir_all(&dst)?;
        copy_dir_flat(dir, &dst)?;
        info!(driver = %name, from = %dir.display(), "guest driver staged");
        out.push((name, dst));
    }
    Ok(out)
}

/// How well this INF's path suits an x64 medium: 0 = an x64 variant, 1 = no
/// architecture in the path at all, `None` = a variant for another
/// architecture, which is not worth staging.
fn arch_score(inf: &Path) -> Option<u8> {
    let path = inf.to_string_lossy().to_lowercase().replace('/', "\\");
    let has = |seg: &str| path.contains(&format!("\\{seg}\\"));
    if has("amd64") || has("x64") || has("win64") {
        return Some(0);
    }
    if has("i386") || has("x86") || has("win32") || has("arm64") || has("arm") {
        return None;
    }
    Some(1)
}

/// Which Windows variant this INF's path is for, as a rank (lower is better).
/// The image is Windows 10, so `…\pvscsi\Win10\amd64\` beats `…\Win7\amd64\`.
fn os_score(inf: &Path) -> u8 {
    let path = inf.to_string_lossy().to_lowercase().replace('/', "\\");
    for (i, tag) in OS_PREFERENCE.iter().enumerate() {
        if path.contains(&format!("\\{}\\", tag.to_lowercase())) {
            return i as u8;
        }
    }
    // No variant in the path at all: better than an old Windows, worse than a
    // folder that says Windows 10 outright.
    if OS_TAGS
        .iter()
        .any(|t| path.contains(&format!("\\{}\\", t.to_lowercase())))
    {
        OS_PREFERENCE.len() as u8 + 1
    } else {
        OS_PREFERENCE.len() as u8
    }
}

/// The name of the driver an INF belongs to: the folder directly under
/// `Drivers` when there is one (`pvscsi`), otherwise the INF's own name.
fn driver_name(inf: &Path) -> String {
    let parts: Vec<String> = inf
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if let Some(i) = parts.iter().position(|p| p.eq_ignore_ascii_case("Drivers")) {
        if let Some(name) = parts.get(i + 1) {
            return name.clone();
        }
    }
    inf.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

/// Every `.inf` under `dir`. Bounded depth: the tools image is a handful of
/// levels and an unbounded walk of a mounted image is a good way to hang.
fn collect_infs(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_infs(&p, depth + 1, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")) {
            out.push(p);
        }
    }
}

/// An INF as text. They are ANSI or UTF-16, and both have to read.
fn inf_text(inf: &Path) -> Option<String> {
    let bytes = std::fs::read(inf).ok()?;
    Some(
        if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            let units: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(&bytes).to_string()
        },
    )
}

/// The files an INF says its package consists of — the `[SourceDisksFiles]`
/// section, which is the list Windows checks a package against.
///
/// A driver package is not the INF but the INF AND everything it names, and a
/// package missing one file is refused by DISM with `0x80070002` and no hint
/// as to which one. Asking the INF ourselves turns that into a sentence with a
/// file name in it. An INF without the section yields nothing, and nothing is
/// claimed.
pub fn inf_source_files(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l.to_lowercase().starts_with("[sourcedisksfiles");
            continue;
        }
        if !inside || l.is_empty() || l.starts_with(';') {
            continue;
        }
        // `vm3dmp.sys = 1,,` — the name is what stands before the equals sign.
        let Some((name, _)) = l.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && !name.contains(['\\', '/', '%']) {
            out.push(name.to_string());
        }
    }
    out
}

/// Make the folder match what the INF asks for, and return what is still
/// missing after that.
///
/// The names in the installer are MSI file KEYS, and a key is an identifier:
/// it cannot hold a hyphen. So VMware's build writes `vm3dmp_debug.sys` for a
/// file the INF calls `vm3dmp-debug.sys`, and thirteen of the display driver's
/// twenty-seven files come out under the wrong name — which is why DISM
/// imported nothing and said only `0x80070002`.
///
/// The INF is the authority on the names, so it is asked: for every file it
/// wants that is not there, a file differing only in `-` against `_` is
/// renamed to it. Only an unambiguous match is taken; anything left over is
/// reported by name rather than left for DISM to trip over.
fn reconcile_with_inf(dir: &Path, inf: &Path) -> Vec<String> {
    let Some(text) = inf_text(inf) else {
        return Vec::new();
    };
    let mut present: std::collections::HashMap<String, String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (name.to_lowercase(), name)
        })
        .collect();

    let mut missing = Vec::new();
    for want in inf_source_files(&text) {
        let low = want.to_lowercase();
        if present.contains_key(&low) {
            continue;
        }
        let key = low.replace('-', "_");
        let candidates: Vec<String> = present
            .keys()
            .filter(|p| p.replace('-', "_") == key)
            .cloned()
            .collect();
        match candidates.as_slice() {
            [only] => {
                let from = present[only].clone();
                match std::fs::rename(dir.join(&from), dir.join(&want)) {
                    Ok(()) => {
                        present.remove(only);
                        present.insert(low, want.clone());
                        info!(from = %from, to = %want, "driver file renamed to what the INF calls it");
                    }
                    Err(e) => {
                        tracing::warn!(file = %from, error = %e, "could not rename to the INF's name");
                        missing.push(want);
                    }
                }
            }
            _ => missing.push(want),
        }
    }
    missing
}

/// The `Class=` of an INF, lower-cased.
fn inf_class(inf: &Path) -> Option<String> {
    let text = inf_text(inf)?;
    for line in text.lines() {
        let l = line.trim();
        // `Class = Display` — and not `ClassGuid=`, which starts the same way.
        let Some(rest) = l.strip_prefix("Class").or_else(|| l.strip_prefix("CLASS")) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(v) = rest.strip_prefix('=') else {
            continue;
        };
        let v = v.split(';').next().unwrap_or("").trim().to_lowercase();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// Copy a driver folder (files plus one level of subfolders — that is how these
/// packages are shaped: the INF beside its .sys, sometimes an `amd64` folder).
fn copy_dir_flat(src: &Path, dst: &Path) -> Result<()> {
    for e in std::fs::read_dir(src)?.flatten() {
        let p = e.path();
        let target = dst.join(e.file_name());
        if p.is_dir() {
            std::fs::create_dir_all(&target)?;
            for f in std::fs::read_dir(&p)?.flatten() {
                if f.path().is_file() {
                    let _ = std::fs::copy(f.path(), target.join(f.file_name()));
                }
            }
        } else {
            let _ = std::fs::copy(&p, &target);
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn ps(script: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| OwnBackupError::ImageFormat(format!("powershell: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(2)
            .collect();
        warn!(script, error = %tail.join(" "), "powershell step failed");
        return Err(OwnBackupError::ImageFormat(format!(
            "powershell: {}",
            tail.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(target_os = "windows")]
fn run(program: &str, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| OwnBackupError::ImageFormat(format!("{program}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    // What the tool said is the useful part — decoded, since it says it in the
    // console's own code page.
    let text = crate::media_layout::console_text(&out.stdout);
    let tail: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .rev()
        .take(2)
        .collect();
    Err(OwnBackupError::ImageFormat(
        tr!(
            "media_guest_drivers_win.tool_failed",
            program = program,
            code = out.status.code().unwrap_or(-1),
            output = tail.into_iter().rev().collect::<Vec<_>>().join(" ")
        )
        .into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_class_out_of_an_inf() {
        let dir = tempfile::tempdir().unwrap();
        let inf = dir.path().join("vm3d.inf");
        std::fs::write(
            &inf,
            "[Version]\r\nSignature=\"$WINDOWS NT$\"\r\nClass=Display\r\n\
             ClassGuid={4d36e968-e325-11ce-bfc1-08002be10318}\r\n",
        )
        .unwrap();
        assert_eq!(inf_class(&inf).as_deref(), Some("display"));
    }

    /// `ClassGuid=` must not be mistaken for `Class=` — it comes first in
    /// plenty of INFs and would make every driver look like a GUID.
    #[test]
    fn classguid_is_not_the_class() {
        let dir = tempfile::tempdir().unwrap();
        let inf = dir.path().join("x.inf");
        std::fs::write(
            &inf,
            "ClassGuid={4d36e97b-e325-11ce-bfc1-08002be10318}\r\nClass = SCSIAdapter\r\n",
        )
        .unwrap();
        assert_eq!(inf_class(&inf).as_deref(), Some("scsiadapter"));
    }

    /// The layout of a real tools image: the drivers are files on it, each
    /// with a variant per architecture, and no installer in sight.
    #[test]
    fn takes_the_x64_variant_of_each_driver_off_the_image() {
        let iso = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let drivers = iso
            .path()
            .join("Program Files")
            .join("VMware")
            .join("VMware Tools")
            .join("Drivers");

        let write_inf = |dir: &Path, name: &str, class: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join(format!("{name}.inf")),
                format!("Class={class}\r\n"),
            )
            .unwrap();
            std::fs::write(dir.join(format!("{name}.sys")), b"binary").unwrap();
        };
        write_inf(
            &drivers.join("pvscsi").join("Win8").join("amd64"),
            "pvscsi",
            "SCSIAdapter",
        );
        write_inf(
            &drivers.join("pvscsi").join("Win8").join("i386"),
            "pvscsi",
            "SCSIAdapter",
        );
        write_inf(&drivers.join("vm3d").join("amd64"), "vm3d", "Display");
        write_inf(&drivers.join("vmxnet3").join("amd64"), "vmxnet3", "Net");
        // Not a rescue driver: shared folders have no business on the medium.
        write_inf(&drivers.join("vmhgfs").join("amd64"), "vmhgfs", "System");

        let mut got = stage_wanted_drivers(iso.path(), staged.path()).unwrap();
        got.sort();
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["pvscsi", "vm3d", "vmxnet3"]);

        // The staged folder is the x64 one, with the driver's files beside it.
        let (_, pvscsi) = got.iter().find(|(n, _)| n == "pvscsi").unwrap();
        assert!(pvscsi.join("pvscsi.inf").is_file());
        assert!(pvscsi.join("pvscsi.sys").is_file());
    }

    #[test]
    fn other_architectures_are_left_alone() {
        assert_eq!(
            arch_score(Path::new(r"D:\Drivers\pvscsi\Win8\amd64\pvscsi.inf")),
            Some(0)
        );
        assert_eq!(
            arch_score(Path::new(r"D:\Drivers\pvscsi\Win8\i386\pvscsi.inf")),
            None
        );
        assert_eq!(arch_score(Path::new(r"D:\Drivers\arm64\vm3d.inf")), None);
        // No architecture in the path is not a reason to skip it.
        assert_eq!(arch_score(Path::new(r"D:\Drivers\vm3d\vm3d.inf")), Some(1));
    }

    /// The file keys inside the installer's cabinets, as they really look.
    #[test]
    fn msi_file_keys_turn_back_into_file_names() {
        assert_eq!(
            unmangle("_vm3d.inf_Win10.4C60EBF3_7457_440E_A848_1BB27D201892"),
            ("vm3d.inf".into(), Some("Win10".into()))
        );
        assert_eq!(
            unmangle("pvscsi.sys_Win7.E3C2961E_3524_4169_97FF_B616A43D0E19"),
            ("pvscsi.sys".into(), Some("Win7".into()))
        );
        assert_eq!(
            unmangle("_7za.exe.869A7E00_8665_0000_83A8_EF0F76CF0001"),
            ("7za.exe".into(), None)
        );
        // A plain name is not decoration and must survive untouched.
        assert_eq!(
            unmangle("install_rvmSetup.cmd"),
            ("install_rvmSetup.cmd".into(), None)
        );
        assert_eq!(unmangle("vm3d.inf"), ("vm3d.inf".into(), None));
    }

    /// A cabinet header carved out of a bigger file, and an `MSCF` that is only
    /// a coincidence inside compressed data.
    #[test]
    fn cabinets_are_found_and_false_ones_are_not() {
        fn cab(names: &[&str]) -> Vec<u8> {
            let mut files = Vec::new();
            for n in names {
                files.extend_from_slice(&[0u8; 16]); // size/offset/folder/date/time/attr
                files.extend_from_slice(n.as_bytes());
                files.push(0);
            }
            let coff = 36u32;
            let size = 36 + files.len() as u32;
            let mut v = Vec::new();
            v.extend_from_slice(b"MSCF");
            v.extend_from_slice(&0u32.to_le_bytes()); // reserved1
            v.extend_from_slice(&size.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes()); // reserved2
            v.extend_from_slice(&coff.to_le_bytes()); // coffFiles
            v.extend_from_slice(&0u32.to_le_bytes()); // reserved3
            v.push(3);
            v.push(1); // version
            v.extend_from_slice(&1u16.to_le_bytes()); // folders
            v.extend_from_slice(&(names.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // flags
            v.extend_from_slice(&0u16.to_le_bytes()); // setID
            v.extend_from_slice(&0u16.to_le_bytes()); // iCabinet
            v.extend_from_slice(&files);
            v
        }

        let mut blob = vec![0xAAu8; 512];
        // A coincidence: the signature with nonsense behind it.
        blob.extend_from_slice(b"MSCF");
        blob.extend_from_slice(&[0xFF; 64]);
        blob.extend_from_slice(&cab(&[
            "_vm3d.inf_Win10.4C60EBF3_7457_440E_A848_1BB27D201892",
        ]));
        blob.extend_from_slice(&vec![0x11u8; 128]);
        blob.extend_from_slice(&cab(&[
            "pvscsi.sys_Win10.9CD0B9A0_B52A_4547_9E49_4A6F72E7F651",
        ]));

        let found = cabinets_in(&blob);
        assert_eq!(
            found.len(),
            2,
            "the false signature must not be taken for a cabinet"
        );
        assert_eq!(unmangle(&found[0].names[0]).0, "vm3d.inf");
        assert_eq!(unmangle(&found[1].names[0]).0, "pvscsi.sys");
    }

    /// Against a real installer, end to end: the cabinets come out of the
    /// compound file, they check out block by block, and the display driver is
    /// among them. `scripts/vmtools-inspect.py` is the same check in Python.
    ///
    /// `VMTOOLS_SETUP=/path/to/setup.exe cargo test -p own-backup-backup -- --ignored`
    #[test]
    #[ignore = "needs a real VMware Tools setup.exe via VMTOOLS_SETUP"]
    fn takes_the_display_driver_out_of_a_real_installer() {
        let Ok(path) = std::env::var("VMTOOLS_SETUP") else {
            return;
        };
        let data = std::fs::read(path).unwrap();
        let compound = crate::ole_compound::Compound::find(&data)
            .expect("the installer carries a compound file");

        let mut cabinets = 0;
        let mut display = None;
        for s in compound.streams() {
            let blob = compound.read(s);
            if !blob.starts_with(b"MSCF") {
                continue;
            }
            cabinets += 1;
            assert!(
                cab_data_intact(&blob),
                "a reassembled cabinet must match its own checksums"
            );
            let Some(cab) = cabinets_in(&blob).into_iter().next() else {
                continue;
            };
            for n in &cab.names {
                let (real, os) = unmangle(n);
                if real.eq_ignore_ascii_case("vm3d.inf") && os.as_deref() == Some("Win10") {
                    display = Some(blob.len());
                }
            }
        }
        assert!(
            cabinets > 5,
            "a tools installer holds a couple of dozen cabinets"
        );
        let size = display.expect("the WDDM display driver must be in there");
        println!(
            "{cabinets} cabinets, all intact; vm3d is {} MB",
            size / 1_048_576
        );
    }

    /// The display driver, as it really comes out of the installer: thirteen of
    /// its files carry an underscore where the INF wants a hyphen, because an
    /// MSI file key cannot hold one. Without this the package is incomplete and
    /// DISM refuses it with nothing but `0x80070002`.
    #[test]
    fn the_inf_decides_what_the_files_are_called() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // What the cabinet gives us, after the mangled keys are undone.
        for f in [
            "vm3d.inf",
            "vm3d.cat",
            "vm3dmp.sys",
            "vm3dmp_debug.sys",
            "vm3dmp_stats.sys",
            "vm3dmp_loader.sys",
            "vm3ddevapi64.dll",
            "vm3ddevapi64_release.dll",
            "vm3dum64_10.dll",
            "vm3dum64_10_stats.dll",
        ] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        // What the INF asks for — hyphens and all.
        std::fs::write(
            d.join("vm3d.inf"),
            "[Version]\r\nClass=Display\r\nCatalogFile = vm3d.cat\r\n\
             [SourceDisksFiles]\r\n\
             vm3dmp.sys = 1,,\r\n\
             vm3dmp-debug.sys = 1,,\r\n\
             vm3dmp-stats.sys = 1,,\r\n\
             vm3dmp_loader.sys = 1,,\r\n\
             vm3ddevapi64.dll = 1,,\r\n\
             vm3ddevapi64-release.dll = 1,,\r\n\
             vm3dum64_10.dll = 1,,\r\n\
             vm3dum64_10-stats.dll = 1,,\r\n",
        )
        .unwrap();

        let missing = reconcile_with_inf(d, &d.join("vm3d.inf"));
        assert!(
            missing.is_empty(),
            "nothing should be left missing: {missing:?}"
        );
        for f in [
            "vm3dmp-debug.sys",
            "vm3dmp-stats.sys",
            "vm3ddevapi64-release.dll",
            "vm3dum64_10-stats.dll",
        ] {
            assert!(
                d.join(f).is_file(),
                "{f} should have been renamed to the INF's name"
            );
        }
        // An underscore the INF itself uses must be left alone.
        assert!(d.join("vm3dmp_loader.sys").is_file());
    }

    /// A file the INF wants that is nowhere near the folder stays reported.
    #[test]
    fn a_genuinely_missing_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(
            d.join("x.inf"),
            "[SourceDisksFiles]\r\nthere.sys = 1,,\r\ngone.sys = 1,,\r\n",
        )
        .unwrap();
        std::fs::write(d.join("there.sys"), b"x").unwrap();
        assert_eq!(
            reconcile_with_inf(d, &d.join("x.inf")),
            vec!["gone.sys".to_string()]
        );
    }

    #[test]
    fn the_pseudo_id_is_recognisable_as_one() {
        assert!(VMWARE_TOOLS_ID.starts_with(PSEUDO_PREFIX));
        assert!(
            !VMWARE_TOOLS_ID.ends_with(".inf"),
            "it must not look like a real package"
        );
    }
}
