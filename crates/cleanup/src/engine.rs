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

use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::catalog::{Base, Item, RegTarget, Target, Trim};
use crate::registry::{Hive, OpenHive};
use crate::users::User;

/// Count, or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Analyze,
    Remove,
}

/// What a run is asked besides the items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct Options {
    /// Keep this many last days of browser history (0: remove it all).
    pub history_keep_days: u32,
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
    /// History entries removed (or, in an analysis, to be removed) from a
    /// database cut down to its last days.
    pub records: u64,
    /// Registry values and keys removed (or to be removed).
    pub entries: u64,
}

impl Tally {
    pub fn add(&mut self, other: &Tally) {
        self.records += other.records;
        self.entries += other.entries;
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
    /// Programs that were open, whose folders were left (`Google Chrome`).
    pub running: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub items: Vec<ItemReport>,
    pub total: Tally,
    pub cancelled: bool,
    /// An analysis' files, item by item — what the window lists before the
    /// cleanup, and what the cleanup is then held to. Kept by the service,
    /// never sent whole.
    #[serde(skip)]
    pub listing: Listing,
}

/// A file an analysis found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// An analysis' files by item id.
pub type Listing = HashMap<String, Vec<Found>>;

/// More files than this in one item are counted, not listed: a cleanup held
/// to the list leaves the rest (the safe way round).
pub const LIST_CAP: usize = 1_000_000;

/// The places of this system the bases stand for.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `C:\Windows`.
    pub windows: PathBuf,
    /// `C:\ProgramData`.
    pub program_data: PathBuf,
    /// The roots of the volumes whose recycle bins are emptied (`C:\`, `D:\`).
    pub volumes: Vec<PathBuf>,
    /// Programs running now, lower-case (`chrome.exe`): their folders are
    /// left.
    pub running: HashSet<String>,
    /// How a user's registry is reached ([`crate::registry::open_user`]).
    pub hives: OpenHive,
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
            running: crate::users::running_programs(),
            hives: crate::registry::open_user,
        }
    }
}

/// `a\b` below `base`, part by part (the separator is Windows'; the tests run
/// anywhere). A part with `*` or `?` is every folder there whose name it
/// matches — folders only, links and junctions never.
fn below(base: &Path, path: &str) -> Vec<PathBuf> {
    let mut found = vec![base.to_path_buf()];
    for part in path.split('\\').filter(|s| !s.is_empty()) {
        if !part.contains(['*', '?']) {
            found.iter_mut().for_each(|p| p.push(part));
            continue;
        }
        found = found
            .iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .filter(|e| matches_mask(part, &e.file_name().to_string_lossy()))
            .filter(|e| fs::symlink_metadata(e.path()).is_ok_and(|m| m.is_dir() && !is_link(&m)))
            .map(|e| e.path())
            .collect();
        found.sort();
    }
    found
}

/// The folders a target stands for — one for a system target, one per user,
/// or one per user and volume for the recycle bin; more where the path has
/// a mask (one per browser profile).
pub fn folders(target: &Target, roots: &Roots, users: &[User]) -> Vec<PathBuf> {
    let per_user = |sub: &str| -> Vec<PathBuf> {
        users
            .iter()
            .flat_map(|u| below(&u.profile.join("AppData").join(sub), target.path))
            .collect()
    };
    match target.base {
        Base::Windows => below(&roots.windows, target.path),
        Base::ProgramData => below(&roots.program_data, target.path),
        Base::LocalAppData => per_user("Local"),
        Base::RoamingAppData => per_user("Roaming"),
        Base::SystemDrive => below(
            roots.windows.parent().unwrap_or(&roots.windows),
            target.path,
        ),
        Base::Profile => users
            .iter()
            .flat_map(|u| below(&u.profile, target.path))
            .collect(),
        Base::RecycleBin => users
            .iter()
            .flat_map(|u| {
                roots
                    .volumes
                    .iter()
                    .flat_map(move |v| below(&v.join("$Recycle.Bin").join(&u.sid), target.path))
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
    options: Options,
    cancel: &AtomicBool,
    progress: impl FnMut(&str, &Tally),
) -> Report {
    run_at_with(
        items,
        roots,
        users,
        mode,
        options,
        cancel,
        progress,
        SystemTime::now(),
        None,
    )
}

/// Clean, removing only files of `only` — what an analysis listed and the
/// window showed. A file that turned up since stays; one that no longer
/// matches (changed, now too young, gone) stays as well, every rule still
/// applies.
pub fn run_only(
    items: &[&Item],
    roots: &Roots,
    users: &[User],
    options: Options,
    cancel: &AtomicBool,
    progress: impl FnMut(&str, &Tally),
    only: &HashSet<PathBuf>,
) -> Report {
    run_at_with(
        items,
        roots,
        users,
        Mode::Remove,
        options,
        cancel,
        progress,
        SystemTime::now(),
        Some(only),
    )
}

/// [`run`] with the clock given: ages are measured from `now`.
#[cfg(test)]
pub(crate) fn run_at(
    items: &[&Item],
    roots: &Roots,
    users: &[User],
    mode: Mode,
    cancel: &AtomicBool,
    progress: impl FnMut(&str, &Tally),
    now: SystemTime,
) -> Report {
    run_at_with(
        items,
        roots,
        users,
        mode,
        Options::default(),
        cancel,
        progress,
        now,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_at_with(
    items: &[&Item],
    roots: &Roots,
    users: &[User],
    mode: Mode,
    options: Options,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&str, &Tally),
    now: SystemTime,
    only: Option<&HashSet<PathBuf>>,
) -> Report {
    let mut report = Report {
        items: Vec::new(),
        total: Tally::default(),
        cancelled: false,
        listing: Listing::new(),
    };
    for item in items {
        let mut tally = Tally::default();
        let mut found = Vec::new();
        let mut running: Vec<String> = Vec::new();
        // Whether one of `programs` runs; if so, it is named for the window.
        let mut open = |programs: &[&str]| {
            let Some(open) = programs
                .iter()
                .find(|p| roots.running.contains(&p.to_ascii_lowercase()))
            else {
                return false;
            };
            let name = crate::catalog::program_name(open).to_string();
            if !running.contains(&name) {
                running.push(name);
            }
            true
        };
        let keys: Vec<&RegTarget> = item.keys.iter().filter(|k| !open(k.programs)).collect();
        for target in item.targets {
            if open(target.programs) {
                continue;
            }
            for folder in folders(target, roots, users) {
                let mut walk = Walk {
                    target,
                    mode,
                    now,
                    cancel,
                    tally: &mut tally,
                    seen: 0,
                    progress: &mut |t| progress(item.id, t),
                    only,
                    found: (mode == Mode::Analyze).then_some(&mut found),
                    options,
                };
                walk.root(&folder);
            }
        }
        if !keys.is_empty() {
            let mut reg = Registry {
                mode,
                cancel,
                tally: &mut tally,
                only,
                found: (mode == Mode::Analyze).then_some(&mut found),
            };
            for user in users {
                reg.user(&keys, user, roots.hives);
            }
        }
        if mode == Mode::Analyze {
            report.listing.insert(item.id.to_string(), found);
        }
        progress(item.id, &tally);
        tracing::info!(
            item = item.id,
            ?mode,
            files = tally.files,
            bytes = tally.bytes,
            left = tally.left_files,
            entries = tally.entries,
            "cleanup item done"
        );
        report.total.add(&tally);
        report.items.push(ItemReport {
            id: item.id.to_string(),
            tally,
            running,
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
    /// Removal held to these files (an analysis' list).
    only: Option<&'a HashSet<PathBuf>>,
    /// Analysis: where the files found go.
    found: Option<&'a mut Vec<Found>>,
    options: Options,
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
            if self.wanted(&name) && self.trims() {
                // The database stays, cut down; its journal is not touched.
                if name.eq_ignore_ascii_case("History") {
                    self.trim(&path);
                }
                left = true;
                continue;
            }
            if !self.wanted(&name)
                || !self.old_enough(&meta)
                || self.only.is_some_and(|only| !only.contains(&path))
            {
                left = true;
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
                                path: path.clone(),
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

    /// This target's database is cut down to its last days, not removed.
    fn trims(&self) -> bool {
        self.target.trim == Some(Trim::ChromiumHistory) && self.options.history_keep_days > 0
    }

    /// Count (analysis) or remove (cleanup) the history older than the days
    /// kept; a database that cannot be edited safely is left and counted so.
    fn trim(&mut self, db: &Path) {
        let days = u64::from(self.options.history_keep_days);
        let before = self.now - Duration::from_secs(days * 24 * 3600);
        let done = match self.mode {
            Mode::Analyze => crate::history::count_old(db, before),
            Mode::Remove => crate::history::trim(db, before),
        };
        match done {
            Ok(n) => self.tally.records += n,
            Err(e) => {
                tracing::info!(db = %db.display(), error = %e, "cleanup: history left as it is");
                self.tally.left_files += 1;
            }
        }
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

/// An item's registry keys of one user after another: counted and listed, or
/// emptied. An entry is listed as `HKEY_USERS\<SID>\<key>\<name>` (a subkey
/// with a `\` after it), followed by ` = <text>` where its data reads as
/// text — the same line a cleanup held to the list looks for, so an entry
/// whose text changed since the analysis stays.
struct Registry<'a> {
    mode: Mode,
    cancel: &'a AtomicBool,
    tally: &'a mut Tally,
    only: Option<&'a HashSet<PathBuf>>,
    found: Option<&'a mut Vec<Found>>,
}

impl Registry<'_> {
    fn user(&mut self, keys: &[&RegTarget], user: &User, open: OpenHive) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let hive = match open(user, self.mode == Mode::Remove) {
            Ok(hive) => hive,
            // No hive: nothing of this user's to clean.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::info!(sid = %user.sid, error = %e, "cleanup: registry not reached, left");
                self.tally.left_files += 1;
                return;
            }
        };
        let root = format!(r"HKEY_USERS\{}", user.sid);
        for key in keys {
            self.key(hive.as_ref(), &root, key);
        }
    }

    fn key(&mut self, hive: &dyn Hive, root: &str, key: &RegTarget) {
        for path in expand(hive, key.path) {
            self.one_key(hive, root, key, &path);
        }
    }

    /// One key `path` stands for (its masks resolved).
    fn one_key(&mut self, hive: &dyn Hive, root: &str, key: &RegTarget, path: &str) {
        let at = format!(r"{root}\{path}");
        for v in hive.values(path) {
            // The default value is the key's own.
            let wanted = !v.name.is_empty()
                && (key.values.is_empty() || key.values.iter().any(|m| matches_mask(m, &v.name)));
            if !wanted {
                continue;
            }
            let shown = match &v.text {
                Some(text) => format!(r"{at}\{} = {text}", v.name),
                None => format!(r"{at}\{}", v.name),
            };
            self.one(PathBuf::from(shown), v.size, || {
                hive.delete_value(path, &v.name)
            });
        }
        if key.subkeys {
            for s in hive.subkeys(path) {
                let shown = format!(r"{at}\{}\", s.name);
                self.one(PathBuf::from(shown), s.size, || {
                    hive.delete_subkey(path, &s.name)
                });
            }
        }
    }

    /// One value or subkey: counted and listed, or removed.
    fn one(&mut self, shown: PathBuf, size: u64, remove: impl FnOnce() -> std::io::Result<()>) {
        if self.only.is_some_and(|only| !only.contains(&shown)) {
            return;
        }
        match self.mode {
            Mode::Analyze => {
                self.tally.entries += 1;
                if let Some(found) = self.found.as_deref_mut() {
                    if found.len() < LIST_CAP {
                        found.push(Found {
                            path: shown,
                            size,
                            modified: None,
                        });
                    }
                }
            }
            Mode::Remove => match remove() {
                Ok(()) => self.tally.entries += 1,
                Err(e) => {
                    tracing::debug!(entry = %shown.display(), error = %e, "cleanup: left");
                    self.tally.left_files += 1;
                }
            },
        }
    }
}

/// The keys `path` stands for: a part with `*` or `?` is every subkey there
/// whose name it matches.
fn expand(hive: &dyn Hive, path: &str) -> Vec<String> {
    let mut found = vec![String::new()];
    for part in path.split('\\').filter(|s| !s.is_empty()) {
        let join = |at: &str, name: &str| {
            if at.is_empty() {
                name.to_string()
            } else {
                format!(r"{at}\{name}")
            }
        };
        if !part.contains(['*', '?']) {
            found = found.iter().map(|at| join(at, part)).collect();
            continue;
        }
        found = found
            .iter()
            .flat_map(|at| {
                hive.subkey_names(at)
                    .into_iter()
                    .filter(|name| matches_mask(part, name))
                    .map(|name| join(at, &name))
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    found
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
