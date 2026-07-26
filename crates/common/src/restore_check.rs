//! Dry run: everything that can be checked about a [`RestorePlan`] before a
//! single byte is written (D5).
//!
//! There is no rollback once a restore starts — the old partition table is
//! gone, the old filesystem is gone. The only defence is to find every problem
//! *first*, show the user the full list, and let them decide. So these checks
//! run to completion and report everything, rather than stopping at the first
//! error.
//!
//! The checks here are pure: no disk access, no platform calls, no I/O. That is
//! deliberate — it makes them testable on Linux, where the rest of the restore
//! path cannot even be compiled meaningfully. Whatever needs the machine (is
//! this the system disk? which letters are taken?) is gathered by the caller
//! and passed in through [`CheckContext`].

use serde::{Deserialize, Serialize};

use crate::restore_plan::{
    PartRef, PartitionScheme, PartitionSpec, PartitionTableSpec, RestoreMode, RestoreOp,
    RestorePlan,
};
use crate::types::FileSystem;

/// Conventional partition alignment. Not a hard requirement — a partition at
/// any offset works — but on a 512e or 4Kn disk a misaligned filesystem turns
/// every write into a read-modify-write of the underlying physical sector.
pub const ALIGNMENT: u64 = 1024 * 1024;

/// Headroom over the source's used bytes when restoring file-by-file into a
/// smaller partition (D1). The filesystem's own metadata — MFT, bitmaps,
/// directory indexes — does not come for free, and a target filled to the brim
/// is unusable anyway.
pub const FILE_MODE_HEADROOM: f64 = 1.05;

/// Sectors a GPT reserves at each end of the disk: the header plus a 128-entry
/// array of 128 bytes each = 1 + 32 sectors at 512 B/sector.
const GPT_RESERVED_SECTORS: u64 = 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckLevel {
    /// Purely informational — worth reading, changes nothing.
    Info,
    /// The restore will run, but something will not be as the user expects.
    Warning,
    /// The plan cannot be executed as written.
    Error,
}

/// Stable identifier for a finding, so the frontend can react to a specific
/// problem without matching on translated prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCode {
    /// Two partitions claim the same sectors.
    PartitionsOverlap,
    /// A partition runs past the end of the disk (or into the GPT's reserve).
    PartitionOutOfBounds,
    /// `first_lba > last_lba`, or a zero-length partition.
    PartitionEmpty,
    /// Not aligned to [`ALIGNMENT`].
    Misaligned,
    /// The target is smaller than the source volume it must hold verbatim.
    TargetTooSmallForSectors,
    /// The target cannot hold the source's used data plus headroom.
    TargetTooSmallForFiles,
    /// File-by-file restore of a filesystem Windows cannot read.
    FileModeUnsupportedFs,
    /// The source's used size is unknown, so the fit cannot be verified.
    UsedSizeUnknown,
    /// Extending anything other than NTFS.
    ExtendNotNtfs,
    /// Source and target disagree on logical sector size.
    SectorSizeMismatch,
    /// A drive letter was already taken; a free one was substituted.
    LetterReassigned,
    /// No free drive letters left.
    NoFreeLetter,
    /// The plan references a partition index the image does not contain.
    UnknownSourcePartition,
    /// An operation points at a partition that is not in the table being written.
    OpNotInTable,
    /// An operation's byte offset/size disagrees with the table entry.
    PartRefMismatch,
    /// Two operations write data into the same partition.
    DuplicateRestore,
    /// A partition is both restored sector-by-sector and formatted — the format
    /// would erase what was just written.
    FormatAfterSectorRestore,
    /// UEFI boot finalisation without an EFI System Partition.
    MissingEsp,
    /// The target is the disk the running OS boots from.
    TargetIsSystemDisk,
    /// MBR cannot address the requested geometry.
    MbrLimit,
    /// More than four primary partitions on an MBR disk.
    MbrTooManyPrimaries,
    /// Free space is not being wiped, so old data survives in the gaps.
    FreeSpaceNotWiped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub level: CheckLevel,
    pub code: CheckCode,
    /// User-facing text, Russian — this goes straight into the confirmation
    /// dialog.
    pub message: String,
    /// Partition on the *target* disk the finding concerns, when it concerns one.
    pub part_index: Option<u32>,
}

impl CheckResult {
    fn new(level: CheckLevel, code: CheckCode, part_index: Option<u32>, message: String) -> Self {
        Self { level, code, message, part_index }
    }
}

/// What the image says about one of its partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePartition {
    /// Partition index inside the image.
    pub index: u32,
    /// Full size of the source volume.
    pub size_bytes: u64,
    /// Bytes actually in use. 0 means "unknown" — the figure is unavailable for
    /// volumes without a mounted filesystem.
    pub used_bytes: u64,
    /// Filesystem *inside* the volume. For a LUKS/BitLocker source this is the
    /// decrypted filesystem, because that is what the image holds (D14).
    pub filesystem: FileSystem,
    pub label: String,
}

/// Facts about the machine and the image that the pure checks cannot discover
/// for themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckContext {
    /// Logical sector size the image's layout was captured with.
    pub source_sector_size: u32,
    pub sources: Vec<SourcePartition>,
    /// Drive letters currently in use on this machine.
    pub taken_letters: Vec<char>,
    /// The target disk is the one the running OS boots from — restoring onto it
    /// is refused outright; that job needs a Boot CD (D13).
    pub target_is_system_disk: bool,
}

impl CheckContext {
    fn source(&self, index: u32) -> Option<&SourcePartition> {
        self.sources.iter().find(|s| s.index == index)
    }
}

/// One drive letter the plan asked for and could not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LetterChange {
    pub part_index: u32,
    pub requested: char,
    /// The letter actually assigned, or `None` when none was free.
    pub assigned: Option<char>,
}

/// Resolve drive-letter conflicts in place (D11): a letter already taken by the
/// machine, or claimed twice within the plan, is replaced with the next free
/// one. Returns what changed so the caller can tell the user.
///
/// This mutates the plan rather than only reporting, because a letter conflict
/// is not a reason to make the user go back and edit — the correct behaviour is
/// obvious, and stopping for it would be pedantry.
pub fn resolve_letter_conflicts(plan: &mut RestorePlan, taken: &[char]) -> Vec<LetterChange> {
    let mut used: Vec<char> = taken.iter().map(|c| c.to_ascii_uppercase()).collect();
    let mut changes = Vec::new();

    for op in plan.ops.iter_mut() {
        let RestoreOp::AssignLetter { part, letter } = op else { continue };
        let requested = letter.to_ascii_uppercase();
        if !used.contains(&requested) {
            *letter = requested;
            used.push(requested);
            continue;
        }
        let assigned = next_free_letter(requested, &used);
        changes.push(LetterChange { part_index: part.index, requested, assigned });
        if let Some(c) = assigned {
            *letter = c;
            used.push(c);
        }
    }
    changes
}

/// Next unused letter at or after `from`, wrapping to `D` — A/B are historically
/// floppies and C is the system volume, so wrapping past Z back to D rather than
/// to A avoids handing out letters that half of Windows still treats specially.
fn next_free_letter(from: char, used: &[char]) -> Option<char> {
    let start = from.to_ascii_uppercase();
    let order = ('A'..='Z')
        .filter(|c| *c >= start)
        .chain('D'..start.min('Z'))
        .collect::<Vec<_>>();
    order.into_iter().find(|c| !used.contains(c))
}

/// Run every check against `plan`. The result is a full list, deliberately not
/// short-circuited: the user should see all the problems at once, not discover
/// them one restore attempt at a time.
pub fn check_plan(plan: &RestorePlan, ctx: &CheckContext) -> Vec<CheckResult> {
    let mut out = Vec::new();

    if ctx.target_is_system_disk {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::TargetIsSystemDisk,
            None,
            format!(
                "Диск {} — системный: с него загружена работающая Windows, и переписать \
                 его разметку на ходу невозможно. Загрузитесь с Boot CD и повторите.",
                plan.target_disk
            ),
        ));
    }

    let table = plan.table();
    if let Some(t) = table {
        check_table(t, &mut out);
        check_sector_size(t, ctx, &mut out);
    }
    check_ops(plan, table, ctx, &mut out);

    out
}

/// Geometry of the table itself: bounds, overlaps, alignment, MBR limits.
fn check_table(t: &PartitionTableSpec, out: &mut Vec<CheckResult>) {
    let sector = t.sector_size.max(512) as u64;
    let disk_sectors = t.disk_size_bytes / sector;

    // Last sector a partition may occupy. GPT keeps a mirror of its header and
    // entry array at the very end of the disk; a partition reaching into it
    // would corrupt the backup table.
    let last_usable = match t.scheme {
        PartitionScheme::Gpt => disk_sectors.saturating_sub(GPT_RESERVED_SECTORS + 1),
        PartitionScheme::Mbr => disk_sectors.saturating_sub(1),
    };
    let first_usable = match t.scheme {
        PartitionScheme::Gpt => GPT_RESERVED_SECTORS + 1,
        PartitionScheme::Mbr => 1,
    };

    for e in &t.entries {
        if e.last_lba < e.first_lba {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::PartitionEmpty,
                Some(e.index),
                format!(
                    "Раздел {}: конец ({}) раньше начала ({}) — нулевая или отрицательная длина.",
                    e.index, e.last_lba, e.first_lba
                ),
            ));
            continue;
        }
        if e.first_lba < first_usable || e.last_lba > last_usable {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::PartitionOutOfBounds,
                Some(e.index),
                format!(
                    "Раздел {} ({}–{}) выходит за пределы, доступные на диске ({}–{} сект.).",
                    e.index, e.first_lba, e.last_lba, first_usable, last_usable
                ),
            ));
        }
        let offset = e.offset_bytes(t.sector_size);
        if offset % ALIGNMENT != 0 {
            out.push(CheckResult::new(
                CheckLevel::Warning,
                CheckCode::Misaligned,
                Some(e.index),
                format!(
                    "Раздел {} начинается со смещения {} Б — не кратно 1 МиБ. На дисках 512e/4Kn \
                     это стоит производительности: каждая запись превращается в чтение-правку-запись \
                     физического сектора. Так была устроена исходная разметка, поэтому ошибкой это \
                     не считаем.",
                    e.index, offset
                ),
            ));
        }
    }

    // Overlaps. Compared pairwise on a sorted copy: only neighbours can touch.
    let mut sorted: Vec<&PartitionSpec> = t.entries.iter().collect();
    sorted.sort_by_key(|e| e.first_lba);
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.end_lba_exclusive() > b.first_lba {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::PartitionsOverlap,
                Some(b.index),
                format!(
                    "Разделы {} ({}–{}) и {} ({}–{}) перекрываются.",
                    a.index, a.first_lba, a.last_lba, b.index, b.first_lba, b.last_lba
                ),
            ));
        }
    }

    if t.scheme == PartitionScheme::Mbr {
        check_mbr_limits(t, disk_sectors, out);
    }
}

/// MBR-specific limits: 32-bit LBA fields and four primary slots.
fn check_mbr_limits(t: &PartitionTableSpec, disk_sectors: u64, out: &mut Vec<CheckResult>) {
    const MAX_LBA: u64 = u32::MAX as u64;

    for e in &t.entries {
        if e.end_lba_exclusive() > MAX_LBA {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::MbrLimit,
                Some(e.index),
                format!(
                    "Раздел {} заканчивается на секторе {} — MBR хранит адреса в 32 битах и \
                     дальше {} сект. (~2 ТиБ при секторе 512 Б) не адресует. Нужен GPT.",
                    e.index, e.last_lba, MAX_LBA
                ),
            ));
        }
    }
    if disk_sectors > MAX_LBA {
        out.push(CheckResult::new(
            CheckLevel::Warning,
            CheckCode::MbrLimit,
            None,
            format!(
                "Диск больше того, что адресует MBR ({} сект.) — хвост останется недоступен.",
                MAX_LBA
            ),
        ));
    }

    let primaries = t
        .entries
        .iter()
        .filter(|e| {
            matches!(&e.kind, crate::restore_plan::PartitionKind::Mbr { logical, .. } if !logical)
        })
        .count();
    if primaries > 4 {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::MbrTooManyPrimaries,
            None,
            format!(
                "Основных разделов {} — в MBR их не может быть больше четырёх. \
                 Лишние надо сделать логическими.",
                primaries
            ),
        ));
    }
}

/// A layout captured at one sector size cannot be reproduced at another: every
/// stored LBA would address a different byte, and the filesystem's own boot
/// sector records the size it was made for.
fn check_sector_size(t: &PartitionTableSpec, ctx: &CheckContext, out: &mut Vec<CheckResult>) {
    if ctx.source_sector_size == 0 || ctx.source_sector_size == t.sector_size {
        return;
    }
    out.push(CheckResult::new(
        CheckLevel::Error,
        CheckCode::SectorSizeMismatch,
        None,
        format!(
            "Копия снята с диска с логическим сектором {} Б, у целевого — {} Б. Разметку \
             нельзя перенести: сохранённые адреса секторов указывали бы на другие байты.",
            ctx.source_sector_size, t.sector_size
        ),
    ));
}

/// Per-operation checks: does the target fit, is the mode possible, does the
/// operation agree with the table.
fn check_ops(
    plan: &RestorePlan,
    table: Option<&PartitionTableSpec>,
    ctx: &CheckContext,
    out: &mut Vec<CheckResult>,
) {
    let mut restored: Vec<u32> = Vec::new();
    let mut sector_restored: Vec<u32> = Vec::new();
    let mut formatted: Vec<(u32, crate::restore_plan::FormatFs)> = Vec::new();

    for op in &plan.ops {
        if let Some(part) = op.part() {
            check_part_ref(part, table, out);
        }
        match op {
            RestoreOp::RestorePartition { src_index, dst, mode, wipe_free_space } => {
                if restored.contains(&dst.index) {
                    out.push(CheckResult::new(
                        CheckLevel::Error,
                        CheckCode::DuplicateRestore,
                        Some(dst.index),
                        format!("В раздел {} пишут данные две операции сразу.", dst.index),
                    ));
                }
                restored.push(dst.index);
                if *mode == RestoreMode::Sectors {
                    sector_restored.push(dst.index);
                    if !wipe_free_space {
                        out.push(CheckResult::new(
                            CheckLevel::Info,
                            CheckCode::FreeSpaceNotWiped,
                            Some(dst.index),
                            format!(
                                "Раздел {}: копируются только занятые блоки, поэтому в промежутках \
                                 физически останутся прежние данные. Файловая система их не увидит; \
                                 чтобы стереть — включите затирание свободного места.",
                                dst.index
                            ),
                        ));
                    }
                }

                let Some(src) = ctx.source(*src_index) else {
                    out.push(CheckResult::new(
                        CheckLevel::Error,
                        CheckCode::UnknownSourcePartition,
                        Some(dst.index),
                        format!("В копии нет раздела с номером {}.", src_index),
                    ));
                    continue;
                };
                check_fit(src, dst, *mode, out);
            }
            RestoreOp::Format { part, fs, .. } => formatted.push((part.index, *fs)),
            RestoreOp::ExtendFilesystem { part } => {
                check_extend(plan, ctx, part, out);
            }
            RestoreOp::FinalizeBoot { windows_part, esp, uefi } => {
                if *uefi && esp.is_none() {
                    out.push(CheckResult::new(
                        CheckLevel::Error,
                        CheckCode::MissingEsp,
                        Some(windows_part.index),
                        "Для загрузки UEFI нужен раздел EFI (ESP), а в плане его нет."
                            .to_string(),
                    ));
                }
            }
            RestoreOp::WritePartitionTable { .. } | RestoreOp::AssignLetter { .. } => {}
        }
    }

    // Formatting runs after the raw writes, so a partition that is both
    // sector-restored and formatted loses everything that was restored.
    for (index, _) in &formatted {
        if sector_restored.contains(index) {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::FormatAfterSectorRestore,
                Some(*index),
                format!(
                    "Раздел {} восстанавливается посекторно и тут же форматируется — \
                     форматирование сотрёт всё, что было записано. Посекторное восстановление \
                     приносит готовую файловую систему, форматировать не нужно.",
                    index
                ),
            ));
        }
    }
}

/// An operation's byte offset and size must match the table entry it names.
/// They are stored twice — the table addresses sectors, the operation addresses
/// bytes for `part:<disk>:<offset>:<len>` — and this is where they are made to
/// agree.
fn check_part_ref(part: PartRef, table: Option<&PartitionTableSpec>, out: &mut Vec<CheckResult>) {
    let Some(t) = table else { return };
    let Some(e) = t.entries.iter().find(|e| e.index == part.index) else {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::OpNotInTable,
            Some(part.index),
            format!("Операция ссылается на раздел {}, которого нет в новой разметке.", part.index),
        ));
        return;
    };
    let (offset, size) = (e.offset_bytes(t.sector_size), e.size_bytes(t.sector_size));
    if offset != part.offset || size != part.size {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::PartRefMismatch,
            Some(part.index),
            format!(
                "Раздел {}: операция адресует {}..+{} Б, а в разметке он лежит по {}..+{} Б.",
                part.index, part.offset, part.size, offset, size
            ),
        ));
    }
}

/// Does the source fit into the target, in the requested mode?
fn check_fit(src: &SourcePartition, dst: &PartRef, mode: RestoreMode, out: &mut Vec<CheckResult>) {
    match mode {
        RestoreMode::Sectors => {
            if dst.size < src.size_bytes {
                out.push(CheckResult::new(
                    CheckLevel::Error,
                    CheckCode::TargetTooSmallForSectors,
                    Some(dst.index),
                    format!(
                        "Раздел {} меньше исходного тома «{}»: {} против {}. Посекторное \
                         восстановление переносит том целиком вместе с его файловой системой, \
                         поэтому приёмник не может быть меньше. Уместить данные можно только \
                         пофайлово.",
                        dst.index,
                        src.label,
                        human(dst.size),
                        human(src.size_bytes)
                    ),
                ));
            }
        }
        RestoreMode::Files => {
            if !src.filesystem.is_windows_family() {
                out.push(CheckResult::new(
                    CheckLevel::Error,
                    CheckCode::FileModeUnsupportedFs,
                    Some(dst.index),
                    format!(
                        "Том «{}» размечен как {} — Windows не умеет его читать, значит и \
                         скопировать пофайлово нельзя. Такой раздел переносится только \
                         посекторно, в приёмник не меньше исходного.",
                        src.label,
                        src.filesystem.label()
                    ),
                ));
                return;
            }
            if src.used_bytes == 0 {
                out.push(CheckResult::new(
                    CheckLevel::Warning,
                    CheckCode::UsedSizeUnknown,
                    Some(dst.index),
                    format!(
                        "Для тома «{}» неизвестен объём занятых данных, поэтому проверить, \
                         влезут ли они в раздел {} ({}), заранее нельзя.",
                        src.label,
                        dst.index,
                        human(dst.size)
                    ),
                ));
                return;
            }
            let needed = (src.used_bytes as f64 * FILE_MODE_HEADROOM) as u64;
            if dst.size < needed {
                out.push(CheckResult::new(
                    CheckLevel::Error,
                    CheckCode::TargetTooSmallForFiles,
                    Some(dst.index),
                    format!(
                        "В раздел {} ({}) не влезут данные тома «{}»: занято {}, а с запасом \
                         на служебные структуры файловой системы нужно не меньше {}.",
                        dst.index,
                        human(dst.size),
                        src.label,
                        human(src.used_bytes),
                        human(needed)
                    ),
                ));
            }
        }
    }
}

/// `FSCTL_EXTEND_VOLUME` only exists for NTFS. Work out which filesystem will
/// actually be on the partition by the time the extend runs: the one the image
/// brings for a sector restore, the one [`RestoreOp::Format`] creates otherwise.
fn check_extend(
    plan: &RestorePlan,
    ctx: &CheckContext,
    part: &PartRef,
    out: &mut Vec<CheckResult>,
) {
    let from_image = plan.ops.iter().find_map(|op| match op {
        RestoreOp::RestorePartition { src_index, dst, mode: RestoreMode::Sectors, .. }
            if dst.index == part.index =>
        {
            ctx.source(*src_index).map(|s| s.filesystem)
        }
        _ => None,
    });
    let from_format = plan.ops.iter().find_map(|op| match op {
        RestoreOp::Format { part: p, fs, .. } if p.index == part.index => Some(*fs),
        _ => None,
    });

    let fs_name = match (from_image, from_format) {
        (Some(FileSystem::Ntfs), _) => return,
        (_, Some(crate::restore_plan::FormatFs::Ntfs)) => return,
        (Some(fs), _) => fs.label().to_string(),
        (None, Some(fs)) => fs.as_str().to_string(),
        (None, None) => {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::ExtendNotNtfs,
                Some(part.index),
                format!(
                    "Расширять раздел {} нечего: в него ничего не восстанавливается и он не \
                     форматируется, значит файловой системы там не будет.",
                    part.index
                ),
            ));
            return;
        }
    };

    out.push(CheckResult::new(
        CheckLevel::Error,
        CheckCode::ExtendNotNtfs,
        Some(part.index),
        format!(
            "Раздел {} размечен как {} — автоматически расширить Windows умеет только NTFS. \
             Свободный хвост останется неразмеченным внутри раздела; его придётся дорастить \
             средствами самой системы.",
            part.index, fs_name
        ),
    ));
}

/// Binary units, matching what the interface shows.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ", "ПиБ"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore_plan::{FormatFs, PartitionKind};

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn entry(index: u32, first_mib: u64, size_mib: u64) -> PartitionSpec {
        let first_lba = first_mib * MIB / 512;
        PartitionSpec {
            index,
            first_lba,
            last_lba: first_lba + size_mib * MIB / 512 - 1,
            kind: PartitionKind::Gpt {
                type_guid: crate::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                part_guid: String::new(),
                attributes: 0,
                name: format!("p{index}"),
            },
        }
    }

    fn table(entries: Vec<PartitionSpec>) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: 512 * GIB,
            disk_id: String::new(),
            entries,
        }
    }

    fn part_ref(t: &PartitionTableSpec, index: u32) -> PartRef {
        let e = t.entries.iter().find(|e| e.index == index).unwrap();
        PartRef {
            index,
            offset: e.offset_bytes(t.sector_size),
            size: e.size_bytes(t.sector_size),
        }
    }

    fn ctx(sources: Vec<SourcePartition>) -> CheckContext {
        CheckContext {
            source_sector_size: 512,
            sources,
            taken_letters: vec![],
            target_is_system_disk: false,
        }
    }

    fn source(index: u32, size: u64, used: u64, fs: FileSystem) -> SourcePartition {
        SourcePartition { index, size_bytes: size, used_bytes: used, filesystem: fs, label: "Data".into() }
    }

    fn plan_with(table: PartitionTableSpec, mut ops: Vec<RestoreOp>) -> RestorePlan {
        ops.insert(0, RestoreOp::WritePartitionTable { table });
        RestorePlan::new(1, "/tmp/img.vhdx".into(), ops)
    }

    fn codes(results: &[CheckResult]) -> Vec<CheckCode> {
        results.iter().map(|r| r.code).collect()
    }

    fn errors(results: &[CheckResult]) -> Vec<CheckCode> {
        results.iter().filter(|r| r.level == CheckLevel::Error).map(|r| r.code).collect()
    }

    #[test]
    fn a_sane_plan_produces_no_errors() {
        let t = table(vec![entry(1, 1, 100), entry(2, 101, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 40 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).is_empty(), "{results:#?}");
    }

    #[test]
    fn overlapping_partitions_are_an_error() {
        // Second starts before the first ends.
        let t = table(vec![entry(1, 1, 100), entry(2, 50, 100)]);
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::PartitionsOverlap), "{results:#?}");
    }

    #[test]
    fn a_partition_reaching_into_the_backup_gpt_is_an_error() {
        let mut t = table(vec![entry(1, 1, 100)]);
        let disk_sectors = t.disk_size_bytes / 512;
        t.entries[0].last_lba = disk_sectors - 2; // inside the 33-sector mirror
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::PartitionOutOfBounds), "{results:#?}");
    }

    #[test]
    fn misalignment_warns_but_does_not_block() {
        // LBA 34 — where a GPT's first partition can legally start, and where
        // the user's own source disk actually put its ESP.
        let mut t = table(vec![entry(1, 1, 100)]);
        t.entries[0].first_lba = 34;
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(codes(&results).contains(&CheckCode::Misaligned));
        assert!(errors(&results).is_empty(), "{results:#?}");
    }

    #[test]
    fn sector_restore_into_a_smaller_target_is_refused() {
        let t = table(vec![entry(1, 1, 50)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).contains(&CheckCode::TargetTooSmallForSectors), "{results:#?}");
    }

    #[test]
    fn file_restore_into_a_smaller_target_is_allowed_when_the_data_fits() {
        let t = table(vec![entry(1, 1, 50)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::Format {
                    part: p1,
                    fs: FormatFs::Ntfs,
                    label: "Data".into(),
                    quick: true,
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                },
            ],
        );
        // 40 MiB used * 1.05 = 42 MiB, fits in 50 MiB.
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 40 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).is_empty(), "{results:#?}");
    }

    #[test]
    fn file_restore_refuses_when_headroom_does_not_fit() {
        let t = table(vec![entry(1, 1, 50)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Files,
                wipe_free_space: false,
            }],
        );
        // 49 MiB * 1.05 = 51.45 MiB > 50 MiB.
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 49 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).contains(&CheckCode::TargetTooSmallForFiles), "{results:#?}");
    }

    #[test]
    fn file_restore_of_a_linux_filesystem_is_refused() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Files,
                wipe_free_space: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Btrfs)]));
        assert!(errors(&results).contains(&CheckCode::FileModeUnsupportedFs), "{results:#?}");
    }

    #[test]
    fn extending_a_non_ntfs_volume_is_refused() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                },
                RestoreOp::ExtendFilesystem { part: p1 },
            ],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ext4)]));
        assert!(errors(&results).contains(&CheckCode::ExtendNotNtfs), "{results:#?}");
    }

    #[test]
    fn extending_ntfs_from_the_image_is_fine() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                },
                RestoreOp::ExtendFilesystem { part: p1 },
            ],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).is_empty(), "{results:#?}");
    }

    #[test]
    fn formatting_a_sector_restored_partition_is_refused() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                },
                RestoreOp::Format { part: p1, fs: FormatFs::Ntfs, label: String::new(), quick: true },
            ],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).contains(&CheckCode::FormatAfterSectorRestore), "{results:#?}");
    }

    #[test]
    fn a_sector_size_mismatch_is_refused() {
        let t = table(vec![entry(1, 1, 100)]);
        let mut c = ctx(vec![]);
        c.source_sector_size = 4096;
        let results = check_plan(&plan_with(t, vec![]), &c);
        assert!(errors(&results).contains(&CheckCode::SectorSizeMismatch), "{results:#?}");
    }

    #[test]
    fn an_operation_disagreeing_with_the_table_is_caught() {
        let t = table(vec![entry(1, 1, 100)]);
        let mut p1 = part_ref(&t, 1);
        p1.size += 1; // designer and plan drifted apart
        let plan = plan_with(t, vec![RestoreOp::AssignLetter { part: p1, letter: 'D' }]);
        let results = check_plan(&plan, &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::PartRefMismatch), "{results:#?}");
    }

    #[test]
    fn an_operation_on_a_partition_outside_the_table_is_caught() {
        let t = table(vec![entry(1, 1, 100)]);
        let ghost = PartRef { index: 9, offset: 0, size: MIB };
        let plan = plan_with(t, vec![RestoreOp::AssignLetter { part: ghost, letter: 'D' }]);
        let results = check_plan(&plan, &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::OpNotInTable), "{results:#?}");
    }

    #[test]
    fn restoring_onto_the_system_disk_is_refused() {
        let t = table(vec![entry(1, 1, 100)]);
        let mut c = ctx(vec![]);
        c.target_is_system_disk = true;
        let results = check_plan(&plan_with(t, vec![]), &c);
        assert!(errors(&results).contains(&CheckCode::TargetIsSystemDisk), "{results:#?}");
    }

    #[test]
    fn uefi_boot_without_an_esp_is_refused() {
        let t = table(vec![entry(1, 1, 100)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::FinalizeBoot { windows_part: p1, esp: None, uefi: true }],
        );
        let results = check_plan(&plan, &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::MissingEsp), "{results:#?}");
    }

    #[test]
    fn mbr_refuses_more_than_four_primaries() {
        let mbr = |index: u32, first_mib: u64| PartitionSpec {
            index,
            first_lba: first_mib * MIB / 512,
            last_lba: (first_mib + 10) * MIB / 512 - 1,
            kind: PartitionKind::Mbr { type_byte: 0x07, bootable: false, logical: false },
        };
        let t = PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: 100 * GIB,
            disk_id: "1A2B3C4D".into(),
            entries: vec![mbr(1, 1), mbr(2, 11), mbr(3, 21), mbr(4, 31), mbr(5, 41)],
        };
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::MbrTooManyPrimaries), "{results:#?}");
    }

    #[test]
    fn a_taken_letter_is_swapped_for_the_next_free_one() {
        let t = table(vec![entry(1, 1, 100)]);
        let p1 = part_ref(&t, 1);
        let mut plan = plan_with(t, vec![RestoreOp::AssignLetter { part: p1, letter: 'D' }]);
        let changes = resolve_letter_conflicts(&mut plan, &['C', 'D', 'E']);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].requested, 'D');
        assert_eq!(changes[0].assigned, Some('F'));
        assert!(matches!(plan.ops[1], RestoreOp::AssignLetter { letter: 'F', .. }));
    }

    #[test]
    fn two_operations_asking_for_the_same_letter_do_not_both_get_it() {
        let t = table(vec![entry(1, 1, 100), entry(2, 101, 100)]);
        let (p1, p2) = (part_ref(&t, 1), part_ref(&t, 2));
        let mut plan = plan_with(
            t,
            vec![
                RestoreOp::AssignLetter { part: p1, letter: 'D' },
                RestoreOp::AssignLetter { part: p2, letter: 'D' },
            ],
        );
        let changes = resolve_letter_conflicts(&mut plan, &[]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].assigned, Some('E'));
    }

    #[test]
    fn a_free_letter_is_left_alone() {
        let t = table(vec![entry(1, 1, 100)]);
        let p1 = part_ref(&t, 1);
        let mut plan = plan_with(t, vec![RestoreOp::AssignLetter { part: p1, letter: 'X' }]);
        assert!(resolve_letter_conflicts(&mut plan, &['C']).is_empty());
        assert!(matches!(plan.ops[1], RestoreOp::AssignLetter { letter: 'X', .. }));
    }

    #[test]
    fn letters_wrap_past_z_back_to_d() {
        let t = table(vec![entry(1, 1, 100)]);
        let p1 = part_ref(&t, 1);
        let mut plan = plan_with(t, vec![RestoreOp::AssignLetter { part: p1, letter: 'Z' }]);
        // Everything from Z upward is gone; the search wraps and lands on E.
        let taken: Vec<char> = "ABCDZ".chars().collect();
        let changes = resolve_letter_conflicts(&mut plan, &taken);
        assert_eq!(changes[0].assigned, Some('E'));
    }

    #[test]
    fn no_free_letter_is_reported_rather_than_guessed() {
        let t = table(vec![entry(1, 1, 100)]);
        let p1 = part_ref(&t, 1);
        let mut plan = plan_with(t, vec![RestoreOp::AssignLetter { part: p1, letter: 'D' }]);
        let taken: Vec<char> = ('A'..='Z').collect();
        let changes = resolve_letter_conflicts(&mut plan, &taken);
        assert_eq!(changes[0].assigned, None);
    }

    #[test]
    fn not_wiping_free_space_is_stated_plainly() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        let info: Vec<_> = results.iter().filter(|r| r.code == CheckCode::FreeSpaceNotWiped).collect();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].level, CheckLevel::Info);
    }
}
