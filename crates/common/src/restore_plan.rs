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
/// [`RestoreOp::Format`] operation, not a variant here.
///
/// Which of them a machine can create depends on where the plan runs: Windows
/// has no `mkfs` for the Linux ones (D4 — there a Linux partition is restored
/// sector-by-sector, which brings its filesystem along), and Linux has no ReFS
/// formatter. The checks say so before anything is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatFs {
    Ntfs,
    Fat32,
    // Spelled out rather than left to `rename_all`, which would turn these into
    // "ex_fat" and "re_fs" — names no one would guess when reading the wire.
    #[serde(rename = "exfat")]
    ExFat,
    #[serde(rename = "refs")]
    ReFs,
    /// Linux only (`mkfs.ext4`).
    Ext4,
    /// Linux only (`mkfs.xfs`).
    Xfs,
    /// Linux only (`mkfs.btrfs`).
    Btrfs,
}

impl FormatFs {
    /// Whether this names the same filesystem as `fs`.
    ///
    /// The two enums exist for different reasons — [`FormatFs`] is what we can
    /// create, [`crate::types::FileSystem`] is what we can recognise — and this
    /// is the one place they have to be compared.
    pub fn is_same_as(self, fs: crate::types::FileSystem) -> bool {
        use crate::types::FileSystem;
        matches!(
            (self, fs),
            (FormatFs::Ntfs, FileSystem::Ntfs)
                | (FormatFs::Fat32, FileSystem::Fat32)
                | (FormatFs::ExFat, FileSystem::ExFat)
                | (FormatFs::ReFs, FileSystem::ReFs)
                | (FormatFs::Ext4, FileSystem::Ext4)
                | (FormatFs::Xfs, FileSystem::Xfs)
                | (FormatFs::Btrfs, FileSystem::Btrfs)
        )
    }

    /// The filesystems only Linux can create.
    pub fn is_linux_family(self) -> bool {
        matches!(self, FormatFs::Ext4 | FormatFs::Xfs | FormatFs::Btrfs)
    }

    /// Name as `FormatEx` / `format.com` expect it; the Linux ones as
    /// [`crate::types::FileSystem::label`] spells them.
    pub fn as_str(self) -> &'static str {
        match self {
            FormatFs::Ntfs => "NTFS",
            FormatFs::Fat32 => "FAT32",
            FormatFs::ExFat => "exFAT",
            FormatFs::ReFs => "ReFS",
            FormatFs::Ext4 => "ext4",
            FormatFs::Xfs => "XFS",
            FormatFs::Btrfs => "Btrfs",
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
        /// Put the encrypted container back (D19).
        ///
        /// The copy holds the volume decrypted. With this set, the restore
        /// writes the container's saved header at the partition start and
        /// re-encrypts the payload under the master key its keyslots yield —
        /// producing a byte-identical container, so the restored system's
        /// `crypttab` and initramfs keep working. Without it the partition ends
        /// up holding a bare filesystem, readable but not bootable.
        ///
        /// The passphrase is **not** part of the plan: the plan is persisted to
        /// the restore history, and a password written to disk in the clear is
        /// not a trade worth making. It travels with the apply request instead.
        #[serde(default)]
        reencrypt: bool,
    },
    /// Grow the filesystem to fill its partition, after a sector restore into
    /// a larger target (D2). NTFS on Windows; on Linux also ext2/3/4, XFS and
    /// btrfs.
    ExtendFilesystem { part: PartRef },
    /// Mount the volume under a drive letter.
    AssignLetter { part: PartRef, letter: char },
    /// Rebuild the boot chain (`bcdboot`).
    FinalizeBoot {
        windows_part: PartRef,
        esp: Option<PartRef>,
        uefi: bool,
    },
    /// Put a copy of the bootloader at `\EFI\BOOT\BOOTX64.EFI` on the restored
    /// ESP, if nothing is there yet.
    ///
    /// A UEFI machine boots what its **NVRAM** tells it to, and NVRAM is on the
    /// motherboard, not on the disk. The entry that says "ubuntu →
    /// `\EFI\ubuntu\shimx64.efi`" does not travel with a copy, so a disk
    /// restored into a new machine — or a new virtual machine — has a perfectly
    /// good loader that nothing ever asks for. The firmware then falls back to
    /// one fixed path, `\EFI\BOOT\BOOTX64.EFI`, and drops to a UEFI shell when
    /// that is missing too. Which is exactly what a restored Kubuntu did:
    /// `\EFI\ubuntu\shimx64.efi` present, `\EFI\BOOT` absent, no boot.
    ///
    /// So the restore leaves a copy where the firmware looks by itself. It is
    /// the same trick as a bootable USB stick, and it changes nothing for a
    /// machine that already has its NVRAM entry — that entry keeps winning.
    EnsureFallbackBootloader { esp: PartRef },

    /// Put the machine's UEFI boot entries back, from the copy's own record of
    /// them.
    ///
    /// The one operation here that does not touch the disk at all: it writes
    /// the motherboard's NVRAM. A restored disk can have every partition in the
    /// right place and still offer nothing to boot, because the menu that
    /// starts them was never on the disk to be copied.
    ///
    /// Three gates before this is in a plan, and they are deliberately not one:
    /// the copy has to CARRY entries (only a bootable copy does); the target
    /// disk has to be marked bootable in the restore screen; and the user has
    /// to have been shown what would be written and said yes to it. Firmware is
    /// the one thing a restore touches that outlives the disk — put a stale
    /// menu back on a machine whose disks have moved and it boots to nothing.
    RestoreBootEntries,

    /// Give the restored copy identities of its own (Linux): every
    /// filesystem, LUKS, LVM or md identity on `parts` that another device of
    /// this machine already carries is replaced, and with `fix_references`
    /// the restored system's fstab, crypttab and boot configuration are
    /// edited to match. Never in a plan the window builds: the service adds
    /// it when the person chose to change the duplicates the preview showed.
    /// Runs right after the sectors are checked, while the restore still
    /// holds the target away from udev, so the machine never acts on the
    /// clone under the source's names.
    ChangeIdentities {
        parts: Vec<PartRef>,
        #[serde(default)]
        fix_references: bool,
    },

    /// Re-read the disk, so Windows and the app stop describing the disk that
    /// used to be there.
    ///
    /// Every operation before this one talks to the disk through handles opened
    /// while the restore was running. When the last one finishes, partmgr may
    /// still hold the layout it cached before the table was rewritten, and the
    /// app's own disk list is whatever the inventory read at the start — so the
    /// restored disk keeps showing its former partitions until something forces
    /// a re-read. Making that its own step also gives the user the one thing
    /// the queue could not say before: that the disk they just wrote to is now
    /// what the program thinks it is.
    RefreshDiskState,
}

/// Sort key giving the fixed execution order documented at the top of this
/// module. The executor brings the disk online between [`Self::ORDER_RAW`] and
/// [`Self::ORDER_FORMAT`].
impl RestoreOp {
    const ORDER_TABLE: u8 = 0;
    const ORDER_RAW: u8 = 1;
    const ORDER_IDENTITY: u8 = 2;
    const ORDER_FORMAT: u8 = 3;
    const ORDER_FILES: u8 = 4;
    const ORDER_EXTEND: u8 = 5;
    const ORDER_LETTER: u8 = 6;
    const ORDER_BOOT: u8 = 7;
    const ORDER_FALLBACK: u8 = 8;
    const ORDER_FIRMWARE: u8 = 9;
    const ORDER_REFRESH: u8 = 10;

    pub fn order_key(&self) -> u8 {
        match self {
            RestoreOp::WritePartitionTable { .. } => Self::ORDER_TABLE,
            RestoreOp::RestorePartition {
                mode: RestoreMode::Sectors,
                ..
            } => Self::ORDER_RAW,
            // First after the sectors: what follows (growing, the boot
            // steps, the fallback loader that reads the restored UUIDs) then
            // works with the new identities, and mounts no clone of a mounted
            // filesystem.
            RestoreOp::ChangeIdentities { .. } => Self::ORDER_IDENTITY,
            RestoreOp::Format { .. } => Self::ORDER_FORMAT,
            RestoreOp::RestorePartition {
                mode: RestoreMode::Files,
                ..
            } => Self::ORDER_FILES,
            RestoreOp::ExtendFilesystem { .. } => Self::ORDER_EXTEND,
            RestoreOp::AssignLetter { .. } => Self::ORDER_LETTER,
            RestoreOp::FinalizeBoot { .. } => Self::ORDER_BOOT,
            // Last: it inspects the ESP as the restore leaves it, so anything
            // that writes to the ESP has to have finished — bcdboot included.
            RestoreOp::EnsureFallbackBootloader { .. } => Self::ORDER_FALLBACK,
            // After everything on the disk is final: the entries name
            // partitions by GUID, so they are only true once the table and the
            // ESP are what they are going to be.
            RestoreOp::RestoreBootEntries => Self::ORDER_FIRMWARE,
            // Truly last: it reports the disk as everything before it left it.
            RestoreOp::RefreshDiskState => Self::ORDER_REFRESH,
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
            RestoreOp::EnsureFallbackBootloader { esp } => Some(*esp),
            RestoreOp::RestoreBootEntries => None,
            RestoreOp::ChangeIdentities { .. } => None,
            RestoreOp::RefreshDiskState => None,
        }
    }

    /// Stable slug for logs, telemetry and the frontend — prose changes,
    /// this does not.
    pub fn kind(&self) -> &'static str {
        match self {
            RestoreOp::WritePartitionTable { .. } => "write_partition_table",
            RestoreOp::Format { .. } => "format",
            RestoreOp::RestorePartition {
                mode: RestoreMode::Sectors,
                ..
            } => "restore_sectors",
            RestoreOp::RestorePartition {
                mode: RestoreMode::Files,
                ..
            } => "restore_files",
            RestoreOp::ExtendFilesystem { .. } => "extend_filesystem",
            RestoreOp::AssignLetter { .. } => "assign_letter",
            RestoreOp::FinalizeBoot { .. } => "finalize_boot",
            RestoreOp::EnsureFallbackBootloader { .. } => "ensure_fallback_bootloader",
            RestoreOp::RestoreBootEntries => "restore_boot_entries",
            RestoreOp::ChangeIdentities { .. } => "change_identities",
            RestoreOp::RefreshDiskState => "refresh_disk_state",
        }
    }

    /// One line describing the operation, in the words the user will read.
    ///
    /// A partition is named by number *and* size. The plan carries no labels or
    /// filesystems — those live in the check context — but a bare "раздел 3"
    /// makes the reader go and look it up, and the size alone is usually enough
    /// to recognise which one it is.
    pub fn title(&self) -> String {
        use crate::units::human_bytes;
        let named = |p: &PartRef| format!("{} ({})", p.index, human_bytes(p.size));
        let title = match self {
            RestoreOp::WritePartitionTable { table } => crate::tr!(
                "restore_plan.title_write_table",
                scheme = match table.scheme {
                    PartitionScheme::Gpt => "GPT",
                    PartitionScheme::Mbr => "MBR",
                },
                count = table.entries.len()
            ),
            RestoreOp::Format {
                part,
                fs,
                label,
                quick,
            } => {
                let label = if label.is_empty() {
                    String::new()
                } else {
                    crate::tr!("restore_plan.title_format_label", label = label).into_owned()
                };
                let mode = if *quick {
                    crate::tr!("restore_plan.title_format_quick")
                } else {
                    crate::tr!("restore_plan.title_format_full")
                };
                crate::tr!(
                    "restore_plan.title_format",
                    part = named(part),
                    fs = fs.as_str(),
                    label = label,
                    mode = mode
                )
            }
            RestoreOp::RestorePartition {
                src_index,
                dst,
                mode,
                ..
            } => match mode {
                RestoreMode::Sectors => crate::tr!(
                    "restore_plan.title_restore_sectors",
                    src = src_index,
                    dst = named(dst)
                ),
                RestoreMode::Files => crate::tr!(
                    "restore_plan.title_restore_files",
                    src = src_index,
                    dst = named(dst)
                ),
            },
            RestoreOp::ExtendFilesystem { part } => {
                crate::tr!("restore_plan.title_extend", part = named(part))
            }
            RestoreOp::AssignLetter { part, letter } => {
                crate::tr!(
                    "restore_plan.title_assign_letter",
                    part = named(part),
                    letter = letter
                )
            }
            RestoreOp::FinalizeBoot { uefi, .. } => crate::tr!(
                "restore_plan.title_finalize_boot",
                firmware = if *uefi { "UEFI" } else { "BIOS" }
            ),
            RestoreOp::EnsureFallbackBootloader { .. } => {
                crate::tr!("restore_plan.title_fallback_bootloader")
            }
            RestoreOp::RestoreBootEntries => crate::tr!("restore_plan.title_restore_boot_entries"),
            RestoreOp::ChangeIdentities { fix_references, .. } => {
                if *fix_references {
                    crate::tr!("restore_plan.title_change_identities_and_fix")
                } else {
                    crate::tr!("restore_plan.title_change_identities")
                }
            }
            RestoreOp::RefreshDiskState => crate::tr!("restore_plan.title_refresh_disks"),
        };
        title.into_owned()
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
                | RestoreOp::RestorePartition {
                    mode: RestoreMode::Files,
                    ..
                }
                | RestoreOp::ExtendFilesystem { .. }
                | RestoreOp::AssignLetter { .. }
                | RestoreOp::FinalizeBoot { .. }
                | RestoreOp::EnsureFallbackBootloader { .. }
                | RestoreOp::ChangeIdentities { .. }
                | RestoreOp::RefreshDiskState
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

    /// Housekeeping the user neither asked for nor needs to watch.
    ///
    /// The fallback loader is the case: it covers a situation that may not
    /// apply (a copy restored onto different hardware, whose NVRAM knows
    /// nothing about it), it usually finds a loader already in place and does
    /// nothing, and a line in the queue saying we are about to touch the
    /// bootloader raises a question the answer to which is "probably nothing".
    /// It runs, and reports — but only when it changed something.
    pub fn is_housekeeping(&self) -> bool {
        // "Обновить состояние дисков" joined it on 2026-07-30. It was worth a
        // line while re-reading the disks meant ten seconds of PowerShell —
        // the user could see what the wait was for. The inventory now answers
        // in twenty milliseconds, so the step is over before the eye reaches
        // it, and a queue entry that only ever flashes is noise.
        matches!(
            self,
            RestoreOp::EnsureFallbackBootloader { .. } | RestoreOp::RefreshDiskState
        )
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
                // Not data on a disk, but the machine's boot menu, and it is
                // replaced rather than added to. It also outlives the disk:
                // wrong entries on a machine whose disks have since moved is a
                // machine that boots to nothing.
                | RestoreOp::RestoreBootEntries
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
    /// Which device `target_disk` was when this plan was made.
    ///
    /// `PhysicalDriveN` is a slot, not a disk: unplug a USB drive and plug
    /// another one in and the number moves to the new device. A plan built
    /// against disk 2, reviewed, and then applied after a reconnect would land
    /// on whatever is disk 2 by then — and a restore's first act is to destroy
    /// what it finds (audit AUD-01, point 5).
    ///
    /// Filled in by the service during the dry run, checked again immediately
    /// before execution. `None` in plans made before this existed, and in a
    /// plan that never went through a preview; then there is nothing to
    /// compare against and the mismatch cannot be detected, only said.
    #[serde(default)]
    pub target_disk_id: Option<TargetDiskId>,
    /// Linux: the target disk's device (`/dev/disk/by-id/…`, `/dev/sdb`).
    /// Linux disks have no stable number, so there `target_disk` names
    /// nothing and this is the target; Windows ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device: Option<String>,
    pub ops: Vec<RestoreOp>,
}

/// Enough of a disk's identity to notice that it is a different disk.
///
/// Three independent facts, because each can be missing or unreliable on its
/// own: a serial number is empty on some virtual and USB-bridged disks, a GPT
/// GUID is absent on an unpartitioned or MBR disk, and a size is shared by
/// every disk of the same model. Together they catch the case that matters —
/// the number now pointing at some other device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetDiskId {
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub disk_guid: String,
    #[serde(default)]
    pub size_bytes: u64,
}

impl TargetDiskId {
    /// Is `now` the same device this describes?
    ///
    /// Only fields that were recorded are compared — a blank one was unknown
    /// then and proves nothing now. When nothing at all was recorded the
    /// answer is "cannot tell", which the caller must not read as "yes".
    pub fn matches(&self, now: &TargetDiskId) -> Option<bool> {
        let mut compared = false;
        let mut same = true;
        if !self.serial.trim().is_empty() && !now.serial.trim().is_empty() {
            compared = true;
            same &= self.serial.trim().eq_ignore_ascii_case(now.serial.trim());
        }
        if !self.disk_guid.trim().is_empty() && !now.disk_guid.trim().is_empty() {
            compared = true;
            same &= self
                .disk_guid
                .trim()
                .eq_ignore_ascii_case(now.disk_guid.trim());
        }
        if self.size_bytes > 0 && now.size_bytes > 0 {
            compared = true;
            same &= self.size_bytes == now.size_bytes;
        }
        compared.then_some(same)
    }
}

impl RestorePlan {
    pub fn new(target_disk: u32, image_path: PathBuf, ops: Vec<RestoreOp>) -> Self {
        Self {
            id: Uuid::new_v4(),
            target_disk,
            image_path,
            verify_after: false,
            target_disk_id: None,
            target_device: None,
            ops,
        }
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
        self.ops
            .iter()
            .any(|op| matches!(op, RestoreOp::WritePartitionTable { .. }))
    }

    /// Index into [`Self::ordered`] of the first operation that destroys data.
    /// Cancelling before it is free; from it onward the button goes dark.
    pub fn point_of_no_return(&self) -> Option<usize> {
        self.ordered().iter().position(|op| op.destroys_data())
    }

    /// Every partition on the target the plan disturbs: those it writes into or
    /// formats, plus — when it rewrites the table — those that exist today and
    /// are absent from the new layout, which is a deletion and just as
    /// destructive.
    ///
    /// On Linux the partitions the boot steps write into count too
    /// ([`Self::boot_step_partitions`]): there they are mounted read-write
    /// through their own nodes after the disk is let go, so a step aimed at
    /// the running system's ESP would change it under the system's feet.
    ///
    /// `current` is the target disk's partition indices as they are now; pass an
    /// empty slice when they are not known and only the written ones count.
    pub fn touched_partitions_with(&self, current: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                RestoreOp::RestorePartition { dst, .. } => Some(dst.index),
                RestoreOp::Format { part, .. } => Some(part.index),
                _ => None,
            })
            .collect();
        if cfg!(target_os = "linux") {
            out.extend(self.boot_step_partitions());
        }
        if let Some(t) = self.table() {
            for &i in current {
                if !t.entries.iter().any(|e| e.index == i) {
                    out.push(i);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Partitions the plan's boot steps write into after the data is copied:
    /// the ESP that receives the fallback loader, and the Windows partition and
    /// ESP that finishing Windows' boot changes.
    pub fn boot_step_partitions(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .ops
            .iter()
            .flat_map(|op| match op {
                RestoreOp::EnsureFallbackBootloader { esp } => vec![esp.index],
                RestoreOp::FinalizeBoot {
                    windows_part, esp, ..
                } => std::iter::once(windows_part.index)
                    .chain(esp.map(|e| e.index))
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Partitions the plan writes into or formats.
    pub fn touched_partitions(&self) -> Vec<u32> {
        self.touched_partitions_with(&[])
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
        PartRef {
            index,
            offset,
            size: 1024 * 1024 * 1024,
        }
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
                RestoreOp::FinalizeBoot {
                    windows_part: part(2, 200),
                    esp: None,
                    uefi: true,
                },
                RestoreOp::AssignLetter {
                    part: part(2, 200),
                    letter: 'D',
                },
                RestoreOp::RestorePartition {
                    src_index: 2,
                    dst: part(2, 200),
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                    reencrypt: false,
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
                    reencrypt: false,
                },
                RestoreOp::WritePartitionTable { table: table() },
            ],
        );

        let keys: Vec<u8> = plan.ordered().iter().map(|op| op.order_key()).collect();
        assert_eq!(keys, vec![0, 1, 3, 4, 6, 7]);
    }

    #[test]
    fn identities_change_right_after_the_sectors_and_before_anything_mounts_them() {
        let change = RestoreOp::ChangeIdentities {
            parts: vec![part(1, 0)],
            fix_references: false,
        };
        let raw = RestoreOp::RestorePartition {
            src_index: 1,
            dst: part(1, 0),
            mode: RestoreMode::Sectors,
            wipe_free_space: false,
            reencrypt: false,
        };
        let grow = RestoreOp::ExtendFilesystem { part: part(1, 0) };
        let fallback = RestoreOp::EnsureFallbackBootloader { esp: part(2, 0) };
        assert!(raw.order_key() < change.order_key());
        assert!(change.order_key() < grow.order_key());
        assert!(change.order_key() < fallback.order_key());
        assert!(change.needs_disk_online() && !change.cancellable() && !change.destroys_data());
        assert_eq!(change.kind(), "change_identities");
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
            reencrypt: false,
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
            reencrypt: false,
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
                RestoreOp::AssignLetter {
                    part: part(3, 3000),
                    letter: 'F',
                },
                RestoreOp::AssignLetter {
                    part: part(1, 1000),
                    letter: 'D',
                },
                RestoreOp::AssignLetter {
                    part: part(2, 2000),
                    letter: 'E',
                },
            ],
        );
        let offsets: Vec<u64> = plan
            .ordered()
            .iter()
            .filter_map(|op| op.part())
            .map(|p| p.offset)
            .collect();
        assert_eq!(offsets, vec![1000, 2000, 3000]);
    }

    #[test]
    fn the_fallback_bootloader_is_written_after_everything_that_touches_the_esp() {
        // It reads the ESP as the restore leaves it — so bcdboot, which writes
        // into that same ESP, has to have run first.
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::EnsureFallbackBootloader { esp: part(1, 100) },
                RestoreOp::FinalizeBoot {
                    windows_part: part(2, 200),
                    esp: None,
                    uefi: true,
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(1, 100),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        );
        let kinds: Vec<&str> = plan.ordered().iter().map(|op| op.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                "restore_sectors",
                "finalize_boot",
                "ensure_fallback_bootloader"
            ]
        );
    }

    /// The queue shows the user what is happening to their disk. Steps that
    /// finish before they can be read are hidden — but only those: a queue
    /// missing the step that is about to overwrite a partition would be a
    /// different kind of wrong.
    #[test]
    fn only_the_instant_steps_are_kept_out_of_the_queue() {
        assert!(RestoreOp::RefreshDiskState.is_housekeeping());
        assert!(RestoreOp::EnsureFallbackBootloader { esp: part(1, 100) }.is_housekeeping());
        for op in [
            RestoreOp::WritePartitionTable { table: table() },
            RestoreOp::FinalizeBoot {
                windows_part: part(1, 100),
                esp: None,
                uefi: true,
            },
            RestoreOp::Format {
                part: part(1, 100),
                fs: FormatFs::Ntfs,
                label: String::new(),
                quick: true,
            },
        ] {
            assert!(!op.is_housekeeping(), "{} belongs in the queue", op.kind());
        }
    }

    #[test]
    fn copying_a_bootloader_destroys_nothing_and_needs_a_mounted_esp() {
        let op = RestoreOp::EnsureFallbackBootloader { esp: part(1, 100) };
        assert!(
            !op.destroys_data(),
            "it only adds a file that was not there"
        );
        assert!(
            op.needs_disk_online(),
            "the ESP has to be a mounted volume to write to"
        );
        assert!(!op.cancellable(), "it takes milliseconds");
    }

    #[test]
    fn point_of_no_return_is_the_table_write() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::AssignLetter {
                    part: part(1, 100),
                    letter: 'D',
                },
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
            vec![RestoreOp::AssignLetter {
                part: part(1, 100),
                letter: 'D',
            }],
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
            reencrypt: false,
        };
        assert!(restore.cancellable());
        assert!(!RestoreOp::WritePartitionTable { table: table() }.cancellable());
        assert!(!RestoreOp::ExtendFilesystem { part: part(1, 0) }.cancellable());
        assert!(!RestoreOp::AssignLetter {
            part: part(1, 0),
            letter: 'D'
        }
        .cancellable());
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
                    reencrypt: false,
                },
                RestoreOp::ExtendFilesystem {
                    part: part(1, 1024 * 1024),
                },
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
        assert!(matches!(
            op,
            RestoreOp::RestorePartition {
                wipe_free_space: false,
                ..
            }
        ));
    }

    // --- Записи загрузки: своя ступень, в самом конце ----------------------

    /// Они называют разделы по GUID, поэтому правдой становятся только когда
    /// разметка и ESP уже такие, какими останутся. И это не хозяйственная
    /// мелочь: строка в очереди обязана быть, иначе подтверждать нечего.
    #[test]
    fn boot_entries_are_written_after_everything_on_the_disk_is_final() {
        let esp = part(1, 1024 * 1024);
        let win = part(3, 300 * 1024 * 1024);
        let plan = RestorePlan {
            id: Uuid::new_v4(),
            target_disk: 1,
            image_path: "x.vhdx".into(),
            verify_after: false,
            target_disk_id: None,
            target_device: None,
            ops: vec![
                RestoreOp::RestoreBootEntries,
                RestoreOp::RefreshDiskState,
                RestoreOp::EnsureFallbackBootloader { esp },
                RestoreOp::FinalizeBoot {
                    windows_part: win,
                    esp: Some(esp),
                    uefi: true,
                },
            ],
        };
        let order: Vec<&str> = plan.ordered().iter().map(|op| op.kind()).collect();
        assert_eq!(
            order,
            vec![
                "finalize_boot",
                "ensure_fallback_bootloader",
                "restore_boot_entries",
                "refresh_disk_state",
            ]
        );
    }

    /// Не данные на диске — но заменяются они целиком, и переживают диск, на
    /// который их восстановили.
    #[test]
    fn writing_the_boot_menu_counts_as_destroying_what_was_there() {
        assert!(RestoreOp::RestoreBootEntries.destroys_data());
        // А обновление состояния дисков — нет, иначе точка невозврата
        // оказалась бы в каждом плане.
        assert!(!RestoreOp::RefreshDiskState.destroys_data());
    }

    /// Пользователь должен увидеть эту строку в очереди — он про неё отвечает.
    #[test]
    fn the_boot_menu_step_is_not_hidden_as_housekeeping() {
        assert!(!RestoreOp::RestoreBootEntries.is_housekeeping());
        assert!(!RestoreOp::RestoreBootEntries.title().is_empty());
    }

    #[test]
    fn boot_steps_count_as_touching_their_partitions_on_linux_only() {
        let plan = RestorePlan::new(
            0,
            PathBuf::from("/x.qcow2"),
            vec![
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(3, 300),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
                RestoreOp::EnsureFallbackBootloader { esp: part(1, 100) },
                RestoreOp::FinalizeBoot {
                    windows_part: part(4, 400),
                    esp: Some(part(1, 100)),
                    uefi: true,
                },
                RestoreOp::RestoreBootEntries,
            ],
        );
        assert_eq!(plan.boot_step_partitions(), vec![1, 4]);
        if cfg!(target_os = "linux") {
            assert_eq!(plan.touched_partitions(), vec![1, 3, 4]);
        } else {
            assert_eq!(plan.touched_partitions(), vec![3]);
        }
    }
}
