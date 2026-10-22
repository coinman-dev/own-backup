//! What loaders are on this machine's EFI System Partitions, and which of them
//! anything points at.
//!
//! Stage E3 of `.ai/boot-entries-plan_2026-09-18.md`, and the reason the whole
//! boot section exists. A loader and the entry naming it are two separate
//! things kept in two separate places: the file sits on a partition, the entry
//! sits in the machine's firmware. Losing the entry leaves the file untouched
//! and unreachable, and nothing on screen anywhere says so — the program is
//! simply gone from the boot menu one day.
//!
//! That is what happened here on 2026-09-18: Acronis and R-Drive Image had
//! their loaders on the ESP of disk 3, neither had an entry, and the only way
//! to find that out was to read the partition by hand. This module puts the two
//! lists side by side and names what is in one and not the other.
//!
//! An ESP is FAT32, so Windows mounts it like any other volume — it just gets
//! no drive letter. A volume GUID path opens it without one, which is how
//! [`crate::esp_fallback_win`] already works.

#![cfg(target_os = "windows")]

use std::path::Path;

use own_backup_common::{tr, Result};
use tracing::debug;


/// One `.efi` found on a partition.
#[derive(Debug, Clone)]
pub struct Loader {
    pub disk: u32,
    pub partition: u32,
    pub partition_guid: String,
    /// Inside the partition, the way a boot entry would spell it.
    pub path: String,
    pub size: u64,
    /// What it is, in words — in the window's language.
    pub kind: &'static str,
    /// Neither its name nor its contents said what it is — `kind` is the
    /// generic «загрузчик». The boot page then offers the vendor folder's name
    /// for a new entry instead of that word; a flag, because the word itself is
    /// shown in the window's language.
    pub generic: bool,
    /// Whether a boot entry would sensibly name it, or another loader launches
    /// it. See [`crate::esp_scan::role`].
    pub role: crate::esp_scan::Role,
    /// The removable-media path every firmware tries by itself.
    pub fallback: bool,
    /// An entry in firmware names this file.
    pub named_by: Option<u16>,
}

/// One EFI System Partition.
#[derive(Debug, Clone)]
pub struct Esp {
    pub disk: u32,
    pub partition: u32,
    pub partition_guid: String,
    pub size_bytes: u64,
    /// `None` when Windows has no volume for it — an ESP-typed partition that
    /// holds something Windows cannot read. Disk 3 partition 2 of this machine
    /// is exactly that: the slot says "EFI System" and the contents are ext4,
    /// put there by the Kubuntu installer.
    pub volume_root: Option<String>,
    /// Why nothing could be listed, when that is the case.
    pub unreadable: Option<String>,
    pub loaders: Vec<Loader>,
}

/// The GPT type that marks an EFI System Partition.
const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// Every ESP on every disk, with what is on it and what points at it.
///
/// `entries` is the firmware's side of the comparison; pass what
/// [`crate::efivars_win::read_config`] returned so the two lists come from one
/// moment.
pub fn scan(entries: &[crate::efivars_win::BootEntry]) -> Result<Vec<Esp>> {
    let mut out = Vec::new();
    // Disk numbers are probed rather than enumerated: a machine has a handful,
    // `read_disk_layout` already says "no" for the ones that are not there, and
    // this avoids dragging the whole inventory crate in for a count.
    for disk in 0u32..16 {
        let layout = match crate::gpt_reader::read_disk_layout(disk) {
            Ok(l) => l,
            // A disk without a GPT is not a fault here: it simply has no ESP.
            Err(e) => {
                debug!(disk, error = %e, "esp scan: no GPT on this disk");
                continue;
            }
        };
        for part in layout
            .partitions
            .iter()
            .filter(|p| p.type_guid.eq_ignore_ascii_case(ESP_TYPE))
        {
            let offset = part.first_lba * layout.sector_size as u64;
            let root = quick_volume_root(disk, offset);
            let mut esp = Esp {
                disk,
                partition: part.index,
                partition_guid: part.part_guid.clone(),
                size_bytes: (part.last_lba - part.first_lba + 1) * layout.sector_size as u64,
                volume_root: root.clone(),
                unreadable: None,
                loaders: Vec::new(),
            };
            match root {
                None => esp.unreadable = Some(tr!("esp_scan_win.not_a_volume").into_owned()),
                Some(root) => match collect_loaders(&root, disk, part.index, &part.part_guid) {
                    Ok(mut l) => {
                        for loader in &mut l {
                            loader.named_by =
                                naming_entry(entries, &loader.partition_guid, &loader.path);
                        }
                        esp.loaders = l;
                    }
                    Err(e) => esp.unreadable = Some(e.to_string()),
                },
            }
            out.push(esp);
        }
    }
    Ok(out)
}

/// Which entry, if any, names this file on this partition.
fn naming_entry(
    entries: &[crate::efivars_win::BootEntry],
    partition_guid: &str,
    path: &str,
) -> Option<u16> {
    crate::esp_scan::naming_entry(
        entries.iter().map(|e| (e.number, &e.option)),
        partition_guid,
        path,
    )
}

/// One sweep over the volumes of a disk. Deliberately not
/// [`crate::volume_win::volume_root_at_offset`], which waits ten seconds for a
/// volume to appear — right when a disk has just been attached, wrong when the
/// answer is "there is no volume here and there never will be".
fn quick_volume_root(disk: u32, offset: u64) -> Option<String> {
    crate::volume_win::all_volume_roots_on_disk(disk)
        .into_iter()
        .find(|vr| crate::volume_win::volume_start_on_disk(vr) == Some((disk, offset)))
}

/// Every `.efi` on the volume, as [`crate::esp_scan::collect_loaders`] finds
/// them, placed on this disk and partition.
fn collect_loaders(
    volume_root: &str,
    disk: u32,
    partition: u32,
    partition_guid: &str,
) -> Result<Vec<Loader>> {
    Ok(crate::esp_scan::collect_loaders(Path::new(volume_root))
        .into_iter()
        .map(|f| Loader {
            disk,
            partition,
            partition_guid: partition_guid.to_string(),
            path: f.path,
            size: f.size,
            kind: f.kind,
            generic: f.generic,
            role: f.role,
            fallback: f.fallback,
            named_by: None,
        })
        .collect())
}
