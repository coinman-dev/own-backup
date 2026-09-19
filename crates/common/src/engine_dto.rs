//! Data-transfer types for the direct engine operations (restore / verify /
//! mount / whole-disk restore) that the GUI proxies to the service over gRPC.
//!
//! They live here so the producer (service) and consumer (GUI) share one
//! definition and the JSON contract can't drift. Field names match the frontend
//! `types.ts` shapes exactly (`EngineProgress`, `RestoreSummary`, …).

use serde::{Deserialize, Serialize};

/// Progress payload for restore / verify (Tauri events `restore-progress` /
/// `verify-progress`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineProgress {
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_done: u64,
    pub speed_mbps: f64,
}

/// Outcome of a completed volume restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSummaryDto {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration_secs: f64,
}

/// Outcome of a verify pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySummaryDto {
    pub valid: bool,
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration_secs: f64,
}

/// Where a mounted image appeared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountResultDto {
    pub mount_point: String,
}

/// Result of unlocking + probing an encrypted (LUKS) source volume. The probe
/// opens the raw disk, so it runs in the privileged service (LocalSystem/root),
/// never in the unelevated GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeUnlockDto {
    /// The password unlocked the container.
    pub ok: bool,
    /// Inner filesystem (`ext4`/`btrfs`/`ntfs`/…), empty if not identified.
    pub filesystem: String,
    /// Inner volume label, if any.
    pub label: String,
    /// Bytes in use inside the decrypted volume, 0 if unknown.
    pub used_bytes: u64,
    /// Where the filesystem inside says it was last mounted (ext only): `"/"`
    /// for a Linux system root, `"/home"` or `"/mnt/…"` for data. Empty when
    /// unknown. The wizard needs it to answer, the moment a volume opens,
    /// whether it belongs in a bootable copy.
    #[serde(default)]
    pub linux_last_mounted: String,
}

/// An active mounted image mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMountDto {
    pub image_path: String,
    pub physical_path: String,
    pub read_write: bool,
    /// Drive letters Windows assigned to the volumes inside (e.g. `["J:"]`).
    /// Empty when none are assigned yet or on non-Windows hosts.
    #[serde(default)]
    pub volume_letters: Vec<String>,
}

/// Progress payload for a whole-disk restore (Tauri event `restore-disk-progress`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRestoreProgressDto {
    pub phase: String,
    pub partition_index: u64,
    pub partition_count: u64,
    pub part_label: String,
    pub percent: u64,
    pub bytes_written: u64,
}

/// Outcome of a completed whole-disk restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRestoreSummaryDto {
    pub partitions: u64,
    pub bytes_written: u64,
    pub duration_secs: f64,
}

// --- boot medium (stage 5) -------------------------------------------------

/// A base image the medium can be built on, as offered to the user.
///
/// Everything here was read out of the file itself before it was offered — see
/// `backup::winre_find`. `warnings` are reasons to hesitate, not refusals: a
/// base from another Windows branch usually works, and the choice is the user's.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BaseImageDto {
    pub path: String,
    /// Where it was found, in words ("зарегистрирован в среде восстановления").
    pub origin: String,
    /// Name inside the image ("Microsoft Windows Recovery Environment (x64)").
    pub name: String,
    pub build: u32,
    pub language: String,
    pub architecture: String,
    /// Uncompressed size of the image's contents — what the build will unpack.
    pub total_bytes: u64,
    pub warnings: Vec<String>,
}

/// One driver package on this machine, as offered for selection.
///
/// The size is the point: exporting every package is what made the first medium
/// 2.2 GB, and a display driver alone can be several hundred megabytes that a
/// rescue medium will never use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DriverPackageDto {
    /// Published name (`oem12.inf`) — what the build exports by.
    pub inf: String,
    /// The vendor's own name for it, which is the one that means something.
    pub original_name: String,
    pub class_name: String,
    pub provider: String,
    pub version: String,
    pub size_bytes: u64,
    /// Bound to a device that is present, of a class a rescue medium needs, on
    /// a bus that class can really live on. Ticked by default; the rest is
    /// listed only so the user can see what was left out.
    pub recommended: bool,
    /// What the device is, in words: "контроллер дисков", "сетевой адаптер".
    #[serde(default)]
    pub role: String,
    /// The device's own name — "Intel(R) Ethernet Connection I219-V" is what
    /// the user recognises; "oem34.inf" is not.
    #[serde(default)]
    pub device: String,
    /// Not a package: a present disk or network device on a driver that ships
    /// with Windows. Nothing to choose — shown so the wizard can say so, and
    /// checked against the base image during the build.
    #[serde(default)]
    pub inbox: bool,
}

/// What to build. Paths are the service's, not the interface's: the build runs
/// on the machine the service is on.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaBuildOptionsDto {
    /// Chosen base image; empty means "use the first one found".
    #[serde(default)]
    pub base_wim: String,
    /// Where the finished ISO goes.
    pub output_iso: String,
    /// Published names of the driver packages to include. Empty means none.
    #[serde(default)]
    pub drivers: Vec<String>,
    /// An extra folder of `.inf` drivers.
    #[serde(default)]
    pub extra_drivers: String,
    /// Leave `cmd` as the medium's shell instead of our own desktop — the way
    /// back if the desktop does not come up on some machine.
    #[serde(default)]
    pub classic_console: bool,
    /// Ids of the third-party tools to copy in — see
    /// [`crate::media_tools::MEDIA_TOOLS`]. Empty means none, which is what a
    /// machine with neither installed always sends.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Compress the WebView2 runtime into `boot.wim` instead of leaving it
    /// beside it. Smaller file, more memory at boot — see the field of the same
    /// name on the builder's request for the arithmetic.
    #[serde(default)]
    pub runtime_in_image: bool,
}

/// What the build did.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaBuildReportDto {
    pub iso_path: String,
    pub iso_bytes: u64,
    pub base_name: String,
    pub base_build: u32,
    pub base_language: String,
    /// The image had to be unpacked and recaptured (WIM mounting unavailable).
    pub unpacked: bool,
    pub drivers_added: u32,
    #[serde(default)]
    pub tools_added: u32,
    #[serde(default)]
    pub tools_bytes: u64,
    pub runtime_files_trimmed: u32,
    /// Temporary files removed after the build — a few gigabytes when the image
    /// had to be unpacked.
    #[serde(default)]
    pub temp_bytes_freed: u64,
    pub warnings: Vec<String>,
}

/// A third-party program found on this machine that the medium could carry.
///
/// Size and file count are shown because that is what the choice is about: the
/// medium is loaded into RAM whole, and an installation carrying a dozen
/// plugins is not the size anyone expects.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaToolDto {
    /// Id from [`crate::media_tools::MEDIA_TOOLS`].
    pub id: String,
    pub name: String,
    /// Where it was found, shown so the user can see WHICH installation it is.
    pub source: String,
    pub size_bytes: u64,
    pub files: u32,
}

/// One stage of a build, as the wizard lists them before anything starts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaStageDto {
    pub id: String,
    pub title: String,
}

/// Progress of a build, streamed as an engine event of kind `media`.
///
/// The full stage list travels with every event. It is ten short strings, and
/// sending it repeatedly costs nothing next to never having to keep the
/// interface's copy in step with the service's.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaProgressDto {
    pub stages: Vec<MediaStageDto>,
    /// Which stage is running (index into `stages`).
    pub stage_index: usize,
    /// 0–100 within that stage. The two DISM steps cannot measure themselves
    /// and report 0 on entry and 100 on exit; the wizard shows those as
    /// indeterminate rather than as a bar that sits still for minutes.
    pub percent: u64,
    pub detail: String,
    pub finished: bool,
}

/// Report of the temporary files removed after a build.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaCleanupDto {
    pub bytes_freed: u64,
}
