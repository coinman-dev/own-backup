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

use own_backup_common::{OwnBackupError, Result};

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
        Self { parts: Vec::new(), cannot_tell: Some(why.into()) }
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
            "Нельзя восстанавливать диск, на котором работает Windows, «на живую». \
             Загрузитесь с носителя восстановления (WinPE) или выберите другой диск."
                .into(),
        )),
        Some(_) => Ok(()),
        // The check could not be made. Before this, that meant "go ahead" —
        // and the one case where detection fails is an unusual disk setup,
        // which is exactly where being wrong costs the most.
        None => Err(OwnBackupError::RestoreIncompatible(
            "Не удалось определить, на каком диске работает Windows, поэтому нельзя \
             убедиться, что выбран не он. Загрузитесь с носителя восстановления — \
             там этот вопрос не стоит."
                .into(),
        )),
    }
}

/// The physical drive number that hosts the running Windows (`%SystemDrive%`),
/// or `None` if it can't be determined.
///
/// Asked of the volume itself rather than of PowerShell: the system drive's
/// volume knows which disk it sits on (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`),
/// and this has to work on boot media, where PowerShell does not exist.
#[cfg(target_os = "windows")]
pub fn system_disk_number() -> Option<u32> {
    use own_backup_backup::volume_win::{volume_root_for_path, volume_start_on_disk};

    let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = volume_root_for_path(&format!("{}\\", sysdrive.trim_end_matches('\\')))?;
    // A spanned volume answers with no single disk — and a system on one is not
    // something this guard can reason about anyway.
    volume_start_on_disk(&root).map(|(disk, _)| disk)
}

/// True when running under a Windows recovery environment.
///
/// Two ways of asking, because they fail differently. The MiniNT registry key
/// is what Microsoft documents for exactly this question and is what the rest
/// of the program uses; `SystemDrive == X:` is the observable symptom, and it
/// survives a WinPE that has been customised out of recognition. Either is
/// enough — a false "this is a rescue environment" would DISARM these guards,
/// so both are deliberately positive-only signals of the same fact rather than
/// a vote.
#[cfg(target_os = "windows")]
pub fn is_boot_media() -> bool {
    if own_backup_common::media_mode::is_media_mode() {
        return true;
    }
    std::env::var("SystemDrive")
        .map(|d| d.eq_ignore_ascii_case("X:"))
        .unwrap_or(false)
}

/// Guard a whole-disk restore using live Windows detection.
#[cfg(target_os = "windows")]
pub fn guard_target_disk(target_disk: u32) -> Result<()> {
    check_whole_disk_restore(target_disk, system_disk_number(), is_boot_media())
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
    for path in [
        std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
        format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())),
    ] {
        if let Some(root) = volume_root_for_path(&path) {
            for e in volume_extents(&root) {
                live.push((e.disk_number, e.offset));
            }
        }
    }
    if live.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            "Не удалось определить, где стоит работающая Windows, поэтому нельзя \
             убедиться, что запись пойдёт не на неё. Загрузитесь с носителя \
             восстановления."
                .into(),
        ));
    }

    let Some(root) = volume_root_for_path(target).or_else(|| volume_root_for_path(&format!("{target}\\"))) else {
        // An unrecognised target is not obviously safe: it may be a raw device
        // path pointing at the very volume above.
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "Не удалось определить, что такое «{target}», поэтому запись на него \
             не разрешена. Выберите том в списке."
        )));
    };
    let target_extents = volume_extents(&root);
    if target_extents.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "Не удалось определить, где на диске лежит «{target}», поэтому нельзя \
             убедиться, что это не системный том."
        )));
    }
    for e in &target_extents {
        if live.contains(&(e.disk_number, e.offset)) {
            return Err(OwnBackupError::RestoreIncompatible(
                "Это том, с которого работает Windows. Перезаписать его на живой \
                 системе нельзя — загрузитесь с носителя восстановления."
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn guard_target_volume(_target: &str) -> Result<()> {
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
    // Without the layout nothing below can be answered — and "no partitions
    // protected" would read as a clean bill of health.
    let layout = match own_backup_backup::gpt_reader::read_disk_layout(disk) {
        Ok(l) => l,
        Err(e) => {
            return Protection::unsure(format!(
                "не удалось прочитать разметку диска {disk} ({e}), \
                 поэтому неизвестно, есть ли на нём разделы работающей системы"
            ))
        }
    };
    let sector = layout.sector_size.max(512) as u64;
    let mut out = Vec::new();

    // The volume the running Windows lives on, located by byte offset rather
    // than by drive letter.
    match system_volume_offset(disk) {
        Some(offset) => {
            if let Some(p) = layout.partitions.iter().find(|p| p.first_lba * sector == offset) {
                out.push(p.index);
            }
        }
        None if system_disk_number() == Some(disk) => {
            // Windows says it lives on this disk and we cannot find where.
            return Protection::unsure(format!(
                "Windows работает с диска {disk}, но найти её раздел не удалось"
            ));
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
                if p.type_guid.eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM)
                    && !out.contains(&p.index)
                {
                    out.push(p.index);
                }
            }
        }
    }

    out.sort_unstable();
    Protection { parts: out, cannot_tell: None }
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
    let Ok(config) = efi::read_config() else { return BootEsp::Unknown };
    let Some(current) = config.current else { return BootEsp::Unknown };
    let Ok(Some(entry)) = efi::read_entry(current) else { return BootEsp::Unknown };
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
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let root = own_backup_backup::volume_win::volume_root_for_path(&system_root)?;
    own_backup_backup::volume_win::volume_extents(&root)
        .into_iter()
        .find(|e| e.disk_number == disk)
        .map(|e| e.offset)
}

#[cfg(not(target_os = "windows"))]
pub fn protection(_disk: u32) -> Protection {
    Protection::nothing_is_running()
}

#[cfg(not(target_os = "windows"))]
pub fn protected_partitions(_disk: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(target_os = "windows"))]
pub fn system_disk_number() -> Option<u32> {
    None
}
#[cfg(not(target_os = "windows"))]
pub fn is_boot_media() -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
pub fn guard_target_disk(_target_disk: u32) -> Result<()> {
    Ok(())
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
        assert!(unsure.cannot_tell.is_some(), "the caller must be able to tell these apart");
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
        assert_eq!(hard_drive_guid(&[0x04, 0x01, 0x00, 0x00]), None, "zero length");
        assert_eq!(hard_drive_guid(&[0x04, 0x01, 0xFF, 0xFF]), None, "past the end");
        assert_eq!(hard_drive_guid(&[]), None);
        assert_eq!(hard_drive_guid(&[0x7F, 0xFF, 0x04, 0x00]), None, "end marker only");
    }
}
