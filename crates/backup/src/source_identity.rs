//! Who a source volume really is, and where a destination folder really sits.
//!
//! Both questions used to be answered by comparing NAMES (audit AUD-10):
//!
//! * "is the destination on a volume being copied?" compared drive LETTERS, so
//!   a source given as `\\?\Volume{…}`, as `part:<disk>:<offset>:<len>` or
//!   through a mount point matched nothing at all — and the image could be
//!   written onto the volume being imaged, which grows the source while it is
//!   being read;
//! * "which chain does this volume's images belong to?" went through the image
//!   FILE NAME, whose token is the drive letter or, for an encrypted `part:`
//!   source with no letter, its cleaned-up label. Two volumes labelled the same
//!   are then one volume as far as chain planning is concerned, and an
//!   increment can be written over a parent that is a copy of a different disk.
//!
//! What a volume is called is for showing to people. What it IS, for these two
//! decisions, is the range of sectors it occupies — and, for the chain, an
//! identity that survives being renamed and relettered.
//!
//! Every answer here can also be "I could not work it out", and that is a third
//! answer, not a quiet yes. A destination that cannot be located is not a
//! destination that is known to be safe.

use own_backup_common::Result;

/// Where something lives: a disk and a byte range on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub disk: u32,
    pub offset: u64,
    pub length: u64,
}

impl Extent {
    /// Do these two ranges share a byte?
    pub fn overlaps(&self, other: &Extent) -> bool {
        if self.disk != other.disk {
            return false;
        }
        let a_end = self.offset.saturating_add(self.length);
        let b_end = other.offset.saturating_add(other.length);
        self.offset < b_end && other.offset < a_end
    }
}

/// Whether the destination folder sits on something the job is copying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestPlacement {
    /// Nowhere near any source.
    Clear,
    /// It is on one of them, named.
    OnSource(String),
    /// It could not be located — a network share, or a path whose volume could
    /// not be resolved. The reason, in words.
    CannotTell(String),
}

/// Parse `part:<disk>:<offset>:<len>`, the form used for a partition with no
/// mounted volume (an encrypted container, a Linux partition).
pub fn parse_part_spec(device: &str) -> Option<Extent> {
    let rest = device.trim().strip_prefix("part:")?;
    let mut it = rest.split(':');
    let disk = it.next()?.parse::<u32>().ok()?;
    let offset = it.next()?.parse::<u64>().ok()?;
    let length = it.next()?.parse::<u64>().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(Extent { disk, offset, length })
}

/// The volume root VSS wants for a source device, or `None` when the device is
/// not a volume at all.
///
/// `AddToSnapshotSet` takes a volume NAME with a trailing backslash — a drive
/// letter root like `C:\`, or the canonical GUID form
/// `\\?\Volume{…}\`. Both are equally valid to it.
///
/// This used to accept only `\\.\X:` (found 2026-09-20). Every source without a
/// drive letter therefore got no shadow copy at all — an ESP, a recovery
/// partition, any letterless data volume — and the log said "not a lettered
/// volume; skipping VSS" as though that were a policy rather than the parser
/// giving up. The cost was double: a crash-consistent copy of a live volume,
/// AND the loss of the fast `$MFT` exclusion path, which refuses to read raw
/// metadata of a source nobody froze.
pub fn volume_root_for_snapshot(device_path: &str) -> Option<String> {
    let rest = device_path.strip_prefix(r"\\.\").or_else(|| device_path.strip_prefix(r"\\?\"))?;
    let trimmed = rest.trim_end_matches('\\');
    let bytes = trimmed.as_bytes();
    // A drive letter: `\\.\C:` → `C:\`
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(format!("{trimmed}\\"));
    }
    // A volume GUID path, in either namespace. VSS is given the `\\?\` form,
    // which is the one Windows documents for a volume name.
    if trimmed.len() > "Volume{".len()
        && trimmed[.."Volume{".len()].eq_ignore_ascii_case("Volume{")
        && trimmed.ends_with('}')
    {
        return Some(format!(r"\\?\{trimmed}\"));
    }
    // A physical drive, a partition range, anything else: not a volume.
    None
}

/// Is this a path on another machine? Those have no local extent, and that is
/// an answer rather than a failure to find one.
pub fn is_network_path(path: &str) -> bool {
    let p = path.trim();
    // `\\?\` and `\\.\` are local device namespaces, not UNC shares.
    (p.starts_with(r"\\") && !p.starts_with(r"\\?\") && !p.starts_with(r"\\.\"))
        || p.starts_with("//")
}

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use crate::volume_win::{volume_extents, volume_root_for_path};

    /// The sectors a source device occupies.
    ///
    /// Three shapes reach this: a drive letter or path (`C:`, `D:\Mount\Data`),
    /// a volume GUID path (`\\?\Volume{…}`), and `part:<disk>:<offset>:<len>`
    /// for a partition Windows has not mounted. The first two go through the
    /// volume manager, which is what makes a mount point resolve to the volume
    /// it leads to rather than to the one it sits in.
    pub fn extents_of_source(device: &str) -> Option<Vec<Extent>> {
        if let Some(e) = parse_part_spec(device) {
            return Some(vec![e]);
        }
        let root = volume_root_for_path(device)
            .or_else(|| volume_root_for_path(&format!("{}\\", device.trim_end_matches('\\'))))?;
        let out: Vec<Extent> = volume_extents(&root)
            .into_iter()
            .map(|e| Extent { disk: e.disk_number, offset: e.offset, length: e.length })
            .collect();
        (!out.is_empty()).then_some(out)
    }

    /// The sectors the destination folder's volume occupies. A folder that does
    /// not exist yet is answered for by the nearest ancestor that does — which
    /// is the volume it would be created on.
    pub fn extents_of_destination(folder: &str) -> Option<Vec<Extent>> {
        let mut at = std::path::PathBuf::from(folder);
        loop {
            if let Some(e) = extents_of_source(&at.to_string_lossy()) {
                return Some(e);
            }
            if !at.pop() {
                return None;
            }
            if at.as_os_str().is_empty() {
                return None;
            }
        }
    }

    /// An identity for this source that survives a rename and a new drive
    /// letter — see [`crate::sidecar::SidecarInfo::source_id`].
    pub fn stable_source_id(device: &str) -> Option<String> {
        // A partition with no mounted volume: the disk's own GUID and the
        // offset within it. The disk NUMBER is deliberately not in it — that
        // is the part which moves when a drive is replugged.
        if let Some(e) = parse_part_spec(device) {
            let disk_guid = crate::gpt_reader::read_disk_layout(e.disk).ok()?.disk_guid;
            if disk_guid.is_empty() {
                return None;
            }
            return Some(format!("disk:{}:{}", disk_guid.to_uppercase(), e.offset));
        }
        // A mounted volume: its GUID path, which is what Windows itself uses
        // to name a volume independently of where it is mounted.
        let root = volume_root_for_path(device)
            .or_else(|| volume_root_for_path(&format!("{}\\", device.trim_end_matches('\\'))))?;
        let root = root.trim_end_matches('\\').to_string();
        (!root.is_empty()).then_some(root.to_uppercase())
    }
}

#[cfg(target_os = "windows")]
pub use win::{extents_of_destination, extents_of_source, stable_source_id};

#[cfg(not(target_os = "windows"))]
pub fn extents_of_source(device: &str) -> Option<Vec<Extent>> {
    parse_part_spec(device).map(|e| vec![e])
}

#[cfg(not(target_os = "windows"))]
pub fn extents_of_destination(_folder: &str) -> Option<Vec<Extent>> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn stable_source_id(_device: &str) -> Option<String> {
    None
}

/// Does the destination folder sit on any of these sources?
///
/// The comparison is between the actual ranges, so a DIFFERENT partition of
/// the same disk is fine — writing there does not grow what is being read, and
/// refusing it would be refusing a thing that works (audit AUD-10, point 4).
pub fn where_the_destination_sits(sources: &[String], dest_folder: &str) -> DestPlacement {
    if is_network_path(dest_folder) {
        return DestPlacement::Clear; // another machine's disk entirely
    }
    let Some(dest) = extents_of_destination(dest_folder) else {
        return DestPlacement::CannotTell(format!(
            "не удалось определить, на каком томе лежит «{dest_folder}»"
        ));
    };
    let mut unresolved: Vec<&str> = Vec::new();
    for source in sources {
        let Some(src) = extents_of_source(source) else {
            unresolved.push(source.as_str());
            continue;
        };
        if src.iter().any(|s| dest.iter().any(|d| s.overlaps(d))) {
            return DestPlacement::OnSource(source.clone());
        }
    }
    if !unresolved.is_empty() {
        return DestPlacement::CannotTell(format!(
            "не удалось определить, где лежат источники: {}",
            unresolved.join(", ")
        ));
    }
    DestPlacement::Clear
}

/// The same question, answered as a `Result` for callers that want to refuse.
pub fn refuse_destination_on_a_source(sources: &[String], dest_folder: &str) -> Result<()> {
    match where_the_destination_sits(sources, dest_folder) {
        DestPlacement::Clear => Ok(()),
        DestPlacement::OnSource(s) => Err(own_backup_common::OwnBackupError::Config(format!(
            "Папка назначения «{dest_folder}» лежит на томе, который копируется ({s}). \
             Копия росла бы вместе с источником — выберите другое место."
        ))),
        DestPlacement::CannotTell(why) => {
            Err(own_backup_common::OwnBackupError::Config(format!(
                "Нельзя убедиться, что копия не пишется на копируемый том: {why}."
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partition_spec_is_a_range_on_a_disk() {
        assert_eq!(
            parse_part_spec("part:4:1048576:1000"),
            Some(Extent { disk: 4, offset: 1048576, length: 1000 })
        );
        assert_eq!(parse_part_spec("C:"), None);
        assert_eq!(parse_part_spec("part:4:1048576"), None, "a range needs three numbers");
        assert_eq!(parse_part_spec("part:4:1048576:1000:5"), None, "and no more than three");
        assert_eq!(parse_part_spec("part:x:1:2"), None);
    }

    /// The whole point of comparing ranges rather than names: a different
    /// partition of the same disk is a perfectly good destination.
    #[test]
    fn two_partitions_of_one_disk_do_not_overlap() {
        let a = Extent { disk: 0, offset: 1024 * 1024, length: 100 * 1024 * 1024 };
        let b = Extent { disk: 0, offset: 200 * 1024 * 1024, length: 100 * 1024 * 1024 };
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn the_same_range_on_the_same_disk_overlaps() {
        let a = Extent { disk: 2, offset: 1024, length: 4096 };
        assert!(a.overlaps(&a));
        // Touching but not sharing a byte is not an overlap.
        let after = Extent { disk: 2, offset: 1024 + 4096, length: 4096 };
        assert!(!a.overlaps(&after));
        // One byte inside is.
        let inside = Extent { disk: 2, offset: 1024 + 4095, length: 4096 };
        assert!(a.overlaps(&inside));
    }

    #[test]
    fn the_same_range_on_a_different_disk_is_a_different_place() {
        let a = Extent { disk: 0, offset: 1024, length: 4096 };
        let b = Extent { disk: 1, offset: 1024, length: 4096 };
        assert!(!a.overlaps(&b));
    }

    /// A length that would run past the end of the address space must not wrap
    /// into "no overlap".
    #[test]
    fn a_length_that_would_wrap_is_clamped_not_wrapped() {
        let huge = Extent { disk: 0, offset: u64::MAX - 10, length: u64::MAX };
        let low = Extent { disk: 0, offset: 0, length: 4096 };
        // Saturating means `huge` reaches the end of the disk, not back to 0.
        assert!(!huge.overlaps(&low));
        let at_the_top = Extent { disk: 0, offset: u64::MAX - 5, length: 2 };
        assert!(huge.overlaps(&at_the_top));
    }

    #[test]
    fn a_share_on_another_machine_is_not_a_local_volume() {
        assert!(is_network_path(r"\\server\backups"));
        assert!(is_network_path("//server/backups"));
        assert!(!is_network_path(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}"));
        assert!(!is_network_path(r"\\.\PhysicalDrive0"));
        assert!(!is_network_path(r"D:\Backups"));
    }

    /// A network destination is answered, not left as "could not tell": it is
    /// another machine's disk, and no local source can be on it.
    #[test]
    fn a_network_destination_is_clear_rather_than_unknown() {
        assert_eq!(
            where_the_destination_sits(&["C:".into()], r"\\nas\backups"),
            DestPlacement::Clear
        );
    }

    /// And a local one that cannot be located says so, instead of passing.
    /// Not knowing where the copy will land is not the same as knowing it is
    /// somewhere safe (audit AUD-10, point 2).
    #[test]
    fn a_destination_that_cannot_be_located_is_not_called_safe() {
        let answer = where_the_destination_sits(&["C:".into()], "Z:\\nowhere-at-all");
        assert!(
            matches!(answer, DestPlacement::CannotTell(_)),
            "expected an explicit 'cannot tell', got {answer:?}"
        );
        assert!(refuse_destination_on_a_source(&["C:".into()], "Z:\\nowhere-at-all").is_err());
    }

    // --- Какой том отдать VSS (найдено живым прогоном 2026-09-20) ----------
    //
    // Разбирался только `\.\X:`, поэтому любой источник без буквы оставался
    // без снимка, а журнал писал «не том с буквой — VSS пропускаем», будто это
    // правило, а не разборщик, который сдался. Платилось дважды: копия живого
    // тома выходила crash-consistent, и терялся быстрый путь исключений —
    // читать сырой `$MFT` у незамороженного источника нельзя.

    #[test]
    fn a_lettered_volume_becomes_its_root() {
        assert_eq!(volume_root_for_snapshot(r"\\.\C:"), Some(r"C:\".into()));
        assert_eq!(volume_root_for_snapshot(r"\\.\e:"), Some(r"e:\".into()));
    }

    /// Случай, которого не хватало: ESP и раздел восстановления буквы не имеют,
    /// а VSS про них знает ровно столько же, сколько про C:.
    #[test]
    fn a_volume_with_no_letter_is_a_volume_too() {
        let guid = "Volume{fb002532-838c-4818-8f14-7824d64763b7}";
        let want = Some(format!(r"\\?\{guid}\"));
        // В каком бы пространстве имён он ни пришёл и с хвостовой чертой или без.
        assert_eq!(volume_root_for_snapshot(&format!(r"\\?\{guid}")), want);
        assert_eq!(volume_root_for_snapshot(&format!(r"\\?\{guid}\")), want);
        assert_eq!(volume_root_for_snapshot(&format!(r"\\.\{guid}")), want);
    }

    /// Не том — значит нечего и снимать: физический диск, диапазон разделов,
    /// путь в никуда.
    #[test]
    fn what_is_not_a_volume_gets_no_snapshot() {
        assert_eq!(volume_root_for_snapshot(r"\\.\PhysicalDrive0"), None);
        assert_eq!(volume_root_for_snapshot("part:4:1048576:1000"), None);
        assert_eq!(volume_root_for_snapshot("C:"), None, "путь должен быть уже нормализован");
        assert_eq!(volume_root_for_snapshot(r"\\.\Volume"), None, "без скобок это не GUID");
        assert_eq!(volume_root_for_snapshot(r"\\?\Volume{"), None);
        assert_eq!(volume_root_for_snapshot(""), None);
    }
}
