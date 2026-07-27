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
use crate::units::human_bytes as human;

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
    /// A partition the running system is standing on would be overwritten.
    TargetIsLiveSystemPartition,
    /// The plan changes the layout of the disk the running OS boots from.
    LiveSystemDiskLayout,
    /// MBR cannot address the requested geometry.
    MbrLimit,
    /// More than four primary partitions on an MBR disk.
    MbrTooManyPrimaries,
    /// Free space is not being wiped, so old data survives in the gaps.
    FreeSpaceNotWiped,
    /// A file-by-file restore into a partition nothing formats.
    FileModeNeedsFormat,
    /// The filesystem being created cannot hold what the source holds.
    FormatFsTooLimited,
    /// The source volume was an encrypted container, and the copy holds it
    /// decrypted — the restore produces a plain filesystem.
    EncryptionNotRestored,
    /// Putting the encryption back was asked for, and the copy has no header
    /// to do it with.
    NoLuksHeader,
    /// Putting the encryption back was asked for in file mode, where it cannot
    /// work.
    ReencryptNeedsSectors,
    /// The destination partition currently holds a filesystem with data.
    DestinationWillBeErased,
    /// The destination changes families — a Windows partition becomes a Linux
    /// one or the other way round.
    PartitionChangesFamily,
    /// A filesystem was chosen when creating the partition, and the restore
    /// brings a different one along.
    FormatChoiceIgnored,
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
    /// The source volume lived inside a LUKS or BitLocker container and the
    /// backup captured it **decrypted**. The restore therefore produces a plain
    /// filesystem where an encrypted container used to be (D14).
    #[serde(default)]
    pub encrypted_at_source: bool,
    /// The copy carries the container's own header, so the restore can put the
    /// encryption back exactly as it was (D19). False for copies taken before
    /// headers were kept — those can only be restored decrypted.
    #[serde(default)]
    pub luks_header_available: bool,
}

/// A filesystem the user picked in the "create partition" dialog.
///
/// It is not part of the plan when the partition also receives a sector
/// restore — formatting there would erase what was just written, so no format
/// operation is generated. The choice is passed here anyway, precisely so the
/// checks can point out that it will not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedFormat {
    pub index: u32,
    pub fs: crate::restore_plan::FormatFs,
}

/// What a destination partition holds **right now**, before the restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPartition {
    pub index: u32,
    pub size_bytes: u64,
    /// Bytes in use. 0 when unknown or empty.
    pub used_bytes: u64,
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
    /// The target disk is the one the running OS boots from. On its own this is
    /// no longer a veto — a spare partition there is an ordinary target — but it
    /// does change how the layout has to be applied.
    pub target_is_system_disk: bool,
    /// Partitions on the target disk the running system is standing on: the
    /// volume Windows runs from and the ESP it booted through. Overwriting one
    /// of those stops the machine mid-write, so it is refused. Empty on boot
    /// media, and empty when nothing could be determined.
    #[serde(default)]
    pub protected_parts: Vec<u32>,
    /// The target disk's partitions as they are now, so the checks can say what
    /// is about to be overwritten. Empty when the caller did not supply them.
    #[serde(default)]
    pub targets: Vec<TargetPartition>,
    /// Filesystems chosen in the designer's "create partition" dialog, so the
    /// checks can say when one of them will not be created after all.
    #[serde(default)]
    pub requested_formats: Vec<RequestedFormat>,
}

impl CheckContext {
    /// A partition named the way a person would recognise it: the number, then
    /// what it actually is.
    ///
    /// "Раздел 3" alone asks the reader to go and look it up on the map — and a
    /// message about destroying data is the worst possible place to make
    /// someone do that. `size` and `filesystem` come from whatever is known;
    /// when nothing is, the bare number is still returned rather than a lie.
    fn describe_target(&self, index: u32, table: Option<&PartitionTableSpec>) -> String {
        if let Some(t) = self.target(index) {
            return describe(index, &t.label, t.size_bytes, Some(t.filesystem));
        }
        // A partition the plan creates does not exist yet; the table knows how
        // big it will be.
        let size = table
            .and_then(|t| t.entries.iter().find(|e| e.index == index).map(|e| e.size_bytes(t.sector_size)))
            .unwrap_or(0);
        describe(index, "", size, None)
    }


    fn source(&self, index: u32) -> Option<&SourcePartition> {
        self.sources.iter().find(|s| s.index == index)
    }

    fn target(&self, index: u32) -> Option<&TargetPartition> {
        self.targets.iter().find(|t| t.index == index)
    }

    fn requested_format(&self, index: u32) -> Option<crate::restore_plan::FormatFs> {
        self.requested_formats.iter().find(|r| r.index == index).map(|r| r.fs)
    }
}

/// `3 («Данные», 128.00 ГиБ, NTFS)` — as much of that as is known.
///
/// "Раздел 3" on its own asks the reader to go and look it up on the map, and a
/// message about destroying data is the worst possible place to make someone do
/// that.
fn describe(index: u32, label: &str, size: u64, fs: Option<FileSystem>) -> String {
    let mut bits = Vec::new();
    if !label.trim().is_empty() {
        bits.push(format!("«{}»", label.trim()));
    }
    if size > 0 {
        bits.push(human(size));
    }
    if let Some(fs) = fs.filter(|f| *f != FileSystem::Unknown) {
        bits.push(fs.label().to_string());
    }
    if bits.is_empty() {
        index.to_string()
    } else {
        format!("{index} ({})", bits.join(", "))
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

    let table = plan.table();
    check_live_system(plan, table, ctx, &mut out);
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

    let logical = |e: &PartitionSpec| {
        matches!(&e.kind, crate::restore_plan::PartitionKind::Mbr { logical, .. } if *logical)
    };
    let primaries = t.entries.iter().filter(|e| !logical(e)).count();
    let has_logicals = t.entries.iter().any(logical);
    // Logical partitions live inside an extended partition, and the extended
    // partition is itself one of the four entries — so it costs a slot.
    let budget = if has_logicals { 3 } else { 4 };
    if primaries > budget {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::MbrTooManyPrimaries,
            None,
            if has_logicals {
                format!(
                    "Основных разделов {primaries}, и ещё один слот из четырёх занимает \
                     расширенный раздел, внутри которого лежат логические. Основных остаётся \
                     не больше трёх."
                )
            } else {
                format!(
                    "Основных разделов {primaries} — в MBR их не может быть больше четырёх. \
                     Лишние надо сделать логическими."
                )
            },
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
    // Partitions receiving files, and the filesystem their source held.
    let mut file_targets: Vec<(u32, FileSystem)> = Vec::new();

    for op in &plan.ops {
        if let Some(part) = op.part() {
            check_part_ref(part, table, ctx, out);
        }
        match op {
            RestoreOp::RestorePartition { src_index, dst, mode, wipe_free_space, reencrypt } => {
                let dst_txt = ctx.describe_target(dst.index, table);
                if restored.contains(&dst.index) {
                    out.push(CheckResult::new(
                        CheckLevel::Error,
                        CheckCode::DuplicateRestore,
                        Some(dst.index),
                        format!("В раздел {dst_txt} пишут данные две операции сразу."),
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
                                "Раздел {dst_txt}: копируются только занятые блоки, поэтому в \
                                 промежутках физически останутся прежние данные. Файловая система \
                                 их не увидит; чтобы стереть — включите затирание свободного места."
                            ),
                        ));
                    }
                }

                let Some(src) = ctx.source(*src_index) else {
                    out.push(CheckResult::new(
                        CheckLevel::Error,
                        CheckCode::UnknownSourcePartition,
                        Some(dst.index),
                        format!("В копии нет раздела с номером {src_index}."),
                    ));
                    continue;
                };
                check_fit(src, dst, &dst_txt, *mode, out);
                check_encryption(src, dst, dst.index, &dst_txt, *mode, *reencrypt, out);
                check_destination(ctx, src, dst, &dst_txt, out);
                check_format_choice(ctx, src, dst.index, &dst_txt, *mode, out);
                if *mode == RestoreMode::Files {
                    file_targets.push((dst.index, src.filesystem));
                }
            }
            RestoreOp::Format { part, fs, .. } => formatted.push((part.index, *fs)),
            RestoreOp::ExtendFilesystem { part } => {
                check_extend(plan, ctx, part, &ctx.describe_target(part.index, table), out);
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

    // A file-by-file restore writes onto a filesystem. If nothing creates one,
    // the files land on whatever was there before — or on nothing at all.
    for (index, src_fs) in &file_targets {
        match formatted.iter().find(|(i, _)| i == index) {
            None => out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::FileModeNeedsFormat,
                Some(*index),
                format!(
                    "Раздел {} восстанавливается пофайлово, но в плане нет его \
                     форматирования — копировать файлы некуда.",
                    ctx.describe_target(*index, table)
                ),
            )),
            Some((_, fs)) => {
                check_format_capacity(*index, &ctx.describe_target(*index, table), *src_fs, *fs, out)
            }
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
                    ctx.describe_target(*index, table)
                ),
            ));
        }
    }
}

/// An operation's byte offset and size must match the table entry it names.
/// They are stored twice — the table addresses sectors, the operation addresses
/// bytes for `part:<disk>:<offset>:<len>` — and this is where they are made to
/// agree.
fn check_part_ref(
    part: PartRef,
    table: Option<&PartitionTableSpec>,
    ctx: &CheckContext,
    out: &mut Vec<CheckResult>,
) {
    let Some(t) = table else { return };
    let Some(e) = t.entries.iter().find(|e| e.index == part.index) else {
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::OpNotInTable,
            Some(part.index),
            format!(
                "Операция ссылается на раздел {}, которого нет в новой разметке.",
                ctx.describe_target(part.index, table)
            ),
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
                ctx.describe_target(part.index, table),
                part.offset,
                part.size,
                offset,
                size
            ),
        ));
    }
}

/// Does the source fit into the target, in the requested mode?
fn check_fit(
    src: &SourcePartition,
    dst: &PartRef,
    dst_txt: &str,
    mode: RestoreMode,
    out: &mut Vec<CheckResult>,
) {
    match mode {
        RestoreMode::Sectors => {
            if dst.size < src.size_bytes {
                out.push(CheckResult::new(
                    CheckLevel::Error,
                    CheckCode::TargetTooSmallForSectors,
                    Some(dst.index),
                    format!(
                        "Раздел {dst_txt} меньше исходного тома «{}»: {} против {}. Посекторное \
                         восстановление переносит том целиком вместе с его файловой системой, \
                         поэтому приёмник не может быть меньше. Уместить данные можно только \
                         пофайлово.",
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
                         влезут ли они в раздел {dst_txt}, заранее нельзя.",
                        src.label
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
                        "В раздел {dst_txt} не влезут данные тома «{}»: занято {}, а с запасом \
                         на служебные структуры файловой системы нужно не меньше {}.",
                        src.label,
                        human(src.used_bytes),
                        human(needed)
                    ),
                ));
            }
        }
    }
}

/// What the running system will not let go of.
///
/// The first version of this refused the whole disk Windows boots from. That is
/// too blunt: putting a copy on a spare partition of that disk is an ordinary
/// thing to do — it is how anyone installs a second system alongside an
/// existing one. What genuinely cannot be touched is the volume Windows is
/// running from and the ESP it booted through, and those are two specific
/// partitions, not a whole disk.
///
/// So the veto moved down to the partition, and what is left at the disk level
/// is a statement of fact: the layout of a live disk has to be applied through
/// the partition manager rather than written behind its back.
fn check_live_system(
    plan: &RestorePlan,
    table: Option<&PartitionTableSpec>,
    ctx: &CheckContext,
    out: &mut Vec<CheckResult>,
) {
    if ctx.protected_parts.is_empty() && !ctx.target_is_system_disk {
        return;
    }

    // Everything the plan disturbs: partitions it writes into, and partitions
    // that exist now and are absent from the new layout — those are deletions,
    // and a deletion is as destructive as a write.
    let mut touched: Vec<u32> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { dst, .. } => Some(dst.index),
            RestoreOp::Format { part, .. } => Some(part.index),
            _ => None,
        })
        .collect();
    if let Some(t) = table {
        for cur in &ctx.targets {
            if !t.entries.iter().any(|e| e.index == cur.index) {
                touched.push(cur.index);
            }
        }
    }
    touched.sort_unstable();
    touched.dedup();

    let hit: Vec<u32> =
        touched.iter().copied().filter(|i| ctx.protected_parts.contains(i)).collect();
    if !hit.is_empty() {
        let list = hit
            .iter()
            .map(|i| ctx.describe_target(*i, table))
            .collect::<Vec<_>>()
            .join("; ");
        out.push(CheckResult::new(
            CheckLevel::Error,
            CheckCode::TargetIsLiveSystemPartition,
            hit.first().copied(),
            format!(
                "На этом диске работает загруженная сейчас Windows, и план затрагивает \
                 разделы, на которых она стоит: {list}. Переписать их на ходу невозможно — \
                 система остановится посреди записи. Загрузитесь с Boot CD и повторите, \
                 либо выберите другие разделы."
            ),
        ));
        return; // the rest is noise next to this
    }

    if ctx.target_is_system_disk && table.is_some() {
        out.push(CheckResult::new(
            CheckLevel::Warning,
            CheckCode::LiveSystemDiskLayout,
            None,
            format!(
                "Диск {} — тот, с которого загружена работающая Windows. Разделы, на которых \
                 она стоит, план не трогает, поэтому восстановление возможно: новая разметка \
                 будет применена через диспетчер разделов Windows, как это делает «Управление \
                 дисками». Затронутые тома будут отключены — закройте открытые в них файлы.",
                plan.target_disk
            ),
        ));
    }
}

/// Point out a filesystem chosen at partition creation that will not be
/// created.
///
/// A sector restore brings the source's filesystem along complete — that is
/// what makes it a sector restore — so the partition ends up holding whatever
/// the copy holds, regardless of what was picked when it was created. The plan
/// is right about this (it generates no format operation, which would erase the
/// restored data), but the user picked NTFS and has every reason to expect
/// NTFS.
///
/// Nothing is said when the choice matches what arrives, or when no filesystem
/// was chosen at all — there is no misunderstanding to correct.
fn check_format_choice(
    ctx: &CheckContext,
    src: &SourcePartition,
    dst_index: u32,
    dst_txt: &str,
    mode: RestoreMode,
    out: &mut Vec<CheckResult>,
) {
    if mode != RestoreMode::Sectors {
        return; // a file restore really does format, and the plan says so
    }
    let Some(chosen) = ctx.requested_format(dst_index) else { return };
    if chosen.is_same_as(src.filesystem) {
        return;
    }
    out.push(CheckResult::new(
        CheckLevel::Warning,
        CheckCode::FormatChoiceIgnored,
        Some(dst_index),
        format!(
            "При создании раздела {dst_txt} выбрана файловая система {}, но она создана не \
             будет: посекторное восстановление приносит файловую систему из копии целиком, и \
             в разделе окажется {}. Форматировать тут нечего — форматирование стёрло бы \
             только что восстановленные данные.",
            chosen.as_str(),
            src.filesystem.label()
        ),
    ));
}

/// Say what the destination holds today, and what it becomes.
///
/// A partition the user deletes in the designer is listed in the queue as
/// deleted, so its fate is obvious. A partition merely chosen as a destination
/// is not: the queue says "restore A → B" and nothing about B's current
/// contents, which the restore overwrites completely. Naming them — filesystem,
/// label, how much is in use — is the difference between a decision and a
/// surprise.
///
/// The second half matters when families cross. Restoring a Linux volume onto
/// what is now an NTFS partition works exactly as intended, and the partition
/// then stops being a Windows partition: its GPT type becomes Linux and
/// Explorer no longer lists it. Without a word of warning that reads as a
/// vanished drive.
fn check_destination(
    ctx: &CheckContext,
    src: &SourcePartition,
    dst: &PartRef,
    dst_txt: &str,
    out: &mut Vec<CheckResult>,
) {
    let Some(cur) = ctx.target(dst.index) else { return };
    let has_content = cur.filesystem != FileSystem::Unknown || cur.used_bytes > 0;
    if has_content {
        let label = if cur.label.trim().is_empty() {
            String::new()
        } else {
            format!(" «{}»", cur.label)
        };
        let used = if cur.used_bytes > 0 {
            format!(", занято {}", human(cur.used_bytes))
        } else {
            String::new()
        };
        out.push(CheckResult::new(
            CheckLevel::Warning,
            CheckCode::DestinationWillBeErased,
            Some(dst.index),
            format!(
                "На разделе {dst_txt} сейчас {}{label}{used}. Восстановление перезапишет его \
                 целиком — вернуть то, что там лежит, будет нельзя.",
                cur.filesystem.label()
            ),
        ));
    }

    // Crossing families is legitimate and is what the user asked for; it is the
    // consequence that needs saying.
    if has_content && src.filesystem.is_windows_family() != cur.filesystem.is_windows_family() {
        let becomes_linux = !src.filesystem.is_windows_family();
        out.push(CheckResult::new(
            CheckLevel::Warning,
            CheckCode::PartitionChangesFamily,
            Some(dst.index),
            if becomes_linux {
                format!(
                    "Раздел {dst_txt} перестанет быть разделом Windows: в него ляжет {}, и \
                     тип раздела в таблице сменится на Linux. Windows больше не покажет его \
                     как диск — это нормально для раздела Linux, но букву {} он потеряет.",
                    src.filesystem.label(),
                    cur.label.trim()
                )
            } else {
                format!(
                    "Раздел {dst_txt} сейчас размечен как {}, а в него ляжет {} — после \
                     восстановления Windows начнёт показывать его как обычный диск.",
                    cur.filesystem.label(),
                    src.filesystem.label()
                )
            },
        ));
    }
}

/// The copy holds a decrypted container, so the restore cannot put the
/// encryption back (D14).
///
/// This is the quiet failure of the whole restore path: every file is present
/// and readable, and a Linux system still refuses to boot, because `crypttab`
/// and the initramfs are waiting for a LUKS device that no longer exists.
/// Saying so before the restore costs nothing; discovering it afterwards costs
/// an evening.
fn check_encryption(
    src: &SourcePartition,
    dst: &PartRef,
    dst_index: u32,
    dst_txt: &str,
    mode: RestoreMode,
    reencrypt: bool,
    out: &mut Vec<CheckResult>,
) {
    let _ = dst;
    let _ = dst_index;
    if reencrypt {
        if !src.luks_header_available {
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::NoLuksHeader,
                Some(dst.index),
                format!(
                    "Вернуть шифрование разделу {dst_txt} нельзя: в этой копии не сохранён \
                     заголовок контейнера. Так писались копии до того, как заголовок начали \
                     сохранять — восстановить её можно только расшифрованной."
                ),
            ));
        }
        if mode == RestoreMode::Files {
            // File mode copies through the filesystem driver onto a mounted
            // volume; there is no mounted volume inside a container we have
            // only just written, and no way to hand Windows the master key.
            out.push(CheckResult::new(
                CheckLevel::Error,
                CheckCode::ReencryptNeedsSectors,
                Some(dst.index),
                format!(
                    "Раздел {dst_txt}: вернуть шифрование можно только при посекторном \
                     восстановлении — пофайловое пишет через файловую систему, а внутри \
                     нового контейнера её ещё нет."
                ),
            ));
        }
        return;
    }

    if !src.encrypted_at_source {
        return;
    }
    let remedy = if src.luks_header_available {
        "Заголовок контейнера в копии есть — включите «вернуть шифрование», и раздел \
         восстановится ровно таким, каким был, вместе с прежним паролем."
    } else {
        "Заголовок контейнера в этой копии не сохранён, поэтому вернуть шифрование \
         нельзя. Понадобится Live CD: убрать запись из /etc/crypttab, поправить \
         /etc/fstab на UUID самого раздела и пересобрать initramfs."
    };
    out.push(CheckResult::new(
        CheckLevel::Warning,
        CheckCode::EncryptionNotRestored,
        Some(dst.index),
        format!(
            "Том «{}» был зашифрован, но в копию он попал расшифрованным — после \
             восстановления в разделе {dst_txt} окажется обычная {} без контейнера LUKS. \
             Данные будут целы и читаемы, но система с этого раздела не загрузится: \
             initramfs и /etc/crypttab ждут зашифрованное устройство, которого больше нет. \
             {remedy}",
            src.label,
            src.filesystem.label()
        ),
    ));
}

/// Does the filesystem being created hold what the source held?
fn check_format_capacity(
    index: u32,
    index_txt: &str,
    src_fs: FileSystem,
    fs: crate::restore_plan::FormatFs,
    out: &mut Vec<CheckResult>,
) {
    use crate::restore_plan::FormatFs;
    // FAT32 caps a single file at 4 GiB. Restoring an NTFS volume onto FAT32
    // copies most of it and then fails on the one large file, which reads as a
    // random error rather than a format that cannot express the data.
    if fs == FormatFs::Fat32 && src_fs != FileSystem::Fat32 {
        out.push(CheckResult::new(
            CheckLevel::Warning,
            CheckCode::FormatFsTooLimited,
            Some(index),
            format!(
                "Раздел {index_txt} форматируется в FAT32, а исходный том — {}. \
                 В FAT32 не поместится ни один файл больше 4 ГиБ; такие файлы \
                 скопировать не удастся.",
                src_fs.label()
            ),
        ));
    }
}

/// `FSCTL_EXTEND_VOLUME` only exists for NTFS. Work out which filesystem will
/// actually be on the partition by the time the extend runs: the one the image
/// brings for a sector restore, the one [`RestoreOp::Format`] creates otherwise.
fn check_extend(
    plan: &RestorePlan,
    ctx: &CheckContext,
    part: &PartRef,
    part_txt: &str,
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
                    "Расширять раздел {part_txt} нечего: в него ничего не восстанавливается и \
                     он не форматируется, значит файловой системы там не будет."
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
            "Раздел {part_txt} размечен как {fs_name} — автоматически расширить Windows умеет \
             только NTFS. Свободный хвост останется неразмеченным внутри раздела; его придётся \
             дорастить средствами самой системы."
        ),
    ));
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
            protected_parts: vec![],
            targets: vec![],
            requested_formats: vec![],
        }
    }

    fn source(index: u32, size: u64, used: u64, fs: FileSystem) -> SourcePartition {
        SourcePartition {
            index,
            size_bytes: size,
            used_bytes: used,
            filesystem: fs,
            label: "Data".into(),
            encrypted_at_source: false,
            luks_header_available: false,
        }
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
                reencrypt: false,
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
                reencrypt: false,
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
                    reencrypt: false,
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
                reencrypt: false,
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
                reencrypt: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Btrfs)]));
        assert!(errors(&results).contains(&CheckCode::FileModeUnsupportedFs), "{results:#?}");
    }

    fn target(index: u32, fs: FileSystem, used: u64, label: &str) -> TargetPartition {
        TargetPartition {
            index,
            size_bytes: 200 * MIB,
            used_bytes: used,
            filesystem: fs,
            label: label.into(),
        }
    }

    fn sector_restore_plan(src_fs: FileSystem, chosen: Option<FormatFs>) -> (RestorePlan, CheckContext) {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, src_fs)]);
        c.requested_formats =
            chosen.into_iter().map(|fs| RequestedFormat { index: 1, fs }).collect();
        (plan, c)
    }

    #[test]
    fn choosing_a_filesystem_that_the_copy_will_overwrite_is_called_out() {
        // The case in front of me: a partition created as NTFS, then mapped to
        // a btrfs source. No format operation is generated — correctly, it
        // would erase the restored data — but the user picked NTFS and expects
        // NTFS.
        let (plan, c) = sector_restore_plan(FileSystem::Btrfs, Some(FormatFs::Ntfs));
        let results = check_plan(&plan, &c);
        let found = results.iter().find(|r| r.code == CheckCode::FormatChoiceIgnored).unwrap();
        assert_eq!(found.level, CheckLevel::Warning);
        assert!(found.message.contains("NTFS"), "{}", found.message);
        assert!(found.message.contains("Btrfs"), "{}", found.message);
        assert!(errors(&results).is_empty(), "a misunderstanding, not a mistake");
    }

    #[test]
    fn choosing_the_filesystem_the_copy_brings_says_nothing() {
        let (plan, c) = sector_restore_plan(FileSystem::Ntfs, Some(FormatFs::Ntfs));
        assert!(!codes(&check_plan(&plan, &c)).contains(&CheckCode::FormatChoiceIgnored));
    }

    #[test]
    fn choosing_nothing_says_nothing() {
        let (plan, c) = sector_restore_plan(FileSystem::Btrfs, None);
        assert!(!codes(&check_plan(&plan, &c)).contains(&CheckCode::FormatChoiceIgnored));
    }

    #[test]
    fn a_file_restore_really_does_format_so_the_choice_stands() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::Format { part: p1, fs: FormatFs::Ntfs, label: String::new(), quick: true },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.requested_formats = vec![RequestedFormat { index: 1, fs: FormatFs::Ntfs }];
        assert!(!codes(&check_plan(&plan, &c)).contains(&CheckCode::FormatChoiceIgnored));
    }

    #[test]
    fn what_the_destination_holds_today_is_spelled_out() {
        // The queue says "restore A → B" and nothing about B. A partition the
        // user did not explicitly delete still loses everything on it.
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.targets = vec![target(1, FileSystem::Ntfs, 93 * MIB, "Данные")];
        let results = check_plan(&plan, &c);
        let found = results.iter().find(|r| r.code == CheckCode::DestinationWillBeErased).unwrap();
        assert!(found.message.contains("Данные"), "{}", found.message);
        assert!(found.message.contains("93"), "{}", found.message);
        assert!(errors(&results).is_empty(), "a warning, not a veto");
    }

    #[test]
    fn an_empty_destination_says_nothing() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.targets = vec![target(1, FileSystem::Unknown, 0, "")];
        let results = check_plan(&plan, &c);
        assert!(!codes(&results).contains(&CheckCode::DestinationWillBeErased));
    }

    #[test]
    fn restoring_linux_onto_an_ntfs_partition_warns_that_windows_will_lose_it() {
        // Exactly the case in front of me: a btrfs volume mapped onto what is
        // currently an NTFS partition. It works — and the partition stops being
        // a Windows drive, which reads as a disappearance if nobody says so.
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Btrfs)]);
        c.targets = vec![target(1, FileSystem::Ntfs, 50 * MIB, "NTFS")];
        let results = check_plan(&plan, &c);
        let found = results.iter().find(|r| r.code == CheckCode::PartitionChangesFamily).unwrap();
        assert!(found.message.contains("Windows"), "{}", found.message);
        assert!(errors(&results).is_empty(), "crossing families is allowed");
    }

    #[test]
    fn two_windows_filesystems_do_not_trigger_the_family_warning() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.targets = vec![target(1, FileSystem::ExFat, 50 * MIB, "Flash")];
        let results = check_plan(&plan, &c);
        assert!(!codes(&results).contains(&CheckCode::PartitionChangesFamily));
    }

    #[test]
    fn a_decrypted_container_is_flagged_but_does_not_block() {
        // The data restores fine; the system it belongs to will not boot until
        // crypttab and the initramfs are fixed. That is a warning, not a veto.
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut src = source(1, 100 * MIB, 10 * MIB, FileSystem::Btrfs);
        src.encrypted_at_source = true;
        let results = check_plan(&plan, &ctx(vec![src]));
        assert!(errors(&results).is_empty(), "{results:#?}");
        let found = results.iter().find(|r| r.code == CheckCode::EncryptionNotRestored).unwrap();
        assert_eq!(found.level, CheckLevel::Warning);
        assert!(found.message.contains("crypttab"), "the fix must be spelled out");
    }

    #[test]
    fn an_unencrypted_source_says_nothing_about_encryption() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(!codes(&results).contains(&CheckCode::EncryptionNotRestored));
    }

    #[test]
    fn a_file_restore_with_nothing_to_format_it_is_refused() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Files,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(errors(&results).contains(&CheckCode::FileModeNeedsFormat), "{results:#?}");
    }

    #[test]
    fn formatting_an_ntfs_source_as_fat32_warns_about_the_4_gib_limit() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![
                RestoreOp::Format { part: p1, fs: FormatFs::Fat32, label: String::new(), quick: true },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: p1,
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        assert!(codes(&results).contains(&CheckCode::FormatFsTooLimited), "{results:#?}");
        assert!(errors(&results).is_empty(), "a limit worth knowing, not a veto");
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
                    reencrypt: false,
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
                    reencrypt: false,
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
                    reencrypt: false,
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
    fn overwriting_the_partition_windows_runs_from_is_refused() {
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.target_is_system_disk = true;
        c.protected_parts = vec![1];
        let results = check_plan(&plan, &c);
        assert!(errors(&results).contains(&CheckCode::TargetIsLiveSystemPartition), "{results:#?}");
    }

    #[test]
    fn a_spare_partition_on_the_system_disk_is_allowed() {
        // The case that mattered: Windows runs from partition 3, the copy goes
        // into partition 8. Refusing the whole disk for that was too blunt.
        let t = table(vec![entry(3, 1, 100), entry(8, 101, 200)]);
        let p8 = part_ref(&t, 8);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p8,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let mut c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        c.target_is_system_disk = true;
        c.protected_parts = vec![1, 3];
        let results = check_plan(&plan, &c);
        assert!(errors(&results).is_empty(), "{results:#?}");
        assert!(codes(&results).contains(&CheckCode::LiveSystemDiskLayout));
    }

    #[test]
    fn deleting_the_partition_windows_runs_from_is_refused_too() {
        // It is not in any operation — it simply vanishes from the new layout,
        // which is exactly as destructive as writing over it.
        let t = table(vec![entry(8, 101, 200)]);
        let plan = plan_with(t, vec![]);
        let mut c = ctx(vec![]);
        c.target_is_system_disk = true;
        c.protected_parts = vec![3];
        c.targets = vec![target(3, FileSystem::Ntfs, 40 * MIB, "Windows")];
        let results = check_plan(&plan, &c);
        assert!(errors(&results).contains(&CheckCode::TargetIsLiveSystemPartition), "{results:#?}");
    }

    #[test]
    fn boot_media_protects_nothing() {
        // In WinPE no partition here is running anything.
        let t = table(vec![entry(1, 1, 200)]);
        let p1 = part_ref(&t, 1);
        let plan = plan_with(
            t,
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: p1,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        let c = ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]);
        assert!(errors(&check_plan(&plan, &c)).is_empty());
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

    fn mbr_table(entries: Vec<PartitionSpec>) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: 100 * GIB,
            disk_id: "1A2B3C4D".into(),
            entries,
        }
    }

    fn mbr_entry(index: u32, first_mib: u64, logical: bool) -> PartitionSpec {
        PartitionSpec {
            index,
            first_lba: first_mib * MIB / 512,
            last_lba: (first_mib + 10) * MIB / 512 - 1,
            kind: PartitionKind::Mbr { type_byte: 0x07, bootable: false, logical },
        }
    }

    #[test]
    fn mbr_refuses_more_than_four_primaries() {
        let t = mbr_table(vec![
            mbr_entry(1, 1, false),
            mbr_entry(2, 11, false),
            mbr_entry(3, 21, false),
            mbr_entry(4, 31, false),
            mbr_entry(5, 41, false),
        ]);
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::MbrTooManyPrimaries), "{results:#?}");
    }

    #[test]
    fn four_primaries_are_fine_without_logicals() {
        let t = mbr_table(vec![
            mbr_entry(1, 1, false),
            mbr_entry(2, 11, false),
            mbr_entry(3, 21, false),
            mbr_entry(4, 31, false),
        ]);
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).is_empty(), "{results:#?}");
    }

    #[test]
    fn a_logical_partition_costs_one_of_the_four_primary_slots() {
        // The extended partition that holds the logical one is itself an entry.
        let t = mbr_table(vec![
            mbr_entry(1, 1, false),
            mbr_entry(2, 11, false),
            mbr_entry(3, 21, false),
            mbr_entry(4, 31, false),
            mbr_entry(5, 42, true),
        ]);
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).contains(&CheckCode::MbrTooManyPrimaries), "{results:#?}");
    }

    #[test]
    fn three_primaries_plus_logicals_fit() {
        let t = mbr_table(vec![
            mbr_entry(1, 1, false),
            mbr_entry(2, 11, false),
            mbr_entry(3, 21, false),
            mbr_entry(5, 42, true),
        ]);
        let results = check_plan(&plan_with(t, vec![]), &ctx(vec![]));
        assert!(errors(&results).is_empty(), "{results:#?}");
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
                reencrypt: false,
            }],
        );
        let results = check_plan(&plan, &ctx(vec![source(1, 100 * MIB, 10 * MIB, FileSystem::Ntfs)]));
        let info: Vec<_> = results.iter().filter(|r| r.code == CheckCode::FreeSpaceNotWiped).collect();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].level, CheckLevel::Info);
    }
}
