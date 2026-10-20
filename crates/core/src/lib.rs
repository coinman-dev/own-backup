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
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod backup_run;
pub mod busy;
pub mod capabilities;
mod engine_watch;
pub mod restore_exec;
pub mod source_refresh;
pub mod verification;

/// Where a destination folder sits relative to a job's sources — re-exported
/// so callers do not have to reach into the backup crate for one enum.
pub use own_backup_backup::source_identity::DestPlacement;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Read speed of a verify pass, averaged over the last second.
///
/// The verify callback counts BLOCKS, and a block count says nothing to a
/// person watching a 85 GiB image go by. This turns it into MiB/s the same way
/// the copy phase reports it, over a one-second window so the number is steady
/// enough to read but still follows a disk that slows down.
struct VerifyRate {
    block_bytes: u64,
    window_start: std::time::Instant,
    window_blocks: u64,
    mbps: f64,
}

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
            && matches!(
                self.event_trigger.as_str(),
                "dest_connected" | "usb_connect"
            )
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
/// actually fine, so it costs nothing to ask first. Only a source that IS a
/// drive counts: a mount point `C:\Mount\Data` is another volume, and read
/// as `C` it refused copies to `C:\Backups` (stand, 2026-10-18).
pub fn dest_collision_letter(job: &Job, dest_folder: &str) -> Option<char> {
    let dst = drive_letter_of(dest_folder)?;
    job.source_list()
        .iter()
        .filter_map(|s| own_backup_backup::source_identity::direct_drive_letter(s))
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
pub fn dest_placement(
    job: &Job,
    dest_folder: &str,
) -> own_backup_backup::source_identity::DestPlacement {
    own_backup_backup::source_identity::where_the_destination_sits(&job.source_list(), dest_folder)
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
    /// backup (also the default for old history) | verify.
    pub operation: String,
    pub verification: Option<own_backup_common::engine_dto::JobVerifySummary>,
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
    /// The rules DID run, but the excluded files' catalog entries could not be
    /// deleted from the image — why, in words; empty when they were. Their
    /// contents are not in the copy either way. Kept apart from
    /// `file_scan_skipped` because it means the opposite thing: that one says
    /// the rules never ran, and this run's "Исключено файлов: N" is true.
    #[serde(default)]
    pub excluded_entries_kept: String,
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
    /// The partitions this job copies moved, grew, shrank or were recreated
    /// since the previous run, and the job followed them — in words for the
    /// result screen; empty when nothing changed. Windows Update does this to
    /// the recovery partition, and the copy that follows is a full one, so the
    /// person seeing a large copy after a quiet week is told why.
    #[serde(default)]
    pub layout_changed: String,
    /// Other things the person should know about how this copy was made, one
    /// sentence each, for the result screen: a snapshot store that had to be
    /// allocated in full on a destination without sparse files, the service's
    /// key directory left out of the copy. Empty when there is nothing to say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The run was refused before anything was touched, for a reason in the
    /// job itself (an option this engine does not carry out, a format change
    /// over existing copies, the destination on the source). Trying again
    /// cannot change that answer, so such a run is not retried — only an edit
    /// of the job can.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    /// Why this run is not in the history files although it happened — set
    /// only on what the store hands out while the write keeps failing, never
    /// written to those files itself (`JobStore::unrecorded`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub record_error: String,
}

/// A persisted backup job (task).
/// Whole-disk (bootable) backup metadata attached to a [`Job`]: which physical
/// disk it captures and how each backed-up partition maps to a GPT slot. Used
/// after the run to write the `.obdm` disk-restore manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiskSourceSpec {
    /// Physical drive number (`\\.\PhysicalDrive{disk_number}`).
    pub disk_number: u32,
    /// The disk on Linux: its stable device, `/dev/disk/by-id/…` (decision
    /// R1) — a number names no particular disk there. `None` on Windows and
    /// in every job saved before Linux copied whole disks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
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

impl DiskSourceSpec {
    /// The token the disk's images are named and chained by: `disk<N>` on
    /// Windows; on Linux `disk` and the token of its device
    /// (`source_identity::unix_device_token`), which a by-id name keeps
    /// across reboots while `sdb` does not.
    pub fn file_tag(&self) -> String {
        match self
            .device
            .as_deref()
            .and_then(own_backup_backup::source_identity::unix_device_token)
        {
            Some(token) => format!("disk{token}"),
            None => format!("disk{}", self.disk_number),
        }
    }
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
    /// Folders this job wrote into before `dest_folder` was changed, oldest
    /// first. Kept by the store alone — whatever a client sends is ignored —
    /// so that deleting the job finds the copies there even after the
    /// activity journal (capped at [`ACTIVITY_CAP`] runs) forgot them.
    #[serde(default)]
    pub previous_dest_folders: Vec<String>,
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
    pub verification: verification::VerificationConfig,
    pub scheme: SchemeConfig,
    pub exclusions: ExclusionsConfig,
    pub advanced: AdvancedConfig,
    pub created_at: String,
    pub updated_at: String,
    pub last_run: Option<JobRun>,
    pub last_verification: Option<JobRun>,
    /// Images refused as incremental parents after a failed integrity check.
    pub invalid_images: Vec<String>,
    /// A check could not enumerate/read the job; require a fresh full copy.
    pub verification_blocked: bool,
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
            previous_dest_folders: Vec::new(),
            image_format: own_backup_common::types::ImageFormat::default(),
            archive_encryption: None,
            compression_level: None,
            schedule: ScheduleConfig::default(),
            verification: verification::VerificationConfig::default(),
            scheme: SchemeConfig::default(),
            exclusions: ExclusionsConfig::default(),
            advanced: AdvancedConfig::default(),
            created_at: String::new(),
            updated_at: String::new(),
            last_run: None,
            last_verification: None,
            invalid_images: Vec::new(),
            verification_blocked: false,
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
            self.sources
                .iter()
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .collect()
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
    /// Runs held in memory whose write to the history files failed, by run
    /// id, with the error. Until a later write succeeds they exist only here,
    /// and a restart of the service would lose them; whoever looks at the job
    /// is told so (`record_error`) rather than shown an ordinary run.
    unrecorded: Mutex<std::collections::HashMap<String, String>>,
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

/// Check again after emitting progress: a CancelRunning request can arrive
/// while the client reacts to that event. Keep cancellation set until the
/// worker starts a new run, including callbacks the engine must finalize safely.
fn backup_callback_checkpoint(cancel: &AtomicBool) -> own_backup_common::Result<()> {
    if cancel.load(Ordering::SeqCst) {
        Err(own_backup_common::OwnBackupError::Cancelled)
    } else {
        Ok(())
    }
}

/// Acceptance-only pause at a real production progress callback. It does not
/// manufacture cancellation: the ordinary checkpoint below observes the RPC flag.
#[cfg(all(target_os = "windows", feature = "a07-lab-gate"))]
fn a07_lab_gate(job: &str, phase: &str, done: u64, total: u64) -> own_backup_common::Result<()> {
    use own_backup_common::OwnBackupError;
    let root = std::path::Path::new(r"C:\Lab\a07-gate");
    let Ok(text) = std::fs::read_to_string(root.join("request")) else {
        return Ok(());
    };
    let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(());
    };
    if request["job"].as_str() != Some(job) || request["phase"].as_str() != Some(phase) {
        return Ok(());
    }
    let ready = root.join("ready");
    if ready.exists() {
        return Ok(());
    }
    let report = serde_json::json!({"job":job,"phase":phase,"done":done,"total":total});
    std::fs::write(&ready, report.to_string()).map_err(OwnBackupError::Io)?;
    info!(
        job,
        phase, done, total, "A07 lab gate waiting at actual progress callback"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !root.join("release").exists() {
        if std::time::Instant::now() > deadline {
            return Err(OwnBackupError::Device("A07 lab gate timeout".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    info!(job, phase, done, total, "A07 lab gate released");
    Ok(())
}

impl JobStore {
    pub fn load(data_dir: &std::path::Path) -> Arc<Self> {
        Self::try_load(data_dir).expect("job store must be readable before use")
    }

    /// Open the store without treating permission or rename failures as an
    /// empty database. The service uses this so it refuses to start instead of
    /// overwriting job history it could not read.
    pub fn try_load(data_dir: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let jobs_path = data_dir.join("jobs.json");
        let activity_path = data_dir.join("activity.json");
        let jobs: Vec<Job> = read_json(&jobs_path)?.unwrap_or_default();
        // Serde defaults keep old job fields compatible, but also turn a
        // structurally wrong row into a job with no identity. Starting with
        // that row would allow the local-key sweep to retire real authorizations.
        let mut identities = std::collections::HashSet::new();
        if jobs
            .iter()
            .any(|job| job.id.trim().is_empty() || !identities.insert(&job.id))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                tr!("lib.job_store_invalid_identities").into_owned(),
            ));
        }
        let activity = read_json(&activity_path)?.unwrap_or_default();
        Ok(Arc::new(Self {
            jobs_path,
            activity_path,
            jobs: Mutex::new(jobs),
            activity: Mutex::new(activity),
            unrecorded: Mutex::new(std::collections::HashMap::new()),
        }))
    }

    pub fn list(&self) -> Vec<Job> {
        let mut jobs = self.jobs.lock().unwrap().clone();
        for job in &mut jobs {
            self.mark_unrecorded(job);
        }
        jobs
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        let mut job = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()?;
        self.mark_unrecorded(&mut job);
        Some(job)
    }

    /// Say on the copies handed out which of their runs are not on disk.
    fn mark_unrecorded(&self, job: &mut Job) {
        let unrecorded = self.unrecorded.lock().unwrap();
        if unrecorded.is_empty() {
            return;
        }
        for run in [job.last_run.as_mut(), job.last_verification.as_mut()]
            .into_iter()
            .flatten()
        {
            if let Some(error) = unrecorded.get(&run.id) {
                run.record_error = tr!("lib.run_not_recorded", error = error).into_owned();
            }
        }
    }

    /// Write the history again when a run could not be recorded before.
    /// `Ok(true)` when something was waiting and is now on disk.
    pub fn retry_unrecorded(&self) -> std::io::Result<bool> {
        if self.unrecorded.lock().unwrap().is_empty() {
            return Ok(false);
        }
        write_json(&self.jobs_path, &*self.jobs.lock().unwrap())?;
        write_json(&self.activity_path, &*self.activity.lock().unwrap())?;
        self.unrecorded.lock().unwrap().clear();
        Ok(true)
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

    fn upsert_inner(&self, job: Job, authorized_encryption: bool) -> std::io::Result<Job> {
        self.upsert_inner_checked(job, authorized_encryption, None)
    }

    fn upsert_inner_checked(
        &self,
        mut job: Job,
        authorized_encryption: bool,
        expected_revision: Option<&str>,
    ) -> std::io::Result<Job> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut jobs = self.jobs.lock().unwrap();
        let previous = jobs.iter().find(|existing| existing.id == job.id);
        if let Some(expected) = expected_revision {
            let unchanged = match previous {
                Some(existing) => existing.updated_at == expected,
                None => expected.is_empty(),
            };
            if !unchanged {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    tr!("lib.job_changed_during_encryption").into_owned(),
                ));
            }
        }
        job.verification
            .validate()
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        backup_encryption::validate_update(&mut job, previous, authorized_encryption)?;
        let mut next = jobs.clone();
        if job.id.is_empty() {
            job.id = uuid::Uuid::new_v4().to_string();
            job.created_at = now.clone();
            job.updated_at = now;
            job.last_run = None; // only the worker may create run history
            job.last_verification = None;
            job.invalid_images.clear();
            job.verification_blocked = false;
            job.previous_dest_folders.clear();
            next.push(job.clone());
        } else if let Some(existing) = next.iter_mut().find(|j| j.id == job.id) {
            job.previous_dest_folders = existing.previous_dest_folders.clone();
            let old = existing.dest_folder.trim();
            if !old.is_empty()
                && !old.eq_ignore_ascii_case(job.dest_folder.trim())
                && !job
                    .previous_dest_folders
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(old))
            {
                job.previous_dest_folders.push(old.to_string());
            }
            job.created_at = existing.created_at.clone();
            job.last_run = existing.last_run.clone(); // preserve run history
            job.last_verification = existing.last_verification.clone();
            job.invalid_images = existing.invalid_images.clone();
            job.verification_blocked = existing.verification_blocked;
            job.updated_at = now;
            *existing = job.clone();
        } else {
            job.created_at = now.clone();
            job.updated_at = now;
            job.last_run = None; // a caller-selected id is still a new job
            job.last_verification = None;
            job.invalid_images.clear();
            job.verification_blocked = false;
            job.previous_dest_folders.clear();
            next.push(job.clone());
        }
        write_json(&self.jobs_path, &next)?;
        *jobs = next;
        Ok(job)
    }

    /// Write back what [`source_refresh`] found: the job's devices, disk number
    /// and disk snapshot — and nothing else, so an edit saved meanwhile keeps
    /// its other fields. Returns the stored job.
    pub fn refresh_sources(&self, refreshed: &Job) -> std::io::Result<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let mut next = jobs.clone();
        let Some(job) = next.iter_mut().find(|j| j.id == refreshed.id) else {
            // Deleted while it waited for the slot: nothing left to update.
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "job not found",
            ));
        };
        job.source = refreshed.source.clone();
        job.sources = refreshed.sources.clone();
        job.disk_source = refreshed.disk_source.clone();
        job.source_disks = refreshed.source_disks.clone();
        job.updated_at = chrono::Utc::now().to_rfc3339();
        let saved = job.clone();
        write_json(&self.jobs_path, &next)?;
        *jobs = next;
        Ok(saved)
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
            if let Some(run) = jobs
                .iter()
                .find(|j| j.id == job_id)
                .and_then(|j| j.last_run.as_ref())
            {
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
                if run.operation == "verify" {
                    job.last_verification = Some(run.clone());
                    if run.status != "cancelled" {
                        job.verification_blocked =
                            run.status == "error" && run.verification.is_none();
                    }
                    if let Some(summary) = &run.verification {
                        for image in &summary.images {
                            job.invalid_images
                                .retain(|p| !verification::same_image(p, &image.image_path));
                            if !image.valid {
                                job.invalid_images.push(image.image_path.clone());
                            }
                        }
                    }
                } else {
                    job.last_run = Some(run.clone());
                }
            }
        }
        let id = run.id.clone();
        {
            let mut activity = self.activity.lock().unwrap();
            activity.insert(0, run);
            activity.truncate(ACTIVITY_CAP);
        }
        let jobs = write_json(&self.jobs_path, &*self.jobs.lock().unwrap());
        let activity = write_json(&self.activity_path, &*self.activity.lock().unwrap());
        match jobs.and(activity) {
            // Both files hold everything in memory now, earlier failures too.
            Ok(()) => {
                self.unrecorded.lock().unwrap().clear();
                Ok(())
            }
            Err(e) => {
                self.unrecorded.lock().unwrap().insert(id, e.to_string());
                Err(e)
            }
        }
    }

    pub fn activity(&self) -> Vec<JobRun> {
        let mut activity = self.activity.lock().unwrap().clone();
        let unrecorded = self.unrecorded.lock().unwrap();
        for run in activity.iter_mut() {
            if let Some(error) = unrecorded.get(&run.id) {
                run.record_error = tr!("lib.run_not_recorded", error = error).into_owned();
            }
        }
        activity
    }

    /// Most recent successful run of a job. Chain planning keys on this, not
    /// on `last_run`: a failed or cancelled run must not reset an incremental
    /// chain back to a full backup.
    pub fn last_success(&self, job_id: &str) -> Option<JobRun> {
        self.activity
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.job_id == job_id && r.status == "success" && r.operation != "verify")
            .cloned()
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> std::io::Result<Option<T>> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&data) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            // Unreadable is not the same as absent, and the difference matters:
            // starting empty means the next save writes an empty list over
            // whatever was there. The file is moved aside first, so the jobs
            // are recoverable by hand instead of gone.
            let mut kept = path.with_extension("json.bad");
            if kept.exists() {
                kept = path.with_extension(format!("json.bad.{}", uuid::Uuid::new_v4()));
            }
            std::fs::rename(path, &kept)?;
            error!(
                path = %path.display(),
                error = %e,
                kept = %kept.display(),
                "store file could not be read - starting empty, the old one is kept"
            );
            Ok(None)
        }
    }
}

pub(crate) fn write_json<T: Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    // Pushed out of the cache BEFORE the rename. Without it the rename can
    // land while the bytes have not, and a crash then leaves a file that
    // exists, is the right size and holds nothing.
    use std::io::Write as _;
    let mut f = create_private(&tmp)?;
    let done = restrict_private(&f)
        .and_then(|()| f.write_all(&bytes))
        .and_then(|()| f.sync_all());
    drop(f);
    // A disk that filled up mid-write left the half-written file behind,
    // still holding the space the next save needs (stand, 2026-10-17). Only
    // a file this call created: a failed create has returned above.
    done.and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    sync_parent(path);
    Ok(())
}

/// The temporary file of [`write_json`]. On Unix owner-only from the first
/// instant: the files hold the jobs and the activity journal with their
/// Argon2id-sealed archive keys, and the umask made them world-readable —
/// an offline password-guessing target for every local user.
#[cfg(unix)]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// A temporary file a crash left behind keeps the mode it was created with,
/// and the rename carries it over to the real file.
#[cfg(unix)]
fn restrict_private(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_private(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// Make the rename itself durable. The new contents are already visible, so a
/// failure here is logged rather than reported as a failed save: the caller
/// would roll back its memory to the old state while the file holds the new.
#[cfg(unix)]
fn sync_parent(path: &std::path::Path) {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    if let Err(e) = std::fs::File::open(parent).and_then(|d| d.sync_all()) {
        warn!(path = %path.display(), error = %e, "directory not synced after the rename");
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &std::path::Path) {}

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
    background_operations: usize,
}

/// Keeps scheduled checks visible to the after-backup power decision while
/// they wait for, or hold, the shared operation reservation.
pub struct BackgroundOperation(RunQueue);

impl Drop for BackgroundOperation {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().background_operations -= 1;
    }
}

impl RunQueue {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueState::default())),
        }
    }

    pub fn register_background_operation(&self) -> BackgroundOperation {
        self.state.lock().unwrap().background_operations += 1;
        BackgroundOperation(self.clone())
    }

    pub fn is_idle(&self) -> bool {
        let q = self.state.lock().unwrap();
        q.current.is_none() && q.pending.is_empty() && q.background_operations == 0
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let q = self.state.lock().unwrap();
        QueueSnapshot {
            current: q.current.clone(),
            pending: q.pending.iter().cloned().collect(),
        }
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
        let Some(at) = q.pending.iter().position(|j| j == id) else {
            return false;
        };
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
    /// Seconds the engine has gone without any progress, once that is long
    /// enough to say — a destination that stopped taking writes holds a run
    /// like this for as long as it fails (`engine_watch`). `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_secs: Option<u64>,
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

/// Password store for encrypted volumes. The installed Windows service uses a
/// private database under its SYSTEM-only key directory; foreground/WinPE
/// Windows instances can use the memory cache but cannot persist passwords.
pub struct PasswordStore {
    cache: std::sync::Mutex<std::collections::HashMap<String, zeroize::Zeroizing<String>>>,
    db_path: PathBuf,
    private: bool,
    /// Key-file path for the OS secret store (used only off Windows): on
    /// Linux only the legacy key a private store migrates away from, and the
    /// key of a foreground (development) instance.
    key_path: PathBuf,
    /// What seals the private database's passwords when it is not the
    /// platform default: on Linux the machine key of the service's key store
    /// (`systemd-creds`). `None` on Windows (DPAPI, as before).
    sealer: Option<Arc<dyn own_backup_backup::secrets::SecretStore + Send + Sync>>,
    /// `false` when nothing may be persisted: the Linux service whose key
    /// store could not be opened keeps passwords for this run only.
    persist: bool,
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
            private: false,
            key_path,
            sealer: None,
            persist: true,
        }
    }

    /// Use a database whose parent directory was validated as service-only
    /// before SQLite was allowed to create the database or its WAL/SHM files.
    pub fn new_private(public_db_path: PathBuf, private_db_path: PathBuf) -> Self {
        let key_path = public_db_path
            .parent()
            .map(|d| d.join("secret.key"))
            .unwrap_or_else(|| PathBuf::from("secret.key"));
        Self {
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            db_path: private_db_path,
            private: true,
            key_path,
            sealer: None,
            persist: true,
        }
    }

    /// The Linux service's private database (in its root-only key directory,
    /// excluded from every copy), its passwords sealed by `sealer` — the key
    /// store's machine key. Legacy passwords sealed by `secret.key` beside the
    /// public database are re-sealed by [`Self::migrate_legacy`].
    pub fn new_private_sealed(
        public_db_path: PathBuf,
        private_db_path: PathBuf,
        sealer: Arc<dyn own_backup_backup::secrets::SecretStore + Send + Sync>,
    ) -> Self {
        Self {
            sealer: Some(sealer),
            ..Self::new_private(public_db_path, private_db_path)
        }
    }

    /// A store that remembers passwords for this run only: what the
    /// installed Linux service falls back to when its key store cannot be
    /// opened, instead of not starting (or keeping them beside public data).
    pub fn memory_only(db_path: PathBuf) -> Self {
        Self {
            persist: false,
            ..Self::new(db_path)
        }
    }

    /// Seals and opens what this store persists.
    fn store(&self) -> Arc<dyn own_backup_backup::secrets::SecretStore + Send + Sync> {
        match &self.sealer {
            Some(s) => s.clone(),
            None => Arc::from(own_backup_backup::secrets::default_store(&self.key_path)),
        }
    }

    /// Move legacy machine-scope blobs out of the public history database.
    /// Each private insert is committed and read through another connection
    /// before its public row is deleted. A durable marker resumes cleanup if a
    /// crash occurs after the last delete but before VACUUM/checkpoint.
    pub fn migrate_legacy(&self, public_db_path: &Path) -> own_backup_common::Result<usize> {
        if !self.private {
            return Err(own_backup_common::OwnBackupError::Encryption(
                "private volume-secret database is required for migration".into(),
            ));
        }
        let private = own_backup_metadata::open_volume_secret_database(&self.db_path)?;
        let public = own_backup_metadata::open_database(public_db_path)?;
        public
            .execute_batch("PRAGMA secure_delete=ON;")
            .map_err(|e| own_backup_common::OwnBackupError::Database(e.to_string()))?;
        let legacy = own_backup_metadata::list_volume_secrets(&public)?;
        if legacy.is_empty()
            && !own_backup_metadata::volume_secret_migration_pending(&private)?
            && own_backup_metadata::volume_secret_migration_done(&private)?
        {
            // A run that died between the marker and the removal below, or
            // whose removal failed, leaves the key: it goes now.
            self.remove_legacy_key();
            return Ok(0);
        }
        own_backup_metadata::mark_volume_secret_migration_pending(&private)?;
        // Legacy rows were sealed by the platform default; private ones by
        // this store's own sealer. The same thing on Windows (DPAPI), where a
        // row moves as it is; on Linux a row is re-sealed under the machine
        // key on its way.
        let os_store = own_backup_backup::secrets::default_store(&self.key_path);
        let private_store = self.store();
        let mut moved = 0;
        for (key, blob) in legacy {
            let old_plain = os_store.unprotect(&blob)?;
            if let Some(existing) = own_backup_metadata::get_volume_secret(&private, &key)? {
                let existing_plain = private_store.unprotect(&existing)?;
                if existing_plain.as_slice() != old_plain.as_slice() {
                    return Err(own_backup_common::OwnBackupError::Encryption(format!(
                        "conflicting private and legacy passwords for volume {key}"
                    )));
                }
            } else if self.sealer.is_some() {
                let resealed = private_store.protect(&old_plain)?;
                own_backup_metadata::upsert_volume_secret(&private, &key, &resealed)?;
            } else {
                own_backup_metadata::upsert_volume_secret(&private, &key, &blob)?;
            }
            let confirmed = own_backup_metadata::open_volume_secret_database(&self.db_path)?;
            let persisted =
                own_backup_metadata::get_volume_secret(&confirmed, &key)?.ok_or_else(|| {
                    own_backup_common::OwnBackupError::Database(format!(
                        "private password for volume {key} was not committed"
                    ))
                })?;
            if private_store.unprotect(&persisted)?.as_slice() != old_plain.as_slice() {
                return Err(own_backup_common::OwnBackupError::Encryption(format!(
                    "private password verification failed for volume {key}"
                )));
            }
            drop(confirmed);
            own_backup_metadata::delete_volume_secret(&public, &key)?;
            moved += 1;
        }
        own_backup_metadata::purge_legacy_volume_secret_remnants(&public)?;
        own_backup_metadata::clear_volume_secret_migration_pending(&private)?;
        own_backup_metadata::mark_volume_secret_migration_done(&private)?;
        self.remove_legacy_key();
        Ok(moved)
    }

    /// Nothing sealed by the legacy key is left once the migration is done,
    /// and the key itself sits beside the public data — inside every copy of
    /// that volume. Gone, when this store has a sealer of its own.
    fn remove_legacy_key(&self) {
        if self.sealer.is_some() {
            match std::fs::remove_file(&self.key_path) {
                Ok(()) => info!("legacy volume-password key removed"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(error = %e, "legacy volume-password key could not be removed"),
            }
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
            if !self.persist {
                return Err(own_backup_common::OwnBackupError::Encryption(
                    "the service key store is unavailable: volume passwords cannot be remembered"
                        .into(),
                ));
            }
            #[cfg(target_os = "windows")]
            if !self.private {
                return Err(own_backup_common::OwnBackupError::Encryption(
                    "only the installed service can persist volume passwords".into(),
                ));
            }
            let blob = self.store().protect(password.as_bytes())?;
            let conn = if self.private {
                own_backup_metadata::open_volume_secret_database(&self.db_path)?
            } else {
                own_backup_metadata::open_database(&self.db_path)?
            };
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
        if !self.persist {
            return Ok(());
        }
        #[cfg(target_os = "windows")]
        if !self.private {
            return Err(own_backup_common::OwnBackupError::Encryption(
                "only the installed service can remove persisted volume passwords".into(),
            ));
        }
        let conn = if self.private {
            own_backup_metadata::open_volume_secret_database(&self.db_path)?
        } else {
            own_backup_metadata::open_database(&self.db_path)?
        };
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
        if !self.persist {
            return None;
        }
        #[cfg(target_os = "windows")]
        if !self.private {
            return None;
        }
        // Fall back to the OS-protected store (a remembered password).
        let conn = if self.private {
            own_backup_metadata::open_volume_secret_database(&self.db_path).ok()?
        } else {
            own_backup_metadata::open_database(&self.db_path).ok()?
        };
        let blob = own_backup_metadata::get_volume_secret(&conn, volume_token).ok()??;
        let plain = self.store().unprotect(&blob).ok()?;
        Some(zeroize::Zeroizing::new(
            String::from_utf8(plain.to_vec()).ok()?,
        ))
    }
}

/// Holds a copy to `speed_limit_mbps` by sleeping in its progress callback.
///
/// The engine calls that callback on the copying thread after every block it
/// reads (1 MiB), so a sleep there is a pause of the copy itself — no second
/// thread, nothing inside the engine. The pace is measured from the first
/// block of each copy: a volume of a multi-volume job starts its own.
struct Pace {
    bytes_per_sec: f64,
    block_bytes: f64,
    started: Option<std::time::Instant>,
}

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
        Self(
            low_priority
                .then(own_backup_backup::power_win::BackgroundIo::enter)
                .flatten(),
        )
    }
}

/// The same on Linux: `ioprio`/`nice` of the copying thread.
#[cfg(target_os = "linux")]
struct QuietCopy(#[allow(dead_code)] Option<own_backup_backup::power_linux::BackgroundIo>);

#[cfg(target_os = "linux")]
impl QuietCopy {
    fn enter(low_priority: bool) -> Self {
        Self(
            low_priority
                .then(own_backup_backup::power_linux::BackgroundIo::enter)
                .flatten(),
        )
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
    if !queue.is_idle() {
        info!(job = %job_name, action, "after backup: other work is queued — skipped");
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let (action, job_name, queue) = (action.to_string(), job_name.to_string(), queue.clone());
        tokio::spawn(async move {
            if !queue.is_idle() {
                return;
            }
            let result = if action == "shutdown" {
                own_backup_backup::power_win::shutdown_in(
                    SHUTDOWN_GRACE_SECS,
                    &tr!("lib.shutdown_message", job = job_name),
                )
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(SLEEP_AFTER_SECS)).await;
                if !queue.is_idle() {
                    info!(job = %job_name, "after backup: work was queued meanwhile — not sleeping");
                    return;
                }
                own_backup_backup::power_win::sleep_now()
            };
            if let Err(e) = result {
                warn!(job = %job_name, action = %action, error = %e, "after backup: failed");
            }
        });
    }
    // The same through systemd-logind: a scheduled poweroff that logind
    // announces to everyone signed in (`shutdown -c` cancels it), or a
    // suspend after the same minute's wait. The D-Bus calls block briefly,
    // so they run off the runtime's threads.
    #[cfg(target_os = "linux")]
    {
        let (action, job_name, queue) = (action.to_string(), job_name.to_string(), queue.clone());
        tokio::spawn(async move {
            if !queue.is_idle() {
                return;
            }
            let result = if action == "shutdown" {
                let message = tr!("lib.shutdown_message", job = job_name).into_owned();
                tokio::task::spawn_blocking(move || {
                    own_backup_backup::power_linux::shutdown_in(SHUTDOWN_GRACE_SECS, &message)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(SLEEP_AFTER_SECS)).await;
                if !queue.is_idle() {
                    info!(job = %job_name, "after backup: work was queued meanwhile — not sleeping");
                    return;
                }
                tokio::task::spawn_blocking(own_backup_backup::power_linux::sleep_now)
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            };
            if let Err(e) = result {
                warn!(job = %job_name, action = %action, error = %e, "after backup: failed");
            }
        });
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (SLEEP_AFTER_SECS, SHUTDOWN_GRACE_SECS);
        info!(job = %job_name, action, "after backup: not implemented on this platform");
    }
}

/// Bring the job's partitions up to date with the disk before it runs (see
/// [`source_refresh`]): the job to run, the note for the run record when its
/// partitions moved, and why it cannot run when one of them is gone.
#[cfg(target_os = "windows")]
async fn refresh_job_sources(store: &JobStore, job: Job) -> (Job, String, Option<String>) {
    use source_refresh::Refreshed;
    match source_refresh::reconcile_with_live_disks(&job).await {
        Refreshed::Unchanged => (job, String::new(), None),
        Refreshed::Missing(why) => (job, String::new(), Some(why)),
        Refreshed::Updated(r) => {
            for (old, new) in &r.renamed {
                info!(job = %job.name, old = %old, new = %new, "a source partition has a new device");
            }
            if !r.layout_note.is_empty() {
                warn!(job = %job.name, note = %r.layout_note, "the source disk was re-partitioned");
            }
            // Saved, so the screen and the next run see the disk as it is.
            // Not saved, the run still goes ahead with what was found: the
            // check repeats before every run.
            let job = match store.refresh_sources(&r.job) {
                Ok(saved) => saved,
                Err(e) => {
                    warn!(job = %job.name, error = %e, "the updated partitions could not be saved");
                    r.job
                }
            };
            (job, r.layout_note, None)
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn refresh_job_sources(_store: &JobStore, job: Job) -> (Job, String, Option<String>) {
    (job, String::new(), None)
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
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    // Retries used by each job's current run of failures. In memory: a
    // service restart forgets them, and the schedule carries on as usual.
    let mut retries_used: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    loop {
        let job_id = tokio::select! {
            next = rx.recv() => match next { Some(id) => id, None => break },
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
                continue;
            }
        };
        if *stop.borrow() {
            break;
        }
        // Retire a stop request aimed at the previous run before a NEW run
        // becomes visible as current. Once it is visible, never reset its flag.
        cancel.store(false, Ordering::SeqCst);
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
        let _slot = tokio::select! {
            slot = busy.acquire(crate::busy::Doing::new(tr!("lib.doing_backup"), job.name.clone())) => slot,
            _ = stop.changed() => {
                queue.finish();
                sink.queue_changed(queue.snapshot());
                break;
            }
        };
        if *stop.borrow() {
            queue.finish();
            sink.queue_changed(queue.snapshot());
            break;
        }

        // A key repair can commit while this queued run waits for the busy
        // slot. Load its new local reference only after acquiring the slot;
        // otherwise the retired reference from the old snapshot would fail.
        // A run an earlier write could not record is written again first.
        match store.retry_unrecorded() {
            Ok(true) => info!("runs that could not be recorded earlier are now in the history"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, "the history still cannot be written"),
        }
        let Some(job) = store.get(&job_id) else {
            queue.finish();
            sink.queue_changed(queue.snapshot());
            continue;
        };
        // The partitions as they are NOW, not as they were when the job was
        // saved — Windows Update re-creates the recovery partition under it.
        let (job, layout_note, sources_missing) = refresh_job_sources(&store, job).await;

        // No sleep while copying — laptops included, verification included.
        // Released when this iteration ends, whatever the outcome.
        #[cfg(target_os = "windows")]
        let _awake = own_backup_backup::power_win::KeepAwake::new(&tr!(
            "lib.keep_awake_reason",
            job = job.name
        ));
        #[cfg(target_os = "linux")]
        let _awake = own_backup_backup::power_linux::KeepAwake::new(&tr!(
            "lib.keep_awake_reason",
            job = job.name
        ));
        let last_success = store.last_success(&job.id);
        // The keys of an encrypted job are opened here, once, before anything
        // runs — and if they cannot be opened the run fails saying so, rather
        // than waiting for a password nobody will type at night or writing the
        // copy in the clear (§7.2).
        let run = if cancel.load(Ordering::SeqCst) {
            let mut run = run_skeleton(&job);
            run.finished_at = chrono::Utc::now().to_rfc3339();
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_by_user").into_owned();
            run
        } else if let Some(why) = sources_missing {
            let mut run = run_skeleton(&job);
            run.finished_at = chrono::Utc::now().to_rfc3339();
            run.status = "error".into();
            run.error = why;
            run
        } else {
            match store.archive_keys_for_run(&job) {
                Err(message) => {
                    let mut run = run_skeleton(&job);
                    run.finished_at = chrono::Utc::now().to_rfc3339();
                    run.status = "error".into();
                    run.error = message;
                    run
                }
                Ok(keys) => {
                    perform_backup(&sink, &job, &cancel, last_success, &passwords, keys).await
                }
            }
        };
        let mut run = run;
        run.layout_changed = layout_note;
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
            // The backup happened; only the record of it did not. It stays in
            // memory, marked, and is written again before the next run; until
            // then the screen says so instead of showing an ordinary run.
            error!(error = %e, job = %job.name, "the finished run could not be written down");
            run.record_error = tr!("lib.run_not_recorded", error = e).into_owned();
        }

        queue.finish();
        sink.queue_changed(queue.snapshot());

        // A failure is tried again, a set number of times; anything else ends
        // the streak.
        let retrying = if run.status == "error" && !run.refused && !*stop.borrow() {
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
        if !retrying && run.status != "cancelled" && !*stop.borrow() {
            after_backup(&job.advanced.after_backup, &job.name, &queue);
        }
        sink.job_finished(run);
        if *stop.borrow() {
            break;
        }
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
    let last_kind = if last.backup_kind.is_empty() {
        "full"
    } else {
        &last.backup_kind
    };

    // "Полная версия после каждых N версий": the chain holds at most N children.
    if job
        .scheme
        .rolls_over_after()
        .is_some_and(|n| last.increment_index >= n)
    {
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

/// A short, filename-safe, underscore-free token identifying a source volume
/// within a job's images/chains. A lettered volume is its letter ("C:" → "C").
/// A letterless volume addressed by a GUID path (`\\?\Volume{1a2b…}`) becomes
/// `V` + the first 8 hex digits of the GUID — unique per volume, so several
/// letterless partitions (e.g. two EFI parts) never collide. A Linux device
/// path becomes `U`/`F` with hex digits of its PARTUUID / filesystem UUID, or
/// `H` with a hash of the path (`/dev/mapper/…`) — see
/// [`own_backup_backup::source_identity::unix_device_token`]. Falls back to
/// "X". Stable key for a source volume, used both for chain planning and to
/// look up a saved password. The GUI/service compute the same token to store
/// passwords; the window mirrors it in `lib/sourceToken.ts`.
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
    if let Some(rest) = d
        .strip_prefix(r"\\?\Volume{")
        .or_else(|| d.strip_prefix(r"\\.\Volume{"))
    {
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
    // A Linux device path. Asked before the letter, which read `/dev/sda2` as
    // "no letter": every Linux volume was "X" — one saved password, one file
    // name and one chain for all of them.
    if let Some(token) = own_backup_backup::source_identity::unix_device_token(d) {
        return token;
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
/// sanitized volume label followed by its partition token. The partition
/// suffix keeps two equally named, letterless volumes in different chains;
/// the result is underscore-free so the filename parser can recover it.
/// Distinct from [`source_token`], which stays the stable device key used for
/// passwords and cross-run identity.
fn file_token(job: &Job, device: &str) -> String {
    if device.trim_start().starts_with("part:") {
        let label = job.label_for(device);
        if label != device {
            let safe: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
            if !safe.is_empty() {
                return format!("{safe}{}", source_token(device));
            }
        }
    }
    // A mount point (`C:\Mount\Data`) is not the volume of the letter it
    // starts with: named `C` it shared its file names with a `C:` source of
    // the same job, run for run. Named by its own volume GUID instead, as a
    // GUID-path source is; chains still follow the volume's identity.
    let d = device.trim();
    if d.len() > 3
        && d.as_bytes()[1] == b':'
        && own_backup_backup::source_identity::direct_drive_letter(d).is_none()
    {
        if let Some(id) = own_backup_backup::source_identity::stable_source_id(d) {
            let hex: String = id
                .split_once('{')
                .map(|(_, rest)| {
                    rest.chars()
                        .filter(char::is_ascii_hexdigit)
                        .take(8)
                        .collect()
                })
                .unwrap_or_default();
            if hex.len() == 8 {
                return format!("V{}", hex.to_ascii_uppercase());
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
/// So when BOTH sides recorded who they are, that decides — alone. The name
/// is not asked then either: the same volume named `P:` one day and by its
/// GUID path or a mount point the next (or given a new letter) used to start
/// a new full chain, because its token changed with the name (stand,
/// 2026-10-18). When either side has no identity — an old copy, or a machine
/// that cannot answer — the name is all there is and the behaviour is
/// exactly what it was.
fn chain_belongs_to(
    base: &own_backup_backup::chains::ChainImage,
    token: &str,
    identity: &Option<String>,
) -> bool {
    match (identity, &base.source_id) {
        (Some(mine), Some(theirs)) => mine.eq_ignore_ascii_case(theirs),
        _ => image_token(&base.image_path).is_some_and(|t| t.eq_ignore_ascii_case(token)),
    }
}

/// Retention and consolidation are destructive: unlike parent lookup, an old
/// chain with no source identity cannot be assumed to belong to a current
/// source merely because its (possibly duplicated) label matches.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn chain_source_group_matches(
    base: &own_backup_backup::chains::ChainImage,
    token: &str,
    source_id: Option<&str>,
) -> bool {
    // Identity decides when both have one, as in [`chain_belongs_to`]: a chain
    // continued under a new name stays in its volume's group.
    let named = || image_token(&base.image_path).is_some_and(|t| t.eq_ignore_ascii_case(token));
    match (source_id, base.source_id.as_deref()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => named(),
        _ => false,
    }
}

/// Recover the source token that an image file name encodes. Names are
/// `<name>_<token>_<tag>_<stamp>.<ext>`; token/tag/stamp are always the last
/// three underscore-separated segments, so we read from the end — robust even
/// when `<name>` itself contains underscores.
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

/// Timestamped output file names of one image of a run:
/// `<name>_<token>_<full|inc|diff>_<stamp>.<ext>`.
///
/// The image this run writes, and the name it gets if the engine has to fall
/// back to a full (parent unusable, disk re-partitioned) — one timestamp for
/// both. The name says what the file IS: a full named `_inc_` was found in
/// the owner's folder on 2026-10-14, after a fallback the name never learned.
fn output_paths_for(job: &Job, token: &str, kind: &str) -> (PathBuf, PathBuf) {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    (
        image_name(job, token, kind, &stamp),
        image_name(job, token, "full", &stamp),
    )
}

fn image_name(job: &Job, token: &str, kind: &str, stamp: &str) -> PathBuf {
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
    let chains = own_backup_backup::chains::list_job_chains(folder, &job.id).unwrap_or_default();
    let want = container_extension(job);
    for image in chains.iter().flat_map(|c| c.images.iter()) {
        if image.container != want {
            let name = image
                .image_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some(
                tr!(
                    "lib.format_mismatch",
                    want = want,
                    found = image.container,
                    name = name
                )
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
/// Images one chain may hold before the next run starts a new one, whatever
/// the scheme says. The VHDX reader refuses a parent chain deeper than 64
/// (`vhdx_bat::MAX_CHAIN_DEPTH`): a job of "only increments" with no
/// consolidation — saved before the modes existed, or made outside the
/// window — grew past it on the stand (2026-10-18), every newer image read as
/// unreadable, retention stopped, and each run re-parented on image 63.
const MAX_CHAIN_IMAGES: usize = 60;

fn should_start_new_chain(
    job: &Job,
    chain: &own_backup_backup::chains::Chain,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if chain.images.len() >= MAX_CHAIN_IMAGES {
        info!(
            job = %job.name,
            images = chain.images.len(),
            "the chain is as long as it may get — this run starts a new one"
        );
        return true;
    }
    if job.scheme.is_consolidation() {
        return match job.scheme.cleanup_older_than_days {
            Some(days) => chain.age_days(now) >= days as i64,
            None => false,
        };
    }
    job.scheme
        .rolls_over_after()
        .is_some_and(|n| chain.increments_ever() >= n)
}

/// Any doubt (no chain, rollover reached, parent missing) resolves to a full.
pub fn plan_run_for_volume(job: &Job, folder: &Path, device: &str) -> RunPlan {
    plan_run_for_volume_at(job, folder, device, chrono::Utc::now())
}

/// [`plan_run_for_volume`] with the clock passed in, so the age-based rollover
/// can be tested without waiting three months.
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
    // Who this volume is, independently of what it is called (a volume GUID on
    // Windows, a PARTUUID / filesystem or device-mapper UUID on Linux). `None`
    // when the machine cannot say.
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
            c.images
                .first()
                .is_some_and(|base| chain_belongs_to(base, &token, &identity))
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

    if verification::chain_is_invalid(job, &chain) || should_start_new_chain(job, &chain, now) {
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
pub fn plan_run_for_disk(job: &Job, folder: &Path, disk_number: u32) -> RunPlan {
    plan_run_for_disk_at(job, folder, disk_number, chrono::Utc::now())
}

/// [`plan_run_for_disk`] with the clock passed in (see the volume twin).
pub fn plan_run_for_disk_at(
    job: &Job,
    folder: &Path,
    disk_number: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> RunPlan {
    plan_run_for_disk_tag_at(job, folder, &format!("disk{disk_number}"), now)
}

/// [`plan_run_for_disk`] for the disk whose images carry `tag`
/// ([`DiskSourceSpec::file_tag`]): how Linux names a disk (decision R1).
pub fn plan_run_for_disk_tag(job: &Job, folder: &Path, tag: &str) -> RunPlan {
    plan_run_for_disk_tag_at(job, folder, tag, chrono::Utc::now())
}

/// [`plan_run_for_disk_tag`] with the clock passed in.
pub fn plan_run_for_disk_tag_at(
    job: &Job,
    folder: &Path,
    tag: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> RunPlan {
    use own_backup_backup::chains::list_job_chains;

    let method: &'static str = match job.scheme.method.as_str() {
        "incremental" => "incremental",
        "differential" => "differential",
        _ => return RunPlan::full(),
    };

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
                .is_some_and(|t| t.eq_ignore_ascii_case(tag))
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

    if verification::chain_is_invalid(job, &chain) || should_start_new_chain(job, &chain, now) {
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
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn apply_retention(job: &Job, protected: &[(String, PathBuf)]) {
    use own_backup_backup::chains::{
        delete_chain, list_job_chains_strict, plan_retention_protected, RetentionRules,
    };

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
    let all_chains = match list_job_chains_strict(&folder, &job.id) {
        Ok(c) => c,
        Err(e) => {
            warn!(job = %job.name, error = %e, "Retention scan failed");
            return;
        }
    };

    // One pass per source identity, not per display label. Old images without
    // an identity are left alone when the current source has one.
    let protected_paths: std::collections::HashSet<PathBuf> =
        protected.iter().map(|(_, path)| path.clone()).collect();
    let mut seen = std::collections::HashSet::new();
    for (token, protect_image) in protected {
        let Some(current_chain) = all_chains.iter().find(|c| c.contains(protect_image)) else {
            warn!(job = %job.name, image = %protect_image.display(),
                "Retention: new image not found in a readable chain; skipping deletion");
            continue;
        };
        let source_id = current_chain
            .images
            .first()
            .and_then(|i| i.source_id.clone());
        if !seen.insert((token.to_ascii_uppercase(), source_id.clone())) {
            continue;
        }
        let vol_chains: Vec<_> = all_chains
            .iter()
            .filter(|c| {
                c.images.first().is_some_and(|base| {
                    chain_source_group_matches(base, token, source_id.as_deref())
                })
            })
            .cloned()
            .collect();
        if vol_chains.is_empty() {
            continue;
        }
        let doomed =
            plan_retention_protected(&vol_chains, &rules, chrono::Utc::now(), &protected_paths);
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
    use own_backup_backup::chains::{delete_chain, list_job_chains_strict};

    let mut out = JobFilesDeleted {
        history_truncated: store.history_is_truncated(&job.id),
        ..Default::default()
    };

    // Every folder we have reason to think this job wrote into: the one it is
    // configured for now, plus the parents of everything its history names.
    let from_history = store.job_image_files(&job.id);
    let mut folders: Vec<PathBuf> = Vec::new();
    let push_folder = |p: PathBuf, folders: &mut Vec<PathBuf>| {
        if !p.as_os_str().is_empty() && !folders.contains(&p) {
            folders.push(p);
        }
    };
    push_folder(PathBuf::from(&job.dest_folder), &mut folders);
    for f in &job.previous_dest_folders {
        push_folder(PathBuf::from(f), &mut folders);
    }
    for f in &from_history {
        if let Some(parent) = f.parent() {
            push_folder(parent.to_path_buf(), &mut folders);
        }
    }
    out.folders = folders.iter().map(|f| f.display().to_string()).collect();
    let mut classified = std::collections::HashSet::new();

    for folder in &folders {
        let chains = match list_job_chains_strict(folder, &job.id) {
            Ok(c) => c,
            Err(e) => {
                out.kept.push((folder.display().to_string(), e.to_string()));
                continue;
            }
        };
        for chain in &chains {
            // A classified image is handled by delete_chain, including its
            // dependency and consolidation guards. Never retry it by path.
            classified.extend(chain.images.iter().map(|i| i.image_path.clone()));
            match delete_chain(chain) {
                Ok(report) => {
                    out.deleted
                        .extend(report.deleted.iter().map(|p| p.display().to_string()));
                    out.kept.extend(
                        report
                            .kept
                            .iter()
                            .map(|(p, why)| (p.display().to_string(), why.clone())),
                    );
                    out.kept.extend(report.orphan_sidecars.iter().map(|p| {
                        (
                            p.display().to_string(),
                            tr!("lib.orphan_sidecar").into_owned(),
                        )
                    }));
                }
                Err(e) => {
                    // `delete_chain` rejects an entire busy or consolidating
                    // chain before removing any image. Name every surviving
                    // image and external sidecar in the user-facing report.
                    let reason = e.to_string();
                    for image in &chain.images {
                        for path in [&image.image_path, &image.sidecar_path] {
                            if path.exists() {
                                out.kept.push((path.display().to_string(), reason.clone()));
                            }
                        }
                    }
                }
            }
        }
    }

    // History is a hint about folders, not proof that a file belongs to this
    // job. An unreadable sidecar leaves its chain relationships unknown; a
    // path-only deletion could remove an unrelated file or a surviving base.
    for path in from_history {
        if path.exists() && !classified.contains(&path) {
            out.kept.push((
                path.display().to_string(),
                tr!("lib.history_image_unverified").into_owned(),
            ));
        }
    }

    out
}

/// Every folder there is reason to think a job wrote into: the one it is
/// configured for now, plus the parents of everything its history names.
fn job_folders(store: &JobStore, job: &Job) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !folders.contains(&p) {
            folders.push(p);
        }
    };
    push(PathBuf::from(&job.dest_folder));
    for f in &job.previous_dest_folders {
        push(PathBuf::from(f));
    }
    for file in store.job_image_files(&job.id) {
        if let Some(parent) = file.parent() {
            push(parent.to_path_buf());
        }
    }
    folders
}

/// One stored copy of a job, as «очистить версии» lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVersion {
    pub image_path: String,
    pub file_name: String,
    /// full | incremental | differential
    pub role: String,
    /// vhdx | qcow2 | obk
    pub container: String,
    pub created_at: String,
    /// The image file, plus its external sidecar when it has one.
    pub size_bytes: u64,
    /// The image this one cannot be read without, when there is one.
    pub depends_on: Option<String>,
}

/// One chain: a full copy and everything built on it, oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVersionChain {
    /// The source these are copies of, as their file names spell it: `C`,
    /// `DISK0`, `V1A2B3C4D`. One job copies several volumes into one folder,
    /// and each has chains of its own.
    pub source: String,
    /// The newest chain of its source — the one the next increment continues.
    pub current: bool,
    pub images: Vec<JobVersion>,
}

/// What a job has on disk.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct JobVersions {
    /// Sources in name order, and each source's chains oldest first.
    pub chains: Vec<JobVersionChain>,
    pub folders: Vec<String>,
    /// Folders that could not be read, each with the reason.
    pub unreadable: Vec<(String, String)>,
    /// The history is at its cap, so a folder the job wrote into long ago may
    /// not have been looked at.
    pub history_truncated: bool,
}

fn role_word(role: own_backup_backup::sidecar::ChainRole) -> &'static str {
    use own_backup_backup::sidecar::ChainRole;
    match role {
        ChainRole::Full => "full",
        ChainRole::Incremental => "incremental",
        ChainRole::Differential => "differential",
    }
}

/// The key that says two chains are of one source: the file-name token, and
/// the source's identity when the copies recorded one.
fn source_group(base: &own_backup_backup::chains::ChainImage) -> (String, Option<String>) {
    (
        image_token(&base.image_path).unwrap_or_default(),
        base.source_id.as_ref().map(|id| id.to_ascii_lowercase()),
    )
}

/// The copies a job has on disk, grouped the way they depend on each other.
///
/// Asked of the images themselves — each says in its own sidecar (or, when it
/// is encrypted, in its open header) whose it is and what it is built on — and
/// not of the run history, which is capped and remembers only the first volume
/// of a run. An image that cannot be read is left out here; it is not this
/// list's to delete either.
pub fn job_versions(store: &JobStore, job: &Job) -> JobVersions {
    use own_backup_backup::chains::list_job_chains;

    let folders = job_folders(store, job);
    let mut out = JobVersions {
        folders: folders.iter().map(|f| f.display().to_string()).collect(),
        history_truncated: store.history_is_truncated(&job.id),
        ..Default::default()
    };
    let mut chains = Vec::new();
    for folder in &folders {
        if !folder.is_dir() {
            continue; // a folder the job used once and that is gone: nothing in it
        }
        match list_job_chains(folder, &job.id) {
            Ok(found) => chains.extend(found),
            Err(e) => out
                .unreadable
                .push((folder.display().to_string(), e.to_string())),
        }
    }

    // The newest chain of each source: where the next increment will go.
    let mut newest: HashMap<(String, Option<String>), String> = HashMap::new();
    for chain in &chains {
        let Some(base) = chain.images.first() else {
            continue;
        };
        let at = chain.newest_created_at().to_string();
        let slot = newest.entry(source_group(base)).or_default();
        if at > *slot {
            *slot = at;
        }
    }

    for chain in &chains {
        let Some(base) = chain.images.first() else {
            continue;
        };
        let group = source_group(base);
        let current = newest
            .get(&group)
            .is_some_and(|at| at == chain.newest_created_at());
        let images = chain
            .images
            .iter()
            .map(|image| {
                let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let depends_on = image.parent_file_name.as_ref().and_then(|parent| {
                    chain
                        .images
                        .iter()
                        .find(|other| {
                            other
                                .image_path
                                .file_name()
                                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(parent))
                        })
                        .map(|other| other.image_path.display().to_string())
                });
                JobVersion {
                    image_path: image.image_path.display().to_string(),
                    file_name: image
                        .image_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    role: role_word(image.role).into(),
                    container: image.container.clone(),
                    created_at: image.created_at.clone(),
                    size_bytes: size(&image.image_path) + size(&image.sidecar_path),
                    depends_on,
                }
            })
            .collect();
        out.chains.push(JobVersionChain {
            source: group.0,
            current,
            images,
        });
    }
    // Sources side by side, and within a source the order chains were made in.
    out.chains.sort_by(|a, b| {
        let first = |c: &JobVersionChain| {
            c.images
                .first()
                .map(|i| i.created_at.clone())
                .unwrap_or_default()
        };
        a.source.cmp(&b.source).then(first(a).cmp(&first(b)))
    });
    out
}

/// Delete the copies a person ticked in «очистить версии».
///
/// `images` are paths this job's own listing gave out, and that is checked:
/// a path is deleted only as a member of one of the job's chains, never as a
/// path. What a chain refuses — a set that would leave an increment without
/// the image under it, a chain in use, a consolidation that was interrupted —
/// stays on disk and is named in the report with the reason.
pub fn delete_job_versions(store: &JobStore, job: &Job, images: &[PathBuf]) -> JobFilesDeleted {
    use own_backup_backup::chains::{delete_chain_images, list_job_chains_strict};

    let folders = job_folders(store, job);
    let mut out = JobFilesDeleted {
        folders: folders.iter().map(|f| f.display().to_string()).collect(),
        ..Default::default()
    };
    let wanted: std::collections::HashSet<PathBuf> = images.iter().cloned().collect();
    let mut found = std::collections::HashSet::new();

    for folder in &folders {
        if !folder.is_dir() {
            continue;
        }
        let chains = match list_job_chains_strict(folder, &job.id) {
            Ok(c) => c,
            Err(e) => {
                out.kept.push((folder.display().to_string(), e.to_string()));
                continue;
            }
        };
        for chain in &chains {
            let victims: std::collections::HashSet<PathBuf> = chain
                .images
                .iter()
                .map(|i| i.image_path.clone())
                .filter(|p| wanted.contains(p))
                .collect();
            if victims.is_empty() {
                continue;
            }
            found.extend(victims.iter().cloned());
            match delete_chain_images(chain, &victims) {
                Ok(report) => {
                    out.deleted
                        .extend(report.deleted.iter().map(|p| p.display().to_string()));
                    out.kept.extend(
                        report
                            .kept
                            .iter()
                            .map(|(p, why)| (p.display().to_string(), why.clone())),
                    );
                    out.kept.extend(report.orphan_sidecars.iter().map(|p| {
                        (
                            p.display().to_string(),
                            tr!("lib.orphan_sidecar").into_owned(),
                        )
                    }));
                }
                Err(e) => {
                    let reason = e.to_string();
                    let mut names: Vec<&PathBuf> = victims.iter().collect();
                    names.sort();
                    for path in names {
                        out.kept.push((path.display().to_string(), reason.clone()));
                    }
                }
            }
        }
    }

    let mut strangers: Vec<&PathBuf> = wanted.difference(&found).collect();
    strangers.sort();
    for path in strangers {
        out.kept.push((
            path.display().to_string(),
            tr!("lib.version_not_of_job").into_owned(),
        ));
    }
    out
}

/// Which files «проверить последнюю версию» and «проверить все версии» read.
///
/// The second member says how: `false` — the whole restore point, every block
/// it is made of, through the chain under it; `true` — only the blocks that
/// file itself stores.
///
/// * the latest version: the newest image of each source, read whole. That
///   answers the question being asked — can the latest copy be restored;
/// * all versions: every restore point, read whole through its parents. The
///   allocation tables of an unencrypted container are not authenticated: a
///   damaged entry can mark formerly stored data absent. Checking only what
///   the container still claims to store would skip that missing data and
///   falsely report success. The manifest's checksums decide what to read.
///
/// An image that cannot be read stops the listing with an error rather than
/// being skipped: "all versions are fine" must not be said about a set with a
/// hole in it.
pub fn job_verify_targets(
    store: &JobStore,
    job: &Job,
    all_versions: bool,
) -> own_backup_common::Result<Vec<(PathBuf, bool)>> {
    use own_backup_backup::chains::list_job_chains_strict;

    let mut chains = Vec::new();
    for folder in job_folders(store, job) {
        if folder.is_dir() {
            chains.extend(list_job_chains_strict(&folder, &job.id)?);
        }
    }
    if all_versions {
        return Ok(chains
            .iter()
            .flat_map(|c| c.images.iter().map(|i| (i.image_path.clone(), false)))
            .collect());
    }
    let mut newest: HashMap<(String, Option<String>), (String, PathBuf)> = HashMap::new();
    for chain in &chains {
        let (Some(base), Some(last)) = (chain.images.first(), chain.images.last()) else {
            continue;
        };
        let slot = newest.entry(source_group(base)).or_default();
        if last.created_at > slot.0 {
            *slot = (last.created_at.clone(), last.image_path.clone());
        }
    }
    let mut targets: Vec<(PathBuf, bool)> = newest
        .into_values()
        .map(|(_, path)| (path, false))
        .collect();
    targets.sort();
    Ok(targets)
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
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
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
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
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
fn apply_consolidation(
    job: &Job,
    protected: &[(String, PathBuf)],
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> u32 {
    use own_backup_backup::chains::{list_job_chains_strict, plan_consolidation};
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
    // One step at a time, re-reading the folder after each: a step changes the
    // very chain the next decision is made from. The cap is a backstop against
    // a step that reports success without shortening anything — a loop that
    // never ends would be a far worse bug than a chain left one version long.
    const MAX_STEPS: usize = 64;
    let mut folded = 0u32;
    for _ in 0..MAX_STEPS {
        let chains = match list_job_chains_strict(&folder, &job.id) {
            Ok(c) => c,
            Err(e) => {
                warn!(job = %job.name, error = %e, "Consolidation scan failed");
                return folded;
            }
        };
        let groups: std::collections::HashSet<(String, Option<String>)> = protected
            .iter()
            .filter_map(|(token, path)| {
                chains.iter().find(|c| c.contains(path)).map(|c| {
                    (
                        token.to_ascii_uppercase(),
                        c.images.first().and_then(|i| i.source_id.clone()),
                    )
                })
            })
            .collect();
        let step = chains
            .iter()
            .filter(|c| {
                c.images.first().is_some_and(|base| {
                    groups.iter().any(|(token, source_id)| {
                        chain_source_group_matches(base, token, source_id.as_deref())
                    })
                })
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
    use own_backup_backup::engine_win::{backup_volume, VolumeBackupRequest};
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
    let mut totals = backup_run::VolumeTotals::new(volumes.len());
    // (source device, image path) per success — used to build the whole-disk
    // restore manifest, joining images to GPT partitions by device.
    let mut run_images: Vec<(String, PathBuf)> = Vec::new();
    let mut fatal: Option<(String, String)> = None; // (status, message)

    for (idx, device) in volumes.iter().enumerate() {
        let plan = plan_run_for_volume(job, &folder, device);
        let (image_path, full_image_path) =
            output_paths_for(job, &file_token(job, device), plan.kind);
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
            full_image_path: Some(full_image_path),
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
            // A Windows volume gets no Linux rules (decision R6).
            exclusion_patterns_linux: Vec::new(),
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
        let verify_scope = backup_run::verify_scope(job);

        let low_priority = job.advanced.low_priority;
        let speed_limit_mbps = job.advanced.speed_limit_mbps;
        // Checking an encrypted copy needs its key: without one there is no
        // sidecar to read and nothing to compare checksums against.
        let keys_for_verify = keys.clone();
        // The file this run is writing — named in the progress line, because
        // "WorkDisk-2 → D:\Temp" alone does not say WHAT is appearing there.
        let image_name = image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let watch = engine_watch::EngineWatch::new();
        let progress = backup_run::RunProgress {
            sink: sink.clone(),
            watch: watch.clone(),
            cancel: cancel.clone(),
            job_id: job.id.clone(),
            volume_index: vidx,
            volume_count: count,
            volume: backup_run::ProgressLabel::Volume(vol_label),
            image_file_name: image_name,
        };
        let handle = tokio::task::spawn_blocking(move || {
            let _quiet = QuietCopy::enter(low_priority);
            let mut copy = backup_run::CopyProgress::new(&progress, speed_limit_mbps);
            let summary = backup_volume(&request, |p| copy.report(p))?;

            // Optional post-backup verification — emit its own progress so the
            // UI shows "Проверка…" instead of freezing at 100%.
            let mut verify_cancelled = false;
            if validate_after {
                verify_cancelled = backup_run::verify_copy(
                    &progress,
                    &summary.image_path,
                    keys_for_verify.as_ref(),
                    verify_scope,
                )?;
            }

            Ok::<_, OwnBackupError>((summary, verify_cancelled))
        });
        let result = match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
            engine_watch::Waited::Done(result) => result,
            engine_watch::Waited::LeftBehind => {
                warn!(job = %job.name, volume = %device,
                    "cancelled while the engine was not answering; its thread is left behind");
                fatal = Some((
                    "cancelled".into(),
                    tr!("lib.cancelled_left_behind").into_owned(),
                ));
                break;
            }
        };

        match result {
            Ok(Ok((summary, verify_cancelled))) => {
                let image_path = totals.record(&mut run, job, &plan, idx, device, summary);
                run_images.push((device.clone(), image_path));

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
                fatal = Some((
                    "cancelled".into(),
                    tr!("lib.cancelled_by_user").into_owned(),
                ));
                break;
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                warn!(job = %job.name, volume = %device, error = %msg, "Backup failed");
                fatal = Some(("error".into(), msg));
                break;
            }
            Err(e) => {
                fatal = Some((
                    "error".into(),
                    tr!("lib.task_aborted", error = e).into_owned(),
                ));
                break;
            }
        }
    }

    totals.finish(run, fatal, job, keys.as_ref())
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
    use own_backup_backup::engine_win::{backup_disk, DiskBackupRequest, DiskPartitionBackup};
    use own_backup_common::OwnBackupError;

    use own_backup_common::types::BackupType;

    // Chain planning (plan D5): a scheduled incremental/differential writes a
    // differencing child over the previous whole-disk image of THIS disk; any
    // doubt resolves to a full. VHDX children are `.avhdx` (Windows convention);
    // qcow2 children stay `.qcow2` (the role lives in the sidecar).
    let folder = PathBuf::from(&job.dest_folder);
    let plan = plan_run_for_disk(job, &folder, ds.disk_number);
    // Name carries the kind tag (full/inc/diff) so a glance at the folder tells
    // full from child — the extension alone can't (a qcow2 child is still
    // `.qcow2`). Token position matches the per-volume scheme so `image_token`
    // reads `disk<N>` from `<name>_disk<N>_<tag>_<stamp>`.
    let (image_path, full_image_path) =
        output_paths_for(job, &format!("disk{}", ds.disk_number), plan.kind);

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
        (
            job.exclusions.patterns.clone(),
            job.exclusions.patterns_linux.clone(),
        )
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
            let tail: String = d
                .device_id
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let tail: String = tail.chars().rev().collect();
            tail.parse::<u32>().ok() == Some(ds.disk_number)
        })
        .cloned();

    let request = DiskBackupRequest {
        disk_number: ds.disk_number,
        disk_device: None,
        disk_model: ds.disk_model.clone(),
        partitions,
        source_disk,
        full_image_path: Some(full_image_path.clone()),
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
    let verify_scope = backup_run::verify_scope(job);
    let keys_for_verify = keys.clone();
    let image_name = image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let low_priority = job.advanced.low_priority;
    let speed_limit_mbps = job.advanced.speed_limit_mbps;

    let watch = engine_watch::EngineWatch::new();
    let progress = backup_run::RunProgress {
        sink: sink.clone(),
        watch: watch.clone(),
        cancel: cancel.clone(),
        job_id: job.id.clone(),
        volume_index: 0,
        volume_count: 1,
        volume: backup_run::ProgressLabel::Disk {
            name: ds.disk_number.to_string(),
            model: ds.disk_model.clone(),
        },
        image_file_name: image_name,
    };
    let handle = tokio::task::spawn_blocking(move || {
        let _quiet = QuietCopy::enter(low_priority);
        let mut copy = backup_run::CopyProgress::new(&progress, speed_limit_mbps);
        let summary = backup_disk(&request, |p| copy.report(p))?;

        // Optional post-backup verification — emit "verify" progress so the UI
        // shows "Проверка…", and FAIL the run on any checksum mismatch (a bad
        // image must never be reported as success).
        let mut verify_cancelled = false;
        if validate_after {
            verify_cancelled = backup_run::verify_copy(
                &progress,
                &summary.image_path,
                keys_for_verify.as_ref(),
                verify_scope,
            )?;
        }
        Ok::<_, OwnBackupError>((summary, verify_cancelled))
    });
    let result = match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
        engine_watch::Waited::Done(result) => result,
        engine_watch::Waited::LeftBehind => {
            warn!(job = %job.name, "cancelled while the engine was not answering; its thread is left behind");
            run.finished_at = chrono::Utc::now().to_rfc3339();
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_left_behind").into_owned();
            return run;
        }
    };

    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.image_path = image_path.display().to_string();
    run.base_image_path = image_path.display().to_string();
    run.backup_kind = "full".into();
    run.smart_mode = true;
    // The whole-disk path takes a shadow copy per partition — except on the
    // boot medium, where the system is offline and a direct read is already
    // consistent (see `snapshot_wanted` in the engine).
    run.vss_used = !own_backup_common::media_mode::is_media_mode();

    let disk_token = format!("disk{}", ds.disk_number);
    match result {
        Ok(Ok((summary, verify_cancelled))) => {
            let image_path = backup_run::record_disk(&mut run, job, &plan, &disk_token, summary);
            // A check the user stopped is not a reason to delete older versions
            // — and the image below is NOT removed, unlike a cancelled copy.
            if verify_cancelled {
                run.status = "cancelled".into();
                run.error = verify_cancelled_message();
                return run;
            }
            let protected = [(disk_token, image_path)];
            backup_run::retain_and_consolidate(&mut run, job, &protected, keys.as_ref());
        }
        Ok(Err(OwnBackupError::Cancelled)) => {
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_by_user").into_owned();
            let _ = std::fs::remove_file(&image_path);
            let _ = std::fs::remove_file(&full_image_path);
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

/// Linux backup driver: each source volume is copied by the shared engine
/// (`engine_linux::backup_volume`) through the snapshot taken of it here, with
/// the progress, verification and accounting the Windows driver has
/// (`backup_run`): chains planned from the folder, retention and consolidation
/// after a fully successful run; the job's exclusions on a source that holds
/// still. What the Linux engine does not do yet — VHDX chains, a whole disk —
/// is refused by [`linux_unsupported`] before anything is touched.
#[cfg(target_os = "linux")]
async fn perform_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    _last_success: Option<JobRun>,
    passwords: &Arc<dyn PasswordResolver>,
    // An encrypted job's keys, opened from the service's key store before
    // the run (`archive_keys_for_run`): every image is `.obk`, its parent,
    // check and consolidation read through them.
    keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
) -> JobRun {
    use own_backup_backup::engine_linux::{
        backup_volume, container_key_from, VolumeBackupRequest, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_common::types::BackupType;
    use own_backup_common::OwnBackupError;
    use own_backup_snapshot::SnapshotProvider;

    let mut run = run_skeleton(job);
    let volumes = job.source_list();
    // A whole disk names its partitions in `disk_source`, not in `sources`.
    if volumes.is_empty() && job.disk_source.is_none() {
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
        run.refused = true;
        return run;
    }
    // What the Linux engine does not do yet is refused, not skipped: a run that
    // reports success while ignoring the scheme or the exclusions is a backup
    // plan that turns out to be something else (`.ai/PLAN.md` §6).
    if let Some(msg) = linux_unsupported(job) {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = msg;
        run.refused = true;
        return run;
    }
    // Scheduled runs come here without the check `RunJob` makes; a whole
    // disk's partitions are its sources here.
    let sources: Vec<String> = match &job.disk_source {
        Some(ds) => ds.parts.iter().map(|p| p.device.clone()).collect(),
        None => volumes.clone(),
    };
    if let own_backup_backup::source_identity::DestPlacement::OnSource(source) =
        own_backup_backup::source_identity::where_the_destination_sits(&sources, &job.dest_folder)
    {
        run.finished_at = chrono::Utc::now().to_rfc3339();
        run.status = "error".into();
        run.error = tr!("lib.linux_dest_on_source", source = source).into_owned();
        // Not `refused`: a destination disk not mounted yet puts the folder
        // on the root filesystem until it is, and the retry is what then
        // makes the copy. The retries are counted, so a real mistake ends.
        return run;
    }
    // A whole disk → one image with its GPT and restore manifest.
    if let Some(ds) = job.disk_source.clone() {
        return perform_disk_backup(sink, job, &ds, cancel, passwords, keys, run).await;
    }
    let count = volumes.len() as u64;
    let mut totals = backup_run::VolumeTotals::new(volumes.len());
    let mut fatal: Option<(String, String)> = None; // (status, message)
                                                    // As for a Windows whole disk, by filesystem family: the Windows rules for
                                                    // NTFS and FAT, the Linux rules for ext, btrfs and xfs. The engine applies
                                                    // them only to a source that holds still, and says when it did not.
    let (excl_win, excl_linux) = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        (
            job.exclusions.patterns.clone(),
            job.exclusions.patterns_linux.clone(),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    for (idx, device) in volumes.iter().enumerate() {
        // Full, or the next version of this volume's chain (qcow2).
        let plan = plan_run_for_volume(job, &folder, device);
        let (image_path, full_image_path) =
            output_paths_for(job, &file_token(job, device), plan.kind);
        let token = source_token(device);
        let vol_label = job.label_for(device);
        // The disk this volume lives on, as the GUI captured it at job save —
        // embedded in the sidecar, as on Windows.
        let source_disk = job
            .source_disks
            .iter()
            .find(|d| d.partitions.iter().any(|p| p.volume_device == *device))
            .cloned();

        // A point in time to read: an LVM or device-mapper snapshot (its
        // change area in the destination folder; an opened LUKS container may
        // need its password for that), a short partition held still, or the
        // device itself when nothing writes to it. A volume in use without
        // one is refused — never read live, never frozen for a long copy.
        let provider = Arc::new(
            own_backup_snapshot::LinuxSnapshotProvider::with_cow_dir(&folder)
                .container_key(container_key_from(passwords.resolve(&token))),
        );
        let snapshot = match provider.create_snapshot(device).await {
            Ok(s) => s,
            Err(e) => {
                warn!(job = %job.name, volume = %device, error = %e, "No consistent source");
                fatal = Some(("error".into(), snapshot_error_text(e)));
                break;
            }
        };
        note_dense_store(&mut run, &snapshot, &folder);

        let request = VolumeBackupRequest {
            // The device itself: the image's identity comes from it, and the
            // engine reads the snapshot of it.
            source: device.clone(),
            image_path: image_path.clone(),
            full_image_path: Some(full_image_path),
            block_size: DEFAULT_BLOCK_SIZE,
            all_sectors: job.advanced.sector_by_sector,
            use_vss: true,
            kind: match plan.kind {
                "incremental" => BackupType::Incremental,
                "differential" => BackupType::Differential,
                _ => BackupType::Full,
            },
            parent_image: plan.parent_image.clone(),
            exclusion_patterns: excl_win.clone(),
            exclusion_patterns_linux: excl_linux.clone(),
            job_id: Some(job.id.clone()),
            // For a LUKS or BitLocker source: its decrypted data is copied.
            passphrase: passwords.resolve(&token),
            image_format: job.image_format,
            source_disk,
            encryption: keys.clone(),
            zstd_level: job.zstd_level(),
        };
        let image_name = image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let watch = engine_watch::EngineWatch::new();
        let progress = Arc::new(backup_run::RunProgress {
            sink: sink.clone(),
            watch: watch.clone(),
            cancel: cancel.clone(),
            job_id: job.id.clone(),
            volume_index: idx as u64,
            volume_count: count,
            volume: backup_run::ProgressLabel::Volume(vol_label),
            image_file_name: image_name,
        });

        // The copy says when it no longer reads the snapshot: one left behind
        // still does.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = {
            let progress = progress.clone();
            let snapshot = snapshot.clone();
            let speed_limit_mbps = job.advanced.speed_limit_mbps;
            let low_priority = job.advanced.low_priority;
            tokio::task::spawn_blocking(move || {
                let _quiet = QuietCopy::enter(low_priority);
                let mut copy = backup_run::CopyProgress::new(&progress, speed_limit_mbps);
                let copied = backup_volume(&request, &snapshot, |p| copy.report(p));
                let _ = done_tx.send(());
                copied
            })
        };
        let copied = match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
            engine_watch::Waited::Done(result) => result,
            engine_watch::Waited::LeftBehind => {
                warn!(job = %job.name, volume = %device,
                    "cancelled while the engine was not answering; its thread is left behind");
                release_left_behind(provider, snapshot, done_rx);
                fatal = Some((
                    "cancelled".into(),
                    tr!("lib.cancelled_left_behind").into_owned(),
                ));
                break;
            }
        };
        // The snapshot is needed only while the source is read, and whether
        // it held was asked before the image was published. It is given back
        // now, whatever came of the copy, before the image is read back.
        if let Err(e) = provider.release_snapshot(&snapshot).await {
            warn!(job = %job.name, volume = %device, error = %e, "snapshot release failed");
        }
        let summary = match copied {
            Ok(Ok(summary)) => summary,
            Ok(Err(OwnBackupError::Cancelled)) => {
                fatal = Some((
                    "cancelled".into(),
                    tr!("lib.cancelled_by_user").into_owned(),
                ));
                break;
            }
            Ok(Err(e)) => {
                warn!(job = %job.name, volume = %device, error = %e, "Backup failed");
                fatal = Some(("error".into(), snapshot_error_text(e)));
                break;
            }
            Err(e) => {
                fatal = Some((
                    "error".into(),
                    tr!("lib.task_aborted", error = e).into_owned(),
                ));
                break;
            }
        };

        // Optional post-backup verification, with its own progress.
        let mut verify_cancelled = false;
        if job.advanced.validate_after {
            let image = summary.image_path.clone();
            let scope = backup_run::verify_scope(job);
            let handle = {
                let progress = progress.clone();
                let keys = keys.clone();
                tokio::task::spawn_blocking(move || {
                    backup_run::verify_copy(&progress, &image, keys.as_ref(), scope)
                })
            };
            match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
                engine_watch::Waited::Done(Ok(Ok(cancelled))) => verify_cancelled = cancelled,
                engine_watch::Waited::Done(Ok(Err(e))) => {
                    warn!(job = %job.name, volume = %device, error = %e, "Verification failed");
                    fatal = Some(("error".into(), e.to_string()));
                    break;
                }
                engine_watch::Waited::Done(Err(e)) => {
                    fatal = Some((
                        "error".into(),
                        tr!("lib.task_aborted", error = e).into_owned(),
                    ));
                    break;
                }
                // The copy is published; only reading it back was stopped.
                engine_watch::Waited::LeftBehind => verify_cancelled = true,
            }
        }

        totals.record(&mut run, job, &plan, idx, device, summary);
        // Recorded first, so the finished image is findable in the history;
        // then the run stops short of success, and retention does not run.
        if verify_cancelled {
            fatal = Some(("cancelled".into(), verify_cancelled_message()));
            break;
        }
    }

    totals.finish(run, fatal, job, keys.as_ref())
}

/// A device-mapper snapshot whose store the destination could not keep
/// sparse (FAT, exFAT, NTFS through ntfs3) is allocated in full and capped —
/// fine, but the person is told in the summary, not only in the log: heavy
/// writes to the volume during the copy overflow such a store and the run is
/// refused.
#[cfg(target_os = "linux")]
fn note_dense_store(run: &mut JobRun, snapshot: &own_backup_snapshot::Snapshot, folder: &Path) {
    let Some(bytes) = snapshot
        .metadata
        .get(own_backup_snapshot::linux::DENSE_STORE_BYTES)
        .and_then(|b| b.parse::<u64>().ok())
    else {
        return;
    };
    run.notes.push(
        tr!(
            "lib.snapshot_store_dense",
            volume = snapshot.source_device,
            folder = folder.display(),
            size = own_backup_common::units::human_bytes(bytes)
        )
        .into_owned(),
    );
}

/// Linux whole-disk driver: every partition is held first — an LVM or
/// device-mapper snapshot, a short hold, or the idle device — so the image is
/// as close to one point in time as the disk allows, then the shared engine
/// copies the disk (`engine_linux::backup_disk`), checks every hold before it
/// publishes, and the holds are let go before the image is read back.
#[cfg(target_os = "linux")]
async fn perform_disk_backup(
    sink: &Arc<dyn EventSink>,
    job: &Job,
    ds: &DiskSourceSpec,
    cancel: &Arc<AtomicBool>,
    passwords: &Arc<dyn PasswordResolver>,
    keys: Option<own_backup_backup::seal::context::ArchiveKeys>,
    mut run: JobRun,
) -> JobRun {
    use own_backup_backup::engine_linux::{
        backup_disk, container_key_from, DiskBackupRequest, DiskPartitionBackup, DEFAULT_BLOCK_SIZE,
    };
    use own_backup_common::types::BackupType;
    use own_backup_common::OwnBackupError;
    use own_backup_snapshot::SnapshotProvider;

    let folder = PathBuf::from(&job.dest_folder);
    let tag = ds.file_tag();
    let plan = plan_run_for_disk_tag(job, &folder, &tag);
    let (image_path, full_image_path) = output_paths_for(job, &tag, plan.kind);

    // Every partition held before a block is copied; one that cannot be is
    // the end of the run, and those already held are let go.
    let provider = Arc::new(own_backup_snapshot::LinuxSnapshotProvider::with_cow_dir(
        &folder,
    ));
    let mut snapshots: Vec<own_backup_snapshot::Snapshot> = Vec::new();
    for part in &ds.parts {
        let token = source_token(&part.device);
        let provider_for_part = own_backup_snapshot::LinuxSnapshotProvider::with_cow_dir(&folder)
            .container_key(container_key_from(passwords.resolve(&token)));
        match provider_for_part.create_snapshot(&part.device).await {
            Ok(s) => {
                note_dense_store(&mut run, &s, &folder);
                snapshots.push(s)
            }
            Err(e) => {
                warn!(job = %job.name, partition = %part.device, error = %e, "No consistent source");
                for s in &snapshots {
                    if let Err(e) = provider.release_snapshot(s).await {
                        warn!(volume = %s.source_device, error = %e, "snapshot release failed");
                    }
                }
                run.finished_at = chrono::Utc::now().to_rfc3339();
                run.status = "error".into();
                run.error = snapshot_error_text(e);
                return run;
            }
        }
    }
    run.vss_used = snapshots.iter().any(|s| {
        matches!(
            s.metadata.get("mode").map(String::as_str),
            Some("lvm" | "dm")
        )
    });

    let (excl_win, excl_linux) = if job.exclusions.enabled && !job.advanced.sector_by_sector {
        (
            job.exclusions.patterns.clone(),
            job.exclusions.patterns_linux.clone(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    // The disk as the window captured it at job save, matched by its device:
    // the window keeps the kernel name in `device_id` and the by-id one the
    // job names in `stable_device`.
    let source_disk = ds.device.as_deref().and_then(|device| {
        job.source_disks
            .iter()
            .find(|d| d.device_id == device || d.stable_device == device)
            .cloned()
    });
    let request = DiskBackupRequest {
        disk_number: ds.disk_number,
        disk_device: ds.device.clone(),
        disk_model: ds.disk_model.clone(),
        partitions: ds
            .parts
            .iter()
            .map(|p| DiskPartitionBackup {
                device: p.device.clone(),
                part_index: p.part_index,
                passphrase: passwords.resolve(&source_token(&p.device)),
            })
            .collect(),
        image_path: image_path.clone(),
        full_image_path: Some(full_image_path.clone()),
        block_size: DEFAULT_BLOCK_SIZE,
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
        source_disk,
        bootable: ds.bootable,
        zstd_level: job.zstd_level(),
    };
    let watch = engine_watch::EngineWatch::new();
    let progress = Arc::new(backup_run::RunProgress {
        sink: sink.clone(),
        watch: watch.clone(),
        cancel: cancel.clone(),
        job_id: job.id.clone(),
        volume_index: 0,
        volume_count: 1,
        volume: backup_run::ProgressLabel::Disk {
            name: ds
                .device
                .as_deref()
                .and_then(|d| d.rsplit('/').next())
                .unwrap_or_default()
                .to_string(),
            model: ds.disk_model.clone(),
        },
        image_file_name: image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    });

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = {
        let progress = progress.clone();
        let snapshots = snapshots.clone();
        let speed_limit_mbps = job.advanced.speed_limit_mbps;
        let low_priority = job.advanced.low_priority;
        tokio::task::spawn_blocking(move || {
            let _quiet = QuietCopy::enter(low_priority);
            let mut copy = backup_run::CopyProgress::new(&progress, speed_limit_mbps);
            let copied = backup_disk(&request, &snapshots, |p| copy.report(p));
            let _ = done_tx.send(());
            copied
        })
    };
    let copied = match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
        engine_watch::Waited::Done(result) => result,
        engine_watch::Waited::LeftBehind => {
            warn!(job = %job.name, "cancelled while the engine was not answering; its thread is left behind");
            release_all_left_behind(provider, snapshots, done_rx);
            run.finished_at = chrono::Utc::now().to_rfc3339();
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_left_behind").into_owned();
            return run;
        }
    };
    // Every hold was checked before the image was published; they are let go
    // now, whatever came of the copy, before the image is read back.
    for s in &snapshots {
        if let Err(e) = provider.release_snapshot(s).await {
            warn!(job = %job.name, volume = %s.source_device, error = %e, "snapshot release failed");
        }
    }
    run.finished_at = chrono::Utc::now().to_rfc3339();
    let summary = match copied {
        Ok(Ok(summary)) => summary,
        Ok(Err(OwnBackupError::Cancelled)) => {
            run.status = "cancelled".into();
            run.error = tr!("lib.cancelled_by_user").into_owned();
            return run;
        }
        Ok(Err(e)) => {
            warn!(job = %job.name, error = %e, "Whole-disk backup failed");
            run.status = "error".into();
            run.error = snapshot_error_text(e);
            return run;
        }
        Err(e) => {
            run.status = "error".into();
            run.error = tr!("lib.task_aborted", error = e).into_owned();
            return run;
        }
    };

    let mut verify_cancelled = false;
    if job.advanced.validate_after {
        let image = summary.image_path.clone();
        let scope = backup_run::verify_scope(job);
        let handle = {
            let progress = progress.clone();
            let keys = keys.clone();
            tokio::task::spawn_blocking(move || {
                backup_run::verify_copy(&progress, &image, keys.as_ref(), scope)
            })
        };
        match engine_watch::wait_engine(handle, &watch, cancel, sink).await {
            engine_watch::Waited::Done(Ok(Ok(cancelled))) => verify_cancelled = cancelled,
            engine_watch::Waited::Done(Ok(Err(e))) => {
                run.status = "error".into();
                run.error = e.to_string();
                return run;
            }
            engine_watch::Waited::Done(Err(e)) => {
                run.status = "error".into();
                run.error = tr!("lib.task_aborted", error = e).into_owned();
                return run;
            }
            engine_watch::Waited::LeftBehind => verify_cancelled = true,
        }
    }
    run.smart_mode = true;
    let image_path = backup_run::record_disk(&mut run, job, &plan, &tag, summary);
    // Recorded first, so the finished image is findable in the history; a
    // check the user stopped is no reason to delete older versions.
    if verify_cancelled {
        run.status = "cancelled".into();
        run.error = verify_cancelled_message();
        return run;
    }
    let protected = [(tag, image_path)];
    backup_run::retain_and_consolidate(&mut run, job, &protected, keys.as_ref());
    run
}

/// [`release_left_behind`] for every partition of a whole disk: the
/// partitions held still are let go at once, the snapshot devices once the
/// thread that reads them is done.
#[cfg(target_os = "linux")]
fn release_all_left_behind(
    provider: Arc<own_backup_snapshot::LinuxSnapshotProvider>,
    snapshots: Vec<own_backup_snapshot::Snapshot>,
    done: tokio::sync::oneshot::Receiver<()>,
) {
    use own_backup_snapshot::SnapshotProvider;
    let (holds, devices): (Vec<_>, Vec<_>) = snapshots
        .into_iter()
        .partition(own_backup_snapshot::linux::is_partition_hold);
    tokio::spawn(async move {
        let release = |s: own_backup_snapshot::Snapshot| {
            let provider = provider.clone();
            async move {
                match provider.release_snapshot(&s).await {
                    Ok(()) => {
                        info!(volume = %s.source_device, "snapshot of a left-behind copy released")
                    }
                    Err(e) => warn!(volume = %s.source_device, error = %e,
                        "snapshot of a left-behind copy not released"),
                }
            }
        };
        for s in holds {
            release(s).await;
        }
        if !devices.is_empty() {
            let _ = done.await;
        }
        for s in devices {
            release(s).await;
        }
    });
}

/// A copy left behind — cancelled while its thread did not answer — still
/// holds its source. A partition held still is let go at once: its writers
/// wait, and a thaw does not disturb a read of the device itself. A snapshot
/// device is still open in the thread, and is released once the thread is
/// done with it. Nothing of that copy is published either way: the thread
/// learns at its next progress call that nobody waits for it.
#[cfg(target_os = "linux")]
fn release_left_behind(
    provider: Arc<own_backup_snapshot::LinuxSnapshotProvider>,
    snapshot: own_backup_snapshot::Snapshot,
    done: tokio::sync::oneshot::Receiver<()>,
) {
    use own_backup_snapshot::SnapshotProvider;
    let wait = !own_backup_snapshot::linux::is_partition_hold(&snapshot);
    tokio::spawn(async move {
        if wait {
            let _ = done.await;
        }
        match provider.release_snapshot(&snapshot).await {
            Ok(()) => {
                info!(volume = %snapshot.source_device, "snapshot of a left-behind copy released")
            }
            Err(e) => warn!(volume = %snapshot.source_device, error = %e,
                "snapshot of a left-behind copy not released"),
        }
    });
}

/// The job options the Linux engine does not carry out yet, as one refusal —
/// or `None` when the job asks for nothing it would ignore. The list is the
/// same table the window is told to offer from (`capabilities::linux`).
#[cfg(target_os = "linux")]
fn linux_unsupported(job: &Job) -> Option<String> {
    let what = capabilities::refusals(job, &capabilities::linux());
    (!what.is_empty()).then(|| tr!("lib.linux_unsupported", options = what.join(", ")).into_owned())
}

/// A snapshot refusal carries its own explanation; anything else is shown as
/// the error it is.
#[cfg(target_os = "linux")]
fn snapshot_error_text(e: own_backup_common::OwnBackupError) -> String {
    match e {
        own_backup_common::OwnBackupError::NoSnapshotProvider(msg) => msg,
        other => other.to_string(),
    }
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
    /// The window previews the file name with its own copy of this rule
    /// (`diskFileTag` in `diskNames.ts`), which gives the same token for the
    /// same by-id device: `diskH8B343557` for `ata-TEST`.
    #[test]
    fn a_disk_named_by_its_device_is_tagged_by_its_token() {
        let mut ds = super::DiskSourceSpec {
            disk_number: 0,
            device: Some("/dev/disk/by-id/ata-TEST".into()),
            disk_model: String::new(),
            parts: Vec::new(),
            bootable: false,
        };
        assert_eq!(ds.file_tag(), "diskH8B343557");
        ds.device = None;
        ds.disk_number = 3;
        assert_eq!(ds.file_tag(), "disk3");
    }

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
        let rules: Vec<ExclusionPattern> = default_exclusions()
            .iter()
            .map(|p| ExclusionPattern::parse(p).expect(p))
            .collect();
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
            .upsert(Job {
                name: "nightly".into(),
                ..Default::default()
            })
            .expect_err("the write cannot succeed");
        assert!(!err.to_string().is_empty());

        // And the store is exactly as it was — no job that will disappear at
        // the next restart.
        assert!(
            store.list().is_empty(),
            "nothing was added in memory either"
        );
    }

    /// A job remembers every folder it wrote into, whatever a client sends:
    /// the activity journal forgets them after 200 runs, and deleting the job
    /// must still find the copies there.
    #[test]
    fn a_job_remembers_the_folders_it_left() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let mut job = store
            .upsert(Job {
                name: "j".into(),
                dest_folder: r"S:\A".into(),
                previous_dest_folders: vec![r"X:\forged".into()],
                ..Default::default()
            })
            .unwrap();
        assert!(job.previous_dest_folders.is_empty());
        for (dest, sent) in [
            (r"S:\B", vec![]),
            (r"s:\b", vec![]),
            (r"S:\A", vec![r"Y:\x".to_string()]),
            (r"S:\C", vec![]),
        ] {
            job.dest_folder = dest.into();
            job.previous_dest_folders = sent;
            job = store.upsert(job).unwrap();
        }
        // Spelled as the job had it when it moved on.
        assert_eq!(
            job.previous_dest_folders,
            vec![r"S:\A".to_string(), r"s:\b".to_string()]
        );
        assert_eq!(
            job_folders(&store, &job),
            vec![
                PathBuf::from(r"S:\C"),
                PathBuf::from(r"S:\A"),
                PathBuf::from(r"s:\b")
            ]
        );
    }

    /// A write that fails after the temporary file exists takes the file with
    /// it: on a full disk the half-written copy held the space the next save
    /// needed (stand, 2026-10-17). Here the rename is what fails.
    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("in-the-way"), b"x").unwrap();
        assert!(write_json(&path, &vec![1, 2, 3]).is_err());
        assert!(!dir.path().join("jobs.json.tmp").exists());
        assert!(path.join("in-the-way").exists());
    }

    /// The store files carry the sealed archive keys: owner-only, also when a
    /// crash left a world-readable temporary file in the way.
    #[cfg(unix)]
    #[test]
    fn store_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let stale = dir.path().join("jobs.json.tmp");
        std::fs::write(&stale, b"left by a crash").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_json(&path, &vec![1, 2, 3]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            serde_json::from_slice::<Vec<i32>>(&std::fs::read(&path).unwrap()).unwrap(),
            vec![1, 2, 3]
        );
        assert!(!stale.exists());
    }

    /// The same for removal: a job is gone from the list only once it is gone
    /// from the file.
    #[test]
    fn a_job_that_could_not_be_removed_stays_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let job = store
            .upsert(Job {
                name: "keep".into(),
                ..Default::default()
            })
            .unwrap();

        std::fs::create_dir(dir.path().join("jobs.json.tmp")).unwrap();
        assert!(store.delete(&job.id).is_err());
        assert_eq!(
            store.list().len(),
            1,
            "still there, because the file still has it"
        );
    }

    /// A run the history files could not take is not shown as an ordinary run,
    /// is written once they can take it, and the mark never reaches the files.
    #[test]
    fn a_run_the_history_could_not_take_is_marked_and_written_later() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let job = store
            .upsert(Job {
                name: "copies".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(!store.retry_unrecorded().unwrap(), "nothing is waiting");

        std::fs::create_dir(dir.path().join("activity.json.tmp")).unwrap();
        let run = JobRun {
            id: "run-1".into(),
            job_id: job.id.clone(),
            status: "success".into(),
            ..Default::default()
        };
        assert!(store.record_run(run).is_err());
        let shown = store.get(&job.id).unwrap().last_run.unwrap();
        assert_eq!(shown.id, "run-1");
        assert!(!shown.record_error.is_empty(), "the screen is told");
        assert!(!store.list()[0]
            .last_run
            .clone()
            .unwrap()
            .record_error
            .is_empty());
        assert!(!store.activity()[0].record_error.is_empty());
        assert!(store.retry_unrecorded().is_err(), "still in the way");

        std::fs::remove_dir(dir.path().join("activity.json.tmp")).unwrap();
        assert!(store.retry_unrecorded().unwrap());
        assert!(store
            .get(&job.id)
            .unwrap()
            .last_run
            .unwrap()
            .record_error
            .is_empty());
        let reloaded = JobStore::load(dir.path());
        let kept = reloaded.get(&job.id).unwrap().last_run.unwrap();
        assert_eq!(kept.id, "run-1");
        assert!(kept.record_error.is_empty(), "the mark is not history");
        assert_eq!(reloaded.activity()[0].id, "run-1");
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
        store
            .upsert(Job {
                name: "fresh".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(JobStore::load(dir.path()).list().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_must_not_be_replaced_with_an_empty_one() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let original = JobStore::load(dir.path());
        original
            .upsert(Job {
                id: "original".into(),
                ..Default::default()
            })
            .unwrap();
        drop(original);
        let path = dir.path().join("jobs.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        if std::fs::read(&path).is_err() {
            assert!(JobStore::try_load(dir.path()).is_err());
            assert!(path.exists());
            assert!(!path.with_extension("json.bad").exists());
        }
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

    #[tokio::test]
    async fn stopping_a_backup_waiting_for_the_busy_slot_prevents_it_from_starting() {
        struct QuietSink;
        impl EventSink for QuietSink {
            fn job_progress(&self, _: JobProgress) {}
            fn queue_changed(&self, _: QueueSnapshot) {}
            fn job_finished(&self, _: JobRun) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let mut job = Job {
            id: "waiting".into(),
            name: "waiting".into(),
            ..Default::default()
        };
        job.advanced.retry_attempts = 0;
        store.upsert(job).unwrap();
        let queue = RunQueue::new();
        let busy = crate::busy::Busy::new();
        let held = busy
            .reserve(crate::busy::Doing::new("other", "probe"))
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run_queue_worker(
            Arc::new(QuietSink),
            store.clone(),
            queue.clone(),
            busy.clone(),
            cancel.clone(),
            Arc::new(NoPasswords),
            rx,
            tx.downgrade(),
            stop_rx,
        ));
        queue.enqueue("waiting");
        tx.send("waiting".into()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while queue.snapshot().current.as_deref() != Some("waiting") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.store(true, Ordering::SeqCst);
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.activity().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.activity()[0].status, "cancelled");
        worker.abort();
    }

    #[tokio::test]
    async fn a_waiting_backup_loads_the_job_again_after_a_key_repair_can_commit() {
        struct QuietSink;
        impl EventSink for QuietSink {
            fn job_progress(&self, _: JobProgress) {}
            fn queue_changed(&self, _: QueueSnapshot) {}
            fn job_finished(&self, _: JobRun) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let mut job = Job {
            id: "waiting".into(),
            name: "waiting".into(),
            ..Default::default()
        };
        job.advanced.retry_attempts = 0;
        store.upsert(job).unwrap();
        let queue = RunQueue::new();
        let busy = crate::busy::Busy::new();
        let held = busy
            .reserve(crate::busy::Doing::new("other", "probe"))
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run_queue_worker(
            Arc::new(QuietSink),
            store.clone(),
            queue.clone(),
            busy.clone(),
            cancel.clone(),
            Arc::new(NoPasswords),
            rx,
            tx.downgrade(),
            stop_rx,
        ));
        queue.enqueue("waiting");
        tx.send("waiting".into()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while queue.snapshot().current.as_deref() != Some("waiting") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut updated = store.get("waiting").unwrap();
        updated.name = "latest settings after key repair".into();
        store.upsert(updated).unwrap();
        cancel.store(true, Ordering::SeqCst);
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.activity().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.activity()[0].status, "cancelled");
        assert_eq!(
            store.activity()[0].job_name,
            "latest settings after key repair"
        );
        worker.abort();
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
        let accepted = winners
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|added| *added)
            .count();
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
        assert_eq!(
            store.resolve("V123").map(|z| z.to_string()),
            Some("secret".into())
        );

        store.forget("V123").unwrap();
        assert!(!PasswordStore::new(db).has("V123"));
    }

    /// Without its key store the service keeps a password for the run only:
    /// "remember" is refused, nothing reaches the database, and a password
    /// persisted earlier is not read back.
    #[test]
    fn a_memory_only_store_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.db");
        PasswordStore::new(db.clone())
            .remember("OLD", "earlier".into(), true)
            .unwrap();

        let store = PasswordStore::memory_only(db.clone());
        assert!(store.remember("V1", "secret".into(), true).is_err());
        store.remember("V2", "for-now".into(), false).unwrap();
        assert_eq!(
            store.resolve("V2").map(|z| z.to_string()),
            Some("for-now".into())
        );
        assert!(!store.has("OLD"));
        store.forget("V2").unwrap();
        assert!(!store.has("V2"));
        assert!(!PasswordStore::new(db.clone()).has("V1"));
        assert!(PasswordStore::new(db).has("OLD"));
    }

    /// The Linux service: legacy passwords sealed by `secret.key` beside the
    /// public database move into the private one re-sealed by the key store's
    /// sealer, and the legacy key — inside every copy of that volume — goes.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn sealed_private_store_reseals_legacy_passwords_and_drops_the_old_key() {
        struct Flip;
        impl own_backup_backup::secrets::SecretStore for Flip {
            fn protect(&self, p: &[u8]) -> own_backup_common::Result<Vec<u8>> {
                Ok(p.iter().map(|b| b ^ 0x5a).chain([0xEE]).collect())
            }
            fn unprotect(
                &self,
                b: &[u8],
            ) -> own_backup_common::Result<zeroize::Zeroizing<Vec<u8>>> {
                match b.split_last() {
                    Some((0xEE, body)) => Ok(zeroize::Zeroizing::new(
                        body.iter().map(|b| b ^ 0x5a).collect(),
                    )),
                    _ => Err(own_backup_common::OwnBackupError::Encryption(
                        "not ours".into(),
                    )),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let public_path = dir.path().join("metadata.db");
        let private_path = dir.path().join("private").join("volume-secrets.db");
        std::fs::create_dir(private_path.parent().unwrap()).unwrap();
        let legacy_key = dir.path().join("secret.key");
        let legacy = own_backup_backup::secrets::default_store(&legacy_key);
        let public = own_backup_metadata::open_database(&public_path).unwrap();
        own_backup_metadata::upsert_volume_secret(
            &public,
            "V1",
            &legacy.protect(b"luks words").unwrap(),
        )
        .unwrap();
        drop(public);

        let store = PasswordStore::new_private_sealed(
            public_path.clone(),
            private_path.clone(),
            Arc::new(Flip),
        );
        assert_eq!(store.migrate_legacy(&public_path).unwrap(), 1);
        assert!(!legacy_key.exists(), "the legacy key is gone");
        assert_eq!(store.resolve("V1").unwrap().as_str(), "luks words");
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        let row = own_backup_metadata::get_volume_secret(&private, "V1")
            .unwrap()
            .unwrap();
        assert_eq!(row.last(), Some(&0xEE), "re-sealed by the store's sealer");
        drop(private);

        store.remember("V2", "second".into(), true).unwrap();
        let cold = PasswordStore::new_private_sealed(public_path, private_path, Arc::new(Flip));
        assert_eq!(cold.resolve("V2").unwrap().as_str(), "second");
        assert!(!legacy_key.exists(), "and no new legacy key is made");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn legacy_password_migration_survives_duplicate_and_cleanup_resume() {
        let dir = tempfile::tempdir().unwrap();
        let public_path = dir.path().join("metadata.db");
        let private_path = dir.path().join("private").join("volume-secrets.db");
        std::fs::create_dir(private_path.parent().unwrap()).unwrap();
        let os_store = own_backup_backup::secrets::default_store(&dir.path().join("secret.key"));
        let blob = os_store.protect(b"old-password").unwrap();
        let public = own_backup_metadata::open_database(&public_path).unwrap();
        own_backup_metadata::upsert_volume_secret(&public, "V123", &blob).unwrap();
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        // Crash after the private commit but before the public delete.
        own_backup_metadata::upsert_volume_secret(&private, "V123", &blob).unwrap();
        own_backup_metadata::mark_volume_secret_migration_pending(&private).unwrap();
        drop(private);
        drop(public);

        let store = PasswordStore::new_private(public_path.clone(), private_path.clone());
        assert_eq!(store.migrate_legacy(&public_path).unwrap(), 1);
        assert_eq!(store.resolve("V123").unwrap().as_str(), "old-password");
        let public = own_backup_metadata::open_database(&public_path).unwrap();
        assert!(own_backup_metadata::get_volume_secret(&public, "V123")
            .unwrap()
            .is_none());
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        assert!(!own_backup_metadata::volume_secret_migration_pending(&private).unwrap());
        assert!(own_backup_metadata::volume_secret_migration_done(&private).unwrap());

        // Crash after the final public delete but before VACUUM/checkpoint.
        own_backup_metadata::mark_volume_secret_migration_pending(&private).unwrap();
        drop(private);
        drop(public);
        assert_eq!(store.migrate_legacy(&public_path).unwrap(), 0);
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        assert!(!own_backup_metadata::volume_secret_migration_pending(&private).unwrap());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn empty_legacy_database_is_scrubbed_once() {
        let dir = tempfile::tempdir().unwrap();
        let public_path = dir.path().join("metadata.db");
        let private_path = dir.path().join("private").join("volume-secrets.db");
        std::fs::create_dir(private_path.parent().unwrap()).unwrap();
        own_backup_metadata::open_database(&public_path).unwrap();
        let store = PasswordStore::new_private(public_path.clone(), private_path.clone());
        assert_eq!(store.migrate_legacy(&public_path).unwrap(), 0);
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        assert!(own_backup_metadata::volume_secret_migration_done(&private).unwrap());
        assert!(!own_backup_metadata::volume_secret_migration_pending(&private).unwrap());
        drop(private);
        assert_eq!(store.migrate_legacy(&public_path).unwrap(), 0);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn legacy_password_migration_refuses_conflicting_private_secret() {
        let dir = tempfile::tempdir().unwrap();
        let public_path = dir.path().join("metadata.db");
        let private_path = dir.path().join("private").join("volume-secrets.db");
        std::fs::create_dir(private_path.parent().unwrap()).unwrap();
        let os_store = own_backup_backup::secrets::default_store(&dir.path().join("secret.key"));
        let old_blob = os_store.protect(b"old-password").unwrap();
        let new_blob = os_store.protect(b"different-password").unwrap();
        let public = own_backup_metadata::open_database(&public_path).unwrap();
        own_backup_metadata::upsert_volume_secret(&public, "V123", &old_blob).unwrap();
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        own_backup_metadata::upsert_volume_secret(&private, "V123", &new_blob).unwrap();
        drop(private);
        drop(public);

        let store = PasswordStore::new_private(public_path.clone(), private_path.clone());
        assert!(store.migrate_legacy(&public_path).is_err());
        let public = own_backup_metadata::open_database(&public_path).unwrap();
        assert_eq!(
            own_backup_metadata::get_volume_secret(&public, "V123").unwrap(),
            Some(old_blob)
        );
        let private = own_backup_metadata::open_volume_secret_database(&private_path).unwrap();
        assert_eq!(
            own_backup_metadata::get_volume_secret(&private, "V123").unwrap(),
            Some(new_blob)
        );
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
        let (mut job, mut run, _) =
            job_with_last(dir.path(), "incremental", "incremental", 5, true);
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
        assert_eq!(
            drive_letter_of(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy5"),
            None
        );
    }

    #[test]
    fn a_mount_point_source_is_not_its_folders_drive() {
        let job = Job {
            source: r"C:\Mount\Data\".into(),
            sources: vec![r"C:\Mount\Data\".into()],
            ..Default::default()
        };
        assert_eq!(dest_collision_letter(&job, r"C:\Backups"), None);
        let job = Job {
            source: "C:".into(),
            sources: vec![r"\\.\C:".into(), r"D:\".into()],
            ..Default::default()
        };
        assert_eq!(dest_collision_letter(&job, r"d:\Backups"), Some('D'));
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
        assert_eq!(
            source_token(r"\\?\Volume{1a2b3c4d-5e6f-7089-abcd-ef0123456789}"),
            "V1A2B3C4D"
        );
        // Physical-offset partition → P<disk><offsetHex> (0x52000000 = 1375731712).
        assert_eq!(source_token("part:3:1375731712:1073741824"), "P352000000");
        // Something that is not a volume at all still reads as "X".
        assert_eq!(source_token(""), "X");
        assert_eq!(source_token("//server/share"), "X");

        // Linux devices: one token per volume, never "X", the same on every
        // run and on every machine that names the volume the same way.
        assert_eq!(
            source_token("/dev/disk/by-partuuid/2d272048-0d61-4a8c-a715-08d35535ed93"),
            "U2D272048"
        );
        assert_eq!(
            source_token("/dev/disk/by-partuuid/4955b993-01"),
            "U4955B99301"
        );
        assert_eq!(source_token("/dev/disk/by-uuid/060B-E961"), "F060BE961");
        assert_eq!(
            source_token(" /dev/mapper/luks-16e1f072-b3b5-4aa7-b296-67b738edf0ca "),
            "H29CA025F"
        );
        assert_eq!(source_token("/dev/sda2"), "H1BCE3BBF");
        assert_ne!(source_token("/dev/sda2"), source_token("/dev/sdb2"));
        // And the file name round-trips through the image-name parser.
        let dev = "/dev/mapper/vg0-home";
        let linux = Job {
            sources: vec![dev.into()],
            source_labels: vec!["home".into()],
            ..Default::default()
        };
        let token = file_token(&linux, dev);
        assert_eq!(token, source_token(dev));
        let (name, _) = output_paths_for(&linux, &token, "full");
        assert_eq!(image_token(&name).as_deref(), Some(token.as_str()));

        // Names are `<name>_<token>_<tag>_<stamp>`; token is read from the end
        // so underscores inside <name> do not confuse it.
        assert_eq!(
            image_token(Path::new("job_C_full_20260101-000000.vhdx")).as_deref(),
            Some("C")
        );
        assert_eq!(
            image_token(Path::new("My_Big_Job_V1A2B3C4D_inc_20260101-000000.avhdx")).as_deref(),
            Some("V1A2B3C4D")
        );
        assert_eq!(image_token(Path::new("no_underscores.vhdx")), None);

        // Whole-disk images `<name>_disk<N>_<tag>_<stamp>` → token is `disk<N>`
        // (3rd-from-last), for the full (.vhdx) and its child (.avhdx) alike.
        assert_eq!(
            image_token(Path::new("MODULATOR_disk3_full_20260711-104244.vhdx")).as_deref(),
            Some("DISK3")
        );
        assert_eq!(
            image_token(Path::new("My_Job_disk12_inc_20260711-104244.avhdx")).as_deref(),
            Some("DISK12")
        );
    }

    #[test]
    fn file_token_uses_label_for_encrypted_sources() {
        let dev = "part:4:1048576:1000";
        let mut job = Job::default();
        job.sources = vec![dev.into()];
        // A label with an underscore is stripped to an underscore-free token.
        job.source_labels = vec!["kubuntu_boot".into()];
        let token = file_token(&job, dev);
        assert_eq!(token, "kubuntubootP4100000");
        // The file name round-trips: image_token reads it back (case-insensitive).
        let (name, _) = output_paths_for(&job, &token, "full");
        assert!(image_token(&name).unwrap().eq_ignore_ascii_case(&token));

        // A planned child carries the full's name as well, for a fallback:
        // same stamp, `_full_` and the full's extension.
        let (child, full) = output_paths_for(&job, "disk4", "incremental");
        let (child, full) = (
            child.file_name().unwrap().to_string_lossy().into_owned(),
            full.file_name().unwrap().to_string_lossy().into_owned(),
        );
        assert!(
            child.contains("_disk4_inc_") && child.ends_with(".avhdx"),
            "{child}"
        );
        assert!(
            full.contains("_disk4_full_") && full.ends_with(".vhdx"),
            "{full}"
        );
        assert_eq!(
            child.replace("_inc_", "_full_").replace(".avhdx", ".vhdx"),
            full
        );

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
        assert_eq!(
            multi.source_list(),
            vec!["C:".to_string(), "E:".to_string()]
        );
        assert_eq!(multi.label_for("E:"), "E: Data");
        assert_eq!(multi.label_for("Z:"), "Z:"); // unknown → device itself
    }

    /// Write a real (parseable) sidecar + dummy image so `list_job_chains`
    /// discovers it. `name` must follow `<n>_<letter>_<tag>_<stamp>.<ext>`.
    pub(super) fn put_real_image(
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
                stable_device: String::new(),
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
        put_real_image(
            d,
            "job_C_full_20260101-000000.vhdx",
            ChainRole::Full,
            None,
            "job1",
            "2026-01-01T00:00:00Z",
        );
        put_real_image(
            d,
            "job_E_full_20260101-000000.vhdx",
            ChainRole::Full,
            None,
            "job1",
            "2026-01-01T00:00:00Z",
        );

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

        put_real_image(
            d,
            "job_C_full_20260101-000000.vhdx",
            ChainRole::Full,
            None,
            "job1",
            "2026-01-01T00:00:00Z",
        );
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
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
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
        put_real_image(
            d,
            base_name,
            ChainRole::Full,
            None,
            "job1",
            "2026-01-01T00:00:00Z",
        );
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
        put_real_image(
            d,
            base_name,
            ChainRole::Full,
            None,
            "job1",
            "2026-04-01T00:00:00Z",
        );
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
        put_real_image(
            d,
            "job_disk3_full_20260101-000000.vhdx",
            ChainRole::Full,
            None,
            "job1",
            "2026-01-01T00:00:00Z",
        );
        put_real_image(
            d,
            "job_disk3_inc_20260102-000000.avhdx",
            ChainRole::Incremental,
            Some("job_disk3_full_20260101-000000.vhdx"),
            "job1",
            "2026-01-02T00:00:00Z",
        );
        // A different disk's full must never be picked as disk 3's parent.
        put_real_image(
            d,
            "job_disk4_full_20260103-000000.vhdx",
            ChainRole::Full,
            None,
            "job1",
            "2026-01-03T00:00:00Z",
        );

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
                stable_device: String::new(),
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
        write_sidecar(
            &own_backup_backup::sidecar_path_for(&image),
            &info,
            &manifest,
            &[],
        )
        .unwrap();
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

    #[test]
    fn client_run_history_cannot_authorize_deletion_of_an_unrelated_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images");
        std::fs::create_dir(&dest).unwrap();
        let unrelated = dir.path().join("unrelated.txt");
        std::fs::write(&unrelated, b"keep this").unwrap();
        let store = a_store(dir.path());

        for id in ["", "caller-selected-id"] {
            let saved = store
                .upsert(Job {
                    id: id.into(),
                    dest_folder: dest.display().to_string(),
                    last_run: Some(JobRun {
                        image_path: unrelated.display().to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .unwrap();
            assert!(saved.last_run.is_none());
            let report = delete_job_files(&store, &saved);
            assert!(unrelated.exists(), "{report:?}");
            assert!(report.deleted.is_empty(), "{report:?}");
        }
    }

    #[test]
    fn job_delete_never_retries_a_base_kept_for_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images");
        let base = put_image_of_job(&dest, "j_C_full_1.vhdx", "job1");
        let child = put_image_of_job(&dest, "j_C_inc_2.avhdx", "job1");
        let meta_path = own_backup_backup::sidecar_path_for(&child);
        let sidecar = own_backup_backup::sidecar::read_sidecar(&meta_path).unwrap();
        let mut info = sidecar.info;
        info.chain_role = own_backup_backup::sidecar::ChainRole::Incremental;
        info.parent_image_file_name = Some("j_C_full_1.vhdx".into());
        let mut manifest = sidecar.manifest;
        manifest.created_at = "2026-01-02T00:00:00Z".into();
        own_backup_backup::sidecar::write_sidecar(&meta_path, &info, &manifest, &[]).unwrap();
        std::fs::remove_file(&child).unwrap();
        std::fs::create_dir(&child).unwrap(); // deterministic remove_file failure

        let store = a_store(dir.path());
        let job = store
            .upsert(Job {
                id: "job1".into(),
                dest_folder: dest.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        store
            .record_run(JobRun {
                job_id: job.id.clone(),
                image_path: child.display().to_string(),
                base_image_path: base.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        let report = delete_job_files(&store, &job);
        assert!(child.exists() && base.exists(), "{report:?}");
        assert!(!report.deleted.contains(&base.display().to_string()));
        let deleted: std::collections::HashSet<&str> =
            report.deleted.iter().map(String::as_str).collect();
        assert!(
            report
                .kept
                .iter()
                .all(|(path, _)| !deleted.contains(path.as_str())),
            "{report:?}"
        );
    }

    #[test]
    fn unreadable_child_cannot_hide_its_dependency_from_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images");
        let base = put_image_of_job(&dest, "j_C_full_1.vhdx", "job1");
        let unreadable_child = dest.join("j_C_inc_2.qcow2");
        std::fs::write(&unreadable_child, b"cannot read embedded sidecar").unwrap();

        let store = a_store(dir.path());
        let job = store
            .upsert(Job {
                id: "job1".into(),
                dest_folder: dest.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        let report = delete_job_files(&store, &job);
        assert!(base.exists() && unreadable_child.exists(), "{report:?}");
        assert!(report.deleted.is_empty(), "{report:?}");
        assert!(!report.complete());
    }

    #[test]
    fn retention_does_not_delete_bases_when_an_image_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let old_base = put_image_of_job(dir.path(), "j_C_full_1.vhdx", "job1");
        let new_base = put_image_of_job(dir.path(), "j_C_full_2.vhdx", "job1");
        std::fs::write(dir.path().join("j_C_inc_3.qcow2"), b"unreadable child").unwrap();

        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = dir.path().display().to_string();
        job.scheme.keep_chains = Some(1);
        apply_retention(&job, &[("C".into(), new_base.clone())]);
        assert!(old_base.exists() && new_base.exists());
    }

    #[test]
    fn job_delete_never_retries_a_chain_with_a_consolidation_marker() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images");
        let base = put_image_of_job(&dest, "j_C_full_1.vhdx", "job1");
        let child = put_image_of_job(&dest, "j_C_inc_2.avhdx", "job1");
        let child_sidecar = own_backup_backup::sidecar_path_for(&child);
        let sidecar = own_backup_backup::sidecar::read_sidecar(&child_sidecar).unwrap();
        let mut info = sidecar.info;
        info.chain_role = own_backup_backup::sidecar::ChainRole::Incremental;
        info.parent_image_file_name = Some("j_C_full_1.vhdx".into());
        let mut manifest = sidecar.manifest;
        manifest.created_at = "2026-01-02T00:00:00Z".into();
        own_backup_backup::sidecar::write_sidecar(&child_sidecar, &info, &manifest, &[]).unwrap();
        let marker = own_backup_backup::consolidate::marker_path(&base);
        std::fs::write(&marker, b"{}").unwrap();
        let store = a_store(dir.path());
        let job = store
            .upsert(Job {
                id: "job1".into(),
                dest_folder: dest.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        store
            .record_run(JobRun {
                job_id: job.id.clone(),
                image_path: base.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        let report = delete_job_files(&store, &job);
        assert!(
            base.exists() && child.exists() && marker.exists(),
            "{report:?}"
        );
        assert!(report.deleted.is_empty(), "{report:?}");
        for path in [
            &base,
            &child,
            &own_backup_backup::sidecar_path_for(&base),
            &child_sidecar,
        ] {
            assert!(
                report
                    .kept
                    .iter()
                    .any(|(kept, _)| kept == &path.display().to_string()),
                "{path:?}: {report:?}"
            );
        }
    }

    #[test]
    fn retention_separates_sources_sharing_a_label_and_keeps_both_new_images() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images");
        let make = |name: &str, source: &str, stamp: &str| {
            let path = put_image_of_job(&dest, name, "job1");
            let sidecar_path = own_backup_backup::sidecar_path_for(&path);
            let sidecar = own_backup_backup::sidecar::read_sidecar(&sidecar_path).unwrap();
            let mut info = sidecar.info;
            info.source_id = Some(source.into());
            let mut manifest = sidecar.manifest;
            manifest.created_at = stamp.into();
            own_backup_backup::sidecar::write_sidecar(&sidecar_path, &info, &manifest, &[])
                .unwrap();
            path
        };
        let old_a = make("j_DATA_full_1.vhdx", "source-A", "2026-01-01T00:00:00Z");
        let old_b = make("j_DATA_full_2.vhdx", "source-B", "2026-01-02T00:00:00Z");
        let new_a = make("j_DATA_full_3.vhdx", "source-A", "2026-01-03T00:00:00Z");
        let new_b = make("j_DATA_full_4.vhdx", "source-B", "2026-01-04T00:00:00Z");
        let mut job = Job::default();
        job.id = "job1".into();
        job.dest_folder = dest.display().to_string();
        job.scheme.keep_chains = Some(1);
        apply_retention(
            &job,
            &[
                ("DATA".into(), new_a.clone()),
                ("DATA".into(), new_b.clone()),
            ],
        );
        assert!(!old_a.exists() && !old_b.exists());
        assert!(new_a.exists() && new_b.exists());
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
        assert!(
            !dest.join("j_D_full_1.vhdx").exists(),
            "второй том тоже принадлежит заданию"
        );
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
        assert!(
            dest.join("theirs_full_1.vhdx").exists(),
            "чужое остаётся нетронутым"
        );
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
        assert!(
            !old.join("j_C_full_1.vhdx").exists(),
            "старая папка: {report:?}"
        );
        assert!(
            !new.join("j_C_full_2.vhdx").exists(),
            "новая папка: {report:?}"
        );
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
        assert!(
            report
                .kept
                .iter()
                .any(|(p, _)| p.contains("j_C_full_1.vhdx")),
            "{report:?}"
        );
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
        assert!(
            found.contains(&PathBuf::from("/dest/second.vhdx")),
            "{found:?}"
        );
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

    /// Цепочка без предела по схеме («только инкременты», консолидации нет)
    /// всё равно начинается заново на 60 образах: читатель VHDX глубже 64 не
    /// идёт, и на стенде 18.10 всё новее 63-го стало нечитаемым.
    #[test]
    fn an_endless_scheme_still_starts_over_before_the_reader_limit() {
        let image = |i: usize| own_backup_backup::chains::ChainImage {
            image_path: PathBuf::from(format!("j_C_inc_{i}.avhdx")),
            sidecar_path: PathBuf::from(format!("j_C_inc_{i}.avhdx.obkm")),
            role: own_backup_backup::sidecar::ChainRole::Incremental,
            container: "vhdx".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            parent_file_name: None,
            source_id: None,
            consolidated_increments: 0,
            chain_started_at: None,
        };
        let mut job = Job::default();
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true;
        let now = chrono::Utc::now();
        let short = own_backup_backup::chains::Chain {
            images: (0..MAX_CHAIN_IMAGES - 1).map(image).collect(),
        };
        assert!(!should_start_new_chain(&job, &short, now));
        let long = own_backup_backup::chains::Chain {
            images: (0..MAX_CHAIN_IMAGES).map(image).collect(),
        };
        assert!(should_start_new_chain(&job, &long, now));
    }

    /// Тот же том под другим именем — буквой, путём GUID или точкой
    /// монтирования, или с новой буквой: идентичность записана с обеих
    /// сторон, и цепочка его, хотя токен в имени файла другой (стенд,
    /// 18.10: смена `P:` на путь GUID начинала новую полную копию).
    #[test]
    fn the_same_volume_under_another_name_continues_its_chain() {
        let id = r"\?\VOLUME{99A5A6D1-57E5-4920-9DF1-CA5BF32BE042}";
        let base = own_backup_backup::chains::ChainImage {
            image_path: PathBuf::from("j_P_full_1.vhdx"),
            sidecar_path: PathBuf::from("j_P_full_1.vhdx.obkm"),
            role: own_backup_backup::sidecar::ChainRole::Full,
            container: "vhdx".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            parent_file_name: None,
            source_id: Some(id.into()),
            consolidated_increments: 0,
            chain_started_at: None,
        };
        assert!(chain_belongs_to(
            &base,
            "V99A5A6D1",
            &Some(id.to_lowercase())
        ));
        assert!(chain_source_group_matches(&base, "V99A5A6D1", Some(id)));
        // Другой том с тем же именем по-прежнему чужой.
        let other = r"\?\VOLUME{22222222-2222-2222-2222-222222222222}";
        assert!(!chain_belongs_to(&base, "P", &Some(other.into())));
        assert!(!chain_source_group_matches(&base, "P", Some(other)));
    }

    /// Ноль значит «выключено» — как у keep_chains, cleanup_older_than_days и
    /// rolling_incrementals, и как обещает справка CLI. Оба места писали
    /// `full_every_n.max(1)`, поэтому ноль становился единицей и «не начинать
    /// новую цепочку» давало самую агрессивную настройку из возможных: полную
    /// копию после каждого инкремента. Поймано живым прогоном 2026-09-20.
    #[test]
    fn zero_means_never_rolling_over_not_rolling_over_every_time() {
        let mut s = SchemeConfig {
            method: "incremental".into(),
            ..Default::default()
        };
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

    // --- «Очистить версии» и «проверить версии»: что у задания лежит на диске

    /// Задание `job1` с двумя источниками в одной папке:
    /// `C`: full ← inc1 ← inc2 и следом новая цепочка full;  `D`: full ← inc.
    fn job_with_versions(dir: &Path) -> (Arc<JobStore>, Job, PathBuf) {
        use own_backup_backup::sidecar::ChainRole::{Full, Incremental};
        let dest = dir.join("images");
        std::fs::create_dir_all(&dest).unwrap();
        let day = |n: u32| format!("2026-01-{n:02}T00:00:00Z");
        put_real_image(&dest, "j_C_full_1.vhdx", Full, None, "job1", &day(1));
        put_real_image(
            &dest,
            "j_C_inc_2.avhdx",
            Incremental,
            Some("j_C_full_1.vhdx"),
            "job1",
            &day(2),
        );
        put_real_image(
            &dest,
            "j_C_inc_3.avhdx",
            Incremental,
            Some("j_C_inc_2.avhdx"),
            "job1",
            &day(3),
        );
        put_real_image(&dest, "j_C_full_4.vhdx", Full, None, "job1", &day(4));
        put_real_image(&dest, "j_D_full_1.vhdx", Full, None, "job1", &day(1));
        put_real_image(
            &dest,
            "j_D_inc_5.avhdx",
            Incremental,
            Some("j_D_full_1.vhdx"),
            "job1",
            &day(5),
        );
        // Чужое задание в той же папке не должно попасть ни в один список.
        put_real_image(&dest, "x_C_full_9.vhdx", Full, None, "other", &day(9));
        let store = a_store(dir);
        let job = store
            .upsert(Job {
                id: "job1".into(),
                dest_folder: dest.display().to_string(),
                ..Default::default()
            })
            .unwrap();
        (store, job, dest)
    }

    #[test]
    fn versions_are_listed_by_source_and_chain_with_what_each_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());
        let versions = job_versions(&store, &job);
        assert!(versions.unreadable.is_empty(), "{versions:?}");

        let shape: Vec<(String, bool, Vec<String>)> = versions
            .chains
            .iter()
            .map(|c| {
                (
                    c.source.clone(),
                    c.current,
                    c.images.iter().map(|i| i.file_name.clone()).collect(),
                )
            })
            .collect();
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                (
                    "C".to_string(),
                    false,
                    names(&["j_C_full_1.vhdx", "j_C_inc_2.avhdx", "j_C_inc_3.avhdx"])
                ),
                ("C".to_string(), true, names(&["j_C_full_4.vhdx"])),
                (
                    "D".to_string(),
                    true,
                    names(&["j_D_full_1.vhdx", "j_D_inc_5.avhdx"])
                ),
            ]
        );

        let inc3 = &versions.chains[0].images[2];
        assert_eq!(inc3.role, "incremental");
        assert_eq!(
            inc3.depends_on.as_deref(),
            Some(dest.join("j_C_inc_2.avhdx").display().to_string().as_str())
        );
        assert!(versions.chains[0].images[0].depends_on.is_none());
        // Образ и его описание рядом: размер — оба файла, а не ноль.
        assert!(inc3.size_bytes > 3, "{}", inc3.size_bytes);
    }

    #[test]
    fn ticked_versions_are_deleted_and_the_rest_of_the_job_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());
        // Вся старая цепочка C и хвост цепочки D.
        let ticked: Vec<PathBuf> = [
            "j_C_full_1.vhdx",
            "j_C_inc_2.avhdx",
            "j_C_inc_3.avhdx",
            "j_D_inc_5.avhdx",
        ]
        .iter()
        .map(|n| dest.join(n))
        .collect();
        let report = delete_job_versions(&store, &job, &ticked);
        assert!(report.kept.is_empty(), "{report:?}");
        for gone in &ticked {
            assert!(!gone.exists(), "{}", gone.display());
        }
        for stays in ["j_C_full_4.vhdx", "j_D_full_1.vhdx", "x_C_full_9.vhdx"] {
            assert!(dest.join(stays).exists(), "{stays}");
        }
        let left: Vec<String> = job_versions(&store, &job)
            .chains
            .iter()
            .flat_map(|c| c.images.iter().map(|i| i.file_name.clone()))
            .collect();
        assert_eq!(left, ["j_C_full_4.vhdx", "j_D_full_1.vhdx"]);
    }

    /// Окно само отмечает зависимые версии. Служба ему не верит: набор, после
    /// которого инкремент остался бы без основы, не выполняется вовсе.
    #[test]
    fn a_set_that_would_leave_an_increment_without_its_base_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());
        let report = delete_job_versions(&store, &job, &[dest.join("j_C_inc_2.avhdx")]);
        assert!(report.deleted.is_empty(), "{report:?}");
        assert_eq!(report.kept.len(), 1, "{report:?}");
        assert!(dest.join("j_C_inc_2.avhdx").exists() && dest.join("j_C_inc_3.avhdx").exists());
    }

    /// Путь удаляется только как член цепочки ЭТОГО задания — не как путь.
    #[test]
    fn a_path_that_is_not_this_jobs_copy_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());
        let foreign = dest.join("x_C_full_9.vhdx");
        let plain = dest.join("notes.txt");
        std::fs::write(&plain, b"mine").unwrap();
        let report = delete_job_versions(&store, &job, &[foreign.clone(), plain.clone()]);
        assert!(report.deleted.is_empty(), "{report:?}");
        assert_eq!(report.kept.len(), 2, "{report:?}");
        assert!(foreign.exists() && plain.exists());
    }

    #[test]
    fn the_latest_version_is_one_image_per_source_and_all_versions_is_every_image() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());

        let latest = job_verify_targets(&store, &job, false).unwrap();
        assert_eq!(
            latest,
            vec![
                (dest.join("j_C_full_4.vhdx"), false),
                (dest.join("j_D_inc_5.avhdx"), false),
            ]
        );

        let all = job_verify_targets(&store, &job, true).unwrap();
        assert_eq!(all.len(), 6, "{all:?}");
        assert!(all.iter().all(|(_, own_blocks_only)| !*own_blocks_only));
        assert!(!all.iter().any(|(p, _)| p.ends_with("x_C_full_9.vhdx")));
    }

    /// «Все версии целы» нельзя сказать про набор с дырой: образ, который не
    /// читается, останавливает проверку, а не пропускается молча.
    #[test]
    fn an_unreadable_image_fails_the_verify_listing_instead_of_being_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (store, job, dest) = job_with_versions(dir.path());
        std::fs::write(
            dest.join("j_C_inc_6.qcow2"),
            b"cannot read embedded sidecar",
        )
        .unwrap();
        assert!(job_verify_targets(&store, &job, true).is_err());
    }
}

/// The Linux driver end to end, on what an unprivileged test can hold: an
/// image file as the source (read as it is — nobody vouched it holds still),
/// copied by the shared engine, published, read back and recorded.
#[cfg(all(test, target_os = "linux"))]
mod linux_run_tests {
    use super::*;

    #[derive(Default)]
    struct Phases(Mutex<Vec<String>>);
    impl EventSink for Phases {
        fn job_progress(&self, p: JobProgress) {
            self.0.lock().unwrap().push(p.phase);
        }
        fn queue_changed(&self, _: QueueSnapshot) {}
        fn job_finished(&self, _: JobRun) {}
    }

    #[tokio::test]
    async fn a_file_source_is_copied_published_verified_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("volume.img");
        let data: Vec<u8> = (0..6u32 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dest = dir.path().join("backups");
        std::fs::create_dir(&dest).unwrap();
        let mut job = Job {
            id: "linux-run".into(),
            name: "linux run".into(),
            source: src.to_str().unwrap().into(),
            dest_folder: dest.to_str().unwrap().into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            ..Default::default()
        };
        job.advanced.validate_after = true;
        // A new job's default rules; Linux refuses them until L3.
        job.exclusions.enabled = false;
        let phases = Arc::new(Phases::default());
        let sink: Arc<dyn EventSink> = phases.clone();
        let passwords: Arc<dyn PasswordResolver> = Arc::new(NoPasswords);

        let run = perform_backup(
            &sink,
            &job,
            &Arc::new(AtomicBool::new(false)),
            None,
            &passwords,
            None,
        )
        .await;

        assert_eq!(run.status, "success", "{}", run.error);
        assert_eq!(run.backup_kind, "full");
        assert_eq!(run.images.len(), 1);
        let image = PathBuf::from(&run.images[0].image_path);
        assert_eq!(run.image_path, run.images[0].image_path);
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("linux_run_") && name.contains("_full_") && name.ends_with(".qcow2"),
            "{name}"
        );
        // The only file in the folder: nothing private is left beside it.
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
        // Not vouched for: copied whole, and the record says why.
        assert!(!run.smart_mode && !run.vss_used);
        assert_eq!(run.used_blocks, run.total_blocks);
        assert_eq!(run.source_bytes, data.len() as u64);
        assert!(!run.file_scan_skipped.is_empty());
        let sidecar = own_backup_backup::sidecar::read_image_sidecar(&image).unwrap();
        assert_eq!(sidecar.manifest.source_disk.device_id, job.source);
        assert_eq!(sidecar.info.job_id.as_deref(), Some("linux-run"));
        let shown = phases.0.lock().unwrap().clone();
        for phase in ["backup", "finalize", "verify"] {
            assert!(shown.iter().any(|p| p == phase), "{phase}: {shown:?}");
        }
    }

    /// A qcow2 chain job on Linux (plan L2): the first run is a full, the
    /// next ones increments of it; with a window of two increments the
    /// oldest is folded into the base, and the newest image still holds the
    /// source exactly as it is.
    #[tokio::test]
    async fn a_qcow2_chain_grows_and_is_consolidated() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("volume.img");
        let data: Vec<u8> = (0..6u32 << 20).map(|i| (i % 241) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dest = dir.path().join("backups");
        std::fs::create_dir(&dest).unwrap();
        let mut job = Job {
            id: "linux-chain".into(),
            name: "linux chain".into(),
            source: src.to_str().unwrap().into(),
            dest_folder: dest.to_str().unwrap().into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            ..Default::default()
        };
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true;
        job.scheme.rolling_incrementals = Some(2);
        job.exclusions.enabled = false;
        let sink: Arc<dyn EventSink> = Arc::new(Phases::default());
        let passwords: Arc<dyn PasswordResolver> = Arc::new(NoPasswords);
        let cancel = Arc::new(AtomicBool::new(false));

        let mut kinds = Vec::new();
        let mut last = None;
        for n in 0..4u64 {
            if n > 0 {
                // Image names carry the second they were made in.
                std::thread::sleep(std::time::Duration::from_millis(1100));
                let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
                f.write_all_at(&[n as u8; 1000], n << 20).unwrap();
            }
            let run = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
            assert_eq!(run.status, "success", "run {n}: {}", run.error);
            kinds.push(run.backup_kind.clone());
            if n > 0 {
                assert_eq!(run.bytes_copied, 1 << 20, "run {n}: one changed block");
            }
            last = Some(run);
        }
        assert_eq!(kinds, ["full", "incremental", "incremental", "incremental"]);
        let last = last.unwrap();
        assert_eq!(last.consolidated_increments, 1, "the window holds two");
        let files: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 3, "base + two increments: {files:?}");

        // The newest version restores to the source as it is now.
        let target = dir.path().join("restored.img");
        std::fs::File::create(&target)
            .unwrap()
            .set_len(data.len() as u64)
            .unwrap();
        own_backup_backup::engine_linux::restore_volume(
            &own_backup_backup::engine_linux::LinuxRestoreRequest {
                image_path: PathBuf::from(&last.image_path),
                target: target.to_str().unwrap().into(),
                verify_checksums: true,
                encryption: None,
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());

        // A VHDX job chains too (CHECKS 12): a full, then a differencing child.
        job.image_format = own_backup_common::types::ImageFormat::Vhdx;
        let fresh = dir.path().join("vhdx");
        std::fs::create_dir(&fresh).unwrap();
        job.dest_folder = fresh.to_str().unwrap().into();
        assert!(crate::capabilities::refusals(&job, &crate::capabilities::linux()).is_empty());
        let first = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
        assert_eq!(first.status, "success", "{}", first.error);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
        assert_eq!(second.status, "success", "{}", second.error);
        assert_eq!(second.backup_kind, "incremental");
        assert!(
            second.image_path.ends_with(".avhdx"),
            "{}",
            second.image_path
        );
    }

    /// An encrypted job on Linux (decision 6): with the keys the service
    /// opened before the run, every version is an `.obk` — a full, then an
    /// increment over it, consolidated in their window — checked with those
    /// keys, and the newest restores only with them.
    #[tokio::test]
    async fn an_encrypted_chain_is_written_checked_and_restored_on_linux() {
        use own_backup_backup::seal::context::ArchiveKeys;
        use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("volume.img");
        let data: Vec<u8> = (0..4u32 << 20).map(|i| (i % 239) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dest = dir.path().join("backups");
        std::fs::create_dir(&dest).unwrap();

        let pair = JobKeyPair::generate().unwrap();
        let recovery = own_backup_backup::seal::recovery::RecoveryKey::generate().unwrap();
        let portable = SealedPrivateKey::seal(
            "linux-obk",
            &pair,
            "archive password",
            &recovery,
            &KdfParams {
                algorithm: "argon2id".into(),
                m_kib: 64 * 1024,
                t: 3,
                p: 1,
            },
        )
        .unwrap();
        let keys = ArchiveKeys::from_private(portable.clone(), pair.private.clone()).unwrap();
        let mut job = Job {
            id: "linux-obk".into(),
            name: "linux obk".into(),
            source: src.to_str().unwrap().into(),
            dest_folder: dest.to_str().unwrap().into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            archive_encryption: Some(crate::backup_encryption::ArchiveEncryption {
                portable_key: portable,
                local_key: serde_json::from_value(serde_json::json!({"id": uuid::Uuid::new_v4()}))
                    .unwrap(),
            }),
            ..Default::default()
        };
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true;
        job.scheme.rolling_incrementals = Some(1);
        job.exclusions.enabled = false;
        job.advanced.validate_after = true;
        assert!(crate::capabilities::refusals(&job, &crate::capabilities::linux()).is_empty());
        let sink: Arc<dyn EventSink> = Arc::new(Phases::default());
        let passwords: Arc<dyn PasswordResolver> = Arc::new(NoPasswords);
        let cancel = Arc::new(AtomicBool::new(false));

        let mut last = None;
        for n in 0..3u64 {
            if n > 0 {
                std::thread::sleep(std::time::Duration::from_millis(1100));
                let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
                f.write_all_at(&[0xA0 + n as u8; 1000], n << 20).unwrap();
            }
            let run =
                perform_backup(&sink, &job, &cancel, None, &passwords, Some(keys.clone())).await;
            assert_eq!(run.status, "success", "run {n}: {}", run.error);
            assert!(run.image_path.ends_with(".obk"), "{}", run.image_path);
            last = Some(run);
        }
        let last = last.unwrap();
        assert_eq!(last.backup_kind, "incremental");
        assert_eq!(last.consolidated_increments, 1, "the window holds one");
        for entry in std::fs::read_dir(&dest).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(name.ends_with(".obk"), "only encrypted images: {name}");
        }
        // Nothing of the source is in the folder in the clear.
        let probe = &data[(2 << 20) + 4096..(2 << 20) + 8192];
        for entry in std::fs::read_dir(&dest).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(probe.len()).any(|w| w == probe));
        }

        let target = dir.path().join("restored.img");
        std::fs::File::create(&target)
            .unwrap()
            .set_len(data.len() as u64)
            .unwrap();
        let restore = |encryption| {
            own_backup_backup::engine_linux::restore_volume(
                &own_backup_backup::engine_linux::LinuxRestoreRequest {
                    image_path: PathBuf::from(&last.image_path),
                    target: target.to_str().unwrap().into(),
                    verify_checksums: true,
                    encryption,
                },
                |_| Ok(()),
            )
        };
        assert!(restore(None).is_err(), "not without its keys");
        let by_password =
            ArchiveKeys::from_image_password(Path::new(&last.image_path), "archive password")
                .unwrap();
        restore(Some(by_password)).unwrap();
        assert!(std::fs::read(&target).unwrap() == std::fs::read(&src).unwrap());
    }

    /// A job with its default exclusion rules is run, not refused (plan L3):
    /// a source nobody holds still is copied whole, and the run says the
    /// rules were not applied — rather than "0 files excluded".
    #[tokio::test]
    async fn exclusions_on_a_source_not_held_still_are_reported_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("volume.img");
        std::fs::write(&src, vec![3u8; 2 << 20]).unwrap();
        let dest = dir.path().join("backups");
        std::fs::create_dir(&dest).unwrap();
        let job = Job {
            id: "linux-excl".into(),
            name: "linux excl".into(),
            source: src.to_str().unwrap().into(),
            dest_folder: dest.to_str().unwrap().into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            ..Default::default()
        };
        assert!(job.exclusions.enabled && !job.exclusions.patterns_linux.is_empty());
        let sink: Arc<dyn EventSink> = Arc::new(Phases::default());
        let passwords: Arc<dyn PasswordResolver> = Arc::new(NoPasswords);
        let run = perform_backup(
            &sink,
            &job,
            &Arc::new(AtomicBool::new(false)),
            None,
            &passwords,
            None,
        )
        .await;
        assert_eq!(run.status, "success", "{}", run.error);
        assert_eq!(run.excluded_files, 0);
        // The engine's own sentence (its catalog, not this crate's).
        assert!(!run.file_scan_skipped.is_empty());
        assert!(run.excluded_entries_kept.is_empty());
    }

    /// A whole disk on Linux through a job (plan E6): named by its device,
    /// its images by the device's token, a chain planned from them; a disk
    /// named by number only is refused.
    #[tokio::test]
    async fn a_whole_disk_job_names_its_images_by_the_device() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let mib = 1u64 << 20;
        // Two "partitions" as files, and the disk holding them behind a GPT.
        let p1 = dir.path().join("p1.img");
        let p2 = dir.path().join("p2.img");
        std::fs::write(&p1, vec![0x11; 2 * mib as usize]).unwrap();
        std::fs::write(&p2, vec![0x22; 3 * mib as usize]).unwrap();
        let layout = own_backup_common::gpt::DiskLayout {
            disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0".into(),
            sector_size: 512,
            size_bytes: 8 * mib,
            partitions: [(1u32, mib, 2 * mib), (2, 3 * mib, 3 * mib)]
                .iter()
                .map(
                    |&(index, at, len)| own_backup_common::gpt::GptPartitionEntry {
                        index,
                        type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into(),
                        part_guid: format!("11111111-0000-0000-0000-00000000000{index}"),
                        first_lba: at / 512,
                        last_lba: (at + len) / 512 - 1,
                        attributes: 0,
                        name: String::new(),
                    },
                )
                .collect(),
        };
        let disk = dir.path().join("disk.img");
        let f = std::fs::File::create(&disk).unwrap();
        f.set_len(layout.size_bytes).unwrap();
        for op in
            own_backup_backup::gpt_writer::disk_gpt_write_ops(&layout, layout.size_bytes).unwrap()
        {
            f.write_all_at(&op.bytes, op.offset).unwrap();
        }
        drop(f);
        let dest = dir.path().join("backups");
        std::fs::create_dir(&dest).unwrap();
        let ds = DiskSourceSpec {
            disk_number: 0,
            device: Some(disk.to_str().unwrap().into()),
            disk_model: "file disk".into(),
            parts: [(1, &p1), (2, &p2)]
                .iter()
                .map(|(i, p)| DiskPartSpec {
                    part_index: *i,
                    device: p.to_str().unwrap().into(),
                    filesystem: String::new(),
                })
                .collect(),
            bootable: false,
        };
        let mut job = Job {
            id: "linux-disk".into(),
            name: "linux disk".into(),
            dest_folder: dest.to_str().unwrap().into(),
            image_format: own_backup_common::types::ImageFormat::Qcow2,
            disk_source: Some(ds.clone()),
            ..Default::default()
        };
        job.scheme.method = "incremental".into();
        job.scheme.only_incremental = true;
        job.exclusions.enabled = false;
        job.advanced.validate_after = true;
        let sink: Arc<dyn EventSink> = Arc::new(Phases::default());
        let passwords: Arc<dyn PasswordResolver> = Arc::new(NoPasswords);
        let cancel = Arc::new(AtomicBool::new(false));

        let first = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
        assert_eq!(first.status, "success", "{}", first.error);
        assert_eq!(first.backup_kind, "full");
        let tag = ds.file_tag();
        assert!(tag.starts_with("diskH"), "{tag}");
        let name = PathBuf::from(&first.image_path);
        assert_eq!(image_token(&name).unwrap(), tag.to_ascii_uppercase());
        assert_eq!(first.images.len(), 1);
        assert_eq!(first.images[0].source, tag);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p2)
            .unwrap()
            .write_all_at(&[0x33; 100], mib)
            .unwrap();
        let second = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
        assert_eq!(second.status, "success", "{}", second.error);
        assert_eq!(second.backup_kind, "incremental");
        assert_eq!(second.base_image_path, first.image_path);
        assert_eq!(second.increment_index, 1);

        // Named by number only: made for Windows, refused here.
        job.disk_source.as_mut().unwrap().device = None;
        let refused = perform_backup(&sink, &job, &cancel, None, &passwords, None).await;
        assert_eq!(refused.status, "error");
        assert!(
            refused
                .error
                .contains(&*tr!("lib.linux_unsupported_whole_disk")),
            "{}",
            refused.error
        );
        // Refused by the job itself: not a failure a retry could turn around.
        assert!(refused.refused);
        assert!(!second.refused && !first.refused);
    }
}
