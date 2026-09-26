//! Backup orchestrator core (plan v2, phase 4).
//!
//! The single owner of backup orchestration: the rich [`Job`] config model,
//! JSON persistence ([`JobStore`]), a serial run queue ([`RunQueue`] +
//! [`run_queue_worker`]) and the per-run driver ([`perform_backup`]) that
//! calls the Windows engine, plus chain planning ([`plan_run`]) and retention.
//!
//! This crate is host-agnostic: progress and lifecycle events are delivered
//! through the [`EventSink`] trait, so the Tauri GUI (window events) and the
//! service (gRPC streams) each adapt it their own way. Nothing here depends on
//! Tauri. Runs are serialized through an mpsc queue processed by one worker,
//! so a second "run" while a backup is in progress is queued, not rejected.

// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod archive_setup;
pub mod backup_encryption;
pub mod busy;
pub mod restore_exec;

/// Where a destination folder sits relative to a job's sources — re-exported
/// so callers do not have to reach into the backup crate for one enum.
pub use own_backup_backup::source_identity::DestPlacement;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Read speed of a verify pass, averaged over the last second.
///
/// The verify callback counts BLOCKS, and a block count says nothing to a
/// person watching a 85 GiB image go by. This turns it into MiB/s the same way
/// the copy phase reports it, over a one-second window so the number is steady
/// enough to read but still follows a disk that slows down.
// Обе проверки после копии живут в Windows-путях; на Linux структура
// компилируется, но никем не зовётся.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct VerifyRate {
    block_bytes: u64,
    window_start: std::time::Instant,
    window_blocks: u64,
    mbps: f64,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl VerifyRate {
    /// `image` is read once for its block size — a verify of an image with a
    /// different block size must not be reported in someone else's units.
    ///
    /// With the key, when the copy is encrypted: read without one it fell back
    /// to a default that happens to be right today, which is the kind of
    /// correctness that stops being correct without anyone noticing.
    fn new(
        image: &std::path::Path,
        keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    ) -> Self {
        let block_bytes = own_backup_backup::sidecar::read_sidecar_with_keys(image, keys)
            .map(|s| s.manifest.block_size as u64)
            .unwrap_or(1024 * 1024);
        Self {
            block_bytes,
            window_start: std::time::Instant::now(),
            window_blocks: 0,
            mbps: 0.0,
        }
    }

    /// Feed the running block count; returns MiB/s to report right now.
    fn sample(&mut self, blocks_done: u64) -> f64 {
        let elapsed = self.window_start.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let read = blocks_done.saturating_sub(self.window_blocks) * self.block_bytes;
            // Same unit as the copy phase reports (`speed_bps / 1e6`), so the
            // two numbers on the same line mean the same thing.
            self.mbps = read as f64 / elapsed.as_secs_f64() / 1_000_000.0;
            self.window_start = std::time::Instant::now();
            self.window_blocks = blocks_done;
        }
        self.mbps
    }
}


use own_backup_common::tr;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Config model (persisted)
// ---------------------------------------------------------------------------

/// Schedule tab. Only `kind`/`days`/`time` shape the scheduler; the rest tune
/// how the service fires runs (wake, missed, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// none | daily | weekly | monthly | event | nonstop
    pub kind: String,
    /// weekday keys for `weekly`: mon,tue,wed,thu,fri,sat,sun
    pub days: Vec<String>,
    /// "HH:MM"
    pub time: String,
    /// Day of month for `monthly` (1–28 only, so it exists in every month —
    /// avoids the "31st never fires in February" trap).
    pub day_of_month: u32,
    /// For `kind = event`: what fires the run. One of "dest_connected" (the
    /// drive the copy goes to appears — see [`Self::is_dest_connected_trigger`]),
    /// "logon", "logoff", "shutdown", "startup".
    pub event_trigger: String,
    /// Event runs at most once per calendar day when set (like Acronis).
    pub event_once_per_day: bool,
    /// Minutes to wait after startup before the "startup" event fires. 0–60.
    pub event_startup_delay_min: u32,
    /// Wake the machine from sleep for a daily/weekly/monthly run: the service
    /// arms a wake timer a minute before the next slot. Needs wake timers
    /// allowed in the power plan.
    ///
    /// `only_when_locked` and `prevent_sleep` used to sit here, saved and never
    /// read (audit AUD-16). The first is gone; the second is no longer a
    /// choice — no copy lets the machine sleep while it runs (2026-09-26).
    /// Old jobs that still carry them load fine: unknown keys are ignored.
    pub wake_to_run: bool,
    pub run_missed: bool,
    /// Minutes to wait after the service (≈ system) starts before catching up
    /// missed runs — lets the machine settle after boot. 1–60.
    pub missed_delay_min: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            kind: "none".into(),
            days: vec!["mon".into(), "wed".into(), "fri".into()],
            time: "14:00".into(),
            day_of_month: 1,
            event_trigger: "startup".into(),
            event_once_per_day: true,
            event_startup_delay_min: 0,
            wake_to_run: false,
            run_missed: true,
            missed_delay_min: 1,
        }
    }
}

impl ScheduleConfig {
    /// An event job that fires when its destination drive appears. Jobs saved
    /// before 2026-09-26 say "usb_connect" — "an external device was plugged
    /// in", which was never wired; the drive the copy goes to appearing is
    /// what that setting was for.
    pub fn is_dest_connected_trigger(&self) -> bool {
        self.kind == "event"
            && matches!(self.event_trigger.as_str(), "dest_connected" | "usb_connect")
    }
}

/// Scheme tab (method + retention).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchemeConfig {
    /// full | incremental | differential
    pub method: String,
    /// incremental: only-incremental vs full-every-N
    pub only_incremental: bool,
    pub full_every_n: u32,
    pub cleanup_older_than_days: Option<u32>,
    pub keep_chains: Option<u32>,
    pub keep_first: bool,
    /// Sliding window: keep at most this many increments behind the full
    /// backup, folding the oldest one into it when a new run overflows the
    /// window (`None` = off, which is how every job written before this
    /// existed keeps behaving).
    ///
    /// The window costs the DEEPEST restore point — the base stops standing
    /// for the day it was taken and starts standing for the day of the
    /// increment it absorbed. Nothing in the middle is lost: the count of
    /// restore points stays at `rolling_incrementals + 1`.
    ///
    /// Independent of `full_every_n`, which still starts a NEW chain every N
    /// runs — that counter lives in the run history, not in the files, so
    /// consolidating does not reset it. A job with both set gets a bounded
    /// window inside each chain and a fresh full now and then, which is the
    /// combination worth recommending: a base that is rewritten forever is one
    /// file with no independent copy of anything.
    #[serde(default)]
    pub rolling_incrementals: Option<u32>,
}

impl Default for SchemeConfig {
    fn default() -> Self {
        Self {
            method: "full".into(),
            only_incremental: false,
            full_every_n: 3,
            cleanup_older_than_days: None,
            keep_chains: Some(3),
            keep_first: false,
            rolling_incrementals: None,
        }
    }
}

impl SchemeConfig {
    /// True when the job runs the **consolidation** scheme: ONE chain, kept
    /// short by folding its oldest increment into the base, instead of a new
    /// chain every N versions.
    ///
    /// Derived rather than stored — `rolling_incrementals` being set IS the
    /// mode — so there is no second field that can disagree with the first.
    pub fn is_consolidation(&self) -> bool {
        self.method == "incremental" && self.rolling_incrementals.is_some()
    }

    /// After how many increments a new chain is started, or `None` when the
    /// chain never rolls over by count.
    ///
    /// **Zero means OFF** — the way it does for `keep_chains`,
    /// `cleanup_older_than_days` and `rolling_incrementals`, every other number
    /// in this group, and what the CLI has always documented ("0 — не начинать
    /// новую цепочку").
    ///
    /// It did not. The two places that asked wrote `full_every_n.max(1)`, so a
    /// zero became a one and "never start a new chain" produced the most
    /// aggressive setting there is: a fresh FULL copy after every single
    /// increment. Found on 2026-09-20 by setting it and watching a third run
    /// write a second full — the flag did the exact opposite of its own help
    /// text, and quietly.
    ///
    /// `only_incremental` says the same thing in its own field and still wins;
    /// the two are redundant, which is why the zero was never noticed.
    pub fn rolls_over_after(&self) -> Option<u32> {
        (!self.only_incremental && self.full_every_n > 0).then_some(self.full_every_n)
    }
}

/// Exclusions tab. Patterns are kept in two OS-specific lists so a whole-disk
/// backup that spans both worlds (Windows NTFS + Linux ext/btrfs partitions)
/// excludes the right things on each: the engine applies `patterns` to
/// Windows-family filesystems (NTFS/ReFS/FAT) and `patterns_linux` to Linux
/// ones. `patterns` keeps its name for backward compatibility with saved jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExclusionsConfig {
    pub enabled: bool,
    /// Windows-filesystem exclusion masks.
    pub patterns: Vec<String>,
    /// Linux-filesystem exclusion masks.
    pub patterns_linux: Vec<String>,
}

impl Default for ExclusionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            patterns: default_exclusions(),
            patterns_linux: default_linux_exclusions(),
        }
    }
}

/// Uppercase Windows drive letter of a path or device spec, if it has one.
/// `E:`, `E:\Foo`, `\\.\E:`, `\\?\E:\` → `Some('E')`; UNC shares
/// (`\\server\share`) and VSS/GLOBALROOT device paths → `None`.
pub fn drive_letter_of(s: &str) -> Option<char> {
    let t = s.trim();
    let t = t
        .strip_prefix(r"\\.\")
        .or_else(|| t.strip_prefix(r"\\?\"))
        .unwrap_or(t);
    let b = t.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(b[0].to_ascii_uppercase() as char)
    } else {
        None
    }
}

/// True when the destination folder sits on the very volume being backed up.
/// Writing the image there would grow the source while it is being imaged —
/// forbidden. UNC/network destinations (no drive letter) are always allowed.
pub fn dest_on_source_volume(source: &str, dest_folder: &str) -> bool {
    match (drive_letter_of(source), drive_letter_of(dest_folder)) {
        (Some(src), Some(dst)) => src == dst,
        _ => false,
    }
}

/// For a multi-volume job: the drive letter of the first source that collides
/// with the destination folder, or `None` if the destination is safe for all
/// sources.
///
/// By LETTERS only, so it sees nothing when a source is a volume GUID path, a
/// `part:` range or a mount point. Kept as the cheap first pass, with the real
/// answer in [`dest_placement`] — this one never refuses something that is
/// actually fine, so it costs nothing to ask first.
pub fn dest_collision_letter(job: &Job, dest_folder: &str) -> Option<char> {
    let dst = drive_letter_of(dest_folder)?;
    job.source_list()
        .iter()
        .filter_map(|s| drive_letter_of(s))
        .find(|&src| src == dst)
}

/// Where the destination folder actually sits relative to the job's sources.
///
/// Compares the RANGES the volumes occupy, not their names (audit AUD-10). A
/// source given as `\\?\Volume{…}`, as `part:<disk>:<offset>:<len>` or reached
/// through a mount point matched nothing at all by letter, so the image could
/// be written onto the volume being imaged. And it is by range on purpose: a
/// DIFFERENT partition of the same disk is a perfectly ordinary destination,
/// and refusing it would be refusing something that works.
pub fn dest_placement(job: &Job, dest_folder: &str) -> own_backup_backup::source_identity::DestPlacement {
    own_backup_backup::source_identity::where_the_destination_sits(
        &job.source_list(),
        dest_folder,
    )
}

pub fn default_exclusions() -> Vec<String> {
    [
        // Volatile system files — huge and pointless to image.
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
        // Recycle bin and VSS/system state — restored/rebuilt by Windows.
        "$Recycle.Bin",
        "System Volume Information",
        // Temp / scratch — any Temp/Tmp directory at any depth on the volume,
        // plus loose temp files. `**` matches any run of path components.
        "**\\Temp\\**",
        "**\\Tmp\\**",
        "*.tmp",
        "*.~",
        // What Windows rebuilds by itself — anchored to the volume root, so they
        // only ever match on the system volume. A folder is excluded by its
        // CONTENTS (a trailing `\**`): the directory itself stays, with its
        // permissions, and Windows fills it again.
        //
        // Update downloads and the update database. Emptying this folder is
        // Microsoft's own "reset Windows Update" step; the history list
        // starts over, nothing else is lost.
        "Windows\\SoftwareDistribution\\**",
        // Launch traces — rebuilt as programs start.
        "Windows\\Prefetch\\**",
        // Delivery Optimization's peer cache (update pieces for other PCs).
        "Windows\\ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache\\**",
        // Crash dumps and error reports: Microsoft's own list of files not to
        // back up (`FilesNotToBackup`: "Kernel Dumps", "WER").
        "Windows\\MEMORY.DMP",
        "Windows\\Minidump\\**",
        "Windows\\LiveKernelReports\\**",
        "ProgramData\\Microsoft\\Windows\\WER\\**",
        "Users\\*\\AppData\\Local\\CrashDumps\\**",
        // Avast / AVG working folder.
        "$AV_ASW\\**",
        // Caches every program refills on its own: browsers (every profile),
        // compiled shaders, Explorer's thumbnails. Gigabytes on a machine
        // that is used, and nothing a restore needs.
        "Users\\*\\AppData\\Local\\Microsoft\\Windows\\INetCache\\**",
        "Users\\*\\AppData\\Local\\Microsoft\\Windows\\Explorer\\thumbcache_*.db",
        "Users\\*\\AppData\\Local\\Google\\Chrome\\User Data\\*\\Cache\\**",
        "Users\\*\\AppData\\Local\\Google\\Chrome\\User Data\\*\\Code Cache\\**",
        "Users\\*\\AppData\\Local\\Google\\Chrome\\User Data\\*\\GPUCache\\**",
        "Users\\*\\AppData\\Local\\Microsoft\\Edge\\User Data\\*\\Cache\\**",
        "Users\\*\\AppData\\Local\\Microsoft\\Edge\\User Data\\*\\Code Cache\\**",
        "Users\\*\\AppData\\Local\\Microsoft\\Edge\\User Data\\*\\GPUCache\\**",
        "Users\\*\\AppData\\Local\\Mozilla\\Firefox\\Profiles\\*\\cache2\\**",
        "Users\\*\\AppData\\Local\\D3DSCache\\**",
        "Users\\*\\AppData\\Local\\NVIDIA\\DXCache\\**",
        "Users\\*\\AppData\\Local\\NVIDIA\\GLCache\\**",
        "Users\\*\\AppData\\Local\\AMD\\DxCache\\**",
        // NOT here, on purpose: `*.vhdx`, `*.avhdx`, `*.obkm` used to be, "in
        // case the destination lives on the source". A destination on a source
        // volume is refused outright (`dest_placement`, at save and before
        // every run), so they guarded nothing — and silently dropped every
        // virtual machine's disk from the copy (2026-09-26). Nor `Windows.old`
        // or `$WINDOWS.~BT`: they are an upgrade's way back.
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Default exclusion masks for a Linux filesystem (ext/btrfs/xfs) — the same
/// idea as [`default_exclusions`] but by Linux meaning. Paths are volume-root
/// relative (the volume root is `/`), `/`-separated. Enforcement on ext/btrfs
/// filesystems is a follow-up; today these are stored and applied once the
/// Linux-FS exclusion scanner lands (NTFS/ReFS/FAT are enforced now).
pub fn default_linux_exclusions() -> Vec<String> {
    [
        // Pseudo / volatile filesystems — mount points, normally empty on the
        // image; listed for clarity and bind-mount edge cases.
        "proc/**",
        "sys/**",
        "dev/**",
        "run/**",
        // Swap.
        "swapfile",
        "swap.img",
        // Temp / scratch.
        "tmp/**",
        "var/tmp/**",
        // Caches — rebuilt on demand.
        "var/cache/**",
        "**/.cache/**",
        "**/.thumbnails/**",
        // Trash.
        "**/.local/share/Trash/**",
        "**/.Trash-*/**",
        // fsck leftovers.
        "lost+found",
        // No image masks (`*.qcow2`, `*.vhdx`…): a destination on a source
        // volume is refused, and those masks dropped every VM disk from the
        // copy — see `default_exclusions`.
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Advanced tab. Every field here is read by the run (2026-09-26: the ones
/// that were saved and never read are gone — see the note at the end).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    pub sector_by_sector: bool,
    pub validate_after: bool,
    /// With `validate_after`: read only the blocks the new copy itself wrote,
    /// not the whole chain under it. Faster on a long chain, and blind to
    /// damage in the older copies — which is why it is off by default.
    pub verify_latest_only: bool,
    /// A run that ends in an error is queued again this many times, each
    /// `retry_interval_min` after the previous failure. 0 = off. A cancelled
    /// run is never retried: someone stopped it.
    ///
    /// The case it is for is in the activity log of the machine it was built
    /// on: «Устройство не готово (os error 21)» — a destination drive that was
    /// not there yet, a copy that a second try minutes later would have made.
    pub retry_attempts: u32,
    pub retry_interval_min: u32,
    /// After the run: "none", "sleep" or "shutdown". Only when nothing else is
    /// queued or running, and not after a run someone cancelled or one that is
    /// about to be retried. Never on the boot medium.
    pub after_backup: String,
    /// Copy at background priority — disk and CPU go to whatever the user is
    /// doing first. Slower when the machine is busy, unnoticeable when it is not.
    pub low_priority: bool,
    /// Cap on how fast the source is read, MB/s. 0 = no cap.
    pub speed_limit_mbps: u32,
    // Removed on 2026-09-26, saved and never read: `backup_unallocated`,
    // `password_enabled` (encryption is the OBK format now), `pre_command` /
    // `post_command` (a command a SYSTEM service runs, from a job any
    // interactive user can save, is a privilege escalation, not a feature),
    // `error_handling`, `shutdown_after` and `performance` (replaced by the
    // fields above). Old jobs carrying them load fine.
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            sector_by_sector: false,
            // On by default: a copy nobody has read back is a copy nobody
            // knows restores. A job that was saved with it off keeps its
            // choice — this is the default for new jobs only.
            validate_after: true,
            verify_latest_only: false,
            // On by default — old jobs included, since they load their missing
            // fields from here: a nightly copy that failed on a drive not yet
            // spun up is exactly what this is for.
            retry_attempts: 2,
            retry_interval_min: 10,
            after_backup: "none".into(),
            low_priority: false,
            speed_limit_mbps: 0,
        }
    }
}

/// One image a run wrote, with the source it came from.
///
/// Kept per volume so "everything this job produced" is answerable from the
/// history for a multi-volume job, not just for its first volume (audit
/// AUD-09).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RunImage {
    /// The source volume/device this image is a copy of.
    pub source: String,
    pub image_path: String,
    /// The chain's full backup this image belongs to (== `image_path` for a
    /// full).
    pub base_image_path: String,
    /// full | incremental | differential
    pub kind: String,
}

/// A single completed (or failed) run — feeds the dashboard and Activity.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub job_name: String,
    pub started_at: String,
    pub finished_at: String,
    /// success | error | cancelled
    pub status: String,
    pub image_path: String,
    pub source_bytes: u64,
    pub bytes_copied: u64,
    pub used_blocks: u64,
    pub total_blocks: u64,
    pub duration_secs: f64,
    pub smart_mode: bool,
    pub vss_used: bool,
    pub error: String,
    /// full | incremental | differential — what was actually created (the
    /// engine may fall back to full when the chain is unusable). Empty in
    /// runs recorded before phase 5 (those were always full).
    pub backup_kind: String,
    /// The chain's full image this run belongs to (== image_path for fulls).
    pub base_image_path: String,
    /// 0 for a full backup, N for the N-th child since the last full.
    pub increment_index: u32,
    /// Used blocks skipped because they matched the parent version.
    pub blocks_unchanged: u64,
    /// Files excluded by patterns (phase 3) and the bytes they freed.
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Real per-category byte totals of the archived files (collected during
    /// the exclusion walk); None when the walk did not run.
    pub categories: Option<own_backup_backup::exclusions::CategorySizes>,
    /// Why that walk did not run, in words for the result screen — empty when
    /// it did. Both the exclusions and the by-type breakdown come out of it, so
    /// this one sentence explains either absence instead of leaving the user
    /// with "Исключено файлов: 0" over twelve rules that were never applied.
    #[serde(default)]
    pub file_scan_skipped: String,
    /// Every image this run produced — one per source volume.
    ///
    /// `image_path` and `base_image_path` above describe only the FIRST volume
    /// (they drive the single-value dashboard fields). A two-volume job
    /// therefore had half its output recorded nowhere, and "delete the job's
    /// copies" left the second volume's images on disk (audit AUD-09). They
    /// stay as they are — the dashboard wants one answer, and runs recorded
    /// before this field existed have nothing else.
    #[serde(default)]
    pub images: Vec<RunImage>,
    /// Increments consolidation folded into their base after this run.
    ///
    /// Worth surfacing rather than leaving in the log: it is the one step that
    /// REWRITES an existing copy, and it moves the oldest restore point
    /// forward. A user who sees their version count stay flat deserves to be
    /// told why, in the same place the run is reported.
    #[serde(default)]
    pub consolidated_increments: u32,
}

/// A persisted backup job (task).
/// Whole-disk (bootable) backup metadata attached to a [`Job`]: which physical
/// disk it captures and how each backed-up partition maps to a GPT slot. Used
/// after the run to write the `.obdm` disk-restore manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiskSourceSpec {
    /// Physical drive number (`\\.\PhysicalDrive{disk_number}`).
    pub disk_number: u32,
    pub disk_model: String,
    pub parts: Vec<DiskPartSpec>,
    /// The user ticked "Загрузочная копия диска" for this disk.
    ///
    /// Every whole-disk job captures `disk_source`, bootable or not — that is
    /// what makes one job produce one image file. This says whether the copy is
    /// meant to BOOT, which decides one thing: whether the machine's UEFI boot
    /// entries travel inside the image. They live in the motherboard, not on
    /// the disk, and a copy of a data disk has no business carrying the boot
    /// menu. `false` in jobs saved before the flag existed — those copies
    /// simply carry no entries, exactly as if the switch had been off.
    #[serde(default)]
    pub bootable: bool,
}

/// One partition of a whole-disk backup: its GPT slot and its source device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskPartSpec {
    /// GPT partition number (matches the entry index in the disk's GPT).
    pub part_index: u32,
    /// The partition's source device (letter "C:", volume GUID path, or
    /// `part:<disk>:<off>:<len>`) — the join key to the backed-up image.
    pub device: String,
    /// Filesystem name for display / the manifest.
    pub filesystem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub name: String,
    /// Primary source volume device, e.g. "E:". Kept for file naming and
    /// backward compatibility; `sources` is the full multi-volume selection.
    pub source: String,
    /// Human label for display, e.g. "E: WorkDisk-2 — 347 GB".
    pub source_label: String,
    /// All selected source volumes for a multi-volume job. When empty, the job
    /// is single-volume and `source` is the one volume (old jobs deserialize
    /// this way). Each volume produces its own image/chain within one run.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Display labels paralleling `sources` (same length, same order).
    #[serde(default)]
    pub source_labels: Vec<String>,
    /// At least one source volume is encrypted (LUKS), so the job needs a
    /// password to run. The scheduler skips such a job when no password is
    /// saved (the GUI then makes schedule setup read-only until one is).
    #[serde(default)]
    pub requires_password: bool,
    /// Set when this is a whole-disk (bootable) backup: which physical disk and
    /// how its partitions map to the backed-up images. `sources` still holds the
    /// per-partition devices (so the backup loop is unchanged); this adds the
    /// GPT/partition metadata needed to write the disk-restore manifest.
    #[serde(default)]
    pub disk_source: Option<DiskSourceSpec>,
    /// Snapshot of the source disk layout(s) captured when the job is saved, so
    /// the restore screen can render the archived disk instantly — without
    /// querying the live system, and even when that disk is no longer attached.
    /// Written once by the GUI at creation (it already holds the enumerated
    /// disks); empty for pre-existing jobs, which fall back to a live query.
    #[serde(default)]
    pub source_disks: Vec<own_backup_common::disk::DiskInfo>,
    /// Destination FOLDER; each run creates `<name>_<letter>_<stamp>.<ext>`.
    pub dest_folder: String,
    /// Image container format (decision #13): `vhdx` (default; Windows-native
    /// mounting, chains, true removal) or `qcow2` (zstd-compressed,
    /// cross-platform; full backups only until chains land). Old jobs
    /// deserialize as vhdx.
    #[serde(default)]
    pub image_format: own_backup_common::types::ImageFormat,
    /// Portable archive-key envelope and local service authorization reference.
    /// No password, raw private key or DPAPI blob belongs in the job document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_encryption: Option<backup_encryption::ArchiveEncryption>,
    /// zstd level for this job's qcow2/`.obk` copies (1–19). `None` follows
    /// the program-wide default in the settings (`default_compression_level`),
    /// read at each run — changing that default changes every job that has not
    /// chosen its own. VHDX is not compressed and ignores it.
    #[serde(default)]
    pub compression_level: Option<i32>,
    pub schedule: ScheduleConfig,
    pub scheme: SchemeConfig,
    pub exclusions: ExclusionsConfig,
    pub advanced: AdvancedConfig,
    pub created_at: String,
    pub updated_at: String,
    pub last_run: Option<JobRun>,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            source: String::new(),
            source_label: String::new(),
            sources: Vec::new(),
            source_labels: Vec::new(),
            requires_password: false,
            disk_source: None,
            source_disks: Vec::new(),
            dest_folder: String::new(),
            image_format: own_backup_common::types::ImageFormat::default(),
            archive_encryption: None,
            compression_level: None,
            schedule: ScheduleConfig::default(),
            scheme: SchemeConfig::default(),
            exclusions: ExclusionsConfig::default(),
            advanced: AdvancedConfig::default(),
            created_at: String::new(),
            updated_at: String::new(),
            last_run: None,
        }
    }
}

impl Job {
    /// The zstd level this run compresses with: the job's own, else the
    /// settings' default, always 1–19.
    pub fn zstd_level(&self) -> i32 {
        let level = self.compression_level.unwrap_or_else(|| {
            own_backup_common::config::AppConfig::load_or_default().default_compression_level
        });
        own_backup_backup::qcow2::zstd_level_or_default(level)
    }

    /// The volumes this job backs up, normalized across the old single-volume
    /// model (`source`) and the new multi-volume one (`sources`). Never empty
    /// for a valid job; each entry gets its own image/chain within a run.
    pub fn source_list(&self) -> Vec<String> {
        if !self.sources.is_empty() {
            self.sources.iter().filter(|s| !s.trim().is_empty()).cloned().collect()
        } else if !self.source.trim().is_empty() {
            vec![self.source.clone()]
        } else {
            Vec::new()
        }
    }

    /// Display label for a source device, from the parallel `source_labels`
    /// (falling back to the primary label or the device itself).
    pub fn label_for(&self, device: &str) -> String {
        if let Some(idx) = self.sources.iter().position(|s| s == device) {
            if let Some(l) = self.source_labels.get(idx) {
                if !l.trim().is_empty() {
                    return l.clone();
                }
            }
        }
        if self.source == device && !self.source_label.trim().is_empty() {
            return self.source_label.clone();
        }
        device.to_string()
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Thread-safe, file-backed job store. Runs history is kept in a separate
/// `activity` list capped to the most recent entries.
pub struct JobStore {
    jobs_path: PathBuf,
    activity_path: PathBuf,
    jobs: Mutex<Vec<Job>>,
    activity: Mutex<Vec<JobRun>>,
}

const ACTIVITY_CAP: usize = 200;

/// How a run ends when the user cancelled the post-copy check rather than the
/// copy (audit AUD-12).
///
/// Three things have to stay true at once, and they pull in different
/// directions: the image is finished and must NOT be deleted (unlike a
/// cancelled copy, which leaves a half-written file); the run must not be
/// called a success, because nothing read the image back; and retention must
/// not run, because a stage the user stopped is no basis for deleting older
/// versions. So the run is `cancelled`, with the image path recorded and this
/// sentence saying which half happened.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn verify_cancelled_message() -> String {
    tr!("lib.verify_cancelled").into_owned()
}

impl JobStore {
    pub fn load(data_dir: &std::path::Path) -> Arc<Self> {
        let jobs_path = data_dir.join("jobs.json");
        let activity_path = data_dir.join("activity.json");
        let jobs = read_json(&jobs_path).unwrap_or_default();
        let activity = read_json(&activity_path).unwrap_or_default();
        Arc::new(Self {
            jobs_path,
            activity_path,
            jobs: Mutex::new(jobs),
            activity: Mutex::new(activity),
        })
    }

    pub fn list(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().unwrap().iter().find(|j| j.id == id).cloned()
    }

    /// Insert or update a job; returns the stored job once it is ON DISK.
    ///
    /// **Disk first, memory second.** It used to change memory, hand the job
    /// back as saved and only log if the write failed — so a full disk or a
    /// read-only folder produced a job that was visible, editable and runnable
    /// until the next restart, then gone without a word (audit AUD-08,
    /// reproduced). Now a failed write leaves the store exactly as it was and
    /// the caller is told.
    pub fn upsert(&self, job: Job) -> std::io::Result<Job> {
        self.upsert_inner(job, false)
    }

    fn upsert_inner(&self, mut job: Job, authorized_encryption: bool) -> std::io::Result<Job> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut jobs = self.jobs.lock().unwrap();
        let previous = jobs.iter().find(|existing| existing.id == job.id);
        backup_encryption::validate_update(&mut job, previous, authorized_encryption)?;
        let mut next = jobs.clone();
        if job.id.is_empty() {
            job.id = uuid::Uuid::new_v4().to_string();
            job.created_at = now.clone();
            job.updated_at = now;
            next.push(job.clone());
        } else if let Some(existing) = next.iter_mut().find(|j| j.id == job.id) {
            job.created_at = existing.created_at.clone();
            job.last_run = existing.last_run.clone(); // preserve run history
            job.updated_at = now;
            *existing = job.clone();
        } else {
            job.created_at = now.clone();
            job.updated_at = now;
            next.push(job.clone());
        }
        write_json(&self.jobs_path, &next)?;
        *jobs = next;
        Ok(job)
    }

    /// Remove a job; the list in memory changes only once the disk agrees.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let removed: Option<Job>;
        {
            let mut jobs = self.jobs.lock().unwrap();
            removed = jobs.iter().find(|j| j.id == id).cloned();
            let next: Vec<Job> = jobs.iter().filter(|j| j.id != id).cloned().collect();
            write_json(&self.jobs_path, &next)?;
            *jobs = next;
        }
        // Only now: the job is durably gone, so its local key refers to nothing
        // and must not stay on this machine. The lock is released first —
        // revocation touches the key store, not the job list.
        if let Some(job) = removed {
            self.retire_local_archive_key(&job);
        }
        Ok(())
    }

    /// Distinct image files this job's own history mentions.
    ///
    /// This is the *narrow* answer and it is deliberately kept: only paths the
    /// app itself wrote are returned, so no wildcard sweep can ever reach an
    /// unrelated file. But it is bounded by [`ACTIVITY_CAP`] runs, so it is not
    /// the whole answer — see [`job_files`], which asks the images themselves.
    pub fn job_image_files(&self, job_id: &str) -> Vec<PathBuf> {
        let mut set = std::collections::BTreeSet::new();
        let add = |p: &str, set: &mut std::collections::BTreeSet<PathBuf>| {
            if !p.is_empty() {
                set.insert(PathBuf::from(p));
            }
        };
        {
            let activity = self.activity.lock().unwrap();
            for r in activity.iter().filter(|r| r.job_id == job_id) {
                add(&r.image_path, &mut set);
                add(&r.base_image_path, &mut set);
                for i in &r.images {
                    add(&i.image_path, &mut set);
                    add(&i.base_image_path, &mut set);
                }
            }
        }
        {
            let jobs = self.jobs.lock().unwrap();
            if let Some(run) = jobs.iter().find(|j| j.id == job_id).and_then(|j| j.last_run.as_ref()) {
                add(&run.image_path, &mut set);
                add(&run.base_image_path, &mut set);
                for i in &run.images {
                    add(&i.image_path, &mut set);
                    add(&i.base_image_path, &mut set);
                }
            }
        }
        set.into_iter().collect()
    }

    /// Has the activity journal reached its cap, i.e. are there runs it can no
    /// longer tell us about? Used to say out loud that "everything this job
    /// wrote" may not be everything.
    pub fn history_is_truncated(&self, job_id: &str) -> bool {
        let activity = self.activity.lock().unwrap();
        activity.len() >= ACTIVITY_CAP && activity.iter().any(|r| r.job_id == job_id)
    }

    /// Record a finished run.
    ///
    /// Unlike a save, this cannot be refused: the backup already happened, and
    /// forgetting it in memory would not un-happen it. What it must not do is
    /// stay quiet — an unrecorded run is a chain the next backup will plan
    /// against wrongly — so a failed write is returned and the caller says so.
    pub fn record_run(&self, run: JobRun) -> std::io::Result<()> {
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.iter_mut().find(|j| j.id == run.job_id) {
                job.last_run = Some(run.clone());
            }
        }
        {
            let mut activity = self.activity.lock().unwrap();
            activity.insert(0, run);
            activity.truncate(ACTIVITY_CAP);
        }
        let jobs = write_json(&self.jobs_path, &*self.jobs.lock().unwrap());
        let activity = write_json(&self.activity_path, &*self.activity.lock().unwrap());
        jobs.and(activity)
    }

    pub fn activity(&self) -> Vec<JobRun> {
        self.activity.lock().unwrap().clone()
    }

    /// Most recent successful run of a job. Chain planning keys on this, not
    /// on `last_run`: a failed or cancelled run must not reset an incremental
    /// chain back to a full backup.
    pub fn last_success(&self, job_id: &str) -> Option<JobRun> {
        self.activity
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.job_id == job_id && r.status == "success")
            .cloned()
    }

}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            // Unreadable is not the same as absent, and the difference matters:
            // starting empty means the next save writes an empty list over
            // whatever was there. The file is moved aside first, so the jobs
            // are recoverable by hand instead of gone.
            let kept = path.with_extension("json.bad");
            let saved = std::fs::rename(path, &kept).is_ok();
            error!(
                path = %path.display(),
                error = %e,
                kept = %if saved { kept.display().to_string() } else { "nowhere".into() },
                "store file could not be read - starting empty, the old one is kept"
            );
            None
        }
    }
}

fn write_json<T: Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        // Pushed out of the cache BEFORE the rename. Without it the rename can
        // land while the bytes have not, and a crash then leaves a file that
        // exists, is the right size and holds nothing.
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Run queue + events
// ---------------------------------------------------------------------------

/// Queue state shared between the `run_job` command and the worker.
#[derive(Clone)]
pub struct RunQueue {
    /// Waiting and running under ONE lock, and the fields kept private so it
    /// stays that way.
    ///
    /// They used to be two mutexes and two public fields, and every caller
    /// chose its own order: `snapshot` took current then pending, while the
    /// RPC and the scheduler took pending then current. That is a deadlock
    /// waiting for the right interleaving, and it was reproduced (audit
    /// AUD-04). It also left "is it already queued?" and "put it in the queue"
    /// as two separate lock-holds, so two callers could both decide it was not
    /// there and both add it.
    state: Arc<Mutex<QueueState>>,
}

#[derive(Default)]
struct QueueState {
    pending: VecDeque<String>,
    current: Option<String>,
}

impl RunQueue {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(QueueState::default())) }
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let q = self.state.lock().unwrap();
        QueueSnapshot { current: q.current.clone(), pending: q.pending.iter().cloned().collect() }
    }

    /// Put a job in the queue unless it is already waiting or running.
    ///
    /// The answer and the change happen under the same lock: `false` means it
    /// was already there and the caller must not send a second message about
    /// it.
    pub fn enqueue(&self, id: &str) -> bool {
        let mut q = self.state.lock().unwrap();
        if q.current.as_deref() == Some(id) || q.pending.iter().any(|j| j == id) {
            return false;
        }
        q.pending.push_back(id.to_string());
        true
    }

    /// Move a waiting job to running. `false` when it is no longer waiting —
    /// it was cancelled between the message being sent and the worker
    /// arriving, and must not start.
    pub fn start(&self, id: &str) -> bool {
        let mut q = self.state.lock().unwrap();
        let Some(at) = q.pending.iter().position(|j| j == id) else { return false };
        q.pending.remove(at);
        q.current = Some(id.to_string());
        true
    }

    /// Nothing is running any more.
    pub fn finish(&self) {
        self.state.lock().unwrap().current = None;
    }

    /// Take a job out of the queue while it is still waiting. A running job is
    /// not touched — stopping that one is cancellation, not dequeuing.
    /// Returns whether it was actually waiting.
    pub fn cancel_pending(&self, id: &str) -> bool {
        let mut q = self.state.lock().unwrap();
        let before = q.pending.len();
        q.pending.retain(|j| j != id);
        q.pending.len() != before
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct QueueSnapshot {
    pub current: Option<String>,
    pub pending: Vec<String>,
}

/// Progress payload for a running backup.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JobProgress {
    pub job_id: String,
    /// prepare | backup | finalize | verify
    pub phase: String,
    /// Percent of the CURRENT volume (0..100). Multi-volume jobs report each
    /// volume's own progress; the UI shows which volume via the fields below.
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub speed_mbps: f64,
    /// 0-based index of the volume being copied in a multi-volume job.
    #[serde(default)]
    pub volume_index: u64,
    /// Total number of volumes in this job run (1 for single-volume jobs).
    #[serde(default)]
    pub volume_count: u64,
    /// Human label of the current volume, e.g. "C: Windows".
    #[serde(default)]
    pub volume: String,
    /// File name of the image this run is writing (e.g.
    /// `MINWINPC_F_full_20260807-190504.vhdx`), so the progress line can name
    /// the file next to the destination folder. Empty in old events.
    #[serde(default)]
    pub image_file_name: String,
    /// Разбивка оставшихся файлов по типам — приходит ОДИН раз, сразу после
    /// скана, и дальше не меняется. Сканер видит каждый файл с размером ещё до
    /// копирования, так что показывать её только в конце незачем: человек
    /// смотрит на полосу двадцать минут, и это ответ на вопрос «что в копии».
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<(String, u64)>,
    /// What this run's exclusion rules took out, sent once with `categories`.
    /// `None` until known — the screen then says «…», not a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_bytes: Option<u64>,
}

/// Sink for orchestration events. The GUI forwards these to Tauri window
/// events; the service will push them onto gRPC progress streams.
pub trait EventSink: Send + Sync + 'static {
    fn job_progress(&self, progress: JobProgress);
    fn queue_changed(&self, snapshot: QueueSnapshot);
    fn job_finished(&self, run: JobRun);
}

/// Supplies passphrases for encrypted source volumes at backup time. The
/// service backs this with an in-memory cache and the OS-protected store; a
/// `None` means no password is available (an encrypted volume then cannot be
/// decrypted, so it is backed up as raw sectors or the run is skipped).
pub trait PasswordResolver: Send + Sync + 'static {
    fn resolve(&self, volume_token: &str) -> Option<zeroize::Zeroizing<String>>;
}

/// A [`PasswordResolver`] that never has a password — used where encryption is
/// not wired (tests, the non-Windows stub).
pub struct NoPasswords;
impl PasswordResolver for NoPasswords {
    fn resolve(&self, _volume_token: &str) -> Option<zeroize::Zeroizing<String>> {
        None
    }
}

/// Password store for encrypted volumes, shared by the service (scheduled runs)
/// and the GUI worker. Passwords are cached in memory for the process lifetime;
/// those the user chose to remember are additionally persisted OS-protected
/// (DPAPI) in the metadata DB so scheduled backups can unlock the volume with no
/// one present.
pub struct PasswordStore {
    cache: std::sync::Mutex<std::collections::HashMap<String, zeroize::Zeroizing<String>>>,
    db_path: PathBuf,
    /// Key-file path for the OS secret store (used only off Windows).
    key_path: PathBuf,
}

impl PasswordStore {
    pub fn new(db_path: PathBuf) -> Self {
        let key_path = db_path
            .parent()
            .map(|d| d.join("secret.key"))
            .unwrap_or_else(|| PathBuf::from("secret.key"));
        Self {
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            db_path,
            key_path,
        }
    }

    /// Record a password for `volume_token` (as computed by [`source_token`]).
    /// Always cached in memory; when `remember` is set it is also persisted
    /// OS-protected. `remember` on a platform without a secret store errors.
    pub fn remember(
        &self,
        volume_token: &str,
        password: String,
        remember: bool,
    ) -> own_backup_common::Result<()> {
        if remember {
            let blob = own_backup_backup::secrets::default_store(&self.key_path)
                .protect(password.as_bytes())?;
            let conn = own_backup_metadata::open_database(&self.db_path)?;
            own_backup_metadata::upsert_volume_secret(&conn, volume_token, &blob)?;
        }
        self.cache
            .lock()
            .unwrap()
            .insert(volume_token.to_string(), zeroize::Zeroizing::new(password));
        Ok(())
    }

    /// Forget a password (in-memory and persisted).
    pub fn forget(&self, volume_token: &str) -> own_backup_common::Result<()> {
        self.cache.lock().unwrap().remove(volume_token);
        let conn = own_backup_metadata::open_database(&self.db_path)?;
        own_backup_metadata::delete_volume_secret(&conn, volume_token)?;
        Ok(())
    }

    /// Whether a password is available (cached or persisted) for `volume_token`.
    /// Used by the scheduler to decide if an encrypted job can run unattended.
    pub fn has(&self, volume_token: &str) -> bool {
        self.resolve(volume_token).is_some()
    }
}

impl PasswordResolver for PasswordStore {
    fn resolve(&self, volume_token: &str) -> Option<zeroize::Zeroizing<String>> {
        if let Some(p) = self.cache.lock().unwrap().get(volume_token) {
            return Some(p.clone());
        }
        // Fall back to the OS-protected store (a remembered password).
        let conn = own_backup_metadata::open_database(&self.db_path).ok()?;
        let blob = own_backup_metadata::get_volume_secret(&conn, volume_token).ok()??;
        let plain = own_backup_backup::secrets::default_store(&self.key_path)
            .unprotect(&blob)
            .ok()?;
        Some(zeroize::Zeroizing::new(String::from_utf8(plain.to_vec()).ok()?))
    }
}

/// Holds a copy to `speed_limit_mbps` by sleeping in its progress callback.
///
/// The engine calls that callback on the copying thread after every block it
/// reads (1 MiB), so a sleep there is a pause of the copy itself — no second
/// thread, nothing inside the engine. The pace is measured from the first
/// block of each copy: a volume of a multi-volume job starts its own.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct Pace {
    bytes_per_sec: f64,
    block_bytes: f64,
    started: Option<std::time::Instant>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl Pace {
    /// The engine's block: every copy path reads 1 MiB at a time.
    const BLOCK_BYTES: u64 = 1024 * 1024;

    fn new(speed_limit_mbps: u32) -> Self {
        Self::with_block(speed_limit_mbps, Self::BLOCK_BYTES)
    }

    fn with_block(speed_limit_mbps: u32, block_bytes: u64) -> Self {
        Self {
            bytes_per_sec: speed_limit_mbps as f64 * 1_000_000.0,
            block_bytes: block_bytes as f64,
            started: None,
        }
    }

    /// `blocks_done` blocks have been read; wait until that many were allowed.
    fn after(&mut self, blocks_done: u64) {
        if self.bytes_per_sec <= 0.0 {
            return;
        }
        let started = *self.started.get_or_insert_with(std::time::Instant::now);
        let bytes = blocks_done as f64 * self.block_bytes;
        let allowed_at = std::time::Duration::from_secs_f64(bytes / self.bytes_per_sec);
        let elapsed = started.elapsed();
        if allowed_at > elapsed {
            std::thread::sleep(allowed_at - elapsed);
        }
    }
}

/// The copying thread at background priority for the life of the copy, when
/// the job asks for it (`AdvancedConfig::low_priority`).
#[cfg(target_os = "windows")]
struct QuietCopy(#[allow(dead_code)] Option<own_backup_backup::power_win::BackgroundIo>);

#[cfg(target_os = "windows")]
impl QuietCopy {
    fn enter(low_priority: bool) -> Self {
        Self(low_priority.then(own_backup_backup::power_win::BackgroundIo::enter).flatten())
    }
}

/// Queue a failed run again after `delay` — unless something has run since
/// (a manual copy, a scheduled one), in which case the failure is no longer
/// the job's last word and there is nothing to retry.
fn schedule_retry(
    retry_tx: tokio::sync::mpsc::WeakUnboundedSender<String>,
    queue: RunQueue,
    store: Arc<JobStore>,
    failed: JobRun,
    delay: std::time::Duration,
    attempt: u32,
    of: u32,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let still_last = store
            .get(&failed.job_id)
            .and_then(|j| j.last_run)
            .is_some_and(|last| last.id == failed.id);
        if !still_last {
            info!(job = %failed.job_name, "retry dropped: the job has run since it failed");
            return;
        }
        let Some(tx) = retry_tx.upgrade() else { return };
        if queue.enqueue(&failed.job_id) {
            info!(job = %failed.job_name, attempt, of, "retrying a failed backup");
            if tx.send(failed.job_id.clone()).is_err() {
                queue.cancel_pending(&failed.job_id);
            }
        }
    });
}

/// Seconds the machine is given before it sleeps after a copy — long enough
/// for the result to be seen, and for a next job to be queued (which cancels
/// it).
const SLEEP_AFTER_SECS: u64 = 60;
/// Grace Windows' own shutdown notice counts down; `shutdown /a` cancels it.
const SHUTDOWN_GRACE_SECS: u32 = 90;

/// Carry out `AdvancedConfig::after_backup` once the queue is idle.
fn after_backup(action: &str, job_name: &str, queue: &RunQueue) {
    if action != "sleep" && action != "shutdown" {
        return;
    }
    if own_backup_common::media_mode::is_media_mode() {
        info!(job = %job_name, action, "after backup: nothing to do on the boot medium");
        return;
    }
    let idle = |q: &RunQueue| {
        let snap = q.snapshot();
        snap.current.is_none() && snap.pending.is_empty()
    };
    if !idle(queue) {
        info!(job = %job_name, action, "after backup: other copies are queued — skipped");
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let (action, job_name, queue) = (action.to_string(), job_name.to_string(), queue.clone());
        tokio::spawn(async move {
            let result = if action == "shutdown" {
                own_backup_backup::power_win::shutdown_in(
                    SHUTDOWN_GRACE_SECS,
                    &tr!("lib.shutdown_message", job = job_name),
                )
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(SLEEP_AFTER_SECS)).await;
                if !idle(&queue) {
                    info!(job = %job_name, "after backup: a copy was queued meanwhile — not sleeping");
                    return;
                }
                own_backup_backup::power_win::sleep_now()
            };
            if let Err(e) = result {
                warn!(job = %job_name, action = %action, error = %e, "after backup: failed");
            }
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (SLEEP_AFTER_SECS, SHUTDOWN_GRACE_SECS);
        info!(job = %job_name, action, "after backup: not implemented on this platform");
    }
}

/// Background worker: pulls job ids off the channel and runs them one at a
/// time. Spawned once at startup.
///
/// `retry_tx` is a WEAK sender into its own channel: a failed run is queued
/// again through it, and a weak one does not keep the channel open — the
/// worker still ends when everything else that feeds it has gone.
pub async fn run_queue_worker(
    sink: Arc<dyn EventSink>,
    store: Arc<JobStore>,
    queue: RunQueue,
    busy: Arc<crate::busy::Busy>,
    cancel: Arc<AtomicBool>,
    passwords: Arc<dyn PasswordResolver>,
    mut rx: UnboundedReceiver<String>,
    retry_tx: tokio::sync::mpsc::WeakUnboundedSender<String>,
) {
    // Retries used by each job's current run of failures. In memory: a
    // service restart forgets them, and the schedule carries on as usual.
    let mut retries_used: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    while let Some(job_id) = rx.recv().await {
        // Waiting -> running in one step. Cancelled while it waited, it is no
        // longer in the queue and must not start.
        if !queue.start(&job_id) {
            continue;
        }
        let job = match store.get(&job_id) {
            Some(j) => j,
            None => {
                queue.finish();
                continue;
            }
        };

        sink.queue_changed(queue.snapshot());
        info!(job = %job.name, "Starting queued backup");

        // A copy is a destructive operation on the destination folder: it
        // writes images, merges them and runs retention over them. It takes
        // the machine's one slot like everything else, and WAITS for it
        // rather than failing — a scheduled copy that happens to collide
        // with a restore should run afterwards, not be dropped (audit
        // AUD-02). Released by Drop at the end of the iteration, including
        // if the engine panics.
        let _slot = busy
            .acquire(crate::busy::Doing::new(tr!("lib.doing_backup"), job.name.clone()))
            .await;

        cancel.store(false, Ordering::SeqCst);
        // No sleep while copying — laptops included, verification included.
        // Released when this iteration ends, whatever the outcome.
        #[cfg(target_os = "windows")]
        let _awake = own_backup_backup::power_win::KeepAwake::new(&tr!(
            "lib.keep_awake_reason",
            job = job.name
        ));
        let last_success = store.last_success(&job.id);
        // The keys of an encrypted job are opened here, once, before anything
        // runs — and if they cannot be opened the run fails saying so, rather
        // than waiting for a password nobody will type at night or writing the
        // copy in the clear (§7.2).
        let run = match store.archive_keys_for_run(&job) {
            Err(message) => {
                let mut run = run_skeleton(&job);
                run.finished_at = chrono::Utc::now().to_rfc3339();
                run.status = "error".into();
                run.error = message;
                run
            }
            Ok(keys) => perform_backup(&sink, &job, &cancel, last_success, &passwords, keys).await,
        };
        // How the run ended goes into the log as well as into its record. A
        // failure used to be written only to the record: on 2026-09-25 the log
        // said "Starting queued backup" and then nothing at all, and the reason
        // existed only as a line on the screen.
        match run.status.as_str() {
            "error" => error!(job = %job.name, error = %run.error, "backup run failed"),
            "cancelled" => info!(job = %job.name, reason = %run.error, "backup run cancelled"),
            _ => {}
        }
        if let Err(e) = store.record_run(run.clone()) {
            // The backup happened; only the record of it did not. Saying so is
            // the whole point — the next run plans its chain from this history.
            error!(error = %e, job = %job.name, "the finished run could not be written down");
        }

        queue.finish();
        sink.queue_changed(queue.snapshot());

        // A failure is tried again, a set number of times; anything else ends
        // the streak.
        let retrying = if run.status == "error" {
            let used = retries_used.entry(job.id.clone()).or_insert(0);
            if *used < job.advanced.retry_attempts {
                *used += 1;
                let minutes = job.advanced.retry_interval_min.max(1);
                info!(job = %job.name, attempt = *used, of = job.advanced.retry_attempts,
                    minutes, "the failed backup will be retried");
                schedule_retry(
                    retry_tx.clone(),
                    queue.clone(),
                    store.clone(),
                    run.clone(),
                    std::time::Duration::from_secs(u64::from(minutes) * 60),
                    *used,
                    job.advanced.retry_attempts,
                );
                true
            } else {
                retries_used.remove(&job.id);
                false
            }
        } else {
            retries_used.remove(&job.id);
            false
        };
        // Sleep / shut down — not while a retry is pending, and never after a
        // run someone cancelled: they are at the machine.
        if !retrying && run.status != "cancelled" {
            after_backup(&job.advanced.after_backup, &job.name, &queue);
        }
        sink.job_finished(run);
    }
}

// ---------------------------------------------------------------------------
// Chain planning
// ---------------------------------------------------------------------------

/// What the next run of a job should produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// full | incremental | differential
    pub kind: &'static str,
    /// Parent image for a child run (previous version for incremental, the
    /// chain's full for differential).
    pub parent_image: Option<PathBuf>,
    /// The chain's full image a child run belongs to.
    pub base_image: Option<PathBuf>,
    /// Index of this run within its chain (0 = full).
    pub increment_index: u32,
}

impl RunPlan {
    fn full() -> Self {
        Self {
            kind: "full",
            parent_image: None,
            base_image: None,
            increment_index: 0,
        }
    }
}

/// Decide whether the next run is a full backup or a child of an existing
/// chain, based on the job scheme and the last SUCCESSFUL run (failed or
/// cancelled runs in between do not reset the chain). Any doubt (no history,
/// files missing, rollover reached) resolves to a full backup.
pub fn plan_run(job: &Job, last_success: Option<&JobRun>) -> RunPlan {
    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };
    let Some(last) = last_success.filter(|r| r.status == "success") else {
        return RunPlan::full();
    };
    // Runs recorded before phase 5 have an empty kind — they were all full.
    let last_kind = if last.backup_kind.is_empty() { "full" } else { &last.backup_kind };

    // "Полная версия после каждых N версий": the chain holds at most N children.
    if job.scheme.rolls_over_after().is_some_and(|n| last.increment_index >= n) {
        return RunPlan::full();
    }

    let base = if last_kind == "full" {
        PathBuf::from(&last.image_path)
    } else if !last.base_image_path.is_empty() {
        PathBuf::from(&last.base_image_path)
    } else {
        return RunPlan::full();
    };
    let parent = match method {
        "incremental" => PathBuf::from(&last.image_path),
        _ => base.clone(),
    };

    // The parent must still exist with readable sidecar metadata — embedded
    // (inside the image) or the adjacent .obkm; otherwise start fresh.
    if !parent.exists() || !own_backup_backup::image_sidecar_readable(&parent) {
        return RunPlan::full();
    }

    RunPlan {
        kind: method,
        parent_image: Some(parent),
        base_image: Some(base),
        increment_index: last.increment_index + 1,
    }
}

/// Uppercase drive letter of a source device string ("C:" / `\\.\C:` → 'C').
/// A short, filename-safe, underscore-free token identifying a source volume
/// within a job's images/chains. A lettered volume is its letter ("C:" → "C").
/// A letterless volume addressed by a GUID path (`\\?\Volume{1a2b…}`) becomes
/// `V` + the first 8 hex digits of the GUID — unique per volume, so several
/// letterless partitions (e.g. two EFI parts) never collide. Falls back to "X".
/// Stable key for a source volume, used both for chain planning and to look up
/// a saved password. The GUI/service compute the same token to store passwords.
pub fn source_token(device: &str) -> String {
    let d = device.trim();
    // Windowed partition source `part:<disk>:<offset>:<len>` → P<disk> + offset
    // in hex — unique per partition (offsets differ), so several letterless
    // system partitions never collide.
    if let Some(rest) = d.strip_prefix("part:") {
        let mut it = rest.split(':');
        if let (Some(disk), Some(off)) = (it.next(), it.next()) {
            if let (Ok(disk), Ok(off)) = (disk.parse::<u32>(), off.parse::<u64>()) {
                return format!("P{disk}{off:X}");
            }
        }
    }
    // Volume GUID path → V + first 8 hex of the GUID.
    if let Some(rest) = d.strip_prefix(r"\\?\Volume{").or_else(|| d.strip_prefix(r"\\.\Volume{")) {
        let hex: String = rest
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(8)
            .collect::<String>()
            .to_ascii_uppercase();
        if !hex.is_empty() {
            return format!("V{hex}");
        }
    }
    // Lettered volume ("C:", "\\.\C:") → the letter.
    d.trim_start_matches(r"\\.\")
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "X".to_string())
}

/// Filename identifier for a source volume. Same as [`source_token`] for
/// lettered / GUID volumes; a letterless (encrypted) `part:` source uses its
/// sanitized volume label when the job knows one — so its image is named
/// `Job_kubuntu_full_…` like a lettered `Job_C_full_…`, not `Job_P4AB…`. The
/// result is underscore-free (the token is delimited by `_` in the file name).
/// Distinct from [`source_token`], which stays the stable device key used for
/// passwords and cross-run identity.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn file_token(job: &Job, device: &str) -> String {
    if device.trim_start().starts_with("part:") {
        let label = job.label_for(device);
        if label != device {
            let safe: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
            if !safe.is_empty() {
                return safe;
            }
        }
    }
    source_token(device)
}

/// Is this chain a chain of THIS volume's copies?
///
/// The file-name token has to match, as it always did — but a token is a drive
/// letter, or for an encrypted `part:` source with no letter, its cleaned-up
/// label. Two volumes sharing either are one volume as far as that goes, and
/// an increment written onto a copy of a different disk is not a smaller
/// mistake than a lost one (audit AUD-10, point 3).
///
/// So when BOTH sides recorded who they are, that decides. When either does
/// not — an old copy, or a machine that cannot answer — the name is all there
/// is and the behaviour is exactly what it was. That is the compatible half:
/// this never starts mixing sources, it only stops.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn chain_belongs_to(
    base: &own_backup_backup::chains::ChainImage,
    token: &str,
    identity: &Option<String>,
) -> bool {
    if !image_token(&base.image_path).is_some_and(|t| t.eq_ignore_ascii_case(token)) {
        return false;
    }
    match (identity, &base.source_id) {
        (Some(mine), Some(theirs)) => mine.eq_ignore_ascii_case(theirs),
        _ => true,
    }
}

/// Recover the source token that an image file name encodes. Names are
/// `<name>_<token>_<tag>_<stamp>.<ext>`; token/tag/stamp are always the last
/// three underscore-separated segments, so we read from the end — robust even
/// when `<name>` itself contains underscores.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn image_token(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut it = stem.rsplit('_');
    let _stamp = it.next()?;
    let _tag = it.next()?;
    // Both schemes put the token at the 3rd-from-last segment: per-volume
    // `<name>_<token>_<tag>_<stamp>` (token = "C"/"V1A2…"), whole-disk
    // `<name>_disk<N>_<tag>_<stamp>` (token = "disk<N>"). Reading from the end
    // survives underscores inside `<name>`.
    let token = it.next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_ascii_uppercase())
    }
}

/// Timestamped output file name for one volume of a run:
/// `<name>_<token>_<full|inc|diff>_<stamp>.<vhdx|avhdx>`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn output_path_for(job: &Job, device: &str, kind: &str) -> PathBuf {
    let token = file_token(job, device);
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let safe_name = own_backup_common::file_name::safe_file_stem(&job.name, "backup");
    // VHDX children are AVHDX (the Windows convention); qcow2 children stay
    // .qcow2 (the qemu convention — chain role lives in the sidecar).
    // An encrypted job writes `.obk` throughout, children included: that name
    // is how listings and retention find encrypted images at all.
    let child_ext = match (job.archive_encryption.is_some(), job.image_format) {
        (true, _) => own_backup_backup::seal::obk::OBK_EXTENSION,
        (false, own_backup_common::types::ImageFormat::Vhdx) => "avhdx",
        (false, own_backup_common::types::ImageFormat::Qcow2) => "qcow2",
    };
    let full_ext = container_extension(job);
    let (tag, ext) = match kind {
        "incremental" => ("inc", child_ext),
        "differential" => ("diff", child_ext),
        _ => ("full", full_ext),
    };
    PathBuf::from(&job.dest_folder).join(format!("{safe_name}_{token}_{tag}_{stamp}.{ext}"))
}

/// The container a job writes: its format, or `.obk` when it encrypts.
pub fn container_extension(job: &Job) -> &'static str {
    if job.archive_encryption.is_some() {
        own_backup_backup::seal::obk::OBK_EXTENSION
    } else {
        job.image_format.extension()
    }
}

/// A job's image format must never change over existing archives (decision
/// 2026-07-08): before every run the destination folder's sidecars are checked
/// against the job's format, and any mismatch fails the run with a clear
/// message. A different format means a new job (or removing the old backups).
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn format_change_error(job: &Job, folder: &Path) -> Option<String> {
    let chains =
        own_backup_backup::chains::list_job_chains(folder, &job.id).unwrap_or_default();
    let want = container_extension(job);
    for image in chains.iter().flat_map(|c| c.images.iter()) {
        if image.container != want {
            let name = image
                .image_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some(
                tr!("lib.format_mismatch", want = want, found = image.container, name = name)
                    .into_owned(),
            );
        }
    }
    None
}

/// Decide the next run's plan for one specific volume, reading the on-disk
/// chains for that volume's drive letter (the sidecars are the source of
/// truth). This makes planning correct for multi-volume jobs — each volume has
/// its own independent chain — without relying on a single run-history record.
/// Is it time to abandon this chain and take a fresh full backup?
///
/// The two schemes answer differently, and neither answer is the other's:
///
/// - **Standard** counts versions: after N increments the chain is closed and
///   a new one begins, so old chains age out whole. Counting uses
///   `Chain::increments_ever`, not the files present — consolidation could
///   have deleted some, and a count that ignored that would never reach N.
/// - **Consolidation** keeps ONE chain and bounds it by folding, so no count
///   ever ends it. What ends it is AGE: a base that has been rewritten every
///   night for months is a single copy of the data with no independent second
///   one, and one bad block in it takes every restore point at once. The new
///   chain starts first and the old one is deleted only once the new full has
///   succeeded (see `apply_retention`), so there is never a moment with no
///   backup at all.
fn should_start_new_chain(
    job: &Job,
    chain: &own_backup_backup::chains::Chain,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if job.scheme.is_consolidation() {
        return match job.scheme.cleanup_older_than_days {
            Some(days) => chain.age_days(now) >= days as i64,
            None => false,
        };
    }
    job.scheme.rolls_over_after().is_some_and(|n| chain.increments_ever() >= n)
}

/// Any doubt (no chain, rollover reached, parent missing) resolves to a full.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
pub fn plan_run_for_volume(job: &Job, folder: &Path, device: &str) -> RunPlan {
    plan_run_for_volume_at(job, folder, device, chrono::Utc::now())
}

/// [`plan_run_for_volume`] with the clock passed in, so the age-based rollover
/// can be tested without waiting three months.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn plan_run_for_volume_at(
    job: &Job,
    folder: &Path,
    device: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> RunPlan {
    use own_backup_backup::chains::list_job_chains;

    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };

    let token = file_token(job, device);
    // Who this volume is, independently of what it is called. `None` on a
    // machine that cannot say, and on every non-Windows build.
    let identity = own_backup_backup::source_identity::stable_source_id(device);
    let chains = match list_job_chains(folder, &job.id) {
        Ok(c) => c,
        Err(_) => return RunPlan::full(),
    };
    // Chains are oldest-first; keep only this volume's, then take the newest.
    // Case-insensitive: the token read back from a file name is upper-cased.
    let Some(chain) = chains
        .into_iter()
        .filter(|c| {
            c.images.first().is_some_and(|base| chain_belongs_to(base, &token, &identity))
        })
        .next_back()
    else {
        return RunPlan::full();
    };

    let base = match chain.images.first() {
        Some(img) => img.image_path.clone(),
        None => return RunPlan::full(),
    };
    // Increments EVER held, not files present: consolidation deletes the
    // oldest increment after folding it into the base, so counting files would
    // both stop the rollover firing and make the version number go backwards.
    let child_count = chain.increments_ever();

    if should_start_new_chain(job, &chain, now) {
        return RunPlan::full();
    }

    // Incremental parents the previous (newest) image; differential parents the
    // chain's full/root.
    let parent = match method {
        "incremental" => chain
            .images
            .last()
            .map(|i| i.image_path.clone())
            .unwrap_or_else(|| base.clone()),
        _ => base.clone(),
    };

    // The parent must still exist with readable sidecar metadata — embedded
    // (inside the image) or the adjacent .obkm; otherwise start fresh.
    if !parent.exists() || !own_backup_backup::image_sidecar_readable(&parent) {
        return RunPlan::full();
    }

    RunPlan {
        kind: method,
        parent_image: Some(parent),
        base_image: Some(base),
        increment_index: child_count + 1,
    }
}

/// Decide the next run's plan for a whole-disk (bootable) backup of `disk_number`
/// (plan D5). Mirrors [`plan_run_for_volume`] but keys on the `disk<N>` file tag
/// instead of a volume token: the newest chain of whole-disk images for this disk
/// determines full vs incremental/differential. Any doubt (no chain, rollover,
/// parent missing) resolves to a full — a whole-disk full is always self-contained.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
pub fn plan_run_for_disk(job: &Job, folder: &Path, disk_number: u32) -> RunPlan {
    plan_run_for_disk_at(job, folder, disk_number, chrono::Utc::now())
}

/// [`plan_run_for_disk`] with the clock passed in (see the volume twin).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn plan_run_for_disk_at(
    job: &Job,
    folder: &Path,
    disk_number: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> RunPlan {
    use own_backup_backup::chains::list_job_chains;

    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };

    let tag = format!("disk{disk_number}");
    let chains = match list_job_chains(folder, &job.id) {
        Ok(c) => c,
        Err(_) => return RunPlan::full(),
    };
    // Keep only this disk's chains (base image tagged `disk<N>`), newest last.
    let Some(chain) = chains
        .into_iter()
        .filter(|c| {
            c.images
                .first()
                .and_then(|i| image_token(&i.image_path))
                .is_some_and(|t| t.eq_ignore_ascii_case(&tag))
        })
        .next_back()
    else {
        return RunPlan::full();
    };

    let base = match chain.images.first() {
        Some(img) => img.image_path.clone(),
        None => return RunPlan::full(),
    };
    // Increments EVER held, not files present: consolidation deletes the
    // oldest increment after folding it into the base, so counting files would
    // both stop the rollover firing and make the version number go backwards.
    let child_count = chain.increments_ever();

    if should_start_new_chain(job, &chain, now) {
        return RunPlan::full();
    }

    let parent = match method {
        "incremental" => chain
            .images
            .last()
            .map(|i| i.image_path.clone())
            .unwrap_or_else(|| base.clone()),
        _ => base.clone(),
    };

    if !parent.exists() || !own_backup_backup::image_sidecar_readable(&parent) {
        return RunPlan::full();
    }

    RunPlan {
        kind: method,
        parent_image: Some(parent),
        base_image: Some(base),
        increment_index: child_count + 1,
    }
}

/// Apply the job's retention rules after a fully successful run, independently
/// per source volume (identified by its image token). "Keep N chains" means N
/// chains *for each volume* — a two-volume job never lets one volume's backups
/// evict the other's. Failures are logged, never propagated. `protected` lists
/// the images just created (one per volume); their chains are always exempt.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn apply_retention(job: &Job, protected: &[(String, PathBuf)]) {
    use own_backup_backup::chains::{delete_chain, list_job_chains, plan_retention, RetentionRules};

    let rules = if job.scheme.is_consolidation() {
        // Consolidation means ONE chain. Its age setting does not delete
        // anything — it makes the planner START a new chain (see
        // `should_start_new_chain`), and this is what then removes the old one.
        //
        // The ordering is the whole safety argument: `apply_retention` runs
        // only after a fully successful run, and never touches the chain
        // holding the image that run just wrote. So the old chain survives
        // until the new full is on disk and complete, and there is no instant
        // in which the data is unprotected.
        RetentionRules {
            keep_chains: Some(1),
            cleanup_older_than_days: None,
            keep_first: false,
        }
    } else {
        RetentionRules {
            keep_chains: job.scheme.keep_chains,
            cleanup_older_than_days: job.scheme.cleanup_older_than_days,
            keep_first: job.scheme.keep_first,
        }
    };
    if rules.keep_chains.is_none() && rules.cleanup_older_than_days.is_none() {
        return;
    }
    let folder = PathBuf::from(&job.dest_folder);
    let all_chains = match list_job_chains(&folder, &job.id) {
        Ok(c) => c,
        Err(e) => {
            warn!(job = %job.name, error = %e, "Retention scan failed");
            return;
        }
    };

    // One retention pass per distinct volume token, over that volume's chains.
    let mut seen = std::collections::HashSet::new();
    for (token, protect_image) in protected {
        if !seen.insert(token.clone()) {
            continue;
        }
        let vol_chains: Vec<_> = all_chains
            .iter()
            .filter(|c| {
                c.images
                    .first()
                    .and_then(|i| image_token(&i.image_path))
                    .is_some_and(|t| t.eq_ignore_ascii_case(token))
            })
            .cloned()
            .collect();
        if vol_chains.is_empty() {
            continue;
        }
        let doomed = plan_retention(&vol_chains, &rules, chrono::Utc::now(), protect_image);
        for idx in doomed {
            let base = vol_chains[idx]
                .images
                .first()
                .map(|i| i.image_path.display().to_string())
                .unwrap_or_default();
            info!(
                job = %job.name,
                volume = %token,
                base = %base,
                images = vol_chains[idx].images.len(),
                "Retention: deleting old backup chain"
            );
            // A chain that could not be deleted whole is worth a line of its
            // own: the files that stayed are still readable backups, and the
            // old code said "deleted backup version" about every one of them
            // (audit AUD-05).
            match delete_chain(&vol_chains[idx]) {
                Ok(report) if report.complete() => {}
                Ok(report) => {
                    for (path, why) in &report.kept {
                        warn!(job = %job.name, image = %path.display(), reason = %why,
                            "Retention: this version is still on disk");
                    }
                    for path in &report.orphan_sidecars {
                        warn!(job = %job.name, sidecar = %path.display(),
                            "Retention: the image is gone but its sidecar stayed");
                    }
                    warn!(job = %job.name, base = %base, deleted = report.files_deleted(),
                        kept = report.kept.len(),
                        "Retention: the old chain was only partly deleted");
                }
                Err(e) => {
                    warn!(job = %job.name, base = %base, error = %e,
                        "Retention: the old chain was left alone");
                }
            }
        }
    }
}

/// What "delete the job's copies" actually did, in the words the user is shown.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct JobFilesDeleted {
    pub deleted: Vec<String>,
    /// Things still on disk, each with the reason.
    pub kept: Vec<(String, String)>,
    /// The folders that were searched for this job's images.
    pub folders: Vec<String>,
    /// True when the activity journal is at its cap, so a destination the job
    /// used long ago may not be in the list above at all. Said out loud rather
    /// than presented as a clean sweep (audit AUD-09).
    pub history_truncated: bool,
}

impl JobFilesDeleted {
    pub fn complete(&self) -> bool {
        self.kept.is_empty() && !self.history_truncated
    }
}

/// Delete every copy a job produced, and report exactly what happened.
///
/// The old version asked the GUI's activity journal — capped at
/// [`ACTIVITY_CAP`] runs, and for a multi-volume job recording only the FIRST
/// volume's image — and then removed each path with `remove_file`, one at a
/// time, in no particular order (audit AUD-09). Older versions and every
/// non-first volume stayed on disk, and a differencing child could lose its
/// base while surviving itself.
///
/// The reliable question is not "what does the journal remember" but "which
/// images say they belong to this job": that is written in each image's own
/// sidecar, it covers every volume and every run, and it is what retention
/// has always used. So the folders are scanned by sidecar and deleted as
/// CHAINS, with the leaf-first ordering and the failure rules from AUD-05.
///
/// What cannot be answered from here is a destination the job used before the
/// journal scrolled past it. Rather than pretend, the report says the history
/// was truncated.
pub fn delete_job_files(store: &JobStore, job: &Job) -> JobFilesDeleted {
    use own_backup_backup::chains::{delete_chain, list_job_chains};

    let mut out = JobFilesDeleted { history_truncated: store.history_is_truncated(&job.id), ..Default::default() };

    // Every folder we have reason to think this job wrote into: the one it is
    // configured for now, plus the parents of everything its history names.
    let from_history = store.job_image_files(&job.id);
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut push_folder = |p: PathBuf, folders: &mut Vec<PathBuf>| {
        if !p.as_os_str().is_empty() && !folders.contains(&p) {
            folders.push(p);
        }
    };
    push_folder(PathBuf::from(&job.dest_folder), &mut folders);
    for f in &from_history {
        if let Some(parent) = f.parent() {
            push_folder(parent.to_path_buf(), &mut folders);
        }
    }
    out.folders = folders.iter().map(|f| f.display().to_string()).collect();

    for folder in &folders {
        let chains = match list_job_chains(folder, &job.id) {
            Ok(c) => c,
            Err(e) => {
                out.kept.push((folder.display().to_string(), e.to_string()));
                continue;
            }
        };
        for chain in &chains {
            match delete_chain(chain) {
                Ok(report) => {
                    out.deleted.extend(report.deleted.iter().map(|p| p.display().to_string()));
                    out.kept.extend(
                        report.kept.iter().map(|(p, why)| (p.display().to_string(), why.clone())),
                    );
                    out.kept.extend(report.orphan_sidecars.iter().map(|p| {
                        (p.display().to_string(), tr!("lib.orphan_sidecar").into_owned())
                    }));
                }
                Err(e) => {
                    let base = chain
                        .images
                        .first()
                        .map(|i| i.image_path.display().to_string())
                        .unwrap_or_default();
                    out.kept.push((base, e.to_string()));
                }
            }
        }
    }

    // Anything the history names that the chain scan did not reach: an image
    // whose sidecar has become unreadable, or one left in a folder that holds
    // nothing else of ours. Still precise — these are paths this job wrote.
    for path in from_history {
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                out.deleted.push(path.display().to_string());
                let sidecar = own_backup_backup::sidecar_path_for(&path);
                if std::fs::remove_file(&sidecar).is_ok() {
                    out.deleted.push(sidecar.display().to_string());
                }
            }
            Err(e) => out.kept.push((path.display().to_string(), e.to_string())),
        }
    }

    out
}

/// Finish whatever a crash left half-done in the destination folder — before
/// anything reads or plans against the chains in it.
///
/// **Where this runs is the whole of the fix** (audit AUD-06). It used to live
/// inside [`apply_consolidation`], which is the last thing a run does: after
/// the format check, after a parent had been chosen, after the new copy had
/// been written, and after retention had deleted whole chains. Every one of
/// those decisions was therefore taken against a chain that was in the middle
/// of a fold — a base already rewritten with an increment's data while the
/// increment still sat beside it, unrepointed.
///
/// And it sat behind the consolidation switch, so turning consolidation off
/// turned RECOVERY off with it, leaving a half-folded chain that nothing would
/// ever finish. Recovery is not a new consolidation: it is the completion of
/// one that was already started, and no setting cancels it.
///
/// Returns the message to fail the run with when a chain of THIS job could not
/// be brought to a known state. That is deliberate: an unreadable or unfinished
/// marker means the chain's parent is possibly mid-rewrite, and writing an
/// increment onto it — or deleting it by retention — is how a broken chain
/// becomes a lost one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn recover_interrupted_consolidations(
    job: &Job,
    folder: &Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> Option<String> {
    use own_backup_backup::consolidate::resume_pending_with_keys;

    let report = match resume_pending_with_keys(folder, keys) {
        Ok(r) => r,
        Err(e) => {
            warn!(job = %job.name, error = %e, "Could not scan for interrupted consolidations");
            return None; // the folder could not be read; the run's own checks will say so
        }
    };
    if !report.finished.is_empty() {
        info!(job = %job.name, steps = report.finished.len(),
            "Finished consolidations interrupted earlier");
    }
    if report.all_finished() {
        return None;
    }

    // Only a chain this job would actually touch stops this job. A stuck
    // marker belonging to another job in a shared folder is that job's
    // problem, and it is reported there.
    let ours: Vec<String> = report
        .stuck_bases()
        .iter()
        .filter(|base| belongs_to_job(base, &job.id))
        .map(|base| base.display().to_string())
        .collect();
    for (marker, why) in &report.unfinished {
        warn!(job = %job.name, marker = %marker.display(), reason = %why,
            "An interrupted consolidation could not be finished");
    }
    if ours.is_empty() {
        return None;
    }
    let bases = ours.join(", ");
    Some(tr!("lib.consolidation_unfinished", bases = bases).into_owned())
}

/// Does this image belong to the given job? Asked of the image's own sidecar,
/// which is the source of truth for ownership (the job history is not).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn belongs_to_job(image: &Path, job_id: &str) -> bool {
    let ext = image
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    own_backup_backup::sidecar::read_embedded_sidecar_header(image, &ext)
        .or_else(|_| {
            own_backup_backup::sidecar::read_sidecar_header(
                &own_backup_backup::sidecar::sidecar_path_for(image),
            )
        })
        .map(|(info, _)| info.job_id.as_deref() == Some(job_id))
        // A base we cannot even read the ownership of is treated as ours: the
        // cautious answer is the one that stops the run.
        .unwrap_or(true)
}

/// Fold overflowed increments into their base after a successful run.
///
/// Runs AFTER [`apply_retention`] on purpose: retention deletes whole chains,
/// and folding an increment into a base that is about to be deleted anyway is
/// pure wear. Failures are logged and never fail the run — the backup itself
/// already succeeded, and a chain that did not get consolidated is a chain
/// that is merely longer than asked for, not a broken one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // used by the Windows backup path
fn apply_consolidation(
    job: &Job,
    protected: &[(String, PathBuf)],
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> u32 {
    use own_backup_backup::chains::{list_job_chains, plan_consolidation};
    use own_backup_backup::consolidate::consolidate_step_with_keys;

    let Some(keep) = job.scheme.rolling_incrementals else {
        return 0;
    };
    // Differential chains hang every child off the base, so moving the base
    // forward would silently rebase the rest. `plan_consolidation` refuses
    // them too; bailing here keeps the log quiet about a thing we never do.
    if job.scheme.method != "incremental" {
        return 0;
    }
    let folder = PathBuf::from(&job.dest_folder);
    let tokens: std::collections::HashSet<String> =
        protected.iter().map(|(t, _)| t.to_ascii_uppercase()).collect();

    // One step at a time, re-reading the folder after each: a step changes the
    // very chain the next decision is made from. The cap is a backstop against
    // a step that reports success without shortening anything — a loop that
    // never ends would be a far worse bug than a chain left one version long.
    const MAX_STEPS: usize = 64;
    let mut folded = 0u32;
    for _ in 0..MAX_STEPS {
        let chains = match list_job_chains(&folder, &job.id) {
            Ok(c) => c,
            Err(e) => {
                warn!(job = %job.name, error = %e, "Consolidation scan failed");
                return folded;
            }
        };
        let step = chains
            .iter()
            .filter(|c| {
                c.images
                    .first()
                    .and_then(|i| image_token(&i.image_path))
                    .is_some_and(|t| tokens.contains(&t.to_ascii_uppercase()))
            })
            .find_map(|c| plan_consolidation(c, keep));
        let Some(step) = step else { return folded };

        match consolidate_step_with_keys(&step, keys) {
            Ok(outcome) => {
                folded += 1;
                info!(
                    job = %job.name,
                    base = %outcome.base.display(),
                    folded = %outcome.folded.display(),
                    secs = outcome.duration.as_secs(),
                    "Consolidation: increment folded into the base"
                );
            }
            Err(e) => {
                warn!(
                    job = %job.name,
                    base = %step.base.image_path.display(),
                    increment = %step.victim.image_path.display(),
                    error = %e,
                    "Consolidation step failed — the chain is left as it was"
                );
                return folded;
            }
        }
    }
    warn!(job = %job.name, "Consolidation stopped at the step cap — chains stay longer than asked");
    folded
}

fn run_skeleton(job: &Job) -> JobRun {
    JobRun {
        id: uuid::Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        job_name: job.name.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

/// Sum two optional category breakdowns field-by-field (multi-volume runs
/// combine each volume's walk into one dashboard total).
#[cfg(target_os = "windows")]
fn merge_categories(
    acc: Option<own_backup_backup::exclusions::CategorySizes>,
    add: Option<own_backup_backup::exclusions::CategorySizes>,
) -> Option<own_backup_backup::exclusions::CategorySizes> {
    use own_backup_backup::exclusions::CategorySizes;
    match (acc, add) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(CategorySizes {
            images: a.images + b.images,
            video: a.video + b.video,
            audio: a.audio + b.audio,
            documents: a.documents + b.documents,
            windows: a.windows + b.windows,
            linux: a.linux + b.linux,
            other: a.other + b.other,
        }),
    }
}

#[cfg(target_os = "windows")]
async fn perform_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    passwords: &Arc<dyn PasswordResolver>,
    keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
) -> JobRun {
    use own_backup_backup::engine_win::{
        backup_volume, verify_image_scoped, VerifyScope, VolumeBackupRequest,
    };
    use own_backup_common::types::BackupType;
    use own_backup_common::OwnBackupError;

    let mut run = run_skeleton(job);

    // A job may back up several volumes; each produces its own image/chain and
    // is planned independently from the on-disk chains for its drive letter.
    let volumes = job.source_list();
    if volumes.is_empty() {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = tr!("lib.no_source_volume").into_owned();
        return run;
    }
    let folder = PathBuf::from(&job.dest_folder);
    // FIRST, before the format check, before a parent is chosen and long
    // before anything is written: finish what a crash left half-done. Every
    // decision below reads these chains (audit AUD-06).
    if let Some(msg) = recover_interrupted_consolidations(job, &folder, keys.as_ref()) {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = msg;
        return run;
    }
    if let Some(msg) = format_change_error(job, &folder) {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = msg;
        return run;
    }
    let count = volumes.len() as u64;

    // Whole-disk (bootable) copy → one image for the whole disk with the real
    // GPT + embedded manifest (plan D1), instead of a per-volume image each.
    if let Some(ds) = job.disk_source.clone() {
        return perform_disk_backup(sink, job, &ds, cancel, passwords, keys, run).await;
    }

    // Exclusions only make sense in smart NTFS mode; a sector-by-sector image
    // is a verbatim copy by definition.
    let exclusion_patterns = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        job.exclusions.patterns.clone()
    } else {
        Vec::new()
    };

    // Aggregates across all volumes of this run.
    let mut protected: Vec<(String, PathBuf)> = Vec::new();
    // (source device, image path) per success — used to build the whole-disk
    // restore manifest, joining images to GPT partitions by device.
    let mut run_images: Vec<(String, PathBuf)> = Vec::new();
    let mut kinds: Vec<&'static str> = Vec::new();
    let mut total_dur = 0.0f64;
    let mut all_smart = true;
    let mut fatal: Option<(String, String)> = None; // (status, message)

    for (idx, device) in volumes.iter().enumerate() {
        let plan = plan_run_for_volume(job, &folder, device);
        let image_path = output_path_for(job, device, plan.kind);
        let token = source_token(device);
        let vol_label = job.label_for(device);
        let vidx = idx as u64;

        // Physical disk this volume lives on, from the layout snapshot the GUI
        // captured at job save — embedded in the sidecar so a restore shows the
        // whole disk and highlights this partition without jobs.json. `None` on
        // jobs saved before snapshots existed (the engine then omits it).
        let source_disk = job
            .source_disks
            .iter()
            .find(|d| d.partitions.iter().any(|p| p.volume_device == *device))
            .cloned();

        let request = VolumeBackupRequest {
            source: device.clone(),
            image_path: image_path.clone(),
            source_disk,
            block_size: own_backup_backup::engine_win::DEFAULT_BLOCK_SIZE,
            all_sectors: job.advanced.sector_by_sector,
            use_vss: true,
            kind: match plan.kind {
                "incremental" => BackupType::Incremental,
                "differential" => BackupType::Differential,
                _ => BackupType::Full,
            },
            parent_image: plan.parent_image.clone(),
            exclusion_patterns: exclusion_patterns.clone(),
            job_id: Some(job.id.clone()),
            // A saved/cached passphrase for this volume, if any. The engine uses
            // it only when the source is actually a LUKS container.
            passphrase: passwords.resolve(&token),
            image_format: job.image_format,
            // Each volume of this run gets its own context; they all hold the
            // same job key, opened once before the run started.
            encryption: keys.clone(),
            zstd_level: job.zstd_level(),
        };
        let validate_after = job.advanced.validate_after;
        // Only the blocks this copy wrote, or everything the restore point
        // stands on — the job says which (default: everything).
        let verify_scope = if job.advanced.verify_latest_only {
            VerifyScope::LatestCopyOnly
        } else {
            VerifyScope::Everything
        };

        let sink_bg = sink.clone();
        let job_id = job.id.clone();
        let cancel_bg = cancel.clone();
        let low_priority = job.advanced.low_priority;
        let speed_limit_mbps = job.advanced.speed_limit_mbps;
        let image_for_verify = image_path.clone();
        // Checking an encrypted copy needs its key: without one there is no
        // sidecar to read and nothing to compare checksums against.
        let keys_for_verify = keys.clone();
        let vol_label_bg = vol_label.clone();
        // The file this run is writing — named in the progress line, because
        // "WorkDisk-2 → D:\Temp" alone does not say WHAT is appearing there.
        let image_name = image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let result = tokio::task::spawn_blocking(move || {
            let _quiet = QuietCopy::enter(low_priority);
            let mut pace = Pace::new(speed_limit_mbps);
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let summary = backup_volume(&request, |p| {
                if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Err(OwnBackupError::Cancelled);
                }
                // The engine tags non-copy phases via backup_id: "prepare" for
                // the exclusion scan, "finalize" for the flush/detach at the
                // end. Both carry blocks_total == 0.
                let phase = match p.backup_id.as_str() {
                    "prepare" => "prepare",
                    "finalize" => "finalize",
                    // The post-copy exclusion passes (true removal, and the
                    // in-image pass for an encrypted source) — minutes of real
                    // work that used to hide under "сброс кэша" and read as a
                    // hang.
                    "exclusions" => "exclusions",
                    _ => "backup",
                };
                if phase == "backup" {
                    pace.after(p.blocks_done);
                }
                let permille = if p.blocks_total > 0 {
                    p.blocks_done * 1000 / p.blocks_total
                } else {
                    0
                };
                // "finalize" has no granular progress — always emit it so the UI
                // leaves the frozen 100% state; otherwise throttle as usual.
                // Сообщение со сводкой по типам приходит одно и в единственный
                // момент — сразу после скана. Через троттлинг его пропускать
                // нельзя: другого такого не будет.
                let has_categories = p.categories.is_some();
                if has_categories
                    || phase == "finalize"
                    || phase == "exclusions"
                    || permille != last_permille
                    || last_emit.elapsed().as_millis() >= 400
                {
                    last_permille = permille;
                    last_emit = std::time::Instant::now();
                    sink_bg.job_progress(JobProgress {
                        job_id: job_id.clone(),
                        phase: phase.into(),
                        percent: permille / 10,
                        blocks_done: p.blocks_done,
                        blocks_total: p.blocks_total,
                        speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                        volume_index: vidx,
                        volume_count: count,
                        volume: vol_label_bg.clone(),
                        image_file_name: image_name.clone(),
                        categories: p.categories.clone().unwrap_or_default(),
                        excluded_files: p.excluded.map(|(f, _)| f),
                        excluded_bytes: p.excluded.map(|(_, b)| b),
                    });
                }
                Ok(())
            })?;

            // Optional post-backup verification — emit its own progress so the
            // UI shows "Проверка…" instead of freezing at 100%.
            let mut verify_cancelled = false;
            if validate_after {
                let mut v_permille = u64::MAX;
                let mut v_emit = std::time::Instant::now();
                let mut v_rate = VerifyRate::new(&image_for_verify, keys_for_verify.as_ref());
                let verified = verify_image_scoped(
                    &image_for_verify,
                    keys_for_verify.as_ref(),
                    verify_scope,
                    |done, total| {
                    // Cancel reaches the check too, not just the copy (audit
                    // AUD-12). Reading back a whole image is minutes of work;
                    // before this, pressing Отмена during it did nothing and
                    // the run then finished as a success.
                    if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        return Err(OwnBackupError::Cancelled);
                    }
                    let mbps = v_rate.sample(done);
                    let permille = if total > 0 { done * 1000 / total } else { 0 };
                    if permille != v_permille || v_emit.elapsed().as_millis() >= 400 {
                        v_permille = permille;
                        v_emit = std::time::Instant::now();
                        sink_bg.job_progress(JobProgress {
                            job_id: job_id.clone(),
                            phase: "verify".into(),
                            percent: permille / 10,
                            blocks_done: done,
                            blocks_total: total,
                            speed_mbps: mbps,
                            volume_index: vidx,
                            volume_count: count,
                            volume: vol_label_bg.clone(),
                            image_file_name: image_name.clone(),
                        categories: Vec::new(),
                        excluded_files: None,
                        excluded_bytes: None,
                        });
                    }
                    Ok(())
                    },
                );
                match verified {
                    Ok(v) if !v.valid() => {
                        return Err(OwnBackupError::ImageFormat(
                            tr!(
                                "lib.verify_failed_volume",
                                volume = vol_label_bg,
                                count = v.mismatched_blocks.len()
                            )
                            .into_owned(),
                        ))
                    }
                    Ok(_) => {}
                    // The copy itself is finished and on disk; only the reading
                    // back was stopped. That is a different outcome from a
                    // cancelled copy, and the image must survive it.
                    Err(OwnBackupError::Cancelled) => verify_cancelled = true,
                    Err(e) => return Err(e),
                }
            }

            Ok::<_, OwnBackupError>((summary, verify_cancelled))
        })
        .await;

        match result {
            Ok(Ok((summary, verify_cancelled))) => {
                use own_backup_common::types::BackupType as BT;
                run.source_bytes += summary.source_bytes;
                run.bytes_copied += summary.bytes_copied;
                run.used_blocks += summary.used_blocks;
                run.total_blocks += summary.total_blocks;
                run.blocks_unchanged += summary.blocks_unchanged;
                run.excluded_files += summary.excluded_files;
                run.excluded_bytes += summary.excluded_bytes;
                total_dur += summary.duration.as_secs_f64();
                all_smart &= summary.smart_mode;
                run.vss_used |= summary.vss_used;
                run.categories = merge_categories(run.categories.take(), summary.categories);
                // One line per volume that went unwalked; a run over three
                // volumes must not report the reason of whichever came last.
                if let Some(why) = summary.file_scan_skipped {
                    let line = if volumes.len() > 1 { format!("{device}: {why}") } else { why };
                    if !run.file_scan_skipped.is_empty() {
                        run.file_scan_skipped.push_str("; ");
                    }
                    run.file_scan_skipped.push_str(&line);
                }

                let actual_kind = match summary.backup_kind {
                    BT::Full => "full",
                    BT::Incremental => "incremental",
                    _ => "differential",
                };
                kinds.push(actual_kind);

                // Where this volume's copy went, and which chain it joined.
                // Computed for EVERY volume: the two single-value fields below
                // describe the first one only, and for years that was the whole
                // record of a multi-volume run (audit AUD-09).
                let base_of_this_volume = match summary.backup_kind {
                    BT::Full => image_path.display().to_string(),
                    _ => plan
                        .base_image
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                };
                run.images.push(RunImage {
                    source: device.clone(),
                    image_path: image_path.display().to_string(),
                    base_image_path: base_of_this_volume.clone(),
                    kind: actual_kind.into(),
                });

                // The primary (first) volume drives the single-value dashboard
                // fields (image to open, chain base, increment index).
                if idx == 0 {
                    run.image_path = image_path.display().to_string();
                    run.base_image_path = base_of_this_volume;
                    run.increment_index = match summary.backup_kind {
                        BT::Full => 0,
                        _ => plan.increment_index,
                    };
                }
                // Retention matches images by their file-name token (label for
                // encrypted volumes), not the device password key.
                run_images.push((device.clone(), image_path.clone()));
                protected.push((file_token(job, device), image_path));

                // Everything above is recorded first, so the finished image is
                // findable in the history; then the run stops short of success
                // because nobody checked it. Retention and consolidation do not
                // run — a cancelled read must not delete anything.
                if verify_cancelled {
                    fatal = Some(("cancelled".into(), verify_cancelled_message()));
                    break;
                }
            }
            Ok(Err(OwnBackupError::Cancelled)) => {
                fatal = Some(("cancelled".into(), tr!("lib.cancelled_by_user").into_owned()));
                break;
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                warn!(job = %job.name, volume = %device, error = %msg, "Backup failed");
                fatal = Some(("error".into(), msg));
                break;
            }
            Err(e) => {
                fatal = Some(("error".into(), tr!("lib.task_aborted", error = e).into_owned()));
                break;
            }
        }
    }

    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.duration_secs = total_dur;
    run.smart_mode = !protected.is_empty() && all_smart;

    // A single volume failure fails the whole run; images already written stay
    // on disk (never fake success), and retention does not run on a partial set.
    if let Some((status, msg)) = fatal {
        run.status = status;
        run.error = msg;
        return run;
    }

    run.status = "success".into();
    // Combined kind: the shared kind if every volume agreed, else "mixed".
    run.backup_kind = match kinds.first() {
        Some(first) if kinds.iter().all(|k| k == first) => (*first).into(),
        Some(_) => "mixed".into(),
        None => "full".into(),
    };

    // (Whole-disk bootable copies never reach here — they take the single
    // whole-disk-image path above, which embeds the restore manifest.)

    // Retention runs after a fully successful backup, independently per volume.
    // Its failures are logged, never surfaced as a run failure.
    apply_retention(job, &protected);
    run.consolidated_increments = apply_consolidation(job, &protected, keys.as_ref());

    run
}

/// Back up a whole disk (bootable copy) into ONE image with the real GPT and
/// an embedded restore manifest — no per-volume files, no external `.obdm`.
#[cfg(target_os = "windows")]
async fn perform_disk_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    ds: &DiskSourceSpec,
    cancel: &Arc<AtomicBool>,
    passwords: &Arc<dyn PasswordResolver>,
    keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
    mut run: JobRun,
) -> JobRun {
    use own_backup_backup::engine_win::{
        backup_disk, verify_image_scoped, VerifyScope, DiskBackupRequest, DiskPartitionBackup,
    };
    use own_backup_common::OwnBackupError;

    use own_backup_common::types::BackupType;

    let safe_name = own_backup_common::file_name::safe_file_stem(&job.name, "backup");

    // Chain planning (plan D5): a scheduled incremental/differential writes a
    // differencing child over the previous whole-disk image of THIS disk; any
    // doubt resolves to a full. VHDX children are `.avhdx` (Windows convention);
    // qcow2 children stay `.qcow2` (the role lives in the sidecar).
    let folder = PathBuf::from(&job.dest_folder);
    let plan = plan_run_for_disk(job, &folder, ds.disk_number);
    let child_ext = match (job.archive_encryption.is_some(), job.image_format) {
        (true, _) => own_backup_backup::seal::obk::OBK_EXTENSION,
        (false, own_backup_common::types::ImageFormat::Vhdx) => "avhdx",
        (false, own_backup_common::types::ImageFormat::Qcow2) => "qcow2",
    };
    // Name carries the kind tag (full/inc/diff) so a glance at the folder tells
    // full from child — the extension alone can't (a qcow2 child is still
    // `.qcow2`). Token position matches the per-volume scheme so `image_token`
    // reads `disk<N>` from `<name>_disk<N>_<tag>_<stamp>`.
    let (tag, ext) = match plan.kind {
        "incremental" => ("inc", child_ext),
        "differential" => ("diff", child_ext),
        _ => ("full", container_extension(job)),
    };
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let image_path =
        folder.join(format!("{safe_name}_disk{}_{tag}_{stamp}.{ext}", ds.disk_number));

    let partitions: Vec<DiskPartitionBackup> = ds
        .parts
        .iter()
        .map(|p| DiskPartitionBackup {
            device: p.device.clone(),
            part_index: p.part_index,
            passphrase: passwords.resolve(&source_token(&p.device)),
        })
        .collect();

    // Exclusions only make sense in smart mode (a sector-by-sector image is a
    // verbatim copy). Applied per partition inside the whole-disk image (D2),
    // chosen by filesystem family: Windows masks for NTFS/ReFS/FAT, Linux masks
    // for ext/btrfs/xfs.
    let (excl_win, excl_linux) = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        (job.exclusions.patterns.clone(), job.exclusions.patterns_linux.clone())
    } else {
        (Vec::new(), Vec::new())
    };

    // Full disk-layout snapshot for THIS disk (captured by the GUI at job save)
    // — embedded into the backup file so a Boot/Live-CD restore can identify the
    // archived disk WITHOUT jobs.json. Matched by trailing PhysicalDrive number;
    // None on jobs saved before snapshots existed (engine then omits it).
    let source_disk = job
        .source_disks
        .iter()
        .find(|d| {
            let tail: String =
                d.device_id.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
            let tail: String = tail.chars().rev().collect();
            tail.parse::<u32>().ok() == Some(ds.disk_number)
        })
        .cloned();

    let request = DiskBackupRequest {
        disk_number: ds.disk_number,
        disk_model: ds.disk_model.clone(),
        partitions,
        source_disk,
        bootable: ds.bootable,
        zstd_level: job.zstd_level(),
        image_path: image_path.clone(),
        block_size: own_backup_backup::engine_win::DEFAULT_BLOCK_SIZE,
        image_format: job.image_format,
        job_id: Some(job.id.clone()),
        encryption: keys.clone(),
        exclusion_patterns_windows: excl_win,
        exclusion_patterns_linux: excl_linux,
        kind: match plan.kind {
            "incremental" => BackupType::Incremental,
            "differential" => BackupType::Differential,
            _ => BackupType::Full,
        },
        parent_image: plan.parent_image.clone(),
    };
    let validate_after = job.advanced.validate_after;
    // Only the blocks this copy wrote, or everything the restore point
    // stands on — the job says which (default: everything).
    let verify_scope = if job.advanced.verify_latest_only {
        VerifyScope::LatestCopyOnly
    } else {
        VerifyScope::Everything
    };
    let image_for_verify = image_path.clone();
    let keys_for_verify = keys.clone();
    let image_name = image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let sink_bg = sink.clone();
    let job_id = job.id.clone();
    let cancel_bg = cancel.clone();
    let low_priority = job.advanced.low_priority;
    let speed_limit_mbps = job.advanced.speed_limit_mbps;
    let model = ds.disk_model.clone();
    let disk_number = ds.disk_number;

    let result = tokio::task::spawn_blocking(move || {
        let _quiet = QuietCopy::enter(low_priority);
        let mut pace = Pace::new(speed_limit_mbps);
        let mut last_permille = u64::MAX;
        let mut last_emit = std::time::Instant::now();
        let summary = backup_disk(&request, |p| {
            if cancel_bg.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(OwnBackupError::Cancelled);
            }
            // The engine tags non-copy phases via backup_id: "prepare" for the
            // per-partition exclusion scan ("Поиск исключаемых файлов…"),
            // "finalize" for the flush/detach + true-removal pass. Everything
            // else is the block copy.
            let phase = match p.backup_id.as_str() {
                "prepare" => "prepare",
                "finalize" => "finalize",
                // Post-copy exclusion passes — see the per-volume path.
                "exclusions" => "exclusions",
                _ => "backup",
            };
            if phase == "backup" {
                pace.after(p.blocks_done);
            }
            let permille =
                if p.blocks_total > 0 { p.blocks_done * 1000 / p.blocks_total } else { 0 };
            // "finalize" has no granular progress — always emit it so the UI
            // leaves the frozen 100 %; "prepare" (permille 0) shows on its first
            // tick then rides the 400 ms throttle, same as the per-volume path.
            // Сводка по типам приходит один раз, сразу после скана раздела —
            // через троттлинг её пропускать нельзя, другой такой не будет.
            let has_categories = p.categories.is_some();
            if has_categories
                || phase == "finalize"
                || phase == "exclusions"
                || permille != last_permille
                || last_emit.elapsed().as_millis() >= 400
            {
                last_permille = permille;
                last_emit = std::time::Instant::now();
                sink_bg.job_progress(JobProgress {
                    job_id: job_id.clone(),
                    phase: phase.into(),
                    percent: permille / 10,
                    blocks_done: p.blocks_done,
                    blocks_total: p.blocks_total,
                    speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                    volume_index: 0,
                    volume_count: 1,
                    volume: tr!("lib.disk_label", disk = disk_number, model = model).into_owned(),
                    image_file_name: image_name.clone(),
                    categories: p.categories.clone().unwrap_or_default(),
                    excluded_files: p.excluded.map(|(f, _)| f),
                    excluded_bytes: p.excluded.map(|(_, b)| b),
                });
            }
            Ok(())
        })?;

        // Optional post-backup verification — emit "verify" progress so the UI
        // shows "Проверка…", and FAIL the run on any checksum mismatch (a bad
        // image must never be reported as success).
        let mut verify_cancelled = false;
        if validate_after {
            let mut v_permille = u64::MAX;
            let mut v_emit = std::time::Instant::now();
            let mut v_rate = VerifyRate::new(&image_for_verify, keys_for_verify.as_ref());
            let verified =
                verify_image_scoped(&image_for_verify, keys_for_verify.as_ref(), verify_scope, |done, total| {
                if cancel_bg.swap(false, Ordering::SeqCst) {
                    return Err(OwnBackupError::Cancelled);
                }
                let mbps = v_rate.sample(done);
                let permille = if total > 0 { done * 1000 / total } else { 0 };
                if permille != v_permille || v_emit.elapsed().as_millis() >= 400 {
                    v_permille = permille;
                    v_emit = std::time::Instant::now();
                    sink_bg.job_progress(JobProgress {
                        job_id: job_id.clone(),
                        phase: "verify".into(),
                        percent: permille / 10,
                        blocks_done: done,
                        blocks_total: total,
                        speed_mbps: mbps,
                        volume_index: 0,
                        volume_count: 1,
                        volume: tr!("lib.disk_label", disk = disk_number, model = model)
                            .into_owned(),
                        image_file_name: image_name.clone(),
                        categories: Vec::new(),
                        excluded_files: None,
                        excluded_bytes: None,
                    });
                }
                Ok(())
            });
            match verified {
                Ok(v) if !v.valid() => {
                    return Err(OwnBackupError::ImageFormat(
                        tr!("lib.verify_failed", count = v.mismatched_blocks.len()).into_owned(),
                    ))
                }
                Ok(_) => {}
                Err(OwnBackupError::Cancelled) => verify_cancelled = true,
                Err(e) => return Err(e),
            }
        }
        Ok::<_, OwnBackupError>((summary, verify_cancelled))
    })
    .await;

    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.image_path = image_path.display().to_string();
    run.base_image_path = image_path.display().to_string();
    run.backup_kind = "full".into();
    run.smart_mode = true;
    // The whole-disk path takes a shadow copy per partition — except on the
    // boot medium, where the system is offline and a direct read is already
    // consistent (see `snapshot_wanted` in the engine).
    run.vss_used = !own_backup_common::media_mode::is_media_mode();

    match result {
        Ok(Ok((summary, verify_cancelled))) => {
            run.status = "success".into();
            run.bytes_copied = summary.bytes_copied;
            run.used_blocks = summary.blocks_copied;
            run.blocks_unchanged = summary.blocks_unchanged;
            run.excluded_files = summary.excluded_files;
            run.excluded_bytes = summary.excluded_bytes;
            run.categories = summary.categories;
            run.file_scan_skipped = summary.file_scan_skipped.unwrap_or_default();
            run.duration_secs = summary.duration.as_secs_f64();
            // Record what was ACTUALLY produced (the engine may have fallen back
            // to a full when the parent chain was unusable) so the next run plans
            // correctly off the history.
            let (kind_str, is_child) = match summary.backup_kind {
                BackupType::Incremental => ("incremental", true),
                BackupType::Differential => ("differential", true),
                _ => ("full", false),
            };
            run.backup_kind = kind_str.into();
            if is_child {
                if let Some(base) = &plan.base_image {
                    run.base_image_path = base.display().to_string();
                }
                run.increment_index = plan.increment_index;
            }
            // One image for the whole disk, but the list is filled in all the
            // same: "everything this job wrote" must be answerable the same way
            // whatever the job's shape (audit AUD-09).
            run.images.push(RunImage {
                source: format!("disk{}", ds.disk_number),
                image_path: run.image_path.clone(),
                base_image_path: run.base_image_path.clone(),
                kind: kind_str.into(),
            });
            info!(job = %job.name, image = %image_path.display(), kind = kind_str,
                partitions = summary.partitions, unchanged = summary.blocks_unchanged,
                "Whole-disk backup complete");
            // A check the user stopped is not a reason to delete older versions
            // — and the image below is NOT removed, unlike a cancelled copy.
            if verify_cancelled {
                run.status = "cancelled".into();
                run.error = verify_cancelled_message();
                return run;
            }
            let protected = [(format!("disk{}", ds.disk_number), image_path)];
            apply_retention(job, &protected);
            run.consolidated_increments = apply_consolidation(job, &protected, keys.as_ref());
        }
        Ok(Err(OwnBackupError::Cancelled)) => {
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_by_user").into_owned();
            let _ = std::fs::remove_file(&image_path);
        }
        Ok(Err(e)) => {
            run.status = "error".into();
            run.error = e.to_string();
        }
        Err(e) => {
            run.status = "error".into();
            run.error = tr!("lib.task_aborted", error = e).into_owned();
        }
    }
    run
}

/// Linux backup driver: full backups into raw images (one per source volume),
/// with progress + cancellation and optional post-backup verification. No chain
/// planning / retention yet — those follow with the dynamic-VHDX writer.
#[cfg(target_os = "linux")]
async fn perform_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    passwords: &Arc<dyn PasswordResolver>,
    // The local key store is Windows-only, so an encrypted job never gets this
    // far on Linux: `archive_keys_for_run` fails first and the run says so.
    _keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
) -> JobRun {
    use own_backup_backup::engine_linux::{
        backup_volume, verify_image, LinuxBackupRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_common::progress::BackupProgress;
    use own_backup_common::OwnBackupError;
    use own_backup_snapshot::SnapshotProvider;

    let mut run = run_skeleton(job);
    let volumes = job.source_list();
    if volumes.is_empty() {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = tr!("lib.no_source_volume").into_owned();
        return run;
    }
    let folder = PathBuf::from(&job.dest_folder);
    // FIRST, before the format check, before a parent is chosen and long
    // before anything is written: finish what a crash left half-done. Every
    // decision below reads these chains (audit AUD-06).
    if let Some(msg) = recover_interrupted_consolidations(job, &folder, None) {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = msg;
        return run;
    }
    if let Some(msg) = format_change_error(job, &folder) {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = msg;
        return run;
    }
    let count = volumes.len() as u64;
    let started = std::time::Instant::now();

    for (idx, device) in volumes.iter().enumerate() {
        let vidx = idx as u64;
        let token = source_token(device);
        let vol_label = job.label_for(device);
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let safe = own_backup_common::file_name::safe_file_stem(&job.name, "backup");
        let image_path = folder.join(format!(
            "{safe}_{}_full_{stamp}.{}",
            file_token(job, device),
            container_extension(job)
        ));

        // Take a point-in-time snapshot (LVM/fsfreeze) so a live filesystem is
        // read consistently; degrades to a crash-consistent copy of the device
        // when snapshotting isn't possible (non-root / no LVM / unmounted).
        let snap_provider = own_backup_snapshot::LinuxSnapshotProvider::new();
        let snapshot = snap_provider.create_snapshot(device).await.ok();
        let read_source = snapshot
            .as_ref()
            .map(|s| s.snapshot_device_path.clone())
            .unwrap_or_else(|| device.clone());

        let req = LinuxBackupRequest {
            source: read_source,
            image_path: image_path.clone(),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: job.advanced.sector_by_sector,
            passphrase: passwords.resolve(&token),
            job_id: Some(job.id.clone()),
            image_format: job.image_format,
            encryption: None,
            zstd_level: job.zstd_level(),
        };
        let validate_after = job.advanced.validate_after;
        let sink_bg = sink.clone();
        let job_id = job.id.clone();
        let cancel_bg = cancel.clone();
        let vol_label_bg = vol_label.clone();
        let image_for_verify = image_path.clone();
        let image_name = image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let result = tokio::task::spawn_blocking(move || {
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let summary = backup_volume(&req, |p: &BackupProgress| {
                if cancel_bg.load(Ordering::SeqCst) {
                    return Err(OwnBackupError::Cancelled);
                }
                let permille = if p.blocks_total > 0 {
                    p.blocks_done * 1000 / p.blocks_total
                } else {
                    0
                };
                if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                    last_permille = permille;
                    last_emit = std::time::Instant::now();
                    sink_bg.job_progress(JobProgress {
                        job_id: job_id.clone(),
                        phase: "backup".into(),
                        percent: permille / 10,
                        blocks_done: p.blocks_done,
                        blocks_total: p.blocks_total,
                        speed_mbps: p.speed_bps as f64 / 1_000_000.0,
                        volume_index: vidx,
                        volume_count: count,
                        volume: vol_label_bg.clone(),
                        image_file_name: image_name.clone(),
                        categories: Vec::new(),
                        excluded_files: None,
                        excluded_bytes: None,
                    });
                }
                Ok(())
            })?;
            let mut verify_cancelled = false;
            if validate_after {
                let verified = verify_image(&image_for_verify, |_, _| {
                    if cancel_bg.load(Ordering::SeqCst) {
                        return Err(OwnBackupError::Cancelled);
                    }
                    Ok(())
                });
                match verified {
                    Ok(v) if !v.valid() => {
                        return Err(OwnBackupError::ImageFormat(
                            tr!("lib.verify_failed", count = v.mismatched_blocks.len())
                                .into_owned(),
                        ))
                    }
                    Ok(_) => {}
                    Err(OwnBackupError::Cancelled) => verify_cancelled = true,
                    Err(e) => return Err(e),
                }
            }
            Ok((summary, verify_cancelled))
        })
        .await;

        // Release the snapshot (thaw / lvremove) regardless of the outcome.
        if let Some(s) = &snapshot {
            if let Err(e) = snap_provider.release_snapshot(s).await {
                warn!(error = %e, "snapshot release failed");
            }
        }

        match result {
            Ok(Ok((summary, verify_cancelled))) => {
                run.image_path = summary.image_path.display().to_string();
                run.base_image_path = run.image_path.clone();
                run.images.push(RunImage {
                    source: device.clone(),
                    image_path: run.image_path.clone(),
                    base_image_path: run.image_path.clone(),
                    kind: "full".into(),
                });
                run.source_bytes += summary.source_bytes;
                run.bytes_copied += summary.bytes_copied;
                run.used_blocks += summary.used_blocks;
                run.total_blocks += summary.total_blocks;
                run.smart_mode = summary.smart_mode;
                run.backup_kind = "full".into();
                if verify_cancelled {
                    run.status = "cancelled".into();
                    run.error = verify_cancelled_message();
                    break;
                }
            }
            Ok(Err(OwnBackupError::Cancelled)) => {
                run.status = "cancelled".into();
                run.error = tr!("lib.cancelled_by_user").into_owned();
                break;
            }
            Ok(Err(e)) => {
                warn!(job = %job.name, volume = %device, error = %e, "Backup failed");
                run.status = "error".into();
                run.error = e.to_string();
                break;
            }
            Err(e) => {
                run.status = "error".into();
                run.error = tr!("lib.task_aborted", error = e).into_owned();
                break;
            }
        }
    }

    if run.status.is_empty() {
        run.status = "success".into();
    }
    run.duration_secs = started.elapsed().as_secs_f64();
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run
}

/// Backup engine is unavailable on this platform (only Windows and Linux).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
async fn perform_backup(
    _sink: &Arc<dyn EventSink>,
    job: &Job,
    _cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    _passwords: &Arc<dyn PasswordResolver>,
    _keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
) -> JobRun {
    let mut run = run_skeleton(job);
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.status = "error".into();
    run.error = tr!("lib.engine_unavailable").into_owned();
    run
}

#[cfg(test)]
mod tests {
    #[test]
    fn pace_holds_a_copy_to_its_limit() {
        // 50 blocks of 1 MB at 100 MB/s: the fiftieth may not be done before
        // half a second has passed.
        let mut pace = Pace::with_block(100, 1_000_000);
        let t = std::time::Instant::now();
        pace.after(1);
        pace.after(50);
        let took = t.elapsed();
        assert!(took >= std::time::Duration::from_millis(480), "{took:?}");
        assert!(took < std::time::Duration::from_millis(1500), "{took:?}");

        // No limit, no waiting — however much was read.
        let mut free = Pace::with_block(0, 1_000_000);
        let t = std::time::Instant::now();
        free.after(1_000_000);
        assert!(t.elapsed() < std::time::Duration::from_millis(50));
    }

    /// Every default rule, asked of paths a real Windows volume has: what
    /// must go, and — the part that matters — what must stay.
    #[test]
    fn default_exclusions_take_caches_and_leave_the_system() {
        use own_backup_backup::exclusions::ExclusionPattern;
        let rules: Vec<ExclusionPattern> =
            default_exclusions().iter().map(|p| ExclusionPattern::parse(p).expect(p)).collect();
        let excluded = |path: &str| {
            let comps: Vec<String> = path.split('\\').map(|c| c.to_lowercase()).collect();
            let name = comps.last().unwrap().clone();
            rules.iter().any(|r| r.matches(&name, &comps))
        };
        for gone in [
            r"Windows\SoftwareDistribution\Download\a1b2\update.cab",
            r"Windows\SoftwareDistribution\DataStore\DataStore.edb",
            r"Windows\Prefetch\CHROME.EXE-1A2B3C4D.pf",
            r"Windows\MEMORY.DMP",
            r"Windows\Minidump\092626-1234-01.dmp",
            r"$AV_ASW\$VAULT$\file",
            r"ProgramData\Microsoft\Windows\WER\ReportArchive\x\Report.wer",
            r"Users\Bob\AppData\Local\Google\Chrome\User Data\Default\Cache\Cache_Data\f_000001",
            r"Users\Bob\AppData\Local\Google\Chrome\User Data\Profile 1\Code Cache\js\index",
            r"Users\Bob\AppData\Local\Microsoft\Edge\User Data\Default\Cache\Cache_Data\data_0",
            r"Users\Bob\AppData\Local\Mozilla\Firefox\Profiles\ab12.default\cache2\entries\X",
            r"Users\Bob\AppData\Local\Microsoft\Windows\Explorer\thumbcache_256.db",
            r"Users\Bob\AppData\Local\NVIDIA\DXCache\x.bin",
            r"Users\Bob\AppData\Local\Temp\setup.log",
        ] {
            assert!(excluded(gone), "should be excluded: {gone}");
        }
        for kept in [
            // The folders themselves: Windows expects them, with their ACLs.
            r"Windows\SoftwareDistribution",
            r"Windows\Prefetch",
            r"Users\Bob\AppData\Local\Google\Chrome\User Data\Default\Cache",
            // The system, the user's data, their browser profile.
            r"Windows\System32\ntoskrnl.exe",
            r"Windows\System32\config\SYSTEM",
            r"Users\Bob\Documents\report.docx",
            r"Users\Bob\AppData\Local\Google\Chrome\User Data\Default\Bookmarks",
            r"Users\Bob\AppData\Local\Google\Chrome\User Data\Default\Login Data",
            // A virtual machine's disk is data (2026-09-26).
            r"VMs\Win11\Win11.vhdx",
            r"Hyper-V\disk_2F3A.avhdx",
            // Only at the root: a user folder named like a system one is theirs.
            r"Projects\Windows\Prefetch\notes.txt",
            // An upgrade's way back.
            r"Windows.old\Windows\System32\kernel32.dll",
        ] {
            assert!(!excluded(kept), "must stay in the copy: {kept}");
        }
    }

    use super::*;
    use std::path::Path;

    /// A save that cannot reach the disk must not be reported as done. It
    /// used to be: memory changed, the job came back as stored, and the
    /// failure went to the log — so the job was visible, editable and runnable
    /// until the next restart, then gone (audit AUD-08).
    #[test]
    fn a_job_that_could_not_be_written_is_not_reported_as_saved() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());

        // Something in the way of the temporary file the store writes first.
        // A directory cannot be replaced by a file, so the write fails the way
        // a full disk or a read-only folder would.
        std::fs::create_dir(dir.path().join("jobs.json.tmp")).unwrap();

        let err = store
            .upsert(Job { name: "nightly".into(), ..Default::default() })
            .expect_err("the write cannot succeed");
        assert!(!err.to_string().is_empty());

        // And the store is exactly as it was — no job that will disappear at
        // the next restart.
        assert!(store.list().is_empty(), "nothing was added in memory either");
    }

    /// The same for removal: a job is gone from the list only once it is gone
    /// from the file.
    #[test]
    fn a_job_that_could_not_be_removed_stays_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let job = store.upsert(Job { name: "keep".into(), ..Default::default() }).unwrap();

        std::fs::create_dir(dir.path().join("jobs.json.tmp")).unwrap();
        assert!(store.delete(&job.id).is_err());
        assert_eq!(store.list().len(), 1, "still there, because the file still has it");
    }

    /// A jobs file that cannot be parsed is kept, not walked over. Starting
    /// empty and saving on top would replace it with an empty list.
    #[test]
    fn an_unreadable_store_file_is_moved_aside_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("jobs.json"), b"{ this is not json").unwrap();

        let store = JobStore::load(dir.path());
        assert!(store.list().is_empty(), "nothing could be read");
        assert!(
            dir.path().join("jobs.json.bad").is_file(),
            "the unreadable one is kept for recovery"
        );

        // And a later save does not resurrect the broken file.
        store.upsert(Job { name: "fresh".into(), ..Default::default() }).unwrap();
        assert_eq!(JobStore::load(dir.path()).list().len(), 1);
    }

    /// The queue's two halves used to be two mutexes taken in opposite orders
    /// by different callers. This is the shape that deadlocks, and the test
    /// that would have caught it: one thread holds the queue while another
    /// asks for it, from both directions at once, many times over. With one
    /// lock it simply cannot hang; with two it hangs and the test times out.
    #[test]
    fn the_queue_cannot_be_deadlocked_from_two_directions() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        let queue = RunQueue::new();
        let done = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(2));

        let readers = {
            let (q, d, g) = (queue.clone(), done.clone(), gate.clone());
            std::thread::spawn(move || {
                g.wait();
                for _ in 0..5_000 {
                    let _ = q.snapshot();
                    d.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let writers = {
            let (q, d, g) = (queue.clone(), done.clone(), gate.clone());
            std::thread::spawn(move || {
                g.wait();
                for n in 0..5_000 {
                    let id = format!("job-{}", n % 7);
                    q.enqueue(&id);
                    q.cancel_pending(&id);
                    d.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        readers.join().unwrap();
        writers.join().unwrap();
        assert_eq!(done.load(Ordering::Relaxed), 10_000);
    }

    /// Asking "is it queued?" and queueing it are one step, or two callers
    /// arriving together both find it absent and both add it — and the job
    /// runs twice.
    #[test]
    fn a_job_is_queued_once_however_many_ask() {
        let queue = RunQueue::new();
        let gate = Arc::new(std::sync::Barrier::new(8));
        let winners: Vec<_> = (0..8)
            .map(|_| {
                let (q, g) = (queue.clone(), gate.clone());
                std::thread::spawn(move || {
                    g.wait();
                    q.enqueue("the-one-job")
                })
            })
            .collect();
        let accepted =
            winners.into_iter().map(|h| h.join().unwrap()).filter(|added| *added).count();
        assert_eq!(accepted, 1, "exactly one caller may be told it was added");
        assert_eq!(queue.snapshot().pending, vec!["the-one-job".to_string()]);
    }

    /// A job cancelled while it waited must not start when the worker finally
    /// reaches its message.
    #[test]
    fn a_job_cancelled_while_waiting_never_starts() {
        let queue = RunQueue::new();
        assert!(queue.enqueue("a"));
        queue.cancel_pending("a");
        assert!(!queue.start("a"), "it is no longer in the queue");
        assert_eq!(queue.snapshot().current, None);
    }

    /// Waiting and running are one move: the job is never in both lists and
    /// never in neither.
    #[test]
    fn starting_a_job_takes_it_out_of_the_queue_in_the_same_step() {
        let queue = RunQueue::new();
        queue.enqueue("a");
        queue.enqueue("b");
        assert!(queue.start("a"));

        let seen = queue.snapshot();
        assert_eq!(seen.current.as_deref(), Some("a"));
        assert_eq!(seen.pending, vec!["b".to_string()]);

        // And a running job is not queued again behind itself.
        assert!(!queue.enqueue("a"));

        queue.finish();
        assert_eq!(queue.snapshot().current, None);
    }

    // Password persistence round-trips through the OS secret store; on non-Windows
    // that is the key-file backend (testable here), on Windows it is DPAPI.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn password_store_remembers_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.db");
        PasswordStore::new(db.clone())
            .remember("V123", "secret".into(), true)
            .unwrap();

        // A fresh store (cold cache) recovers it from the DB via the key file.
        let store = PasswordStore::new(db.clone());
        assert!(store.has("V123"));
        assert_eq!(store.resolve("V123").map(|z| z.to_string()), Some("secret".into()));

        store.forget("V123").unwrap();
        assert!(!PasswordStore::new(db).has("V123"));
    }

    /// A job with `method` plus a successful run that produced `kind` at
    /// `image` (image + sidecar files actually present in `dir` when `create`
    /// is set).
    fn job_with_last(
        dir: &Path,
        method: &str,
        kind: &str,
        index: u32,
        create: bool,
    ) -> (Job, JobRun, PathBuf) {
        let image = dir.join(format!("j_E_{kind}_x.vhdx"));
        let base = dir.join("j_E_full_x.vhdx");
        if create {
            for p in [&image, &base] {
                std::fs::write(p, b"img").unwrap();
                std::fs::write(own_backup_backup::sidecar_path_for(p), b"sc").unwrap();
            }
        }
        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = method.into();
        let run = JobRun {
            job_id: "job1".into(),
            status: "success".into(),
            image_path: image.display().to_string(),
            base_image_path: base.display().to_string(),
            backup_kind: kind.into(),
            increment_index: index,
            ..Default::default()
        };
        (job, run, image)
    }

    #[test]
    fn full_method_always_plans_full() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "full", "full", 0, true);
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());
    }

    #[test]
    fn incremental_without_history_plans_full() {
        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        assert_eq!(plan_run(&job, None), RunPlan::full());
    }

    #[test]
    fn incremental_chains_to_previous_version() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, image) = job_with_last(dir.path(), "incremental", "incremental", 2, true);
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "incremental");
        assert_eq!(plan.parent_image.as_deref(), Some(image.as_path()));
        assert_eq!(
            plan.base_image.as_deref(),
            Some(dir.path().join("j_E_full_x.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 3);
    }

    #[test]
    fn differential_chains_to_the_full_base() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "differential", "differential", 1, true);
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "differential");
        // Parent is the BASE, not the previous differential.
        assert_eq!(
            plan.parent_image.as_deref(),
            Some(dir.path().join("j_E_full_x.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 2);
    }

    #[test]
    fn rollover_after_n_children() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, mut run, _) = job_with_last(dir.path(), "incremental", "incremental", 5, true);
        job.scheme.full_every_n = 5;
        job.scheme.only_incremental = false;
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());

        // Below the limit the chain continues.
        run.increment_index = 4;
        assert_eq!(plan_run(&job, Some(&run)).kind, "incremental");

        // only_incremental disables the rollover entirely.
        run.increment_index = 99;
        job.scheme.only_incremental = true;
        assert_eq!(plan_run(&job, Some(&run)).kind, "incremental");
    }

    #[test]
    fn missing_parent_files_force_full() {
        let dir = tempfile::tempdir().unwrap();
        let (job, run, _) = job_with_last(dir.path(), "incremental", "incremental", 1, false);
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());
    }

    #[test]
    fn legacy_full_run_parents_a_new_chain() {
        // Runs recorded before phase 5 have backup_kind == "" — they were full.
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("old_E_20260701.vhdx");
        std::fs::write(&image, b"img").unwrap();
        std::fs::write(own_backup_backup::sidecar_path_for(&image), b"sc").unwrap();

        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        let run = JobRun {
            status: "success".into(),
            image_path: image.display().to_string(),
            ..Default::default()
        };
        let plan = plan_run(&job, Some(&run));
        assert_eq!(plan.kind, "incremental");
        assert_eq!(plan.parent_image.as_deref(), Some(image.as_path()));
        assert_eq!(plan.base_image.as_deref(), Some(image.as_path()));
        assert_eq!(plan.increment_index, 1);
    }

    #[test]
    fn drive_letter_extraction() {
        assert_eq!(drive_letter_of("E:"), Some('E'));
        assert_eq!(drive_letter_of("e:\\OwnBackup"), Some('E'));
        assert_eq!(drive_letter_of(r"\\.\E:"), Some('E'));
        assert_eq!(drive_letter_of(r"\\?\E:\Foo"), Some('E'));
        // No drive letter: UNC shares and VSS/device paths.
        assert_eq!(drive_letter_of(r"\\server\share\Backups"), None);
        assert_eq!(drive_letter_of(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy5"), None);
    }

    #[test]
    fn rejects_dest_on_source_volume() {
        assert!(dest_on_source_volume("E:", r"E:\OwnBackup"));
        assert!(dest_on_source_volume(r"\\.\E:", r"e:\backups")); // case-insensitive
        assert!(!dest_on_source_volume("E:", r"D:\OwnBackup"));
        // Network destination is fine even when backing up any local drive.
        assert!(!dest_on_source_volume("E:", r"\\nas\share\OwnBackup"));
    }

    #[test]
    fn failed_run_does_not_reset_the_chain() {
        // last_success skips over failed/cancelled runs, so one bad run in
        // between must not force the next backup back to full.
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let (_, good, _) = job_with_last(dir.path(), "incremental", "incremental", 1, true);
        store.record_run(good.clone());
        store.record_run(JobRun {
            job_id: "job1".into(),
            status: "error".into(),
            ..Default::default()
        });
        let found = store.last_success("job1").expect("success run found");
        assert_eq!(found.image_path, good.image_path);
        // Other jobs' runs are invisible.
        assert!(store.last_success("other").is_none());
    }

    // ---- multi-volume (per-letter) planning ------------------------------

    #[test]
    fn source_and_image_token_parsing() {
        assert_eq!(source_token("C:"), "C");
        assert_eq!(source_token("e:\\"), "E");
        assert_eq!(source_token(r"\\.\D:"), "D");
        assert_eq!(source_token(r"\\?\GLOBALROOT\Device\X"), "X"); // no letter → "X"
        // Letterless volume GUID path → V + first 8 hex of the GUID.
        assert_eq!(source_token(r"\\?\Volume{1a2b3c4d-5e6f-7089-abcd-ef0123456789}"), "V1A2B3C4D");
        // Physical-offset partition → P<disk><offsetHex> (0x52000000 = 1375731712).
        assert_eq!(source_token("part:3:1375731712:1073741824"), "P352000000");

        // Names are `<name>_<token>_<tag>_<stamp>`; token is read from the end
        // so underscores inside <name> do not confuse it.
        assert_eq!(image_token(Path::new("job_C_full_20260101-000000.vhdx")).as_deref(), Some("C"));
        assert_eq!(
            image_token(Path::new("My_Big_Job_V1A2B3C4D_inc_20260101-000000.avhdx")).as_deref(),
            Some("V1A2B3C4D")
        );
        assert_eq!(image_token(Path::new("no_underscores.vhdx")), None);

        // Whole-disk images `<name>_disk<N>_<tag>_<stamp>` → token is `disk<N>`
        // (3rd-from-last), for the full (.vhdx) and its child (.avhdx) alike.
        assert_eq!(image_token(Path::new("MODULATOR_disk3_full_20260711-104244.vhdx")).as_deref(), Some("DISK3"));
        assert_eq!(image_token(Path::new("My_Job_disk12_inc_20260711-104244.avhdx")).as_deref(), Some("DISK12"));
    }

    #[test]
    fn file_token_uses_label_for_encrypted_sources() {
        let dev = "part:4:1048576:1000";
        let mut job = Job::default();
        job.sources = vec![dev.into()];
        // A label with an underscore is stripped to an underscore-free token.
        job.source_labels = vec!["kubuntu_boot".into()];
        let token = file_token(&job, dev);
        assert_eq!(token, "kubuntuboot");
        // The file name round-trips: image_token reads it back (case-insensitive).
        let name = output_path_for(&job, dev, "full");
        assert!(image_token(&name).unwrap().eq_ignore_ascii_case(&token));

        // No label → falls back to the stable device token.
        let bare = Job::default();
        assert_eq!(file_token(&bare, dev), source_token(dev));
        // A lettered volume is unaffected by any label.
        let mut lettered = Job::default();
        lettered.sources = vec!["C:".into()];
        lettered.source_labels = vec!["Windows".into()];
        assert_eq!(file_token(&lettered, "C:"), "C");
    }

    #[test]
    fn source_list_normalizes_old_and_new() {
        // Old single-volume job: only `source` set.
        let mut old = Job::default();
        old.source = "E:".into();
        old.source_label = "E: Data".into();
        assert_eq!(old.source_list(), vec!["E:".to_string()]);
        assert_eq!(old.label_for("E:"), "E: Data");

        // New multi-volume job: `sources` takes precedence, blanks dropped.
        let mut multi = Job::default();
        multi.source = "C:".into();
        multi.sources = vec!["C:".into(), "".into(), "E:".into()];
        multi.source_labels = vec!["C: Win".into(), "".into(), "E: Data".into()];
        assert_eq!(multi.source_list(), vec!["C:".to_string(), "E:".to_string()]);
        assert_eq!(multi.label_for("E:"), "E: Data");
        assert_eq!(multi.label_for("Z:"), "Z:"); // unknown → device itself
    }

    /// Write a real (parseable) sidecar + dummy image so `list_job_chains`
    /// discovers it. `name` must follow `<n>_<letter>_<tag>_<stamp>.<ext>`.
    fn put_real_image(
        dir: &Path,
        name: &str,
        role: own_backup_backup::sidecar::ChainRole,
        parent: Option<&str>,
        job_id: &str,
        created_at: &str,
    ) {
        use own_backup_backup::sidecar::SidecarInfo;
        use own_backup_backup::{sidecar_path_for, write_sidecar};
        use own_backup_common::disk::DiskInfo;
        use own_backup_common::image::ImageManifest;
        use own_backup_common::types::{
            BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable,
        };
        use uuid::Uuid;

        let image = dir.join(name);
        std::fs::write(&image, b"img").unwrap();
        let mut info = match parent {
            None => SidecarInfo::new_full(name, 0),
            Some(p) => SidecarInfo::new_child(name, role, p, 0),
        };
        info.job_id = Some(job_id.to_string());
        let manifest = ImageManifest {
            backup_id: Uuid::new_v4(),
            created_at: created_at.into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\C:".into(),
                model: String::new(),
                serial: String::new(),
                size_bytes: 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 512,
            total_blocks: 2,
            used_blocks: 2,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        };
        write_sidecar(&sidecar_path_for(&image), &info, &manifest, &[]).unwrap();
    }

    #[test]
    fn per_volume_plan_isolates_by_drive_letter() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Two volumes, each with its own full backup.
        put_real_image(d, "job_C_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(d, "job_E_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true; // no rollover

        let plan_c = plan_run_for_volume(&job, d, "C:");
        assert_eq!(plan_c.kind, "incremental");
        // C: must parent C:'s full, never E:'s.
        assert_eq!(
            plan_c.parent_image.as_deref(),
            Some(d.join("job_C_full_20260101-000000.vhdx").as_path())
        );
        let plan_e = plan_run_for_volume(&job, d, "E:");
        assert_eq!(
            plan_e.parent_image.as_deref(),
            Some(d.join("job_E_full_20260101-000000.vhdx").as_path())
        );
        assert_eq!(plan_c.increment_index, 1);
    }

    #[test]
    fn per_volume_incremental_parents_newest_and_rolls_over() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        put_real_image(d, "job_C_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(
            d,
            "job_C_inc_20260102-000000.avhdx",
            ChainRole::Incremental,
            Some("job_C_full_20260101-000000.vhdx"),
            "job1",
            "2026-01-02T00:00:00Z",
        );

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = false;
        job.scheme.full_every_n = 5;

        // Incremental parents the NEWEST image; base stays the full.
        let plan = plan_run_for_volume(&job, d, "C:");
        assert_eq!(plan.kind, "incremental");
        assert_eq!(
            plan.parent_image.as_deref(),
            Some(d.join("job_C_inc_20260102-000000.avhdx").as_path())
        );
        assert_eq!(
            plan.base_image.as_deref(),
            Some(d.join("job_C_full_20260101-000000.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 2);

        // Rollover: with N=1 the one-child chain is full already → new full.
        job.scheme.full_every_n = 1;
        assert_eq!(plan_run_for_volume(&job, d, "C:"), RunPlan::full());
    }

    /// Helper: rewrite a base's sidecar with consolidation bookkeeping.
    fn stamp_base(base: &Path, folded: u32, chain_started_at: Option<&str>) {
        use own_backup_backup::sidecar::{read_sidecar, write_sidecar};
        use own_backup_backup::sidecar_path_for;
        let mut sidecar = read_sidecar(&sidecar_path_for(base)).unwrap();
        sidecar.info.consolidated_increments = folded;
        sidecar.info.chain_started_at = chain_started_at.map(|s| s.to_string());
        write_sidecar(
            &sidecar_path_for(base),
            &sidecar.info,
            &sidecar.manifest,
            &sidecar.checksums,
        )
        .unwrap();
    }

    fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&chrono::Utc)
    }

    /// Standard mode counts VERSIONS, and consolidation deletes increment
    /// files — so a job switched from consolidation back to standard must
    /// still count the folded ones. Counting the files left would restart the
    /// rollover from zero and stretch the chain far past N.
    #[test]
    fn folded_increments_still_count_towards_the_standard_rollover() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let base_name = "job_C_full_20260101-000000.vhdx";
        put_real_image(d, base_name, ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(
            d,
            "job_C_inc_20260105-000000.avhdx",
            ChainRole::Incremental,
            Some(base_name),
            "job1",
            "2026-01-05T00:00:00Z",
        );

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = false;
        job.scheme.full_every_n = 3;
        job.scheme.rolling_incrementals = None; // standard mode

        // One increment on disk, none folded: keep going.
        assert_eq!(plan_run_for_volume(&job, d, "C:").kind, "incremental");

        // Two more were folded into the base earlier. One file remains, but
        // the chain has held three versions — time for a full.
        stamp_base(&d.join(base_name), 2, None);
        assert_eq!(
            plan_run_for_volume(&job, d, "C:"),
            RunPlan::full(),
            "1 increment present + 2 folded in = 3 versions, so N=3 must roll over"
        );
    }

    /// Consolidation mode keeps ONE chain, so no version count ever ends it.
    /// Age does — and it keys on when the chain STARTED, which consolidation
    /// moves out of the base's own `created_at`.
    #[test]
    fn consolidation_mode_rolls_over_by_age_not_by_count() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let base_name = "job_C_full_20260401-000000.vhdx";
        // The base was CONSOLIDATED on 1 April, but its chain began 1 January.
        put_real_image(d, base_name, ChainRole::Full, None, "job1", "2026-04-01T00:00:00Z");
        put_real_image(
            d,
            "job_C_inc_20260402-000000.avhdx",
            ChainRole::Incremental,
            Some(base_name),
            "job1",
            "2026-04-02T00:00:00Z",
        );

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.rolling_incrementals = Some(3);
        job.scheme.full_every_n = 1; // would fire instantly in standard mode
        job.scheme.only_incremental = false;

        // No age limit set: the chain lives on however many versions it holds.
        job.scheme.cleanup_older_than_days = None;
        stamp_base(&d.join(base_name), 40, Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            plan_run_for_volume_at(&job, d, "C:", at("2026-04-03T00:00:00Z")).kind,
            "incremental",
            "40 folded versions must NOT end a consolidated chain"
        );

        // With a 90-day limit the chain is 92 days old — start a new one.
        job.scheme.cleanup_older_than_days = Some(90);
        assert_eq!(
            plan_run_for_volume_at(&job, d, "C:", at("2026-04-03T00:00:00Z")),
            RunPlan::full(),
            "the chain began on 1 January, whatever the base's own date now says"
        );

        // The base's own date (1 April) is only two days old — reading THAT
        // instead of the chain's start is the mistake this test exists for.
        stamp_base(&d.join(base_name), 40, None);
        assert_eq!(
            plan_run_for_volume_at(&job, d, "C:", at("2026-04-03T00:00:00Z")).kind,
            "incremental",
            "without a chain stamp the base's own date is the honest fallback"
        );
    }

    #[test]
    fn whole_disk_incremental_parents_newest_and_isolates_by_disk() {
        use own_backup_backup::sidecar::ChainRole;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Disk 3 chain: full + one incremental child.
        put_real_image(d, "job_disk3_full_20260101-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-01T00:00:00Z");
        put_real_image(
            d,
            "job_disk3_inc_20260102-000000.avhdx",
            ChainRole::Incremental,
            Some("job_disk3_full_20260101-000000.vhdx"),
            "job1",
            "2026-01-02T00:00:00Z",
        );
        // A different disk's full must never be picked as disk 3's parent.
        put_real_image(d, "job_disk4_full_20260103-000000.vhdx", ChainRole::Full, None, "job1", "2026-01-03T00:00:00Z");

        let mut job = Job::default();
        job.id = "job1".into();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = false;
        job.scheme.full_every_n = 5;

        let plan = plan_run_for_disk(&job, d, 3);
        assert_eq!(plan.kind, "incremental");
        assert_eq!(
            plan.parent_image.as_deref(),
            Some(d.join("job_disk3_inc_20260102-000000.avhdx").as_path())
        );
        assert_eq!(
            plan.base_image.as_deref(),
            Some(d.join("job_disk3_full_20260101-000000.vhdx").as_path())
        );
        assert_eq!(plan.increment_index, 2);

        // Disk 4 has only a full → its next incremental parents that full.
        let plan4 = plan_run_for_disk(&job, d, 4);
        assert_eq!(plan4.increment_index, 1);
        assert_eq!(
            plan4.parent_image.as_deref(),
            Some(d.join("job_disk4_full_20260103-000000.vhdx").as_path())
        );

        // A disk with no chain → full.
        assert_eq!(plan_run_for_disk(&job, d, 9), RunPlan::full());

        // Rollover: N=1 and the disk-3 chain already has a child → new full.
        job.scheme.full_every_n = 1;
        assert_eq!(plan_run_for_disk(&job, d, 3), RunPlan::full());
    }

    // --- Восстановление запускается ПЕРВЫМ и умеет остановить запуск (AUD-06)

    /// Образ с внешним sidecar, принадлежащий заданию `job_id`.
    fn put_image_of_job(dir: &Path, name: &str, job_id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        use own_backup_backup::sidecar::{write_sidecar, SidecarInfo};
        let image = dir.join(name);
        std::fs::write(&image, b"img").unwrap();
        let mut info = SidecarInfo::new_full(name, 0);
        info.job_id = Some(job_id.into());
        let manifest = own_backup_common::image::ImageManifest {
            backup_id: uuid::Uuid::nil(),
            created_at: "2026-01-01T00:00:00Z".into(),
            backup_type: own_backup_common::types::BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: own_backup_common::disk::DiskInfo {
                device_id: r"\.\E:".into(),
                model: String::new(),
                serial: String::new(),
                size_bytes: 1024 * 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: own_backup_common::types::PartitionTable::Gpt,
                disk_guid: uuid::Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: own_backup_common::types::BootMode::Uefi,
            partition_table: own_backup_common::types::PartitionTable::Gpt,
            compression_algo: own_backup_common::types::CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 65536,
            total_blocks: 16,
            used_blocks: 0,
            backup_mode: own_backup_common::types::BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        };
        write_sidecar(&own_backup_backup::sidecar_path_for(&image), &info, &manifest, &[]).unwrap();
        image
    }

    /// Незавершённая консолидация СВОЕЙ цепочки останавливает запуск. Раньше
    /// восстановление вызывалось последним делом запуска, уже после того как
    /// родитель был выбран, копия записана, а retention удалил цепочки, —
    /// то есть все эти решения принимались о цепочке в середине свёртки.
    #[test]
    fn a_chain_stuck_mid_consolidation_stops_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let base = put_image_of_job(dir.path(), "a_full.vhdx", "job1");
        // Маркер, который прочитать нельзя: состояние цепочки неизвестно.
        std::fs::write(
            own_backup_backup::consolidate::marker_path(&base),
            b"not json at all",
        )
        .unwrap();

        let mut job = Job::default();
        job.id = "job1".into();
        let stop = recover_interrupted_consolidations(&job, dir.path(), None);
        assert!(stop.is_some(), "запуск обязан остановиться");
        assert!(stop.unwrap().contains("a_full.vhdx"));
    }

    /// Чужая застрявшая цепочка в общей папке — проблема того задания, а не
    /// этого. Останавливать здесь нечего.
    #[test]
    fn someone_elses_stuck_chain_does_not_stop_this_job() {
        let dir = tempfile::tempdir().unwrap();
        let base = put_image_of_job(dir.path(), "b_full.vhdx", "other-job");
        std::fs::write(
            own_backup_backup::consolidate::marker_path(&base),
            b"not json at all",
        )
        .unwrap();

        let mut job = Job::default();
        job.id = "job1".into();
        assert!(recover_interrupted_consolidations(&job, dir.path(), None).is_none());
    }

    /// Папка без маркеров — обычный случай, и он ничего не стоит.
    #[test]
    fn a_folder_with_no_markers_lets_the_run_go_on() {
        let dir = tempfile::tempdir().unwrap();
        put_image_of_job(dir.path(), "c_full.vhdx", "job1");
        let mut job = Job::default();
        job.id = "job1".into();
        assert!(recover_interrupted_consolidations(&job, dir.path(), None).is_none());
    }

    /// Базу, у которой вообще не прочитать принадлежность, считаем своей:
    /// осторожный ответ — тот, который останавливает запуск.
    #[test]
    fn a_base_whose_owner_cannot_be_read_counts_as_ours() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("d_full.vhdx");
        std::fs::write(&base, b"not an image").unwrap();
        std::fs::write(
            own_backup_backup::consolidate::marker_path(&base),
            b"not json at all",
        )
        .unwrap();

        let mut job = Job::default();
        job.id = "job1".into();
        assert!(recover_interrupted_consolidations(&job, dir.path(), None).is_some());
    }

    // --- «Удалить задание вместе с копиями» (аудит AUD-09) ------------------
    //
    // Вопрос «что это задание записало» раньше задавали журналу интерфейса:
    // он обрезан до ACTIVITY_CAP запусков, а у многотомного задания в нём
    // лежал только образ ПЕРВОГО тома. Надёжный источник — сами образы: в
    // sidecar каждого написано, чьё это.

    fn a_store(dir: &Path) -> Arc<JobStore> {
        JobStore::load(dir)
    }

    /// Копия второго тома тоже уходит. Раньше она оставалась на диске:
    /// её пути не было ни в одном поле JobRun.
    #[test]
    fn every_volume_of_a_job_is_found_not_just_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let store = a_store(dir.path());

        put_image_of_job(&dest, "j_C_full_1.vhdx", "job1");
        put_image_of_job(&dest, "j_D_full_1.vhdx", "job1");

        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = dest.display().to_string();

        let report = delete_job_files(&store, &job);
        assert!(report.complete(), "{report:?}");
        assert!(!dest.join("j_C_full_1.vhdx").exists());
        assert!(!dest.join("j_D_full_1.vhdx").exists(), "второй том тоже принадлежит заданию");
    }

    /// Чужие копии в той же папке не трогаются — принадлежность читается из
    /// sidecar, а не угадывается по имени файла.
    #[test]
    fn copies_of_another_job_in_the_same_folder_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let store = a_store(dir.path());

        put_image_of_job(&dest, "mine_full_1.vhdx", "job1");
        put_image_of_job(&dest, "theirs_full_1.vhdx", "job2");

        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = dest.display().to_string();

        delete_job_files(&store, &job);
        assert!(!dest.join("mine_full_1.vhdx").exists());
        assert!(dest.join("theirs_full_1.vhdx").exists(), "чужое остаётся нетронутым");
    }

    /// Папка, в которую задание писало РАНЬШЕ, известна только из журнала —
    /// и она тоже просматривается.
    #[test]
    fn a_folder_the_job_used_earlier_is_searched_too() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let store = a_store(dir.path());

        let old_image = put_image_of_job(&old, "j_C_full_1.vhdx", "job1");
        put_image_of_job(&new, "j_C_full_2.vhdx", "job1");

        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = new.display().to_string();
        store.upsert(job.clone()).unwrap();
        store
            .record_run(JobRun {
                job_id: "job1".into(),
                status: "success".into(),
                image_path: old_image.display().to_string(),
                ..Default::default()
            })
            .unwrap();

        let report = delete_job_files(&store, &store.get("job1").unwrap());
        assert!(!old.join("j_C_full_1.vhdx").exists(), "старая папка: {report:?}");
        assert!(!new.join("j_C_full_2.vhdx").exists(), "новая папка: {report:?}");
    }

    /// Занятый файл не удаляется — и об этом говорится, а не умалчивается.
    #[test]
    fn a_file_that_cannot_go_is_named_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let store = a_store(dir.path());

        let image = put_image_of_job(&dest, "j_C_full_1.vhdx", "job1");
        std::fs::remove_file(&image).unwrap();
        std::fs::create_dir(&image).unwrap(); // модель «удалить нельзя»

        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = dest.display().to_string();

        let report = delete_job_files(&store, &job);
        assert!(!report.complete(), "{report:?}");
        assert!(report.kept.iter().any(|(p, _)| p.contains("j_C_full_1.vhdx")), "{report:?}");
    }

    /// Каждый том запуска записывается в history, а не только первый.
    #[test]
    fn a_runs_images_are_all_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let store = a_store(dir.path());
        store
            .record_run(JobRun {
                job_id: "job1".into(),
                image_path: "/dest/first.vhdx".into(),
                base_image_path: "/dest/first.vhdx".into(),
                images: vec![
                    RunImage {
                        source: "C:".into(),
                        image_path: "/dest/first.vhdx".into(),
                        base_image_path: "/dest/first.vhdx".into(),
                        kind: "full".into(),
                    },
                    RunImage {
                        source: "D:".into(),
                        image_path: "/dest/second.vhdx".into(),
                        base_image_path: "/dest/second.vhdx".into(),
                        kind: "full".into(),
                    },
                ],
                ..Default::default()
            })
            .unwrap();

        let found = store.job_image_files("job1");
        assert!(found.contains(&PathBuf::from("/dest/second.vhdx")), "{found:?}");
    }

    // --- Цепочка выбирается по тому, ЧТО это за том (аудит AUD-10) ---------
    //
    // Токен в имени файла — это буква диска, а для зашифрованного part:-тома
    // без буквы — его очищенная метка. Два тома с одинаковой меткой на этом
    // уровне неразличимы, и инкремент мог лечь на копию другого диска.

    /// Имя совпало, а идентичность — нет: такую цепочку брать нельзя.
    #[test]
    fn a_chain_of_another_volume_with_the_same_label_is_not_ours() {
        let base = own_backup_backup::chains::ChainImage {
            image_path: PathBuf::from("j_KUBUNTUBOOT_full_1.vhdx"),
            sidecar_path: PathBuf::from("j_KUBUNTUBOOT_full_1.vhdx.obkm"),
            role: own_backup_backup::sidecar::ChainRole::Full,
            container: "vhdx".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            parent_file_name: None,
            source_id: Some(r"\?\VOLUME{11111111-1111-1111-1111-111111111111}".into()),
            consolidated_increments: 0,
            chain_started_at: None,
        };
        // Тот же токен в имени, другой том.
        assert!(!chain_belongs_to(
            &base,
            "KUBUNTUBOOT",
            &Some(r"\?\Volume{22222222-2222-2222-2222-222222222222}".into()),
        ));
        // Тот же том — берём, регистр значения не имеет.
        assert!(chain_belongs_to(
            &base,
            "kubuntuboot",
            &Some(r"\?\volume{11111111-1111-1111-1111-111111111111}".into()),
        ));
    }

    /// Старая копия ничего про себя не записала — тогда имя это всё, что есть,
    /// и поведение ровно прежнее. Совместимая половина правки: она никогда не
    /// начинает смешивать источники, она только перестаёт.
    #[test]
    fn a_copy_that_never_recorded_who_it_was_still_matches_by_name() {
        let base = own_backup_backup::chains::ChainImage {
            image_path: PathBuf::from("j_C_full_1.vhdx"),
            sidecar_path: PathBuf::from("j_C_full_1.vhdx.obkm"),
            role: own_backup_backup::sidecar::ChainRole::Full,
            container: "vhdx".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            parent_file_name: None,
            source_id: None,
            consolidated_increments: 0,
            chain_started_at: None,
        };
        assert!(chain_belongs_to(&base, "C", &Some("whatever".into())));
        assert!(chain_belongs_to(&base, "C", &None));
        // Имя всё ещё должно совпадать — это не отменяется.
        assert!(!chain_belongs_to(&base, "D", &None));
    }

    /// Ноль значит «выключено» — как у keep_chains, cleanup_older_than_days и
    /// rolling_incrementals, и как обещает справка CLI. Оба места писали
    /// `full_every_n.max(1)`, поэтому ноль становился единицей и «не начинать
    /// новую цепочку» давало самую агрессивную настройку из возможных: полную
    /// копию после каждого инкремента. Поймано живым прогоном 2026-09-20.
    #[test]
    fn zero_means_never_rolling_over_not_rolling_over_every_time() {
        let mut s = SchemeConfig { method: "incremental".into(), ..Default::default() };
        s.full_every_n = 0;
        assert_eq!(s.rolls_over_after(), None, "ноль — это выключено");

        s.full_every_n = 3;
        assert_eq!(s.rolls_over_after(), Some(3));

        // Единица — законная настройка: новая полная после каждого инкремента.
        s.full_every_n = 1;
        assert_eq!(s.rolls_over_after(), Some(1));

        // Отдельный флаг говорит то же самое и по-прежнему главнее.
        s.only_incremental = true;
        assert_eq!(s.rolls_over_after(), None);
    }

    /// То же самое через планировщик: с нулём третий прогон обязан остаться
    /// инкрементом, а не начать вторую цепочку.
    #[test]
    fn with_rollover_off_a_chain_keeps_growing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, run, _) = job_with_last(dir.path(), "incremental", "incremental", 1, true);
        job.scheme.full_every_n = 0;
        assert_eq!(plan_run(&job, Some(&run)).kind, "incremental");

        // А с единицей — начинает, и это по-прежнему работает.
        job.scheme.full_every_n = 1;
        assert_eq!(plan_run(&job, Some(&run)), RunPlan::full());
    }
}
