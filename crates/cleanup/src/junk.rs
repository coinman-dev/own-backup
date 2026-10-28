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

/// The masks offered: files that are junk wherever they are.
pub const DEFAULT_MASKS: &[&str] = &[
    "*.tmp",
    "*.temp",
    "~$*",
    "Thumbs.db",
    "*.dmp",
    "*.gid",
    "*._mp",
    "*.$$$",
];
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
            masks: DEFAULT_MASKS.iter().map(|m| m.to_string()).collect(),
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
    /// Would catch files that are not junk: fewer than two letters besides
    /// `*`, `?` and `.` (`*.*`, `*.d*` — `.docx` too).
    TooBroad,
}

pub fn check_mask(mask: &str) -> Result<(), MaskProblem> {
    let mask = mask.trim();
    if mask.is_empty() {
        return Err(MaskProblem::Empty);
    }
    if mask.contains(['\\', '/', ':']) {
        return Err(MaskProblem::Path);
    }
    let letters = mask
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '.'))
        .count();
    if letters < 2 {
        return Err(MaskProblem::TooBroad);
    }
    Ok(())
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

fn entered(name: &str) -> bool {
    let lower = name.to_lowercase();
    !NEVER_ENTERED.iter().any(|n| n.eq_ignore_ascii_case(name))
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
    };
    for place in places {
        match fs::symlink_metadata(place) {
            Ok(m) if m.is_dir() && !is_link(&m) => search.dir(place),
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
            if is_link(&meta) {
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
                || !self.masks.iter().any(|m| matches_mask(m, &name))
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
        for ok in DEFAULT_MASKS {
            assert_eq!(check_mask(ok), Ok(()), "{ok}");
        }
        assert_eq!(check_mask("*.bak"), Ok(()));
        assert_eq!(check_mask(" "), Err(MaskProblem::Empty));
        for broad in ["*", "*.*", "*.?", "*.d*", "a*", "?.??", "..."] {
            assert_eq!(check_mask(broad), Err(MaskProblem::TooBroad), "{broad}");
        }
        for path in [r"C:\*.tmp", "Temp/*.tmp", r"..\*.tmp"] {
            assert_eq!(check_mask(path), Err(MaskProblem::Path), "{path}");
        }
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
