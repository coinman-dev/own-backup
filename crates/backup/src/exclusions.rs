//! File-exclusion core (plan v2, phase 3) — the cross-platform half.
//!
//! Exclusion patterns come from the job config (`*.tmp`, `pagefile.sys`,
//! `$Recycle.Bin`, `Temp\*.log`, …). The Windows side
//! ([`crate::exclusions_win`]) walks the (snapshotted) volume, matches names
//! against these patterns and resolves each excluded file to its cluster
//! extents via `FSCTL_GET_RETRIEVAL_POINTERS`. This module holds everything
//! that can be tested on mocks:
//!
//! - a small case-insensitive glob matcher (`*`, `?`);
//! - [`ByteRanges`] — a merged set of excluded byte ranges on the volume;
//! - [`ZeroingBlockReader`] — a [`BlockReader`] wrapper that zeroes excluded
//!   ranges inside blocks that are still copied.
//!
//! Correctness note: the used-sector map works in whole backup blocks (1 MiB),
//! so an excluded file usually *shares* blocks with data that must be kept.
//! Dropping such a block would corrupt other files — instead the block is
//! copied with the excluded clusters zeroed. Only blocks whose used clusters
//! are ALL excluded get dropped from the map entirely.

use serde::{Deserialize, Serialize};

use own_backup_common::Result;

use crate::block_reader::BlockReader;

// ---------------------------------------------------------------------------
// File-category sizes (dashboard breakdown)
// ---------------------------------------------------------------------------

/// Byte totals per user-facing file category. Filled during the exclusion
/// walk (which already visits every file on the volume) and shown as the
/// "data breakdown" bar in the dashboards.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategorySizes {
    pub images: u64,
    pub video: u64,
    pub audio: u64,
    pub documents: u64,
    /// Windows OS / binary files (PE: exe/dll/sys/msi/…). `alias = "system"`
    /// loads the old single "system" bucket into this one for pre-split data.
    #[serde(default, alias = "system")]
    pub windows: u64,
    /// Linux OS / binary / package files (ELF/.so/.ko/.deb/.rpm/systemd units…).
    #[serde(default)]
    pub linux: u64,
    pub other: u64,
}

impl CategorySizes {
    /// Пары «категория → байты» для отправки в интерфейс.
    ///
    /// `common::BackupProgress` не знает про эту структуру и не должен: она
    /// живёт в `backup`, а прогресс — общий для всех. Пары развязывают их и
    /// заодно позволяют интерфейсу рисовать список, не зная состава полей.
    pub fn as_pairs(&self) -> Vec<(String, u64)> {
        vec![
            ("images".into(), self.images),
            ("video".into(), self.video),
            ("audio".into(), self.audio),
            ("documents".into(), self.documents),
            ("windows".into(), self.windows),
            ("linux".into(), self.linux),
            ("other".into(), self.other),
        ]
    }

    /// Attribute `bytes` of a kept file to a category. `rel` is the file's path
    /// components from the volume root, lowercased (e.g.
    /// `["windows", "system32", "kernel32.dll"]`).
    ///
    /// Rules (in order):
    /// 1. **Media & documents win by extension**, wherever they live — a photo
    ///    inside `Program Files` is still a photo, not a system file.
    /// 2. Otherwise the file is classified by the **top-level OS system tree**
    ///    it sits in (`Windows`, `Program Files`, … → Windows; `usr`, `etc`, …
    ///    → Linux). Extension is deliberately NOT used to guess the OS here.
    /// 3. Anything else (not media/docs, not inside an OS tree) is **other**.
    pub fn add(&mut self, rel: &[String], bytes: u64) {
        let name_lower = rel.last().map(String::as_str).unwrap_or("");
        let ext = match name_lower.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext,
            _ => "",
        };
        // 1. Photo / video / audio / documents — by extension, any location.
        match ext {
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "tif"
            | "tiff" | "svg" | "ico" | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" => {
                self.images += bytes;
                return;
            }
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
            | "3gp" | "ts" | "m2ts" | "vob" => {
                self.video += bytes;
                return;
            }
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "mid" | "midi"
            | "ape" => {
                self.audio += bytes;
                return;
            }
            "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "pdf" | "txt" | "rtf" | "odt"
            | "ods" | "odp" | "csv" | "md" | "djvu" | "epub" | "fb2" | "xps" => {
                self.documents += bytes;
                return;
            }
            _ => {}
        }
        // 2. By the top-level folder — the OS system trees from the volume root.
        let top = rel.first().map(String::as_str).unwrap_or("");
        if Self::is_windows_dir(top) {
            self.windows += bytes;
        } else if Self::is_linux_dir(top) {
            self.linux += bytes;
        } else {
            // 3. Not media/docs and outside any OS system tree.
            self.other += bytes;
        }
    }

    /// Top-level folders that make up a Windows installation (lowercased).
    fn is_windows_dir(top: &str) -> bool {
        matches!(
            top,
            "windows"
                | "winnt"
                | "program files"
                | "program files (x86)"
                | "programdata"
                | "documents and settings"
                | "windows.old"
                | "$recycle.bin"
                | "system volume information"
                | "recovery"
                | "perflogs"
                | "msocache"
                | "config.msi"
                | "$windows.~bt"
                | "$windows.~ws"
                | "$sysreset"
        )
    }

    /// Top-level folders of a Linux root filesystem (lowercased). `home`, `tmp`,
    /// `mnt`, `media`, `boot` are intentionally excluded: `home` is user data,
    /// the rest are transient/mount points or ambiguous with Windows.
    fn is_linux_dir(top: &str) -> bool {
        matches!(
            top,
            "usr" | "bin"
                | "sbin"
                | "lib"
                | "lib32"
                | "lib64"
                | "libx32"
                | "etc"
                | "opt"
                | "var"
                | "srv"
                | "run"
                | "proc"
                | "sys"
                | "dev"
                | "snap"
        )
    }

    pub fn total(&self) -> u64 {
        self.images
            + self.video
            + self.audio
            + self.documents
            + self.windows
            + self.linux
            + self.other
    }

    /// Add another breakdown into this one (whole-disk sums each partition's).
    pub fn merge(&mut self, o: &CategorySizes) {
        self.images += o.images;
        self.video += o.video;
        self.audio += o.audio;
        self.documents += o.documents;
        self.windows += o.windows;
        self.linux += o.linux;
        self.other += o.other;
    }
}

// ---------------------------------------------------------------------------
// Pattern matching
// ---------------------------------------------------------------------------

/// A parsed exclusion pattern.
///
/// - Patterns without a path separator match the file/directory NAME anywhere
///   on the volume (`pagefile.sys`, `*.tmp`, `$Recycle.Bin`, `Temp`).
/// - Patterns with `\` or `/` match the path RELATIVE to the volume root.
///   A `**` component matches any run of path components, so `**\Temp\**`
///   excludes the contents of every `Temp` directory at any depth, while
///   `Temp\*.log` matches only `.log` files in a root `Temp`.
///
/// ## A trailing `**` is the *contents*, not the directory
///
/// `proc/**` means everything inside `proc`. It does **not** mean `proc`.
///
/// This was not always so, and the difference is not academic. A `**` component
/// matches zero or more path components, so a trailing one used to match the
/// directory itself as well — which meant the default Linux exclusions
/// (`proc/**`, `sys/**`, `dev/**`, `run/**`, `tmp/**`) deleted `/proc`, `/sys`,
/// `/dev`, `/run` and `/tmp` from the image. Those directories are empty; they
/// are also the mount points systemd needs before it can do anything at all.
/// The restored system unlocked its disk, mounted its root, started `init` —
/// and died with `Attempted to kill init`, because there was nowhere to mount
/// `/proc`.
///
/// So a pattern that ends in `**` now requires at least one component after the
/// prefix. A directory is excluded by naming it (`lost+found`, `$Recycle.Bin`),
/// not by asking for its contents.
///
/// Matching is case-insensitive (NTFS semantics); within one component `*`
/// matches any run of characters and `?` matches exactly one character.
#[derive(Debug, Clone)]
pub struct ExclusionPattern {
    components: Vec<String>,
    /// true = match against the full relative path, false = name only.
    anchored: bool,
    /// The pattern ends in `**`: it names what is *inside* a directory.
    contents_only: bool,
}

impl ExclusionPattern {
    pub fn parse(pattern: &str) -> Option<Self> {
        let trimmed = pattern.trim().trim_matches(|c| c == '\\' || c == '/');
        if trimmed.is_empty() {
            return None;
        }
        let components: Vec<String> = trimmed
            .split(['\\', '/'])
            .filter(|c| !c.is_empty())
            .map(|c| c.to_lowercase())
            .collect();
        if components.is_empty() {
            return None;
        }
        // A pattern made only of `**` would match the whole volume — refuse it
        // so a stray entry can never wipe out an entire backup.
        if components.iter().all(|c| c == "**") {
            return None;
        }
        Some(Self {
            anchored: components.len() > 1,
            contents_only: components.len() > 1 && components.last().is_some_and(|c| c == "**"),
            components,
        })
    }

    /// Match a file/dir against this pattern. `name_lower` is the last path
    /// component; `rel_components` is the full path relative to the volume
    /// root, split into components (both already lowercase).
    pub fn matches(&self, name_lower: &str, rel_components: &[String]) -> bool {
        if !self.anchored {
            return glob_match(&self.components[0], name_lower);
        }
        if self.contents_only {
            // Everything inside the directory, and not the directory. Matching
            // the *parent* against the whole pattern says exactly that: the
            // trailing `**` is then left with at least this one component to
            // consume, so a path that IS the directory cannot match.
            let Some((_, parent)) = rel_components.split_last() else { return false };
            return match_path(&self.components, parent);
        }
        match_path(&self.components, rel_components)
    }
}

/// Directories a Linux system cannot start without, at the root of a volume.
///
/// They hold nothing — they are where the kernel and systemd mount `devtmpfs`,
/// `proc`, `sysfs` and the rest. Excluding their contents is right and saves
/// nothing; removing the directories themselves costs the system its boot, and
/// the failure surfaces as a kernel panic with no mention of a missing
/// directory anywhere.
///
/// A last line of defence behind the pattern rules: a hand-written `proc` in
/// the exclusion list still keeps the directory, and only empties it.
pub fn is_essential_linux_dir(rel_components: &[String]) -> bool {
    rel_components.len() == 1
        && matches!(rel_components[0].as_str(), "dev" | "proc" | "sys" | "run" | "tmp")
}

/// Match pattern components against path components. A `**` component matches
/// zero or more whole path components; any other component is a single-component
/// glob (`*` / `?`). Root-anchored, must consume the entire path. Iterative
/// backtracking (same shape as [`glob_match`], one level up).
fn match_path(pat: &[String], path: &[String]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star_pi, mut star_si) = (usize::MAX, 0usize);

    while si < path.len() {
        if pi < pat.len() && pat[pi] != "**" && glob_match(&pat[pi], &path[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == "**" {
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == "**" {
        pi += 1;
    }
    pi == pat.len()
}

/// Parse a raw pattern list, dropping empties.
pub fn parse_patterns(patterns: &[String]) -> Vec<ExclusionPattern> {
    patterns
        .iter()
        .filter_map(|p| ExclusionPattern::parse(p))
        .collect()
}

/// Case-sensitive glob match of `pat` against `text` (callers lowercase both).
/// Supports `*` (any run, including empty) and `?` (exactly one char).
/// Iterative backtracking — no recursion, no pathological blowup.
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// Excluded byte ranges
// ---------------------------------------------------------------------------

/// A set of non-overlapping, sorted byte ranges on the source volume that must
/// not end up in the image (excluded file extents, in volume byte offsets).
#[derive(Debug, Default, Clone)]
pub struct ByteRanges {
    /// Sorted, merged half-open ranges `[start, end)`.
    ranges: Vec<(u64, u64)>,
    /// True until `finalize()` sorts and merges.
    dirty: bool,
}

impl ByteRanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a half-open range `[start, end)`. Ranges may overlap; call
    /// [`Self::finalize`] once after the last `add`.
    pub fn add(&mut self, start: u64, end: u64) {
        if end > start {
            self.ranges.push((start, end));
            self.dirty = true;
        }
    }

    /// Sort and merge all added ranges.
    pub fn finalize(&mut self) {
        if !self.dirty {
            return;
        }
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.ranges.len());
        for &(start, end) in &self.ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        self.ranges = merged;
        self.dirty = false;
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Total number of excluded bytes.
    pub fn total_bytes(&self) -> u64 {
        debug_assert!(!self.dirty, "finalize() before querying");
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }

    /// Iterate the merged ranges.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        debug_assert!(!self.dirty, "finalize() before querying");
        self.ranges.iter().copied()
    }

    /// Zero every part of `data` (which starts at volume offset `data_start`)
    /// that overlaps an excluded range. Returns the number of zeroed bytes.
    pub fn zero_overlaps(&self, data_start: u64, data: &mut [u8]) -> u64 {
        debug_assert!(!self.dirty, "finalize() before querying");
        if data.is_empty() || self.ranges.is_empty() {
            return 0;
        }
        let data_end = data_start + data.len() as u64;
        // First range that could overlap: ranges are sorted by start, find the
        // first with end > data_start.
        let idx = self.ranges.partition_point(|&(_, end)| end <= data_start);
        let mut zeroed = 0u64;
        for &(start, end) in &self.ranges[idx..] {
            if start >= data_end {
                break;
            }
            let from = start.max(data_start) - data_start;
            let to = end.min(data_end) - data_start;
            data[from as usize..to as usize].fill(0);
            zeroed += to - from;
        }
        zeroed
    }

    /// True if ANY part of `[start, end)` overlaps a range (used for btrfs/xfs
    /// reflink-safety: an extent that touches a shared range must be kept).
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        debug_assert!(!self.dirty, "finalize() before querying");
        if end <= start {
            return false;
        }
        let idx = self.ranges.partition_point(|&(_, e)| e <= start);
        matches!(self.ranges.get(idx), Some(&(s, _)) if s < end)
    }

    /// True if the whole `[start, end)` interval is covered by excluded ranges
    /// (used to drop blocks whose used clusters are all excluded).
    pub fn covers(&self, start: u64, end: u64) -> bool {
        debug_assert!(!self.dirty, "finalize() before querying");
        if end <= start {
            return true;
        }
        let idx = self.ranges.partition_point(|&(_, e)| e <= start);
        match self.ranges.get(idx) {
            Some(&(s, e)) => s <= start && e >= end,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Zeroing reader
// ---------------------------------------------------------------------------

/// Wraps a [`BlockReader`] and zeroes excluded byte ranges in every block it
/// returns. Checksums downstream are computed on the zeroed data, so verify
/// and restore stay consistent with what is actually stored.
pub struct ZeroingBlockReader<R: BlockReader> {
    inner: R,
    ranges: ByteRanges,
}

impl<R: BlockReader> ZeroingBlockReader<R> {
    /// `ranges` must be finalized.
    pub fn new(inner: R, ranges: ByteRanges) -> Self {
        Self { inner, ranges }
    }
}

impl<R: BlockReader> BlockReader for ZeroingBlockReader<R> {
    fn read_block(&mut self, block_number: u64) -> Result<Vec<u8>> {
        let mut data = self.inner.read_block(block_number)?;
        let start = block_number * self.inner.block_size() as u64;
        self.ranges.zero_overlaps(start, &mut data);
        Ok(data)
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn total_blocks(&self) -> u64 {
        self.inner.total_blocks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_reader::MockBlockReader;

    fn pat(s: &str) -> ExclusionPattern {
        ExclusionPattern::parse(s).unwrap()
    }

    fn comps(path: &str) -> Vec<String> {
        path.split('\\').map(|c| c.to_lowercase()).collect()
    }

    /// The bug this file exists to never repeat.
    ///
    /// `proc/**` matched `proc` itself, because a `**` component matches zero
    /// or more components. The default Linux exclusions therefore deleted
    /// `/proc`, `/sys`, `/dev`, `/run` and `/tmp` — empty directories, and the
    /// mount points systemd needs before it can start. The restored system
    /// unlocked its disk, mounted its root, and the kernel panicked with
    /// `Attempted to kill init! exitcode=0x00000100`.
    #[test]
    fn a_trailing_globstar_excludes_the_contents_and_keeps_the_directory() {
        for dir in ["proc", "sys", "dev", "run", "tmp"] {
            let p = pat(&format!("{dir}/**"));
            assert!(
                !p.matches(dir, &comps(dir)),
                "{dir}/** must not match {dir} itself — systemd mounts on it"
            );
            assert!(p.matches("1", &comps(&format!("{dir}\\1"))), "…but its contents go");
            assert!(p.matches("x", &comps(&format!("{dir}\\a\\x"))), "…at any depth");
        }
    }

    #[test]
    fn the_same_holds_for_a_globstar_in_the_middle() {
        let p = pat("**\\Temp\\**");
        assert!(!p.matches("temp", &comps("Windows\\Temp")), "the directory survives");
        assert!(!p.matches("temp", &comps("Temp")));
        assert!(p.matches("a.log", &comps("Windows\\Temp\\a.log")), "its contents do not");
        assert!(p.matches("a.log", &comps("Temp\\deep\\a.log")));
    }

    #[test]
    fn a_directory_is_excluded_by_naming_it() {
        // The way to remove a directory is to name it — which is what the
        // default lists do for the ones that really should go.
        let p = pat("lost+found");
        assert!(p.matches("lost+found", &comps("lost+found")));
        let r = pat("$Recycle.Bin");
        assert!(r.matches("$recycle.bin", &comps("$Recycle.Bin")));
    }

    #[test]
    fn essential_mount_points_are_recognised_at_the_root_only() {
        // A guard behind the pattern rules, for a hand-written `proc`.
        for dir in ["dev", "proc", "sys", "run", "tmp"] {
            assert!(is_essential_linux_dir(&comps(dir)));
        }
        assert!(!is_essential_linux_dir(&comps("var\\tmp")), "only at the volume root");
        assert!(!is_essential_linux_dir(&comps("home")));
        assert!(!is_essential_linux_dir(&[]));
    }

    #[test]
    fn every_default_linux_exclusion_leaves_the_root_directories_alone() {
        // The whole default list against the top-level directories of a Linux
        // root: none of them may be removed.
        let patterns: Vec<ExclusionPattern> = [
            "proc/**", "sys/**", "dev/**", "run/**", "tmp/**", "var/tmp/**", "var/cache/**",
            "**/.cache/**", "swapfile", "lost+found",
        ]
        .iter()
        .filter_map(|p| ExclusionPattern::parse(p))
        .collect();
        for dir in [
            "bin", "boot", "dev", "etc", "home", "lib", "lib64", "media", "mnt", "opt", "proc",
            "root", "run", "sbin", "srv", "sys", "tmp", "usr", "var",
        ] {
            let c = comps(dir);
            assert!(
                !patterns.iter().any(|p| p.matches(dir, &c)),
                "/{dir} would be removed from the image"
            );
        }
        // …while what they are for still goes.
        let c = comps("var\\cache\\apt\\archives\\x.deb");
        assert!(patterns.iter().any(|p| p.matches("x.deb", &c)));
    }

    #[test]
    fn name_patterns_match_anywhere() {
        let p = pat("pagefile.sys");
        assert!(p.matches("pagefile.sys", &comps("pagefile.sys")));
        assert!(p.matches("pagefile.sys", &comps("Some\\Deep\\pagefile.sys")));
        assert!(!p.matches("pagefile.sys.bak", &comps("pagefile.sys.bak")));
    }

    #[test]
    fn glob_star_and_question() {
        let p = pat("*.tmp");
        assert!(p.matches("a.tmp", &comps("a.tmp")));
        assert!(p.matches(".tmp", &comps(".tmp")));
        assert!(!p.matches("a.tmpx", &comps("a.tmpx")));

        let q = pat("cache?.dat");
        assert!(q.matches("cache1.dat", &comps("cache1.dat")));
        assert!(!q.matches("cache12.dat", &comps("cache12.dat")));

        let m = pat("*cache*");
        assert!(m.matches("supercache99", &comps("supercache99")));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let p = pat("*.TMP");
        assert!(p.matches("file.tmp", &comps("file.tmp")));
        let d = pat("$Recycle.Bin");
        assert!(d.matches("$recycle.bin", &comps("$recycle.bin")));
    }

    #[test]
    fn anchored_path_patterns() {
        let p = pat(r"Temp\*.log");
        assert!(p.matches("a.log", &comps("Temp\\a.log")));
        assert!(!p.matches("a.log", &comps("Other\\a.log")));
        assert!(!p.matches("a.log", &comps("a.log")));
        // Deeper path with same tail does not match a 2-component pattern.
        assert!(!p.matches("a.log", &comps("X\\Temp\\a.log")));
    }

    #[test]
    fn globstar_matches_any_depth() {
        let p = pat(r"**\Temp\**");
        // The Temp directories themselves survive — a trailing `**` asks for
        // what is inside them, and an emptied directory saves nothing while
        // removing one can cost a system its boot (see the module docs).
        assert!(!p.matches("temp", &comps("Windows\\Temp")));
        assert!(!p.matches("temp", &comps("Users\\Bob\\AppData\\Local\\Temp")));
        // ...and files anywhere under it.
        assert!(p.matches("a.txt", &comps("Windows\\Temp\\a.txt")));
        assert!(p.matches("b.dat", &comps("Users\\Bob\\AppData\\Local\\Temp\\sub\\b.dat")));
        // Unrelated paths do not match.
        assert!(!p.matches("temper", &comps("Windows\\Temper")));
        assert!(!p.matches("x", &comps("Windows\\System32\\x")));

        // Leading anchor: a Temp at root only — again, its contents.
        let r = pat(r"Temp\**");
        assert!(!r.matches("temp", &comps("Temp")));
        assert!(r.matches("a", &comps("Temp\\a")));
        assert!(!r.matches("a", &comps("Windows\\Temp\\a")));

        // Precise middle globs still work.
        let u = pat(r"Users\*\AppData\Local\Temp\**");
        assert!(u.matches("c.tmp", &comps("Users\\Ann\\AppData\\Local\\Temp\\c.tmp")));
        assert!(!u.matches("c.tmp", &comps("Users\\Ann\\Documents\\c.tmp")));
    }

    #[test]
    fn parse_rejects_bare_globstar() {
        assert!(ExclusionPattern::parse("**").is_none());
        assert!(ExclusionPattern::parse(r"**\**").is_none());
        assert!(ExclusionPattern::parse(r"**\Temp\**").is_some());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(ExclusionPattern::parse("").is_none());
        assert!(ExclusionPattern::parse("  ").is_none());
        assert!(ExclusionPattern::parse("\\").is_none());
        assert_eq!(parse_patterns(&["".into(), "*.tmp".into()]).len(), 1);
    }

    #[test]
    fn glob_backtracking() {
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("*", ""));
        assert!(glob_match("**", "x"));
        assert!(!glob_match("a*b", "a"));
        assert!(glob_match("a*a*a", "aaaa"));
        assert!(!glob_match("a*a*ab", "aaaa"));
    }

    #[test]
    fn categories_by_type_and_folder() {
        fn p(path: &str) -> Vec<String> {
            path.split('/').map(|s| s.to_string()).collect()
        }
        let mut c = CategorySizes::default();
        // Media/documents win by extension — even inside a system folder.
        c.add(&p("users/me/photo.jpg"), 100); // images
        c.add(&p("clip.mkv"), 200); // video
        c.add(&p("music/song.flac"), 30); // audio
        c.add(&p("windows/media/report.docx"), 40); // documents (media beats folder)
        // System files by top-level folder (extension is not consulted here).
        c.add(&p("windows/system32/kernel32.dll"), 50); // windows
        c.add(&p("program files/app/data.pak"), 15); // windows (non-media in tree)
        c.add(&p("usr/lib/libc.so.6"), 12); // linux
        c.add(&p("etc/fstab"), 8); // linux (no extension, in /etc)
        // Not media/docs and outside any OS tree → other (even a stray .exe).
        c.add(&p("downloads/setup.exe"), 60); // other
        c.add(&p("readme"), 7); // other
        assert_eq!(c.images, 100);
        assert_eq!(c.video, 200);
        assert_eq!(c.audio, 30);
        assert_eq!(c.documents, 40);
        assert_eq!(c.windows, 65); // 50 + 15
        assert_eq!(c.linux, 20); // 12 + 8
        assert_eq!(c.other, 67); // 60 + 7
        assert_eq!(c.total(), 522);
    }

    #[test]
    fn ranges_merge_and_total() {
        let mut r = ByteRanges::new();
        r.add(100, 200);
        r.add(150, 250);
        r.add(300, 400);
        r.add(400, 500); // touching → merged
        r.add(10, 10); // empty → dropped
        r.finalize();
        let all: Vec<_> = r.iter().collect();
        assert_eq!(all, vec![(100, 250), (300, 500)]);
        assert_eq!(r.total_bytes(), 350);
    }

    #[test]
    fn zero_overlaps_zeroes_only_excluded() {
        let mut r = ByteRanges::new();
        r.add(10, 20);
        r.add(30, 35);
        r.finalize();

        let mut data = vec![0xFFu8; 40];
        let zeroed = r.zero_overlaps(0, &mut data);
        assert_eq!(zeroed, 15);
        assert!(data[0..10].iter().all(|&b| b == 0xFF));
        assert!(data[10..20].iter().all(|&b| b == 0));
        assert!(data[20..30].iter().all(|&b| b == 0xFF));
        assert!(data[30..35].iter().all(|&b| b == 0));
        assert!(data[35..40].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn zero_overlaps_partial_window() {
        let mut r = ByteRanges::new();
        r.add(0, 100);
        r.finalize();
        // Window [90, 110): only the first 10 bytes overlap.
        let mut data = vec![0xAAu8; 20];
        let zeroed = r.zero_overlaps(90, &mut data);
        assert_eq!(zeroed, 10);
        assert!(data[..10].iter().all(|&b| b == 0));
        assert!(data[10..].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn covers_checks_full_containment() {
        let mut r = ByteRanges::new();
        r.add(100, 200);
        r.finalize();
        assert!(r.covers(100, 200));
        assert!(r.covers(150, 160));
        assert!(!r.covers(50, 150));
        assert!(!r.covers(150, 250));
        assert!(!r.covers(300, 400));
    }

    #[test]
    fn zeroing_reader_zeroes_inside_blocks() {
        // 4 blocks of 256 bytes, all 0x77.
        let data = vec![0x77u8; 1024];
        let mut ranges = ByteRanges::new();
        ranges.add(256, 512); // exactly block 1
        ranges.add(600, 700); // middle of block 2
        ranges.finalize();

        let mut reader = ZeroingBlockReader::new(MockBlockReader::new(data, 256), ranges);
        assert!(reader.read_block(0).unwrap().iter().all(|&b| b == 0x77));
        assert!(reader.read_block(1).unwrap().iter().all(|&b| b == 0));
        let b2 = reader.read_block(2).unwrap();
        assert!(b2[..88].iter().all(|&b| b == 0x77)); // 512..600
        assert!(b2[88..188].iter().all(|&b| b == 0)); // 600..700
        assert!(b2[188..].iter().all(|&b| b == 0x77)); // 700..768
        assert!(reader.read_block(3).unwrap().iter().all(|&b| b == 0x77));
    }
}
