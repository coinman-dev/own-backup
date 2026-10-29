//! Junk by file masks — Ace Utilities' "Clean Junk", made safe (owner,
//! 2026-10-28: «только после просмотра списка»).
//!
//! Another cleaner's masks over whole disks took, on the owner's machine, the
//! Acronis agent's keys (`*.prv`), a document's backup (`*.bak`) and DaVinci
//! Resolve's settings. So here:
//!
//! - the search runs through the chosen users' profiles, and through folders
//!   an administrator adds — not through every disk by itself;
//! - whole kinds of folders are never entered whatever the masks: programs
//!   and Windows (`Windows`, `Program Files`, `ProgramData`), programs' data
//!   (`AppData` — its junk is the other items' business), version control
//!   and packages (`.git`, `node_modules`), synced cloud folders (removing a
//!   file there removes it in the cloud) — and links and junctions;
//! - a mask too broad to mean junk (`*.*`, `*.d*`) is refused;
//! - this program's own files (`.own-backup-*`, backup images) never match;
//! - a file is junk only when unchanged for the days given (a week);
//! - the cleanup removes only what the analysis listed and the window showed
//!   ([`crate::catalog::Item::reviewed`]), never a folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::engine::{is_link, matches_mask, remove_file, Found, Mode, Tally, LIST_CAP};
use crate::users::User;

/// What kind of junk a mask finds — how the window groups and explains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Programs' temporary files.
    Temp,
    /// Left by Windows and old programs (help indexes, dumps, thumbnails).
    Junk,
    /// Intermediate files of a build (Visual Studio): a project rebuilds them.
    Build,
    /// Backup copies — of a document too (`Работа.odt.bak`).
    Backup,
    /// Logs.
    Log,
    /// Pieces of files a disk check recovered — they may be yours.
    Recovered,
}

/// A mask the window offers.
#[derive(Debug, Clone, Copy)]
pub struct Known {
    pub mask: &'static str,
    pub kind: Kind,
    pub default_on: bool,
}

const fn known(mask: &'static str, kind: Kind, default_on: bool) -> Known {
    Known {
        mask,
        kind,
        default_on,
    }
}

/// Ace Utilities' list (owner, 2026-10-28), sorted by kind. On by default:
/// temporary files and junk; off: backups (`*.prv` took the Acronis agent's
/// keys, `*.bak` a document's backup on the owner's machine), logs, files a
/// disk check recovered, a build's intermediate files. Ace's `*.tmp*` is not
/// offered at all (owner, 2026-10-28): it takes templates (`.tmpl`) too.
pub const KNOWN: &[Known] = &[
    // From the owner's Linux script (wsl-cleanup.sh): what Windows' copies
    // and macOS' leave behind, and editors' leftovers.
    known("*Zone.Identifier", Kind::Junk, true),
    known(".DS_Store", Kind::Junk, true),
    known("*.swp", Kind::Temp, false),
    known("*.swo", Kind::Temp, false),
    known("*.orig", Kind::Backup, false),
    known("*.rej", Kind::Backup, false),
    known("*.tmp", Kind::Temp, true),
    known("*.temp", Kind::Temp, true),
    known("~$*", Kind::Temp, true),
    known("*.~mp", Kind::Temp, true),
    known("*._mp", Kind::Temp, true),
    known("*._dd", Kind::Temp, true),
    known("*._detmp", Kind::Temp, true),
    known("*.$$$", Kind::Temp, true),
    known("*.$db", Kind::Temp, true),
    known("*.db$", Kind::Temp, true),
    known("*.??$", Kind::Temp, true),
    known("*.?$?", Kind::Temp, true),
    known("*.??~", Kind::Temp, true),
    known("*.?~?", Kind::Temp, true),
    known("*.~??", Kind::Temp, true),
    known("*.~*", Kind::Temp, true),
    known("~*.???", Kind::Temp, true),
    known("~*.", Kind::Temp, true),
    known("*.---", Kind::Temp, true),
    known("*.___", Kind::Temp, true),
    known("*.^", Kind::Temp, true),
    known("Thumbs.db", Kind::Junk, true),
    known("thumbcache*.db", Kind::Junk, true),
    known("*.dmp", Kind::Junk, true),
    known("*.gid", Kind::Junk, true),
    known("*.fts", Kind::Junk, true),
    known("*.ftg", Kind::Junk, true),
    known("*.nch", Kind::Junk, true),
    known("0*.nch", Kind::Junk, true),
    // Off: a project's tests keep expected errors so (`tests/*.err`).
    known("*.err", Kind::Junk, false),
    known("CHKLIST.MS", Kind::Junk, true),
    known("chklist.*", Kind::Junk, true),
    known("*.ilk", Kind::Build, false),
    known("*.aps", Kind::Build, false),
    known("*.ncb", Kind::Build, false),
    known("*.pch", Kind::Build, false),
    known("*.old", Kind::Backup, false),
    known("*.bak", Kind::Backup, false),
    known("*.bk", Kind::Backup, false),
    known("*.db.bak", Kind::Backup, false),
    known("*.wbk", Kind::Backup, false),
    known("*.sik", Kind::Backup, false),
    known("*.prv", Kind::Backup, false),
    known("*.log", Kind::Log, false),
    known("*log.txt", Kind::Log, false),
    known("*.chk", Kind::Recovered, false),
];

/// A mask as the window lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MaskDto {
    pub mask: &'static str,
    pub kind: Kind,
    /// The kind's name («Временные»).
    pub kind_label: String,
    /// What such files are and what removing them means.
    pub about: String,
    pub default_on: bool,
}

/// The masks offered, for the window.
pub fn offered() -> Vec<MaskDto> {
    use own_backup_common::tr;
    KNOWN
        .iter()
        .map(|k| {
            let (label, about) = match k.kind {
                Kind::Temp => (tr!("cleanup.mask_temp"), tr!("cleanup.mask_temp_about")),
                Kind::Junk => (tr!("cleanup.mask_junk"), tr!("cleanup.mask_junk_about")),
                Kind::Build => (tr!("cleanup.mask_build"), tr!("cleanup.mask_build_about")),
                Kind::Backup => (tr!("cleanup.mask_backup"), tr!("cleanup.mask_backup_about")),
                Kind::Log => (tr!("cleanup.mask_log"), tr!("cleanup.mask_log_about")),
                Kind::Recovered => (
                    tr!("cleanup.mask_recovered"),
                    tr!("cleanup.mask_recovered_about"),
                ),
            };
            // A few say more than their kind.
            let about = match k.mask {
                "~$*" => tr!("cleanup.mask_owner_about"),
                "*.prv" => tr!("cleanup.mask_prv_about"),
                "*.wbk" => tr!("cleanup.mask_wbk_about"),
                _ => about,
            };
            MaskDto {
                mask: k.mask,
                kind: k.kind,
                kind_label: label.into_owned(),
                about: about.into_owned(),
                default_on: k.default_on,
            }
        })
        .collect()
}

/// The masks on by default.
pub fn default_masks() -> Vec<String> {
    KNOWN
        .iter()
        .filter(|k| k.default_on)
        .map(|k| k.mask.to_string())
        .collect()
}

/// A week: a temporary file of something still at work is younger.
pub const DEFAULT_AGE_DAYS: u32 = 7;

/// What the search looks for and where.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct JunkOptions {
    pub masks: Vec<String>,
    /// Only files unchanged for this many days (at least one).
    pub age_days: u32,
    /// Folders searched besides the users' profiles — an administrator's
    /// window only.
    pub folders: Vec<PathBuf>,
}

impl Default for JunkOptions {
    fn default() -> Self {
        Self {
            masks: default_masks(),
            age_days: DEFAULT_AGE_DAYS,
            folders: Vec::new(),
        }
    }
}

/// Why a mask is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskProblem {
    Empty,
    /// A name only: no folders, no drive.
    Path,
    /// Would catch files that are not junk: it takes one of [`PRECIOUS`], or
    /// it says too little — fewer than two characters besides `*`, `?` and
    /// `.`, unless one of them is a mark temporary files carry (`~ $ ^ _ -`):
    /// `*.*`, `*.d*` (`.docx` too), `K*`.
    TooBroad,
}

/// Names of files that are anything but junk: a mask taking any of them is
/// too broad.
const PRECIOUS: &[&str] = &[
    "Отчёт.docx",
    "report.doc",
    "table.xlsx",
    "slides.pptx",
    "paper.pdf",
    "photo.jpg",
    "image.png",
    "video.mp4",
    "movie.mkv",
    "song.mp3",
    "archive.zip",
    "archive.7z",
    "setup.exe",
    "library.dll",
    "notes.txt",
    "readme.md",
    "page.html",
    "main.rs",
    "program.py",
    "script.js",
    "config.ini",
    "settings.json",
    "data.xml",
    "database.db",
    "database.sqlite",
    "key.pem",
    "id_rsa",
    "Makefile",
    "book.epub",
    "drawing.psd",
    "project.drp",
    "disk.vhdx",
    "copy.obk",
];

/// Marks temporary files carry in their names.
const TEMP_MARKS: &[char] = &['~', '$', '^', '_', '-'];

pub fn check_mask(mask: &str) -> Result<(), MaskProblem> {
    let mask = mask.trim();
    if mask.is_empty() {
        return Err(MaskProblem::Empty);
    }
    if mask.contains(['\\', '/', ':']) {
        return Err(MaskProblem::Path);
    }
    let literal: Vec<char> = mask
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '.'))
        .collect();
    let says_enough = literal.len() >= 2 || literal.iter().any(|c| TEMP_MARKS.contains(c));
    if !says_enough || PRECIOUS.iter().any(|name| mask_matches(mask, name)) {
        return Err(MaskProblem::TooBroad);
    }
    Ok(())
}

/// A mask as Windows reads it: `*` and `?`, case-insensitive; a mask ending
/// in a dot (`~*.`) takes names without an extension.
pub fn mask_matches(mask: &str, name: &str) -> bool {
    match mask.strip_suffix('.') {
        Some(stem) if !stem.is_empty() => !name.contains('.') && matches_mask(stem, name),
        _ => matches_mask(mask, name),
    }
}

/// Folders never entered, by name, wherever they are.
const NEVER_ENTERED: &[&str] = &[
    // Windows and programs.
    "Windows",
    "Windows.old",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "System Volume Information",
    "Recovery",
    "$WinREAgent",
    "$Windows.~BT",
    "$Windows.~WS",
    "$SysReset",
    "$GetCurrent",
    "Config.Msi",
    "Boot",
    // Programs' data: the other items' business.
    "AppData",
    // Version control and packages.
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    // Python's installed packages.
    "site-packages",
    "dist-packages",
];

/// Synced cloud folders, by the start of their name (`OneDrive - Работа`).
const CLOUD: &[&str] = &[
    "OneDrive",
    "Dropbox",
    "Google Drive",
    "Yandex.Disk",
    "YandexDisk",
    "iCloudDrive",
    "iCloud Drive",
    "Mail.Ru Cloud",
];

/// This program's own files: never junk, whatever the masks.
fn ours(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with(".own-backup")
        || [".obk", ".vhdx", ".avhdx", ".vhd", ".qcow2"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Hidden folders are programs' data — `~/.cargo`, `~/.npm`, `~/.config`
/// on Linux, `.rustup` and `.cargo` in a Windows profile too — as AppData
/// is: on the owner's machine (2026-10-28) `*.err` found 25 test files in a
/// crate's sources under `~/.cargo/registry`, `.DS_Store` others under
/// `~/.cargo/git`. A file there is never junk to a mask.
fn entered(name: &str) -> bool {
    let lower = name.to_lowercase();
    !name.starts_with('.')
        && !NEVER_ENTERED.iter().any(|n| n.eq_ignore_ascii_case(name))
        && !CLOUD.iter().any(|c| lower.starts_with(&c.to_lowercase()))
}

/// Where the search runs: the users' profiles and the folders added, a
/// folder inside another one once.
pub fn places(users: &[User], options: &JunkOptions) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = users
        .iter()
        .map(|u| u.profile.clone())
        .chain(options.folders.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for p in all {
        if !kept.iter().any(|k| p.starts_with(k)) {
            kept.push(p);
        }
    }
    kept
}

/// Search (analysis) or remove (cleanup) the junk below `places`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk(
    places: &[PathBuf],
    options: &JunkOptions,
    mode: Mode,
    now: SystemTime,
    cancel: &AtomicBool,
    only: Option<&std::collections::HashSet<PathBuf>>,
    tally: &mut Tally,
    found: Option<&mut Vec<Found>>,
    progress: &mut dyn FnMut(&Tally),
) {
    let masks: Vec<&str> = options
        .masks
        .iter()
        .map(|m| m.trim())
        .filter(|m| check_mask(m).is_ok())
        .collect();
    if masks.is_empty() {
        return;
    }
    let mut search = Search {
        masks,
        min_age: Duration::from_secs(u64::from(options.age_days.max(1)) * 86_400),
        mode,
        now,
        cancel,
        only,
        tally,
        found,
        progress,
        seen: 0,
        device: None,
    };
    for place in places {
        match fs::symlink_metadata(place) {
            Ok(m) if m.is_dir() && !is_link(&m) => {
                search.device = device_of(&m);
                search.dir(place)
            }
            _ => {}
        }
    }
}

struct Search<'a> {
    masks: Vec<&'a str>,
    min_age: Duration,
    mode: Mode,
    now: SystemTime,
    cancel: &'a AtomicBool,
    only: Option<&'a std::collections::HashSet<PathBuf>>,
    tally: &'a mut Tally,
    found: Option<&'a mut Vec<Found>>,
    progress: &'a mut dyn FnMut(&Tally),
    seen: u64,
    /// The file system of the place being searched (Unix).
    device: Option<u64>,
}

fn device_of(meta: &fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn same_device(place: Option<u64>, meta: &fs::Metadata) -> bool {
    match (place, device_of(meta)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

impl Search<'_> {
    fn dir(&mut self, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if is_link(&meta) || !(meta.is_dir() || meta.is_file()) {
                continue;
            }
            // Never onto another file system (Linux: /proc, a mounted
            // share, a stick) below the place searched.
            if meta.is_dir() && !same_device(self.device, &meta) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if meta.is_dir() {
                if entered(&name) {
                    self.dir(&path);
                }
                continue;
            }
            if ours(&name)
                || !self.masks.iter().any(|m| mask_matches(m, &name))
                || !self.old_enough(&meta)
                || self.only.is_some_and(|only| !only.contains(&path))
            {
                continue;
            }
            let size = meta.len();
            match self.mode {
                Mode::Analyze => {
                    self.tally.files += 1;
                    self.tally.bytes += size;
                    if let Some(found) = self.found.as_deref_mut() {
                        if found.len() < LIST_CAP {
                            found.push(Found {
                                path,
                                size,
                                modified: meta.modified().ok(),
                            });
                        }
                    }
                }
                Mode::Remove => match remove_file(&path, &meta) {
                    Ok(()) => {
                        self.tally.files += 1;
                        self.tally.bytes += size;
                    }
                    Err(e) => {
                        tracing::debug!(file = %path.display(), error = %e, "cleanup: left");
                        self.tally.left_files += 1;
                        self.tally.left_bytes += size;
                    }
                },
            }
            self.seen += 1;
            if self.seen.is_multiple_of(200) {
                (self.progress)(self.tally);
            }
        }
    }

    fn old_enough(&self, meta: &fs::Metadata) -> bool {
        let newest = [meta.modified().ok(), meta.created().ok()]
            .into_iter()
            .flatten()
            .max();
        newest.is_some_and(|t| {
            self.now
                .duration_since(t)
                .is_ok_and(|age| age >= self.min_age)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_too_broad_or_with_a_path_are_refused() {
        // Every mask of Ace Utilities' list passes, once each.
        let mut seen = std::collections::HashSet::new();
        for k in KNOWN {
            assert_eq!(check_mask(k.mask), Ok(()), "{}", k.mask);
            assert!(seen.insert(k.mask.to_lowercase()), "{} twice", k.mask);
        }
        assert!(default_masks().contains(&"*.tmp".to_string()));
        for off in ["*.bak", "*.prv", "*.log", "*log.txt", "*.chk"] {
            assert!(!default_masks().contains(&off.to_string()), "{off}");
        }
        assert_eq!(check_mask(" "), Err(MaskProblem::Empty));
        for broad in [
            "*", "*.*", "*.?", "*.???", "*.d*", "a*", "K*", "?.??", "...", "*.doc*", "*.txt",
            "photo*",
        ] {
            assert_eq!(check_mask(broad), Err(MaskProblem::TooBroad), "{broad}");
        }
        for path in [r"C:\*.tmp", "Temp/*.tmp", r"..\*.tmp"] {
            assert_eq!(check_mask(path), Err(MaskProblem::Path), "{path}");
        }
    }

    #[test]
    fn masks_read_as_windows_reads_them() {
        assert!(mask_matches("~*.", "~WRL0001"));
        assert!(!mask_matches("~*.", "~WRL0001.tmp"));
        assert!(mask_matches("~*.???", "~WRL0001.tmp"));
        assert!(mask_matches("*.~*", "Отчёт.~docx"));
        assert!(mask_matches("thumbcache*.db", "thumbcache_1024.db"));
        assert!(mask_matches("*log.txt", "ResolveDebuglog.txt"));
        assert!(!mask_matches("*.tmp", "page.tmpl"));
        assert!(mask_matches("*.tmp*", "page.tmpl"));
        assert!(!KNOWN.iter().any(|k| k.mask == "*.tmp*"), "not offered");
    }

    #[test]
    fn our_files_and_system_folders_are_never_touched() {
        assert!(ours(".own-backup-new-0123.tmp.vhdx"));
        assert!(ours("Disk_C_1-full_x.obk"));
        assert!(!ours("setup.tmp"));
        assert!(!entered("AppData"));
        assert!(!entered("program files"));
        assert!(!entered("OneDrive - Работа"));
        assert!(!entered(".git"));
        assert!(!entered(".cargo"), "a hidden folder is a program's data");
        assert!(!entered(".config"));
        assert!(!entered("site-packages"));
        assert!(entered("Documents"));
    }

    #[test]
    fn a_folder_inside_another_is_searched_once() {
        let u = |p: &str| User {
            sid: "S-1-5-21-1".into(),
            name: "u".into(),
            profile: PathBuf::from(p),
        };
        let options = JunkOptions {
            folders: vec![PathBuf::from("/u/Documents"), PathBuf::from("/d")],
            ..Default::default()
        };
        assert_eq!(
            places(&[u("/u")], &options),
            vec![PathBuf::from("/d"), PathBuf::from("/u")]
        );
    }
}
