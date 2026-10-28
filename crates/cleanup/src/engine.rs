//! Going through the targets: counting what would go, or removing it.
//!
//! Analysis and removal walk the same way, so the window's numbers are what
//! the removal meets — minus what turns out to be in use. The walk:
//!
//! - starts at the target's folder and goes down through `read_dir` only, so
//!   it cannot leave it; a link or a junction — the folder itself or anything
//!   in it — is neither entered nor removed (Windows: any reparse point,
//!   which also keeps cloud placeholders, whose removal would reach the
//!   cloud);
//! - takes a file's age from the later of its write and creation times: an
//!   unpacker gives a file the date it had in the archive, and a setup
//!   unpacked a minute ago is not a day old;
//! - removes a file plainly (a read-only one too); one it cannot remove — in
//!   use, refused — is counted as left, and the walk goes on;
//! - removes a folder below the target once it is empty and old enough, never
//!   the target folder itself.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::catalog::{Base, Item, Target};
use crate::users::User;

/// Count, or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Analyze,
    Remove,
}

/// What one item came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// Files found (analysis) or removed (removal), and their size.
    pub files: u64,
    pub bytes: u64,
    /// Files that matched but could not be removed — in use, refused.
    pub left_files: u64,
    pub left_bytes: u64,
}

impl Tally {
    pub fn add(&mut self, other: &Tally) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.left_files += other.left_files;
        self.left_bytes += other.left_bytes;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemReport {
    pub id: String,
    #[serde(flatten)]
    pub tally: Tally,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub items: Vec<ItemReport>,
    pub total: Tally,
    pub cancelled: bool,
}

/// The places of this system the bases stand for.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `C:\Windows`.
    pub windows: PathBuf,
    /// `C:\ProgramData`.
    pub program_data: PathBuf,
    /// The roots of the volumes whose recycle bins are emptied (`C:\`, `D:\`).
    pub volumes: Vec<PathBuf>,
}

impl Roots {
    /// This system's, as the service sees them.
    pub fn of_this_system() -> Self {
        let env = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(fallback))
        };
        Self {
            windows: env("SystemRoot", r"C:\Windows"),
            program_data: env("ProgramData", r"C:\ProgramData"),
            volumes: crate::users::fixed_volumes(),
        }
    }
}

/// `a\b` below `base`, part by part (the separator is Windows'; the tests run
/// anywhere).
fn below(base: &Path, path: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for part in path.split('\\').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// The folders a target stands for — one for a system target, one per user,
/// or one per user and volume for the recycle bin.
pub fn folders(target: &Target, roots: &Roots, users: &[User]) -> Vec<PathBuf> {
    match target.base {
        Base::Windows => vec![below(&roots.windows, target.path)],
        Base::ProgramData => vec![below(&roots.program_data, target.path)],
        Base::LocalAppData => users
            .iter()
            .map(|u| below(&below(&u.profile, r"AppData\Local"), target.path))
            .collect(),
        Base::RecycleBin => users
            .iter()
            .flat_map(|u| {
                roots
                    .volumes
                    .iter()
                    .map(move |v| below(&v.join("$Recycle.Bin").join(&u.sid), target.path))
            })
            .collect(),
    }
}

/// Analyse or clean `items` for `users`. `progress` hears of each item as it
/// goes (every few hundred files) and when it is done.
pub fn run(
    items: &[&Item],
    roots: &Roots,
    users: &[User],
    mode: Mode,
    cancel: &AtomicBool,
    progress: impl FnMut(&str, &Tally),
) -> Report {
    run_at(
        items,
        roots,
        users,
        mode,
        cancel,
        progress,
        SystemTime::now(),
    )
}

/// [`run`] with the clock given: ages are measured from `now`.
pub(crate) fn run_at(
    items: &[&Item],
    roots: &Roots,
    users: &[User],
    mode: Mode,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&str, &Tally),
    now: SystemTime,
) -> Report {
    let mut report = Report {
        items: Vec::new(),
        total: Tally::default(),
        cancelled: false,
    };
    for item in items {
        let mut tally = Tally::default();
        for target in item.targets {
            for folder in folders(target, roots, users) {
                let mut walk = Walk {
                    target,
                    mode,
                    now,
                    cancel,
                    tally: &mut tally,
                    seen: 0,
                    progress: &mut |t| progress(item.id, t),
                };
                walk.root(&folder);
            }
        }
        progress(item.id, &tally);
        tracing::info!(
            item = item.id,
            ?mode,
            files = tally.files,
            bytes = tally.bytes,
            left = tally.left_files,
            "cleanup item done"
        );
        report.total.add(&tally);
        report.items.push(ItemReport {
            id: item.id.to_string(),
            tally,
        });
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
    }
    report
}

struct Walk<'a> {
    target: &'a Target,
    mode: Mode,
    now: SystemTime,
    cancel: &'a AtomicBool,
    tally: &'a mut Tally,
    seen: u64,
    progress: &'a mut dyn FnMut(&Tally),
}

impl Walk<'_> {
    fn root(&mut self, folder: &Path) {
        // A target that is not there is nothing to clean; one that is a link
        // or a junction is someone else's folder.
        match fs::symlink_metadata(folder) {
            Ok(meta) if meta.is_dir() && !is_link(&meta) => {
                self.dir(folder);
            }
            Ok(_) => {
                tracing::info!(folder = %folder.display(), "cleanup: not a plain folder, left")
            }
            Err(_) => {}
        }
    }

    /// Clean one folder; whether it is empty now (in analysis: would be).
    fn dir(&mut self, dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut left = false;
        for entry in entries {
            if self.cancel.load(Ordering::Relaxed) {
                return false;
            }
            let Ok(entry) = entry else {
                left = true;
                continue;
            };
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                left = true;
                continue;
            };
            if is_link(&meta) {
                left = true;
                continue;
            }
            if meta.is_dir() {
                let emptied = self.target.recursive && self.dir(&path) && self.old_enough(&meta);
                if !emptied || (self.mode == Mode::Remove && fs::remove_dir(&path).is_err()) {
                    left = true;
                }
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !self.wanted(&name) || !self.old_enough(&meta) {
                left = true;
                continue;
            }
            let size = meta.len();
            match self.mode {
                Mode::Analyze => {
                    self.tally.files += 1;
                    self.tally.bytes += size;
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
                        left = true;
                    }
                },
            }
            self.seen += 1;
            if self.seen.is_multiple_of(500) {
                (self.progress)(self.tally);
            }
        }
        !left
    }

    fn wanted(&self, name: &str) -> bool {
        let t = self.target;
        if t.keep.iter().any(|k| k.eq_ignore_ascii_case(name)) {
            return false;
        }
        t.masks.is_empty() || t.masks.iter().any(|m| matches_mask(m, name))
    }

    fn old_enough(&self, meta: &Metadata) -> bool {
        let hours = self.target.min_age_hours;
        if hours == 0 {
            return true;
        }
        let newest = [meta.modified().ok(), meta.created().ok()]
            .into_iter()
            .flatten()
            .max();
        match newest {
            Some(t) => self
                .now
                .duration_since(t)
                .is_ok_and(|age| age >= Duration::from_secs(u64::from(hours) * 3600)),
            // Nothing says how old it is: it stays.
            None => false,
        }
    }
}

/// A link, a junction, or (Windows) any other reparse point.
fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || meta.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn remove_file(path: &Path, meta: &Metadata) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if meta.permissions().readonly() => {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(path, perms).map_err(|_| e)?;
            fs::remove_file(path)
        }
        Err(e) => Err(e),
    }
}

/// `*` (any run) and `?` (one character), case-insensitive.
pub fn matches_mask(mask: &str, name: &str) -> bool {
    let m: Vec<char> = mask.chars().flat_map(char::to_lowercase).collect();
    let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    let (mut mi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if mi < m.len() && (m[mi] == '?' || m[mi] == n[ni]) {
            mi += 1;
            ni += 1;
        } else if mi < m.len() && m[mi] == '*' {
            star = Some(mi);
            mark = ni;
            mi += 1;
        } else if let Some(s) = star {
            mi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while mi < m.len() && m[mi] == '*' {
        mi += 1;
    }
    mi == m.len()
}

#[cfg(test)]
mod tests;
