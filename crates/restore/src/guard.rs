//! What the running system will not let you overwrite.
//!
//! The first version of this refused the whole disk Windows boots from, which
//! is too blunt: a spare partition on that disk is a perfectly ordinary restore
//! target — it is how anyone puts Linux next to an existing Windows. What
//! actually cannot be touched is a much smaller set: the volume Windows is
//! running from, and the ESP it booted through.
//!
//! Three layers, and they answer different questions:
//!
//! - [`check_whole_disk_restore`] — may the **whole** disk be recreated? Only
//!   from a recovery environment, since that rewrites everything including the
//!   running system.
//! - [`protection`] — which partitions on this disk is the running system
//!   standing on, and is that answer trustworthy? Everything else on the disk
//!   is fair game, provided the partition table is applied through the
//!   partition manager rather than written behind its back (see
//!   `disk_session_win`).
//! - [`guard_target_volume`] — may this raw volume be written over? The direct
//!   `RestoreVolume` path has no plan and no table, so nothing above it applies.
//!
//! # Not knowing is not permission (audit AUD-01)
//!
//! Every one of these used to treat a failed detection as a yes.
//! `check_whole_disk_restore` allowed the restore when the system disk could
//! not be determined; `protected_partitions` returned an empty list — which
//! reads as "nothing to protect here" — when the disk layout could not be read.
//! For an operation with no undo that is the wrong default in every case: the
//! cost of a false refusal is a message, and the cost of a false permission is
//! the machine.
//!
//! So uncertainty now says so, in words, and the caller decides what to do with
//! it — which in practice means refusing and offering the boot medium, where
//! none of these questions arise because nothing is running.

use own_backup_common::{tr, OwnBackupError, Result};

/// What is protected on one disk, and how sure we are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protection {
    /// Partition indices the running system is standing on.
    pub parts: Vec<u32>,
    /// Set when the answer above cannot be trusted, with the reason in words.
    /// An empty `parts` with this set means "nothing found AND nothing could be
    /// checked" — which is not the same as "nothing to protect".
    pub cannot_tell: Option<String>,
}

impl Protection {
    /// Nothing is running here — the boot medium's answer, and the only one
    /// that is both empty and certain.
    pub fn nothing_is_running() -> Self {
        Self::default()
    }

    fn unsure(why: impl Into<String>) -> Self {
        Self {
            parts: Vec::new(),
            cannot_tell: Some(why.into()),
        }
    }
}

/// Decide whether wiping and restoring onto `target_disk` is safe.
///
/// - `system_disk`: the physical drive that hosts the running Windows, if known.
/// - `is_boot_media`: true when running from a recovery environment (WinPE),
///   where no disk hosts a live OS being written to.
///
/// On boot media everything is allowed: nothing is running off any of it.
/// Otherwise the target must be *known* not to be the system disk — an unknown
/// system disk is refused rather than waved through (audit AUD-01).
pub fn check_whole_disk_restore(
    target_disk: u32,
    system_disk: Option<u32>,
    is_boot_media: bool,
) -> Result<()> {
    if is_boot_media {
        return Ok(());
    }
    match system_disk {
        Some(sys) if sys == target_disk => Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.live_system_disk").into_owned(),
        )),
        Some(_) => Ok(()),
        // The check could not be made. Before this, that meant "go ahead" —
        // and the one case where detection fails is an unusual disk setup,
        // which is exactly where being wrong costs the most.
        None => Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.system_disk_unknown").into_owned(),
        )),
    }
}

/// A whole-disk writer must also account for the boot partition that may live
/// on a different physical disk than the Windows volume.
pub fn check_whole_disk_protection(
    target_disk: u32,
    system_disk: Option<u32>,
    is_boot_media: bool,
    protected: &Protection,
) -> Result<()> {
    check_whole_disk_restore(target_disk, system_disk, is_boot_media)?;
    if is_boot_media {
        return Ok(());
    }
    if let Some(why) = &protected.cannot_tell {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.protection_unknown", reason = why).into_owned(),
        ));
    }
    if !protected.parts.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.disk_has_live_boot_partition", disk = target_disk).into_owned(),
        ));
    }
    Ok(())
}

/// The physical drive number that hosts the running Windows,
/// or `None` if it can't be determined.
///
/// Asked of the volume itself rather than of PowerShell: the system drive's
/// volume knows which disk it sits on (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`),
/// and this has to work on boot media, where PowerShell does not exist.
#[cfg(target_os = "windows")]
pub fn system_disk_number() -> Option<u32> {
    use own_backup_backup::volume_win::{volume_root_for_path, volume_start_on_disk};

    let root = volume_root_for_path(&windows_directory()?)?;
    // A spanned volume answers with no single disk — and a system on one is not
    // something this guard can reason about anyway.
    volume_start_on_disk(&root).map(|(disk, _)| disk)
}

/// True when running under a Windows recovery environment.
///
/// This is an OS fact, not the application's `--media`/OWN_BACKUP_MEDIA setting.
/// A UI/storage override must never authorize writing over installed Windows.
/// MiniNT identifies PE; a kernel-reported X: Windows directory on the RAM-disk
/// device also covers the short interval in which PE's launcher hides MiniNT
/// to start EventLog. Caller-controlled SystemDrive/SystemRoot are not evidence.
#[cfg(target_os = "windows")]
pub fn is_boot_media() -> bool {
    if own_backup_common::media_mode::is_windows_pe() {
        return true;
    }
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
    let Some(dir) = windows_directory() else {
        return false;
    };
    if !dir.get(..2).is_some_and(|d| d.eq_ignore_ascii_case("X:")) {
        return false;
    }
    let drive: Vec<u16> = "X:".encode_utf16().chain(Some(0)).collect();
    let mut device = [0u16; 1024];
    // SAFETY: terminated drive name and a valid writable API buffer.
    let count = unsafe { QueryDosDeviceW(PCWSTR(drive.as_ptr()), Some(&mut device)) } as usize;
    if count == 0 || count > device.len() {
        return false;
    }
    let end = device[..count]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(count);
    String::from_utf16_lossy(&device[..end])
        .to_ascii_lowercase()
        .starts_with(r"\device\ramdisk")
}

/// Kernel-reported installation directory, independent of the process's env.
#[cfg(target_os = "windows")]
fn windows_directory() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
    let mut buffer = vec![0u16; 32768];
    // SAFETY: buffer is sized for the documented maximum Windows path.
    let count = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) } as usize;
    if count == 0 || count >= buffer.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..count]))
}

/// Guard a whole-disk restore using live Windows detection.
#[cfg(target_os = "windows")]
pub fn guard_target_disk(target_disk: u32) -> Result<()> {
    let media = is_boot_media();
    let system_disk = system_disk_number();
    let protected = if media {
        Protection::nothing_is_running()
    } else {
        protection(target_disk)
    };
    check_whole_disk_protection(target_disk, system_disk, media, &protected)
}

/// May this raw volume be written over?
///
/// `RestoreVolume` writes sectors straight onto a device path with no plan, no
/// table and none of the checks above — it had no guard at all, so the running
/// system's own volume was a perfectly acceptable target for it (audit AUD-01).
#[cfg(target_os = "windows")]
pub fn guard_target_volume(target: &str) -> Result<()> {
    use own_backup_backup::volume_win::{volume_extents, volume_root_for_path};

    if is_boot_media() {
        return Ok(()); // nothing here is running
    }

    // Where the running system stands, by byte offset on a disk — a drive
    // letter can be absent or reassigned, an extent cannot.
    let mut live: Vec<(u32, u64)> = Vec::new();
    if let Some(path) = windows_directory() {
        if let Some(root) = volume_root_for_path(&path) {
            for e in volume_extents(&root) {
                live.push((e.disk_number, e.offset));
            }
        }
    }
    if live.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.live_system_location_unknown").into_owned(),
        ));
    }

    let Some(root) =
        volume_root_for_path(target).or_else(|| volume_root_for_path(&format!("{target}\\")))
    else {
        // An unrecognised target is not obviously safe: it may be a raw device
        // path pointing at the very volume above.
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.target_unrecognised", target = target).into_owned(),
        ));
    };
    let target_extents = volume_extents(&root);
    if target_extents.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.target_location_unknown", target = target).into_owned(),
        ));
    }
    for e in &target_extents {
        if live.contains(&(e.disk_number, e.offset)) {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!("guard.target_is_live_system").into_owned(),
            ));
        }
        let protected = protection(e.disk_number);
        if let Some(why) = protected.cannot_tell {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!("guard.protection_unknown", reason = why).into_owned(),
            ));
        }
        if !protected.parts.is_empty() {
            let layout = own_backup_backup::gpt_reader::read_disk_layout(e.disk_number)?;
            let sector = layout.sector_size.max(512) as u64;
            let overlaps = layout.partitions.iter().any(|p| {
                let start = p.first_lba.saturating_mul(sector);
                let end = p.last_lba.saturating_add(1).saturating_mul(sector);
                protected.parts.contains(&p.index)
                    && e.offset < end
                    && e.offset.saturating_add(e.length) > start
            });
            if overlaps {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("guard.target_is_live_boot_partition").into_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// May this device be written over (Linux)?
///
/// Only when nothing at all sits on it: no filesystem mounted from it or from
/// anything stacked on it (read-only included — the kernel caches what it
/// read), no swap, no opened LUKS/LVM/md on top. And never the EFI System
/// Partition the machine booted through: systemd may mount that one only on
/// access, so it can look idle while it is what the next boot needs. The
/// restore itself then opens the device with `O_EXCL`, which keeps it that way
/// for as long as it writes.
#[cfg(target_os = "linux")]
pub fn guard_target_volume(target: &str) -> Result<()> {
    guard_linux_volume(target, BootEspRule::Identify)
}

/// [`guard_target_volume`] for a partition a restore plan writes, formats or
/// grows on the disk it holds, after the plan has started.
///
/// What runs on it is checked as always. The boot-ESP rule is not asked
/// again: the plan checked every partition it touches against
/// [`protection_of_device`] before writing anything, and the ESP this machine
/// booted through was not among them (a plan that rewrites the table of a
/// disk with anything protected on it does not start at all). Asked again
/// after the restore it would be wrong: a whole-disk copy of this machine's
/// own system disk carries the running ESP's PARTUUID (PARTUUIDs are never
/// changed), so by PARTUUID alone the copy IS "the ESP this computer booted
/// from" — and every later step on it (identity change, format, grow) would
/// be refused. The real boot ESP stays protected: it is not on this plan's
/// list, so it gets [`guard_target_volume`].
#[cfg(target_os = "linux")]
pub fn guard_plan_volume(target: &str) -> Result<()> {
    guard_linux_volume(target, BootEspRule::SettledByPlan)
}

/// Whether [`guard_linux_volume`] identifies the boot ESP itself.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootEspRule {
    /// Refuse the boot ESP (or any possible one when it is unknown).
    Identify,
    /// A restore plan settled it before writing — see [`guard_plan_volume`].
    SettledByPlan,
}

#[cfg(target_os = "linux")]
fn guard_linux_volume(target: &str, rule: BootEspRule) -> Result<()> {
    use own_backup_common::linux_block as lb;
    use std::path::Path;

    // Not skipped on our boot medium: unlike WinPE, which runs from a RAM
    // disk, Debian live reads its system from the stick it booted from (and
    // logs onto it), so what is mounted is still what must not be overwritten.
    let path = Path::new(target);
    let usage = lb::device_usage(path).map_err(|e| {
        OwnBackupError::RestoreIncompatible(
            tr!("guard.linux_cannot_tell", target = target, reason = e).into_owned(),
        )
    })?;
    let Some(usage) = usage else {
        // Not a block device — an image file: nothing runs from it.
        return Ok(());
    };
    if !usage.idle() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "guard.linux_target_in_use",
                target = target,
                users = linux_usage_text(&usage)
            )
            .into_owned(),
        ));
    }
    let sys = Path::new(lb::SYS_CLASS_BLOCK);
    let Some(name) = lb::kernel_name(path) else {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.target_unrecognised", target = target).into_owned(),
        ));
    };
    if rule == BootEspRule::SettledByPlan {
        return Ok(());
    }
    let stack = lb::stack_above(sys, &name);
    match boot_esp_refusal(
        sys,
        Path::new(lb::UDEV_DATA),
        &stack,
        &linux_boot_esp(),
        is_boot_media(),
    ) {
        Some(message) => Err(OwnBackupError::RestoreIncompatible(message)),
        None => Ok(()),
    }
}

/// Why writing over `stack` (a device and what is stacked on it) would hit the
/// ESP the machine booted through, if it would.
#[cfg(target_os = "linux")]
fn boot_esp_refusal(
    sys: &std::path::Path,
    udev: &std::path::Path,
    stack: &[String],
    esp: &LinuxBootEsp,
    boot_media: bool,
) -> Option<String> {
    use own_backup_common::linux_block as lb;
    match esp {
        LinuxBootEsp::NotUefi => None,
        LinuxBootEsp::PartUuid(boot) => stack
            .iter()
            .any(|n| lb::partuuid(sys, n).as_deref() == Some(boot.as_str()))
            .then(|| tr!("guard.linux_target_is_boot_esp").into_owned()),
        // The medium booted from its own ESP (inside the ISO, or a CD with
        // none); the disks here are the machines being rescued, whose ESPs
        // are exactly what a restore writes.
        LinuxBootEsp::Unknown if boot_media => None,
        // Booted through one of these disks, partition not said: any ESP on
        // them — or a partition whose type udev does not know — might be it.
        LinuxBootEsp::Disks(disks) => stack
            .iter()
            .filter(|n| lb::is_partition(sys, n))
            .filter(|n| lb::parent_disk(sys, n).is_some_and(|d| disks.contains(&d)))
            .any(|n| {
                lb::dev_numbers(sys, n)
                    .and_then(|(ma, mi)| lb::partition_type(udev, ma, mi))
                    .is_none_or(|t| t == lb::ESP_TYPE_GUID)
            })
            .then(|| tr!("guard.linux_boot_esp_unknown").into_owned()),
        // Which ESP started the machine is not known: any ESP — or any
        // partition whose type udev does not know — might be it.
        LinuxBootEsp::Unknown => stack
            .iter()
            .filter(|n| lb::is_partition(sys, n))
            .any(|n| {
                lb::dev_numbers(sys, n)
                    .and_then(|(ma, mi)| lb::partition_type(udev, ma, mi))
                    .is_none_or(|t| t == lb::ESP_TYPE_GUID)
            })
            .then(|| tr!("guard.linux_boot_esp_unknown").into_owned()),
    }
}

/// Mount points, swap and stacked devices, for a refusal.
#[cfg(target_os = "linux")]
fn linux_usage_text(usage: &own_backup_common::linux_block::Usage) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mounts: Vec<&str> = usage
        .mounted_rw
        .iter()
        .chain(&usage.mounted_ro)
        .map(String::as_str)
        .collect();
    if !mounts.is_empty() {
        parts.push(tr!("guard.linux_usage_mounted", mounts = mounts.join(", ")).into_owned());
    }
    if !usage.swap.is_empty() {
        parts.push(tr!("guard.linux_usage_swap").into_owned());
    }
    if !usage.holders.is_empty() {
        parts.push(
            tr!(
                "guard.linux_usage_holders",
                holders = usage.holders.join(", ")
            )
            .into_owned(),
        );
    }
    parts.join("; ")
}

#[cfg(target_os = "linux")]
enum LinuxBootEsp {
    /// A BIOS machine: there is no ESP to protect.
    NotUefi,
    /// The PARTUUID (lowercase) out of `BootCurrent`'s device path.
    PartUuid(String),
    /// `BootCurrent` names a device and no partition (the firmware's own
    /// entry for a disk booted through `\EFI\BOOT\BOOTX64.EFI`): the disks
    /// behind that device, by kernel name. Their ESPs are the candidates.
    Disks(Vec<String>),
    /// UEFI, but the firmware variables could not be read or understood.
    Unknown,
}

/// `BootCurrent` → `Boot####` → the GPT partition GUID of its Hard Drive node,
/// read from efivarfs (world-readable: four bytes of attributes, then data).
#[cfg(target_os = "linux")]
fn linux_boot_esp() -> LinuxBootEsp {
    const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
    let vars = std::path::Path::new("/sys/firmware/efi/efivars");
    if !std::path::Path::new("/sys/firmware/efi").exists() {
        return LinuxBootEsp::NotUefi;
    }
    let read = |name: &str| -> Option<Vec<u8>> {
        let raw = std::fs::read(vars.join(format!("{name}-{EFI_GLOBAL}"))).ok()?;
        (raw.len() > 4).then(|| raw[4..].to_vec())
    };
    let Some(current) = read("BootCurrent").filter(|b| b.len() >= 2) else {
        return LinuxBootEsp::Unknown;
    };
    let number = u16::from_le_bytes([current[0], current[1]]);
    let Some(entry) = read(&own_backup_backup::efivars::entry_name(number)) else {
        return LinuxBootEsp::Unknown;
    };
    let Ok(option) = own_backup_backup::efivars::LoadOption::parse(&entry) else {
        return LinuxBootEsp::Unknown;
    };
    if let Some(guid) = option.hard_drive().and_then(|hd| hd.partition_guid()) {
        return LinuxBootEsp::PartUuid(guid.to_ascii_lowercase());
    }
    // No partition in the path: which disk it was, from the PCI device the
    // entry names (24.10: «UEFI Misc Device», and every ESP was protected).
    own_backup_backup::efivars_linux::disks_behind_boot_device(
        &option.device_path(),
        std::path::Path::new("/sys/devices"),
        // Disks only: `/sys/class/block` lists their partitions too.
        std::path::Path::new("/sys/block"),
    )
    .map(LinuxBootEsp::Disks)
    .unwrap_or(LinuxBootEsp::Unknown)
}

/// Partition numbers of the whole disk at `device` that the running Linux
/// stands on, and whether that answer can be trusted — the device-named
/// counterpart of [`protection`] (Linux disks have no stable number).
///
/// Protected: every partition that is mounted, used as swap, or held by a
/// device-mapper/md device (an opened LUKS container, an LVM physical volume,
/// a RAID member), and the ESP the machine booted through (`BootCurrent`).
/// When which ESP that was cannot be told, every partition that is or may be
/// an ESP is protected instead. A disk whose own device is in use (a
/// filesystem or a PV straight on it) protects all of it.
#[cfg(target_os = "linux")]
pub fn protection_of_device(device: &str) -> Protection {
    use own_backup_common::linux_block as lb;
    use std::path::Path;

    // The boot medium is not exempt: see `guard_target_volume`.
    let sys = Path::new(lb::SYS_CLASS_BLOCK);
    let Some(name) = lb::kernel_name(Path::new(device)) else {
        return Protection::unsure(tr!("guard.target_unrecognised", target = device).into_owned());
    };
    if lb::is_partition(sys, &name) {
        return Protection::unsure(tr!("guard.linux_not_a_disk", target = device).into_owned());
    }
    let mounts = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(t) => lb::parse_mountinfo(&t),
        Err(e) => {
            return Protection::unsure(
                tr!("guard.linux_cannot_tell", target = device, reason = e).into_owned(),
            )
        }
    };
    let swaps = lb::parse_swaps(&std::fs::read_to_string("/proc/swaps").unwrap_or_default());
    let usage = |n: &str| {
        let stack: Vec<(String, Option<(u32, u32)>)> = lb::stack_above(sys, n)
            .into_iter()
            .map(|x| {
                let d = lb::dev_numbers(sys, &x);
                (x, d)
            })
            .collect();
        lb::usage_of(n, &stack, &mounts, &swaps, |p| {
            lb::kernel_name(Path::new(p))
        })
    };
    let partitions = lb::partitions_of(sys, &name);
    let number = |p: &str| -> Option<u32> {
        std::fs::read_to_string(sys.join(p).join("partition"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };

    // The disk's own device: anything on it other than its partitions.
    let own = usage(&name);
    let own_users = Usage {
        holders: own
            .holders
            .iter()
            .filter(|h| !partitions.contains(h))
            .filter(|h| {
                // A holder of a partition is that partition's business.
                !partitions
                    .iter()
                    .any(|p| lb::stack_above(sys, p).contains(h))
            })
            .cloned()
            .collect(),
        ..Default::default()
    };
    let direct_mounts = mounts.iter().any(|m| {
        lb::dev_numbers(sys, &name) == Some((m.major, m.minor))
            || (m.source.starts_with('/')
                && lb::kernel_name(Path::new(&m.source)).as_deref() == Some(name.as_str()))
    });
    if direct_mounts || !own_users.holders.is_empty() {
        let all: Vec<u32> = partitions.iter().filter_map(|p| number(p)).collect();
        if all.is_empty() {
            return Protection::unsure(
                tr!(
                    "guard.linux_target_in_use",
                    target = device,
                    users = linux_usage_text(&own)
                )
                .into_owned(),
            );
        }
        return Protection {
            parts: all,
            cannot_tell: None,
        };
    }

    let esp = linux_boot_esp();
    let udev = Path::new(lb::UDEV_DATA);
    let mut parts = Vec::new();
    for p in &partitions {
        let Some(n) = number(p) else {
            return Protection::unsure(
                tr!("guard.linux_cannot_tell", target = device, reason = p).into_owned(),
            );
        };
        let busy = !usage(p).idle();
        let boot_esp = match &esp {
            LinuxBootEsp::NotUefi => false,
            LinuxBootEsp::PartUuid(boot) => lb::partuuid(sys, p).as_deref() == Some(boot.as_str()),
            // Booted through one of these disks: their ESPs (or partitions
            // whose type cannot be read) are protected, other disks' are not.
            LinuxBootEsp::Disks(disks) if !disks.contains(&name) => false,
            LinuxBootEsp::Disks(_) => lb::dev_numbers(sys, p)
                .and_then(|(ma, mi)| lb::partition_type(udev, ma, mi))
                .is_none_or(|t| t == lb::ESP_TYPE_GUID),
            LinuxBootEsp::Unknown if is_boot_media() => false,
            LinuxBootEsp::Unknown => lb::dev_numbers(sys, p)
                .and_then(|(ma, mi)| lb::partition_type(udev, ma, mi))
                .is_none_or(|t| t == lb::ESP_TYPE_GUID),
        };
        if busy || boot_esp {
            parts.push(n);
        }
    }
    parts.sort_unstable();
    Protection {
        parts,
        cannot_tell: None,
    }
}

#[cfg(target_os = "linux")]
use own_backup_common::linux_block::Usage;

/// May the whole disk at `device` be recreated? Only when the running system
/// stands on none of it, or from the boot medium.
#[cfg(target_os = "linux")]
pub fn guard_target_disk_device(device: &str) -> Result<()> {
    let protected = protection_of_device(device);
    if let Some(why) = &protected.cannot_tell {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("guard.protection_unknown", reason = why).into_owned(),
        ));
    }
    if !protected.parts.is_empty() {
        let parts = protected
            .parts
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "guard.linux_disk_has_live_partitions",
                disk = device,
                parts = parts
            )
            .into_owned(),
        ));
    }
    Ok(())
}

/// Partition indices on `disk` that the running Windows is standing on, and
/// whether that answer can be trusted.
///
/// Two things qualify:
/// - the volume `%SystemRoot%` lives on — overwrite it and the machine stops
///   mid-write;
/// - **the EFI System Partition the machine actually booted through**, which is
///   how it starts at all. That one is not necessarily on the same disk as
///   Windows, and the old version only ever looked at the system disk, so an
///   ESP on a different disk was unprotected (audit AUD-01). It is identified
///   by the partition GUID in the firmware's own `BootCurrent` entry; when the
///   firmware cannot be asked, every ESP on the disk is protected instead,
///   which errs the safe way.
///
/// A data volume with open files is deliberately *not* on this list: it will
/// refuse to dismount when the restore reaches it, which is a clearer and more
/// specific failure than a rule guessing in advance.
#[cfg(target_os = "windows")]
pub fn protection(disk: u32) -> Protection {
    if is_boot_media() {
        return Protection::nothing_is_running();
    }
    if system_disk_number().is_none() {
        return Protection::unsure(tr!("guard.live_system_location_unknown").into_owned());
    }
    // Without the layout nothing below can be answered — and "no partitions
    // protected" would read as a clean bill of health.
    let layout = match own_backup_backup::gpt_reader::read_disk_layout(disk) {
        Ok(l) => l,
        Err(e) => {
            return Protection::unsure(
                tr!("guard.layout_unreadable", disk = disk, error = e).into_owned(),
            )
        }
    };
    let sector = layout.sector_size.max(512) as u64;
    let mut out = Vec::new();

    // The volume the running Windows lives on, located by byte offset rather
    // than by drive letter.
    match system_volume_offset(disk) {
        Some(offset) => {
            if let Some(p) = layout
                .partitions
                .iter()
                .find(|p| p.first_lba * sector == offset)
            {
                out.push(p.index);
            }
        }
        None if system_disk_number() == Some(disk) => {
            // Windows says it lives on this disk and we cannot find where.
            return Protection::unsure(
                tr!("guard.system_partition_not_found", disk = disk).into_owned(),
            );
        }
        None => {} // the running system is not on this disk at all
    }

    // The ESP the firmware actually booted through.
    match boot_esp_guid() {
        BootEsp::NotUefi => {}
        BootEsp::Guid(guid) => {
            if let Some(p) = layout
                .partitions
                .iter()
                .find(|p| p.part_guid.eq_ignore_ascii_case(&guid))
            {
                if !out.contains(&p.index) {
                    out.push(p.index);
                }
            }
        }
        BootEsp::Unknown => {
            // Cannot ask the firmware which one it was, so none of them may go.
            for p in &layout.partitions {
                if p.type_guid
                    .eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM)
                    && !out.contains(&p.index)
                {
                    out.push(p.index);
                }
            }
        }
    }

    out.sort_unstable();
    Protection {
        parts: out,
        cannot_tell: None,
    }
}

/// The partitions to protect, for callers that only want the list. Prefer
/// [`protection`], which also says when the list cannot be trusted.
#[cfg(target_os = "windows")]
pub fn protected_partitions(disk: u32) -> Vec<u32> {
    protection(disk).parts
}

/// Which ESP the firmware booted through.
#[cfg(target_os = "windows")]
enum BootEsp {
    /// A BIOS machine: there is no ESP to protect.
    NotUefi,
    /// The partition GUID out of `BootCurrent`'s device path.
    Guid(String),
    /// UEFI, but the firmware could not be asked.
    Unknown,
}

/// Read `BootCurrent`, find its `Boot####` entry, and pull the GPT partition
/// GUID out of the Hard Drive node of its device path.
#[cfg(target_os = "windows")]
fn boot_esp_guid() -> BootEsp {
    use own_backup_backup::efivars_win as efi;

    if !efi::is_uefi() {
        return BootEsp::NotUefi;
    }
    let Ok(config) = efi::read_config() else {
        return BootEsp::Unknown;
    };
    let Some(current) = config.current else {
        return BootEsp::Unknown;
    };
    let Ok(Some(entry)) = efi::read_entry(current) else {
        return BootEsp::Unknown;
    };
    match hard_drive_guid(&entry.file_path_list) {
        Some(g) => BootEsp::Guid(g),
        None => BootEsp::Unknown,
    }
}

/// Walk an EFI device path and return the GPT partition GUID of its Hard Drive
/// node, if it has one.
///
/// A device path is a chain of nodes: `type (1) | subtype (1) | length (2, LE)`
/// then the payload. The one wanted is Media (4) / Hard Drive (1), whose
/// payload is `partition number (4) | start (8) | size (8) | signature (16) |
/// MBR type (1) | signature type (1)`; signature type 2 means the signature is
/// a GPT partition GUID, stored the same mixed-endian way GPT stores it.
#[cfg(target_os = "windows")]
fn hard_drive_guid(path: &[u8]) -> Option<String> {
    const END_OF_PATH: u8 = 0x7F;
    const MEDIA: u8 = 0x04;
    const HARD_DRIVE: u8 = 0x01;
    const SIGNATURE_IS_GUID: u8 = 0x02;

    let mut at = 0usize;
    while at + 4 <= path.len() {
        let node_type = path[at];
        let sub_type = path[at + 1];
        let len = u16::from_le_bytes([path[at + 2], path[at + 3]]) as usize;
        // A zero or oversized length would loop forever or read past the end.
        if len < 4 || at + len > path.len() {
            return None;
        }
        if node_type == END_OF_PATH {
            return None;
        }
        if node_type == MEDIA && sub_type == HARD_DRIVE && len >= 42 {
            let body = &path[at + 4..at + len];
            let signature_type = body[37];
            if signature_type == SIGNATURE_IS_GUID {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&body[20..36]);
                return Some(uuid::Uuid::from_bytes_le(bytes).to_string().to_uppercase());
            }
            return None; // an MBR signature: not a GUID we can match
        }
        at += len;
    }
    None
}

/// Byte offset on `disk` of the volume holding `%SystemRoot%`.
#[cfg(target_os = "windows")]
fn system_volume_offset(disk: u32) -> Option<u64> {
    let root = own_backup_backup::volume_win::volume_root_for_path(&windows_directory()?)?;
    own_backup_backup::volume_win::volume_extents(&root)
        .into_iter()
        .find(|e| e.disk_number == disk)
        .map(|e| e.offset)
}

/// Not knowing is not permission: Linux disks have no stable number yet that
/// the plan's disk could be matched against, so nothing is certain here —
/// which makes every plan check refuse rather than wave a disk through. The
/// boot medium too: it runs from a stick that a number could name.
#[cfg(not(target_os = "windows"))]
pub fn protection(_disk: u32) -> Protection {
    Protection::unsure(tr!("guard.linux_disk_numbers").into_owned())
}

#[cfg(not(target_os = "windows"))]
pub fn protected_partitions(_disk: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(target_os = "windows"))]
pub fn system_disk_number() -> Option<u32> {
    None
}

/// Running from our own Linux boot medium: its kernel command line carries
/// `own-backup.media` (`.ai/PLAN.md` §6, stage 6). Anywhere else, the system
/// we run on is a live one. Not `--media` or the environment: those are a
/// process's own say, and must not lift a protection on an installed system.
#[cfg(target_os = "linux")]
pub fn is_boot_media() -> bool {
    own_backup_common::media_mode::is_linux_medium()
}
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn is_boot_media() -> bool {
    false
}

/// Whole-disk restore by number: refused, the boot medium included, until
/// Linux disks are addressed by a stable identity (see [`protection`]); the
/// device-named road is [`guard_target_disk_device`].
#[cfg(not(target_os = "windows"))]
pub fn guard_target_disk(target_disk: u32) -> Result<()> {
    let protected = protection(target_disk);
    check_whole_disk_protection(target_disk, system_disk_number(), false, &protected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_live_system_disk_restore() {
        // Target IS the running system disk, not on boot media → denied.
        assert!(check_whole_disk_restore(0, Some(0), false).is_err());
    }

    #[test]
    fn a_separate_live_boot_partition_blocks_whole_disk_restore() {
        let boot = Protection {
            parts: vec![1],
            cannot_tell: None,
        };
        assert!(check_whole_disk_protection(1, Some(0), false, &boot).is_err());
        assert!(check_whole_disk_protection(1, Some(0), true, &boot).is_ok());
        let unknown = Protection {
            parts: vec![],
            cannot_tell: Some("layout unavailable".into()),
        };
        assert!(check_whole_disk_protection(1, Some(0), false, &unknown).is_err());
    }

    #[test]
    fn allows_a_different_disk() {
        assert!(check_whole_disk_restore(1, Some(0), false).is_ok());
    }

    #[test]
    fn allows_system_disk_from_boot_media() {
        // In WinPE the OS is not running from the target — restoring it is fine.
        assert!(check_whole_disk_restore(0, Some(0), true).is_ok());
    }

    /// The case that inverted the default. Detection failing used to mean
    /// "go ahead", and the one situation where it fails is an unusual disk
    /// setup — exactly where being wrong costs the machine (audit AUD-01).
    #[test]
    fn an_undetectable_system_disk_is_refused_not_waved_through() {
        let refused = check_whole_disk_restore(0, None, false)
            .expect_err("not knowing must not mean permission");
        assert!(
            refused.to_string().contains("носителя восстановления"),
            "the message has to say what to do instead: {refused}"
        );
    }

    /// …but on the boot medium there is nothing to determine, so the same
    /// unknown is simply irrelevant.
    #[test]
    fn on_boot_media_an_unknown_system_disk_does_not_matter() {
        assert!(check_whole_disk_restore(0, None, true).is_ok());
    }

    /// "No partitions protected" and "nothing could be checked" are different
    /// answers and must not be spelled the same way.
    #[test]
    fn an_empty_list_and_an_unanswerable_question_are_different() {
        let clear = Protection::nothing_is_running();
        assert!(clear.parts.is_empty() && clear.cannot_tell.is_none());

        let unsure = Protection::unsure("диск не читается");
        assert!(unsure.parts.is_empty());
        assert!(
            unsure.cannot_tell.is_some(),
            "the caller must be able to tell these apart"
        );
    }

    // --- The firmware's own answer to "which ESP did I boot through?" -------

    /// Build an EFI device path: some nodes, then the Hard Drive node, then the
    /// end marker.
    #[cfg(target_os = "windows")]
    fn device_path_with_hard_drive(part_number: u32, guid: uuid::Uuid, sig_type: u8) -> Vec<u8> {
        let mut out = Vec::new();
        // A PCI node in front, so the walk has to actually walk.
        out.extend_from_slice(&[0x01, 0x01, 0x06, 0x00, 0x00, 0x1F]);
        let mut node = vec![0x04, 0x01, 42, 0x00];
        node.extend_from_slice(&part_number.to_le_bytes());
        node.extend_from_slice(&34u64.to_le_bytes()); // start LBA
        node.extend_from_slice(&204800u64.to_le_bytes()); // size
        node.extend_from_slice(&guid.to_bytes_le());
        node.push(0x02); // MBR type: GPT
        node.push(sig_type);
        assert_eq!(node.len(), 42);
        out.extend_from_slice(&node);
        out.extend_from_slice(&[0x7F, 0xFF, 0x04, 0x00]); // end of path
        out
    }

    /// A fake `/sys/class/block` with one partition `name` (`dev` numbers,
    /// PARTUUID) and a udev database giving it `part_type`.
    #[cfg(target_os = "linux")]
    fn fake_partition(
        dir: &std::path::Path,
        name: &str,
        partuuid: &str,
        part_type: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let sys = dir.join("sys");
        let udev = dir.join("udev");
        std::fs::create_dir_all(sys.join(name)).unwrap();
        std::fs::create_dir_all(&udev).unwrap();
        std::fs::write(sys.join(name).join("partition"), "1\n").unwrap();
        std::fs::write(sys.join(name).join("dev"), "8:17\n").unwrap();
        std::fs::write(
            sys.join(name).join("uevent"),
            format!("MAJOR=8\nMINOR=17\nDEVNAME={name}\nPARTUUID={partuuid}\n"),
        )
        .unwrap();
        if let Some(t) = part_type {
            std::fs::write(udev.join("b8:17"), format!("E:ID_PART_ENTRY_TYPE={t}\n")).unwrap();
        }
        (sys, udev)
    }

    /// The boot ESP is refused by its PARTUUID, and — firmware variables
    /// unreadable — any ESP is; a BIOS machine or the boot medium has none
    /// to protect.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_boot_esp_is_refused_by_partuuid_and_any_esp_when_unknown() {
        use own_backup_common::linux_block::ESP_TYPE_GUID;
        let dir = tempfile::tempdir().unwrap();
        let boot = "2d272048-0d61-4a8c-a715-08d35535ed93";
        let (sys, udev) = fake_partition(dir.path(), "sdb1", boot, Some(ESP_TYPE_GUID));
        let stack = vec!["sdb1".to_string()];
        let refused = |esp: &LinuxBootEsp, media: bool| {
            boot_esp_refusal(&sys, &udev, &stack, esp, media).is_some()
        };
        assert!(refused(&LinuxBootEsp::PartUuid(boot.into()), false));
        assert!(!refused(
            &LinuxBootEsp::PartUuid("245ef510-a193-458d-b01f-d13738760455".into()),
            false
        ));
        assert!(refused(&LinuxBootEsp::Unknown, false));
        assert!(!refused(&LinuxBootEsp::Unknown, true));
        assert!(!refused(&LinuxBootEsp::NotUefi, false));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_boot_entry_names_the_partition_it_booted_from() {
        let guid = uuid::Uuid::new_v4();
        let path = device_path_with_hard_drive(1, guid, 0x02);
        assert_eq!(
            hard_drive_guid(&path),
            Some(guid.to_string().to_uppercase()),
            "the GUID must come back exactly as GPT stores it"
        );
    }

    /// An MBR signature is four bytes, not a GUID: there is nothing to compare
    /// against a GPT partition, and guessing would protect the wrong one.
    #[cfg(target_os = "windows")]
    #[test]
    fn an_mbr_signature_is_not_mistaken_for_a_guid() {
        let path = device_path_with_hard_drive(1, uuid::Uuid::new_v4(), 0x01);
        assert_eq!(hard_drive_guid(&path), None);
    }

    /// A device path is attacker-shaped data from the firmware's point of view
    /// — a zero length would loop forever, an oversized one would read past the
    /// buffer.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_malformed_device_path_ends_the_walk_instead_of_running_away_with_it() {
        assert_eq!(
            hard_drive_guid(&[0x04, 0x01, 0x00, 0x00]),
            None,
            "zero length"
        );
        assert_eq!(
            hard_drive_guid(&[0x04, 0x01, 0xFF, 0xFF]),
            None,
            "past the end"
        );
        assert_eq!(hard_drive_guid(&[]), None);
        assert_eq!(
            hard_drive_guid(&[0x7F, 0xFF, 0x04, 0x00]),
            None,
            "end marker only"
        );
    }
}
