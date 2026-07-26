//! Typed restore plan — the contract between the GUI's partition designer and
//! the service that executes it.
//!
//! The designer never touches a disk. Every click (create a partition, delete
//! one, map source → destination) only edits in-memory state; pressing
//! "Восстановить" turns that state into a [`RestorePlan`], which is checked
//! (`restore_check`) and only then handed to the service. Nothing is written
//! until the user confirms the checked plan.
//!
//! ## Why the execution order is fixed
//!
//! The order in which the user clicked is irrelevant — [`RestorePlan::ordered`]
//! sorts operations into the only order that works:
//!
//! 1. [`RestoreOp::WritePartitionTable`] — the table is one structure with its
//!    own checksums, so it is written whole, once, before anything else.
//! 2. [`RestoreMode::Sectors`] restores — raw writes through
//!    `part:<disk>:<offset>:<len>`, done while the disk is still offline.
//! 3. *(executor brings the disk online here — Windows creates the volumes)*
//! 4. [`RestoreOp::Format`] — needs a volume, hence must follow the online step.
//! 5. [`RestoreMode::Files`] restores — need a mounted, formatted target.
//! 6. [`RestoreOp::ExtendFilesystem`], [`RestoreOp::AssignLetter`],
//!    [`RestoreOp::FinalizeBoot`] — finishing touches, seconds each.
//!
//! A sector-mode restore needs no [`RestoreOp::Format`] at all: the filesystem
//! arrives from the image already made. Formatting exists only for the file
//! mode. That is what lets one order serve both — and a plan that mixes the two
//! modes across partitions still executes correctly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Partitioning scheme of the *target* disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionScheme {
    Gpt,
    Mbr,
}

/// Scheme-specific attributes of one partition table entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum PartitionKind {
    Gpt {
        /// Partition *type* GUID (EFI System / Microsoft Basic Data / …).
        type_guid: String,
        /// Partition *unique* GUID. Restored verbatim for partitions coming
        /// from a backup — the BCD and UEFI reference partitions by this GUID,
        /// so preserving it is what keeps the boot chain intact. Empty for a
        /// hand-created partition; the writer then generates a fresh one.
        part_guid: String,
        /// GPT attribute flags (bit 0 = required, bit 60 = read-only, …).
        attributes: u64,
        /// GPT entry name (UTF-16 on disk, ≤ 36 code units). Distinct from the
        /// filesystem's volume label, which belongs to [`RestoreOp::Format`].
        name: String,
    },
    Mbr {
        /// Partition type byte (0x07 NTFS/exFAT, 0x0C FAT32 LBA, 0x83 Linux, …).
        type_byte: u8,
        /// Active (bootable) flag — at most one per disk.
        bootable: bool,
        /// Logical partition inside the extended partition, rather than one of
        /// the four primary slots.
        logical: bool,
    },
}

/// One entry of the target disk's partition table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionSpec {
    /// 1-based partition number. For GPT it is the slot in the entry array;
    /// for MBR, 1–4 are the primary slots and 5+ the logical chain.
    pub index: u32,
    /// First sector, inclusive.
    pub first_lba: u64,
    /// Last sector, **inclusive** — same convention as [`crate::gpt`].
    pub last_lba: u64,
    pub kind: PartitionKind,
}

impl PartitionSpec {
    pub fn offset_bytes(&self, sector_size: u32) -> u64 {
        self.first_lba * sector_size as u64
    }

    pub fn size_bytes(&self, sector_size: u32) -> u64 {
        (self.last_lba + 1 - self.first_lba) * sector_size as u64
    }

    /// Sector one past the end — handy for overlap checks, where the inclusive
    /// `last_lba` is a reliable source of off-by-one errors.
    pub fn end_lba_exclusive(&self) -> u64 {
        self.last_lba + 1
    }
}

/// The complete partition table to write onto the target disk.
///
/// It describes the *final* layout, not a diff: deleting a partition means it
/// is absent from `entries`, creating one means it is present. There is no
/// "delete partition" operation, because a partition table is rewritten whole
/// anyway — expressing deletions separately would only invite the two
/// representations to disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionTableSpec {
    pub scheme: PartitionScheme,
    /// Logical sector size of the target disk (512 or 4096).
    pub sector_size: u32,
    /// Total size of the target disk in bytes.
    pub disk_size_bytes: u64,
    /// GPT: the disk GUID. MBR: the 4-byte disk signature as 8 hex digits.
    /// Empty means "generate a fresh one".
    ///
    /// Note that a whole-disk restore deliberately does **not** reuse the
    /// source disk's GUID: two disks carrying the same GPT GUID make Windows
    /// take one of them offline.
    #[serde(default)]
    pub disk_id: String,
    pub entries: Vec<PartitionSpec>,
}

/// Filesystems we can actually create. Deliberately narrower than
/// [`crate::types::FileSystem`]: "no format" is the absence of a
/// [`RestoreOp::Format`] operation, not a variant here, and Linux filesystems
/// are missing because Windows has no `mkfs` (D4 — a Linux partition is
/// restored sector-by-sector, which brings its filesystem along).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatFs {
    Ntfs,
    Fat32,
    ExFat,
    ReFs,
}

impl FormatFs {
    /// Name as `FormatEx` / `format.com` expect it.
    pub fn as_str(self) -> &'static str {
        match self {
            FormatFs::Ntfs => "NTFS",
            FormatFs::Fat32 => "FAT32",
            FormatFs::ExFat => "exFAT",
            FormatFs::ReFs => "ReFS",
        }
    }
}

/// How a partition's data gets restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    /// Raw blocks at their original offsets. The filesystem comes along intact,
    /// so the target must be at least as large as the source volume.
    Sectors,
    /// File-by-file onto a freshly formatted target. The only way into a
    /// *smaller* partition (D1), and only for Windows filesystems.
    Files,
}

/// Where an operation acts on the target disk. Byte offset and length are
/// carried alongside the index because that is what
/// `part:<disk>:<offset>:<len>` needs, and recomputing them from the table at
/// every step is how the two drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    pub index: u32,
    pub offset: u64,
    pub size: u64,
}

/// A single step of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RestoreOp {
    /// Write the whole partition table (GPT or MBR) in one go.
    WritePartitionTable { table: PartitionTableSpec },
    /// Create a filesystem on a partition. Only ever paired with
    /// [`RestoreMode::Files`], or used on its own for a hand-created partition
    /// that receives no data.
    Format {
        part: PartRef,
        fs: FormatFs,
        /// Volume label (filesystem-level; the GPT entry name is separate).
        label: String,
        quick: bool,
    },
    /// Copy one partition's data out of the image.
    RestorePartition {
        /// Partition index **inside the image**, not on the target disk.
        src_index: u32,
        dst: PartRef,
        mode: RestoreMode,
        /// Overwrite the blocks the image does not cover with zeroes (D16).
        /// Off by default: a sector restore writes only the used blocks, so
        /// whatever the target held before physically survives in the gaps.
        /// The filesystem cannot reach it — the used-block map comes from the
        /// image — but the bytes are there.
        #[serde(default)]
        wipe_free_space: bool,
    },
    /// Grow an NTFS filesystem to fill its partition, after a sector restore
    /// into a larger target (D2).
    ExtendFilesystem { part: PartRef },
    /// Mount the volume under a drive letter.
    AssignLetter { part: PartRef, letter: char },
    /// Rebuild the boot chain (`bcdboot`).
    FinalizeBoot {
        windows_part: PartRef,
        esp: Option<PartRef>,
        uefi: bool,
    },
}

/// Sort key giving the fixed execution order documented at the top of this
/// module. The executor brings the disk online between [`Self::ORDER_RAW`] and
/// [`Self::ORDER_FORMAT`].
impl RestoreOp {
    const ORDER_TABLE: u8 = 0;
    const ORDER_RAW: u8 = 1;
    const ORDER_FORMAT: u8 = 2;
    const ORDER_FILES: u8 = 3;
    const ORDER_EXTEND: u8 = 4;
    const ORDER_LETTER: u8 = 5;
    const ORDER_BOOT: u8 = 6;

    pub fn order_key(&self) -> u8 {
        match self {
            RestoreOp::WritePartitionTable { .. } => Self::ORDER_TABLE,
            RestoreOp::RestorePartition { mode: RestoreMode::Sectors, .. } => Self::ORDER_RAW,
            RestoreOp::Format { .. } => Self::ORDER_FORMAT,
            RestoreOp::RestorePartition { mode: RestoreMode::Files, .. } => Self::ORDER_FILES,
            RestoreOp::ExtendFilesystem { .. } => Self::ORDER_EXTEND,
            RestoreOp::AssignLetter { .. } => Self::ORDER_LETTER,
            RestoreOp::FinalizeBoot { .. } => Self::ORDER_BOOT,
        }
    }

    /// The partition this operation acts on, if any. Used as the tie-breaker
    /// within one order key, so a plan always executes in a reproducible
    /// sequence regardless of the order the user clicked things.
    pub fn part(&self) -> Option<PartRef> {
        match self {
            RestoreOp::WritePartitionTable { .. } => None,
            RestoreOp::Format { part, .. } => Some(*part),
            RestoreOp::RestorePartition { dst, .. } => Some(*dst),
            RestoreOp::ExtendFilesystem { part } => Some(*part),
            RestoreOp::AssignLetter { part, .. } => Some(*part),
            RestoreOp::FinalizeBoot { windows_part, .. } => Some(*windows_part),
        }
    }

    /// Stable slug for logs, telemetry and the frontend — prose changes,
    /// this does not.
    pub fn kind(&self) -> &'static str {
        match self {
            RestoreOp::WritePartitionTable { .. } => "write_partition_table",
            RestoreOp::Format { .. } => "format",
            RestoreOp::RestorePartition { mode: RestoreMode::Sectors, .. } => "restore_sectors",
            RestoreOp::RestorePartition { mode: RestoreMode::Files, .. } => "restore_files",
            RestoreOp::ExtendFilesystem { .. } => "extend_filesystem",
            RestoreOp::AssignLetter { .. } => "assign_letter",
            RestoreOp::FinalizeBoot { .. } => "finalize_boot",
        }
    }

    /// One line describing the operation, in the words the user will read.
    pub fn title(&self) -> String {
        use crate::units::human_bytes;
        match self {
            RestoreOp::WritePartitionTable { table } => format!(
                "Записать разметку {} ({} разд.)",
                match table.scheme {
                    PartitionScheme::Gpt => "GPT",
                    PartitionScheme::Mbr => "MBR",
                },
                table.entries.len()
            ),
            RestoreOp::Format { part, fs, label, quick } => format!(
                "Отформатировать раздел {} в {}{}{}",
                part.index,
                fs.as_str(),
                if label.is_empty() { String::new() } else { format!(", метка «{label}»") },
                if *quick { ", быстрое" } else { ", полное" }
            ),
            RestoreOp::RestorePartition { src_index, dst, mode, .. } => format!(
                "Восстановить раздел {src_index} → {} ({}, {})",
                dst.index,
                human_bytes(dst.size),
                match mode {
                    RestoreMode::Sectors => "посекторно",
                    RestoreMode::Files => "пофайлово",
                }
            ),
            RestoreOp::ExtendFilesystem { part } => {
                format!("Расширить файловую систему раздела {} на весь раздел", part.index)
            }
            RestoreOp::AssignLetter { part, letter } => {
                format!("Назначить разделу {} букву {letter}:", part.index)
            }
            RestoreOp::FinalizeBoot { uefi, .. } => {
                format!("Пересоздать загрузчик ({})", if *uefi { "UEFI" } else { "BIOS" })
            }
        }
    }

    /// Whether this operation needs the disk online and its volumes mounted.
    ///
    /// Raw sector writes go through the physical drive and are happiest with
    /// the disk offline; everything after them talks to a *volume*, which only
    /// exists once Windows has re-read the table and mounted it. The executor
    /// flips the disk between the two groups.
    pub fn needs_disk_online(&self) -> bool {
        matches!(
            self,
            RestoreOp::Format { .. }
                | RestoreOp::RestorePartition { mode: RestoreMode::Files, .. }
                | RestoreOp::ExtendFilesystem { .. }
                | RestoreOp::AssignLetter { .. }
                | RestoreOp::FinalizeBoot { .. }
        )
    }

    /// Whether the user may cancel *while this operation runs*.
    ///
    /// Only the data restore is long enough for cancellation to mean anything;
    /// everything else finishes in well under a second, and interrupting it
    /// would leave a half-written structure for no benefit. The RPC refuses a
    /// cancel request outright while a non-cancellable operation runs, rather
    /// than accepting it and quietly doing nothing (D18).
    pub fn cancellable(&self) -> bool {
        matches!(self, RestoreOp::RestorePartition { .. })
    }

    /// Whether this operation destroys data that cannot be recovered. From the
    /// first such operation onward there is no going back — the old partition
    /// table or filesystem is already gone (D12).
    pub fn destroys_data(&self) -> bool {
        matches!(
            self,
            RestoreOp::WritePartitionTable { .. }
                | RestoreOp::Format { .. }
                | RestoreOp::RestorePartition { .. }
        )
    }
}

/// Everything the service needs to carry out a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlan {
    /// Identifies this run. Cancellation is addressed to it, so a cancel that
    /// arrives late cannot hit the run that started after it (D18).
    pub id: Uuid,
    /// Physical disk number the plan applies to.
    pub target_disk: u32,
    /// Backup image the data comes from.
    pub image_path: PathBuf,
    /// Verify what was written once the plan finishes (D10).
    #[serde(default)]
    pub verify_after: bool,
    pub ops: Vec<RestoreOp>,
}

impl RestorePlan {
    pub fn new(target_disk: u32, image_path: PathBuf, ops: Vec<RestoreOp>) -> Self {
        Self { id: Uuid::new_v4(), target_disk, image_path, verify_after: false, ops }
    }

    /// The plan's operations in execution order (see the module docs). Sorting
    /// is stable and the tie-breaker is the partition's byte offset, so the
    /// same plan always produces the same sequence.
    pub fn ordered(&self) -> Vec<&RestoreOp> {
        let mut out: Vec<&RestoreOp> = self.ops.iter().collect();
        out.sort_by_key(|op| (op.order_key(), op.part().map(|p| p.offset).unwrap_or(0)));
        out
    }

    /// Whether the plan rewrites the partition table. When it does, the whole
    /// disk goes offline for the duration; when it does not, only the volumes
    /// being written are dismounted (D17).
    pub fn touches_partition_table(&self) -> bool {
        self.ops.iter().any(|op| matches!(op, RestoreOp::WritePartitionTable { .. }))
    }

    /// Index into [`Self::ordered`] of the first operation that destroys data.
    /// Cancelling before it is free; from it onward the button goes dark.
    pub fn point_of_no_return(&self) -> Option<usize> {
        self.ordered().iter().position(|op| op.destroys_data())
    }

    /// The partition table the plan writes, if it writes one.
    pub fn table(&self) -> Option<&PartitionTableSpec> {
        self.ops.iter().find_map(|op| match op {
            RestoreOp::WritePartitionTable { table } => Some(table),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(index: u32, offset: u64) -> PartRef {
        PartRef { index, offset, size: 1024 * 1024 * 1024 }
    }

    fn gpt_entry(index: u32, first_lba: u64, last_lba: u64) -> PartitionSpec {
        PartitionSpec {
            index,
            first_lba,
            last_lba,
            kind: PartitionKind::Gpt {
                type_guid: crate::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                part_guid: String::new(),
                attributes: 0,
                name: format!("part{index}"),
            },
        }
    }

    fn table() -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: 512 * 1024 * 1024 * 1024,
            disk_id: String::new(),
            entries: vec![gpt_entry(1, 2048, 4095)],
        }
    }

    #[test]
    fn size_and_offset_use_inclusive_last_lba() {
        let e = gpt_entry(1, 2048, 4095);
        assert_eq!(e.offset_bytes(512), 1024 * 1024);
        assert_eq!(e.size_bytes(512), 1024 * 1024);
        assert_eq!(e.end_lba_exclusive(), 4096);
    }

    #[test]
    fn ordered_ignores_click_order() {
        // Deliberately built back to front, the way an impatient user clicks.
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::FinalizeBoot { windows_part: part(2, 200), esp: None, uefi: true },
                RestoreOp::AssignLetter { part: part(2, 200), letter: 'D' },
                RestoreOp::RestorePartition {
                    src_index: 2,
                    dst: part(2, 200),
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                },
                RestoreOp::Format {
                    part: part(2, 200),
                    fs: FormatFs::Ntfs,
                    label: "Data".into(),
                    quick: true,
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(1, 100),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                },
                RestoreOp::WritePartitionTable { table: table() },
            ],
        );

        let keys: Vec<u8> = plan.ordered().iter().map(|op| op.order_key()).collect();
        assert_eq!(keys, vec![0, 1, 2, 3, 5, 6]);
    }

    #[test]
    fn sector_restore_precedes_format_and_file_restore_follows_it() {
        // The whole reason one order can serve both modes: raw writes happen
        // while the disk is offline, formatting needs it back online.
        let raw = RestoreOp::RestorePartition {
            src_index: 1,
            dst: part(1, 0),
            mode: RestoreMode::Sectors,
            wipe_free_space: false,
        };
        let fmt = RestoreOp::Format {
            part: part(2, 0),
            fs: FormatFs::Ntfs,
            label: String::new(),
            quick: true,
        };
        let files = RestoreOp::RestorePartition {
            src_index: 2,
            dst: part(2, 0),
            mode: RestoreMode::Files,
            wipe_free_space: false,
        };
        assert!(raw.order_key() < fmt.order_key());
        assert!(fmt.order_key() < files.order_key());
    }

    #[test]
    fn same_stage_sorts_by_offset() {
        let plan = RestorePlan::new(
            0,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::AssignLetter { part: part(3, 3000), letter: 'F' },
                RestoreOp::AssignLetter { part: part(1, 1000), letter: 'D' },
                RestoreOp::AssignLetter { part: part(2, 2000), letter: 'E' },
            ],
        );
        let offsets: Vec<u64> =
            plan.ordered().iter().filter_map(|op| op.part()).map(|p| p.offset).collect();
        assert_eq!(offsets, vec![1000, 2000, 3000]);
    }

    #[test]
    fn point_of_no_return_is_the_table_write() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::AssignLetter { part: part(1, 100), letter: 'D' },
                RestoreOp::WritePartitionTable { table: table() },
            ],
        );
        assert_eq!(plan.point_of_no_return(), Some(0));
        assert!(plan.touches_partition_table());
    }

    #[test]
    fn a_plan_that_only_assigns_letters_has_no_point_of_no_return() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::AssignLetter { part: part(1, 100), letter: 'D' }],
        );
        assert_eq!(plan.point_of_no_return(), None);
        assert!(!plan.touches_partition_table());
    }

    #[test]
    fn only_the_data_restore_is_cancellable() {
        let restore = RestoreOp::RestorePartition {
            src_index: 1,
            dst: part(1, 0),
            mode: RestoreMode::Sectors,
            wipe_free_space: false,
        };
        assert!(restore.cancellable());
        assert!(!RestoreOp::WritePartitionTable { table: table() }.cancellable());
        assert!(!RestoreOp::ExtendFilesystem { part: part(1, 0) }.cancellable());
        assert!(!RestoreOp::AssignLetter { part: part(1, 0), letter: 'D' }.cancellable());
    }

    #[test]
    fn round_trips_through_json() {
        let plan = RestorePlan::new(
            2,
            "/backups/disk0.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable {
                    table: PartitionTableSpec {
                        scheme: PartitionScheme::Mbr,
                        sector_size: 512,
                        disk_size_bytes: 960 * 1000 * 1000 * 1000,
                        disk_id: "1A2B3C4D".into(),
                        entries: vec![PartitionSpec {
                            index: 1,
                            first_lba: 2048,
                            last_lba: 2_097_151,
                            kind: PartitionKind::Mbr {
                                type_byte: 0x07,
                                bootable: true,
                                logical: false,
                            },
                        }],
                    },
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(1, 1024 * 1024),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: true,
                },
                RestoreOp::ExtendFilesystem { part: part(1, 1024 * 1024) },
            ],
        );
        let json = serde_json::to_string(&plan).unwrap();
        let back: RestorePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn wipe_free_space_defaults_to_off_when_absent() {
        // Old plans, and plans built by a frontend that has not learnt the flag
        // yet, must not silently start wiping.
        let json = r#"{"op":"restore_partition","src_index":1,
            "dst":{"index":1,"offset":0,"size":1024},"mode":"sectors"}"#;
        let op: RestoreOp = serde_json::from_str(json).unwrap();
        assert!(matches!(op, RestoreOp::RestorePartition { wipe_free_space: false, .. }));
    }
}
