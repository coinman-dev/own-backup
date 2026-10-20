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

/// Progress of «проверить версии» of a job — several images in one go (Tauri
/// event `job-verify-progress`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVerifyProgress {
    /// Which image is being read, counting from zero, and of how many.
    pub index: u32,
    pub count: u32,
    pub file_name: String,
    /// Of the whole operation, every image weighing the same.
    pub percent: u64,
    pub blocks_done: u64,
    pub blocks_total: u64,
}

/// What reading one image of a job found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVerifyImage {
    pub image_path: String,
    pub file_name: String,
    pub valid: bool,
    pub blocks_checked: u64,
    /// How many blocks do not match their checksum.
    pub mismatched_blocks: u64,
    /// Why the image could not be read at all; empty when it could.
    pub error: String,
}

/// Outcome of «проверить версии»: every image that was read, good or not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVerifySummary {
    pub valid: bool,
    pub images: Vec<JobVerifyImage>,
    pub duration_secs: f64,
}

/// Outcome of «сохранить как VHDX».
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSummaryDto {
    pub target_path: String,
    pub virtual_size: u64,
    /// How much of the disk holds data — what the file actually stores.
    pub bytes_stored: u64,
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
    /// `%APPDATA%` of the person building the medium.
    ///
    /// Filled in by the interface, because the build itself does not run as
    /// that person: the service is LocalSystem, whose `%APPDATA%` is the
    /// SYSTEM profile and holds nobody's settings. Empty means "use whatever
    /// this process has", which is right when the build runs in the user's own
    /// session.
    #[serde(default)]
    pub user_appdata: String,
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

// --- duplicate identities (Linux restore and mount) ------------------------

/// An identity the restored or mounted copy carries that a device of this
/// machine already has (`backup::identity_clash`): a filesystem, LUKS, LVM or
/// md UUID, an LVM volume group name, a GPT partition GUID.
///
/// Every field is structured: the window decides from `severity` and `kind`,
/// never from translated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityClashDto {
    /// `ext`, `xfs`, `btrfs`, `luks`, `lvm_pv`, `lvm_vg`, `lvm_vg_name`, `md`,
    /// `partuuid`.
    pub kind: String,
    /// The value as the subsystem's own tools print it.
    pub value: String,
    /// Which source partition of the copy carries it (`None` for a volume copy).
    #[serde(default)]
    pub part_index: Option<u32>,
    /// Kernel names of the devices that already carry it.
    pub already_on: Vec<String>,
    /// `ambiguous` (names may resolve to either), `mount` (the second mount is
    /// refused as is), `critical` (a volume manager or the kernel may merge or
    /// confuse the two).
    pub severity: String,
    /// The command that gives the restored copy a new identity, with `<device>`
    /// for the restored device; empty when none is offered (a PARTUUID).
    #[serde(default)]
    pub change_command: String,
    /// Mount option that lets the copy be mounted beside the original
    /// (`nouuid` for XFS), empty when none is needed.
    #[serde(default)]
    pub mount_hint: String,
}

/// What to do about duplicate identities, said explicitly by the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityChoice {
    /// Restore or mount the copy as it is; the duplicates stay.
    Keep,
    /// Give the restored copy new identities. Editing its fstab, crypttab
    /// and boot configuration to match is a separate choice
    /// (`fix_references` in the request). Restores only: a mounted copy is
    /// read, not changed.
    Change,
}

/// The person's decision about duplicate identities, as a restore request
/// carries it: the choice, and — with [`IdentityChoice::Change`] — whether the
/// restored system's fstab, crypttab and boot configuration are edited too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdentityDecision {
    pub choice: Option<IdentityChoice>,
    pub fix_references: bool,
}

impl From<Option<IdentityChoice>> for IdentityDecision {
    fn from(choice: Option<IdentityChoice>) -> Self {
        Self {
            choice,
            fix_references: false,
        }
    }
}

impl IdentityDecision {
    /// Change the duplicates; `fix_references` edits the restored system too.
    pub fn change(fix_references: bool) -> Self {
        Self {
            choice: Some(IdentityChoice::Change),
            fix_references,
        }
    }
}

/// One identity a restore changed on the restored copy (Linux,
/// [`IdentityChoice::Change`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityChangeDto {
    /// Same slugs as [`IdentityClashDto::kind`].
    pub kind: String,
    pub old: String,
    pub new: String,
    /// The device it was changed on (a partition, or a device the restore
    /// opened on it: `/dev/mapper/…`, `/dev/md…`).
    pub device: String,
    /// The partition number on the target, when it is one.
    #[serde(default)]
    pub part_index: Option<u32>,
}

/// Key of the JSON object carried in the details of the `FAILED_PRECONDITION`
/// status that asks for an [`IdentityChoice`]: `{"identity_clashes": [...]}`.
pub const IDENTITY_CLASHES_DETAIL: &str = "identity_clashes";
