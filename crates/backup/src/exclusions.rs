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
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "tif" | "tiff"
            | "svg" | "ico" | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" => {
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
            "usr"
                | "bin"
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
            let Some((_, parent)) = rel_components.split_last() else {
                return false;
            };
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
        && matches!(
            rel_components[0].as_str(),
            "dev" | "proc" | "sys" | "run" | "tmp"
        )
}

/// The one excluded path a delete pass inside the image cannot win:
/// `System Volume Information` at the volume root.
///
/// Windows recreates that folder the moment the image's volume mounts and
/// holds it open — storsvc writes there, VSS keeps its state there — so the
/// DIRECTORY is not going anywhere while the volume is mounted, and it is the
/// mounted volume that does the deleting.
///
/// What is INSIDE it is ordinary work: those entries came from the source
/// volume, and nothing on this machine has them open. Until 2026-09-18 both
/// were skipped, and only at `debug` level — a run that matched two files
/// inside the folder reported `deleted=0 failed=0` and left both entries in
/// the image's catalog, with nothing in the log to say why. The data was never
/// at risk (its clusters are subtracted before the copy), but "настоящее
/// удаление" is about the entries too.
///
/// Only the root one: a folder of the same name deeper in the tree is a folder
/// like any other.
pub fn recreated_by_windows(rel: &str) -> bool {
    rel.trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case("System Volume Information")
}

/// The longest value that may be resident — kept inside an NTFS file record
/// rather than in clusters: no longer than the record itself (1 KiB, or 4 KiB
/// on a volume of 4K-sector disks). The deletion passes overwrite a wholly
/// excluded file's content and named streams up to this length before the
/// file goes: deleting frees the record but leaves its bytes, here and on
/// Windows. Anything longer is in clusters, which the `$MFT` scan already
/// left out of the copy.
pub const NTFS_RESIDENT_MAX: u64 = 4096;

/// One directory entry, as a Windows directory listing gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub size: u64,
    pub attributes: u32,
    /// The file reference (on NTFS: MFT record number in the low 48 bits,
    /// sequence number above) — only from a listing that asks for it.
    pub file_id: Option<u64>,
}

/// The entries of a `FILE_FULL_DIR_INFO` chain (`with_ids == false`) or a
/// `FILE_ID_BOTH_DIR_INFO` chain (`with_ids`) — what
/// `GetFileInformationByHandleEx` fills in for `FileFullDirectoryInfo` and
/// `FileIdBothDirectoryInfo`, without `.` and `..`. A truncated entry ends
/// the list.
///
/// Layouts (x64): `NextEntryOffset`@0, `EndOfFile`@40, `FileAttributes`@56,
/// `FileNameLength`@60 (bytes); the name at 68 — or, with ids, the short name
/// in between, `FileId`@96 and the name at 104.
pub fn parse_dir_info(buf: &[u8], with_ids: bool) -> Vec<DirEntryInfo> {
    let name_at = if with_ids { 104 } else { 68 };
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(rec) = buf.get(off..).filter(|r| r.len() >= name_at) {
        let next = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
        let size = i64::from_le_bytes(rec[40..48].try_into().unwrap()).max(0) as u64;
        let attributes = u32::from_le_bytes(rec[56..60].try_into().unwrap());
        let name_len = u32::from_le_bytes(rec[60..64].try_into().unwrap()) as usize;
        let file_id = with_ids.then(|| u64::from_le_bytes(rec[96..104].try_into().unwrap()));
        let Some(raw) = rec.get(name_at..name_at + name_len) else {
            break;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if name != "." && name != ".." {
            out.push(DirEntryInfo {
                name,
                size,
                attributes,
                file_id,
            });
        }
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

/// Whether every one of a file's `names` — volume-relative, `\`-separated,
/// as `FindFirstFileNameW` lists the hard links of a file — falls under the
/// rules: by its own path, or by a directory above it. A file with a name
/// outside them is data a kept name still needs.
pub fn every_name_excluded(names: &[String], parsed: &[ExclusionPattern]) -> bool {
    !names.is_empty()
        && names.iter().all(|name| {
            let comps: Vec<String> = name
                .split(['\\', '/'])
                .filter(|c| !c.is_empty())
                .map(str::to_lowercase)
                .collect();
            (1..=comps.len()).any(|k| {
                let prefix = &comps[..k];
                parsed.iter().any(|p| p.matches(&prefix[k - 1], prefix))
            })
        })
}

/// The named data streams in a `FILE_STREAM_INFO` chain (what
/// `GetFileInformationByHandleEx(FileStreamInfo)` fills in): their full names
/// as a path suffix (`:Zone.Identifier:$DATA`) and sizes. The unnamed stream
/// (`::$DATA`) and anything that is not a data stream are left out; a
/// truncated or malformed entry ends the list.
///
/// Layout: `NextEntryOffset` u32, `StreamNameLength` u32 (bytes),
/// `StreamSize` i64, `StreamAllocationSize` i64, then the UTF-16 name.
pub fn named_streams_from_info(buf: &[u8]) -> Vec<(String, u64)> {
    const NAME_AT: usize = 24;
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(rec) = buf.get(off..).filter(|r| r.len() >= NAME_AT) {
        let next = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
        let name_len = u32::from_le_bytes(rec[4..8].try_into().unwrap()) as usize;
        let size = i64::from_le_bytes(rec[8..16].try_into().unwrap()).max(0) as u64;
        let Some(raw) = rec.get(NAME_AT..NAME_AT + name_len) else {
            break;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if name != "::$DATA" && name.starts_with(':') && name.ends_with(":$DATA") {
            out.push((name, size));
        }
        if next == 0 {
            break;
        }
        off += next;
    }
    out
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
/// Patterns every backup applies, whatever the user's list says.
///
/// The service's local key store is the one directory that must never be in a
/// copy. It holds the DPAPI-protected private job keys that let the service
/// open archives without a password, and restoring Windows from a backup must
/// not quietly restore that permission along with it — on a new installation it
/// is arranged again, deliberately (`seal/LOCAL.md`). The name is ours and
/// unique, so excluding it wherever it appears costs nothing else.
///
/// This is not a default the user can remove: it is added here, in the one
/// place every scanner (NTFS `$MFT`, the Windows tree walk, ext/xfs/btrfs/FAT)
/// turns patterns into rules. A raw sector copy runs no scanner at all and must
/// be refused separately by the engine.
pub const MANDATORY_EXCLUSIONS: &[&str] = &[crate::seal::local::KEY_DIRECTORY_NAME];

pub fn parse_patterns(patterns: &[String]) -> Vec<ExclusionPattern> {
    MANDATORY_EXCLUSIONS
        .iter()
        .filter_map(|p| ExclusionPattern::parse(p))
        .chain(patterns.iter().filter_map(|p| ExclusionPattern::parse(p)))
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

// ---------------------------------------------------------------------------
// What a scan found
// ---------------------------------------------------------------------------

/// A matched entry, recorded at its highest match point (a matched directory
/// is one record — its contents are implied). Fed to the mount-&-delete pass
/// ([`crate::true_removal_win`]) that truly removes NTFS/ReFS exclusions from
/// the finished image.
#[derive(Debug, Clone, Default)]
pub struct ExcludedPath {
    /// Path relative to the volume root, original case, `\`-separated.
    pub rel: String,
    pub is_dir: bool,
    /// Junction/symlink: delete the reparse entry itself, never its target.
    pub is_reparse: bool,
    /// From the `$MFT` scan, sorted: the MFT records at or under this path
    /// every name of which is excluded, so the deletion takes the whole file,
    /// and which hold content inside the record
    /// (`ntfs_scan::MatchedPath::whole_records`). The deletion passes — ntfs-3g
    /// on Linux, the Windows driver in `true_removal_win` — clear the resident
    /// content and named streams of these only; empty where no scan said so.
    pub whole_records: Vec<u64>,
}

/// Why something a scan matched (or could not look at) is still in the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftReason {
    /// A hard link whose other name is not excluded: emptying it would empty
    /// that file too.
    HardLink,
    /// Its data is shared with a file that stays (a reflink, a snapshot).
    Shared,
    /// Its data lives inside metadata the scan does not rewrite (an inline or
    /// compressed inline extent, resident data).
    Inline,
    /// Its inode or extent map could not be read.
    Unreadable,
    /// A directory whose entries could not be read: nothing under it was
    /// matched at all, so it may hold anything.
    SubtreeNotWalked,
    /// A btrfs subvolume: walked as its own tree, never removed by name.
    Subvolume,
}

impl LeftReason {
    pub fn as_str(self) -> &'static str {
        match self {
            LeftReason::HardLink => "hard link",
            LeftReason::Shared => "shared data",
            LeftReason::Inline => "inline data",
            LeftReason::Unreadable => "unreadable",
            LeftReason::SubtreeNotWalked => "directory not walked",
            LeftReason::Subvolume => "subvolume",
        }
    }
}

/// A matched file whose content is still in the copy, or a directory the
/// scan could not walk. `path` is relative to the root the scan walked from,
/// lowercased components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftInCopy {
    pub path: Vec<String>,
    pub reason: LeftReason,
}

impl LeftInCopy {
    pub fn new(path: &[String], reason: LeftReason) -> Self {
        Self {
            path: path.to_vec(),
            reason,
        }
    }

    /// `path (reason)`, for the log and the refusal.
    pub fn shown(&self) -> String {
        format!("/{} ({})", self.path.join("/"), self.reason.as_str())
    }
}

/// The proof decision 6/E4 asks of a scan: did every match of the service's
/// key directory leave the copy? The first entry of `left` that says it may
/// not have — a file under a directory of that name whose content stayed, or
/// any directory the scan could not walk (the key directory may be under it,
/// unseen) — or `None` when the scan accounts for all of it.
pub fn key_store_left_behind(left: &[LeftInCopy]) -> Option<&LeftInCopy> {
    let key = crate::seal::local::KEY_DIRECTORY_NAME;
    left.iter().find(|l| {
        l.reason == LeftReason::SubtreeNotWalked
            || l.path.iter().any(|c| c.eq_ignore_ascii_case(key))
    })
}

/// Outcome of an exclusion scan over a volume.
pub struct ExclusionScan {
    /// Merged excluded byte ranges (volume offsets), finalized.
    pub ranges: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    /// Total bytes of excluded extents (before merging).
    pub bytes_excluded: u64,
    /// Byte totals per file category for the files that STAY in the backup
    /// (excluded files are not part of the archive). Powers the dashboard
    /// breakdown — the walk visits every file anyway, so this is free.
    pub categories: CategorySizes,
    /// Matched entries at their highest match point, for the true-removal
    /// (mount-&-delete) pass over the finished image. Includes entries the
    /// zeroing step cannot handle (resident files, reparse points, files
    /// whose extents failed to resolve) — the driver deletes them all the
    /// same.
    pub paths: Vec<ExcludedPath>,
    /// Per-file resolved extent bytes, filled only when the caller asks for it
    /// (`scan-exclusions --list`). Diagnostics for the B0 comparison against
    /// `scripts/ntfs-mft-probe.py`: the two methods agreed on the file set to
    /// 0.26% and disagreed on the byte total by a third, and a per-file diff is
    /// the only thing that says which one is lying.
    pub file_bytes: Vec<(String, u64)>,
}

#[cfg(test)]
mod mandatory_tests {
    use super::*;

    fn matches(patterns: &[String], path: &str) -> bool {
        let parsed = parse_patterns(patterns);
        let rel: Vec<String> = path.split('/').map(|c| c.to_lowercase()).collect();
        let name = rel.last().cloned().unwrap_or_default();
        parsed.iter().any(|p| p.matches(&name, &rel))
    }

    #[test]
    fn the_service_key_store_is_excluded_whatever_the_user_asked_for() {
        let store = crate::seal::local::KEY_DIRECTORY_NAME;
        // No user rules at all, and it is still excluded — wherever it sits.
        assert!(matches(&[], store));
        assert!(matches(&[], &format!("ProgramData/OwnBackup/{store}")));
        assert!(matches(
            &["*.tmp".into()],
            &format!("Users/lab/AppData/OwnBackup/{store}")
        ));
        // The user's own rules keep working next to it.
        assert!(matches(&["*.tmp".into()], "Users/lab/note.tmp"));
        assert!(!matches(&["*.tmp".into()], "Users/lab/note.txt"));
        // And nothing else gets swept up by the mandatory rule.
        assert!(!matches(&[], "ProgramData/OwnBackup/jobs.json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_info(entries: &[(&str, i64)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (i, (name, size)) in entries.iter().enumerate() {
            let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            // Entries are 8-byte aligned, as Windows lays them out.
            let len = (24 + name.len()).div_ceil(8) * 8;
            let next = if i + 1 == entries.len() {
                0
            } else {
                len as u32
            };
            buf.extend_from_slice(&next.to_le_bytes());
            buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&((size + 4095) / 4096 * 4096).to_le_bytes());
            buf.extend_from_slice(&name);
            buf.resize(buf.len().div_ceil(8) * 8, 0);
        }
        buf
    }

    fn dir_info(entries: &[(&str, i64, u32, u64)], with_ids: bool) -> Vec<u8> {
        let name_at = if with_ids { 104 } else { 68 };
        let mut buf = Vec::new();
        for (i, (name, size, attrs, id)) in entries.iter().enumerate() {
            let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let len = (name_at + name.len()).div_ceil(8) * 8;
            let mut rec = vec![0u8; len];
            let next = if i + 1 == entries.len() {
                0
            } else {
                len as u32
            };
            rec[0..4].copy_from_slice(&next.to_le_bytes());
            rec[40..48].copy_from_slice(&size.to_le_bytes());
            rec[56..60].copy_from_slice(&attrs.to_le_bytes());
            rec[60..64].copy_from_slice(&(name.len() as u32).to_le_bytes());
            if with_ids {
                // A short name in between, which must not be read as the name.
                rec[68] = 24;
                rec[70..94].copy_from_slice(&[0x41; 24]);
                rec[96..104].copy_from_slice(&id.to_le_bytes());
            }
            rec[name_at..name_at + name.len()].copy_from_slice(&name);
            buf.extend_from_slice(&rec);
        }
        buf
    }

    /// Both listing layouts: the plain one the walks use, and the one with
    /// file ids the deletion pass reads record numbers from.
    #[test]
    fn directory_listings_are_parsed_with_and_without_ids() {
        let entries = [
            (".", 0, 0x10, 5),
            ("Длинное имя.tmp", 300, 0x20, (7u64 << 48) | 42),
            ("sub", 0, 0x10, 77),
        ];
        for with_ids in [false, true] {
            let got = parse_dir_info(&dir_info(&entries, with_ids), with_ids);
            let want: Vec<DirEntryInfo> = entries[1..]
                .iter()
                .map(|(n, s, a, id)| DirEntryInfo {
                    name: n.to_string(),
                    size: *s as u64,
                    attributes: *a,
                    file_id: with_ids.then_some(*id),
                })
                .collect();
            assert_eq!(got, want, "with ids: {with_ids}");
        }
        let buf = dir_info(&entries, true);
        assert_eq!(parse_dir_info(&buf[..buf.len() - 4], true).len(), 1);
    }

    /// A hard link keeps the data when one of its names stays: the walk sees
    /// the names Windows lists for the file, each judged by its own path or a
    /// directory above it.
    #[test]
    fn every_name_of_a_hard_link_must_be_excluded() {
        let parsed = parse_patterns(&["Temp".to_string(), "*.tmp".to_string()]);
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(every_name_excluded(
            &names(&["\\Temp\\a.bin", "\\Users\\u\\b.tmp"]),
            &parsed
        ));
        assert!(!every_name_excluded(
            &names(&["\\Temp\\a.tmp", "\\Users\\u\\keep.bin"]),
            &parsed
        ));
        assert!(every_name_excluded(
            &names(&["\\x\\Temp\\deep\\c.bin"]),
            &parsed
        ));
        assert!(!every_name_excluded(&[], &parsed), "no names known");
    }

    /// What `FileStreamInfo` reports, named data streams only: the unnamed
    /// one is the file's own data, and a cut-off entry ends the list.
    #[test]
    fn named_streams_are_read_from_a_stream_info_chain() {
        let buf = stream_info(&[
            ("::$DATA", 9000),
            (":Zone.Identifier:$DATA", 26),
            (":Поток:$DATA", 70_000),
        ]);
        assert_eq!(
            named_streams_from_info(&buf),
            vec![
                (":Zone.Identifier:$DATA".to_string(), 26),
                (":Поток:$DATA".to_string(), 70_000)
            ]
        );
        assert!(named_streams_from_info(&[]).is_empty());
        let cut = &buf[..buf.len() - 4];
        assert_eq!(
            named_streams_from_info(cut).len(),
            1,
            "the cut-off entry is dropped"
        );
    }

    /// Decision 6/E4: content left under any directory of the key store's
    /// name, in any letter case, or a directory nobody walked, stands in the
    /// way; content left elsewhere does not.
    #[test]
    fn the_key_store_proof_looks_under_its_name_and_at_unwalked_directories() {
        let key = crate::seal::local::KEY_DIRECTORY_NAME;
        let p = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let elsewhere = LeftInCopy::new(&p(&["home", "a.bin"]), LeftReason::HardLink);
        assert_eq!(
            key_store_left_behind(std::slice::from_ref(&elsewhere)),
            None
        );
        let upper = key.to_uppercase();
        let under = LeftInCopy::new(&p(&["@", &upper, "x.key"]), LeftReason::Shared);
        assert_eq!(
            key_store_left_behind(&[elsewhere.clone(), under.clone()]),
            Some(&under)
        );
        let unwalked = LeftInCopy::new(&p(&["var", "lib"]), LeftReason::SubtreeNotWalked);
        assert_eq!(
            key_store_left_behind(&[elsewhere, unwalked.clone()]),
            Some(&unwalked)
        );
        assert_eq!(unwalked.shown(), "/var/lib (directory not walked)");
    }

    /// Only the folder Windows recreates is beyond the delete pass — what lies
    /// inside it is deleted like anything else.
    #[test]
    fn only_the_system_folder_itself_is_beyond_the_delete_pass() {
        assert!(recreated_by_windows("System Volume Information"));
        assert!(recreated_by_windows("system volume information"));
        assert!(recreated_by_windows(r"System Volume Information\"));
        assert!(!recreated_by_windows(
            r"System Volume Information\tracking.log"
        ));
        assert!(!recreated_by_windows(
            r"System Volume Information\MasterFileStatus.db"
        ));
        assert!(!recreated_by_windows("Windows"));
        // Same name deeper in the tree: an ordinary folder, and ours to delete.
        assert!(!recreated_by_windows(r"Users\me\System Volume Information"));
    }
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
            assert!(
                p.matches("1", &comps(&format!("{dir}\\1"))),
                "…but its contents go"
            );
            assert!(
                p.matches("x", &comps(&format!("{dir}\\a\\x"))),
                "…at any depth"
            );
        }
    }

    #[test]
    fn the_same_holds_for_a_globstar_in_the_middle() {
        let p = pat("**\\Temp\\**");
        assert!(
            !p.matches("temp", &comps("Windows\\Temp")),
            "the directory survives"
        );
        assert!(!p.matches("temp", &comps("Temp")));
        assert!(
            p.matches("a.log", &comps("Windows\\Temp\\a.log")),
            "its contents do not"
        );
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
        assert!(
            !is_essential_linux_dir(&comps("var\\tmp")),
            "only at the volume root"
        );
        assert!(!is_essential_linux_dir(&comps("home")));
        assert!(!is_essential_linux_dir(&[]));
    }

    #[test]
    fn every_default_linux_exclusion_leaves_the_root_directories_alone() {
        // The whole default list against the top-level directories of a Linux
        // root: none of them may be removed.
        let patterns: Vec<ExclusionPattern> = [
            "proc/**",
            "sys/**",
            "dev/**",
            "run/**",
            "tmp/**",
            "var/tmp/**",
            "var/cache/**",
            "**/.cache/**",
            "swapfile",
            "lost+found",
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
        assert!(p.matches(
            "b.dat",
            &comps("Users\\Bob\\AppData\\Local\\Temp\\sub\\b.dat")
        ));
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
        // The user's list minus the empty entry, plus the mandatory rules.
        assert_eq!(
            parse_patterns(&["".into(), "*.tmp".into()]).len(),
            MANDATORY_EXCLUSIONS.len() + 1
        );
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
