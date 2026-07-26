//! Executing a restore plan (R5).
//!
//! The plan arrives as a list of operations the user assembled by clicking
//! around a partition map. This runs them — in the fixed order
//! [`RestorePlan::ordered`] gives, not the order they were clicked — and keeps
//! a [`RestoreRunState`] the service can hand to a reconnecting window.
//!
//! ## There is no rollback
//!
//! Once the partition table is rewritten the old one is gone. Once a
//! filesystem is overwritten it is gone. Nothing here can undo anything, and
//! the code does not pretend otherwise: on failure it stops, marks the
//! remaining operations skipped, and records precisely what was done and what
//! was not. That report is the only thing standing between the user and
//! guesswork.
//!
//! ## Offline, then online
//!
//! Raw sector writes go to the physical drive and want the disk offline, with
//! no filesystem driver holding an opinion about those sectors. Everything
//! afterwards — formatting, copying files, extending, mounting — talks to a
//! *volume*, which only exists once Windows has re-read the table and mounted
//! it. So the run has exactly one transition, taken the first time an operation
//! needs a volume, and [`DiskSession`] guarantees the disk ends up online even
//! when the run fails partway.

use chrono::Utc;
#[cfg(target_os = "windows")]
use tracing::{error, info};
use tracing::warn;

use own_backup_common::cancel::CancelToken;
use own_backup_common::restore_plan::{RestoreMode, RestoreOp, RestorePlan};
use own_backup_common::restore_state::{OpRecord, OpStatus, RestoreRunState, RunStatus};
use own_backup_common::{OwnBackupError, Result};

/// Build the initial state for `plan`, with every operation pending.
///
/// Split out so the caller can publish a run *before* the work starts: a window
/// that opens between "the user confirmed" and "the first byte was written"
/// should see the run, not an empty screen.
pub fn initial_state(plan: &RestorePlan) -> RestoreRunState {
    let mut ops: Vec<OpRecord> = plan
        .ordered()
        .iter()
        .enumerate()
        .map(|(index, op)| OpRecord {
            index,
            kind: op.kind().to_string(),
            title: op.title(),
            part_index: op.part().map(|p| p.index),
            status: OpStatus::Pending,
            percent: 0,
            bytes_done: 0,
            bytes_total: op.part().map(|p| p.size).unwrap_or(0),
            message: String::new(),
            cancellable: op.cancellable(),
            destroys_data: op.destroys_data(),
        })
        .collect();

    // Verification is not part of the plan the user assembled — it is a
    // consequence of the "проверять после восстановления" checkbox (D10). It
    // still belongs in the queue, so the user sees up front that it will run
    // and how long the run really is.
    if plan.verify_after {
        for op in plan.ordered() {
            let RestoreOp::RestorePartition { dst, mode: RestoreMode::Sectors, src_index, .. } = op
            else {
                continue;
            };
            ops.push(OpRecord {
                index: ops.len(),
                kind: "verify_sectors".into(),
                title: format!("Проверить записанное в разделе {} (из раздела {src_index})", dst.index),
                part_index: Some(dst.index),
                status: OpStatus::Pending,
                percent: 0,
                bytes_done: 0,
                bytes_total: dst.size,
                message: String::new(),
                cancellable: true,
                // Reading back destroys nothing; cancelling only means the
                // check did not finish.
                destroys_data: false,
            });
        }
    }

    RestoreRunState {
        plan_id: plan.id,
        target_disk: plan.target_disk,
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
        status: RunStatus::Running,
        ops,
        current: None,
        // Nothing destructive has started yet, so a cancel now costs nothing.
        cancellable: true,
        follow_ups: Vec::new(),
    }
}

/// Execute `plan`, calling `on_update` whenever the state changes.
///
/// Returns the final state. An operation failing is *not* an `Err` — it is a
/// state with `RunStatus::Failed` and a report. `Err` is reserved for the plan
/// never starting at all (a refused target, an unreadable image), where there
/// is nothing to report because nothing happened.
#[cfg(target_os = "windows")]
pub fn execute_plan(
    plan: &RestorePlan,
    cancel: &CancelToken,
    mut on_update: impl FnMut(&RestoreRunState),
) -> Result<RestoreRunState> {
    use own_backup_backup::disk_session_win::DiskSession;
    use own_backup_backup::engine_win::load_sidecar_of;

    // Refuse the running system's own disk here too, not only in the checks:
    // a plan can reach the service by a route that skipped the dry run.
    own_backup_restore::guard_target_disk(plan.target_disk)?;

    let sidecar = load_sidecar_of(&plan.image_path)?;
    let mut state = initial_state(plan);
    on_update(&state);

    let ordered: Vec<RestoreOp> = plan.ordered().into_iter().cloned().collect();

    // The session is only needed when the table is rewritten. Opening it always
    // would take the disk offline for a plan that merely assigns a drive letter.
    let mut session = if plan.touches_partition_table() {
        let mut s = DiskSession::open(plan.target_disk)?;
        s.take_offline()?;
        Some(s)
    } else {
        None
    };

    let mut online = session.is_none();

    for (index, op) in ordered.iter().enumerate() {
        if cancel.is_cancelled() {
            finish_cancelled(&mut state, index);
            on_update(&state);
            return Ok(state);
        }

        // One transition, at the first operation that needs a mounted volume.
        if !online && op.needs_disk_online() {
            if let Some(s) = session.as_mut() {
                s.update_properties()?;
                s.bring_online()?;
            }
            online = true;
        }

        state.current = Some(index);
        state.ops[index].status = OpStatus::Running;
        state.cancellable = op.cancellable();
        on_update(&state);

        let outcome = run_op(op, plan, &sidecar, session.as_mut(), cancel, &mut |percent, bytes| {
            state.ops[index].percent = percent;
            state.ops[index].bytes_done = bytes;
            on_update(&state);
        });

        match outcome {
            Ok(note) => {
                state.ops[index].status = OpStatus::Done;
                state.ops[index].percent = 100;
                state.ops[index].message = note.message;
                state.follow_ups.extend(note.follow_ups);
                info!(op = %op.kind(), index, "restore operation done");
            }
            Err(e) => {
                error!(op = %op.kind(), index, error = %e, "restore operation failed");
                state.ops[index].status = OpStatus::Failed;
                state.ops[index].message = e.to_string();
                for later in state.ops.iter_mut().skip(index + 1) {
                    later.status = OpStatus::Skipped;
                }
                state.status = RunStatus::Failed;
                state.cancellable = false;
                state.current = None;
                state.finished_at = Some(Utc::now().to_rfc3339());
                add_failure_follow_ups(&mut state);
                on_update(&state);
                return Ok(state);
            }
        }
        on_update(&state);
    }

    // Bring the disk back before declaring success, so the user sees their
    // volumes rather than a disk that is technically restored and invisible.
    if let Some(s) = session.as_mut() {
        s.update_properties()?;
        s.bring_online()?;
    }
    drop(session);

    if plan.verify_after {
        run_verification(plan, &sidecar, &ordered, &mut state, cancel, &mut on_update);
    }

    state.current = None;
    state.cancellable = false;
    state.status = if state.ops.iter().any(|o| o.status == OpStatus::Failed) {
        RunStatus::Failed
    } else {
        RunStatus::Completed
    };
    state.finished_at = Some(Utc::now().to_rfc3339());
    info!(plan = %plan.id, ops = state.ops.len(), "restore plan finished");
    on_update(&state);
    Ok(state)
}

/// Read back every sector-restored partition and check it against the copy
/// (R9, D10).
///
/// A verification failure does **not** mark the run failed: the restore itself
/// finished, and what went wrong is a different fact. It is reported as the
/// verify operation failing, with the follow-up spelling out which of the two
/// possible causes it was — a damaged backup, or a target that did not take the
/// write. Those call for different actions, so they are never merged.
#[cfg(target_os = "windows")]
fn run_verification(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    ordered: &[RestoreOp],
    state: &mut RestoreRunState,
    cancel: &CancelToken,
    on_update: &mut impl FnMut(&RestoreRunState),
) {
    use own_backup_backup::restore_win::{verify_partition_sectors, PartitionTarget};

    let sector_ops: Vec<&RestoreOp> = ordered
        .iter()
        .filter(|op| matches!(op, RestoreOp::RestorePartition { mode: RestoreMode::Sectors, .. }))
        .collect();
    let first = ordered.len();

    for (n, op) in sector_ops.iter().enumerate() {
        let RestoreOp::RestorePartition { src_index, dst, .. } = op else { continue };
        let index = first + n;
        if index >= state.ops.len() {
            break;
        }
        if cancel.is_cancelled() {
            state.ops[index].status = OpStatus::Skipped;
            continue;
        }

        state.current = Some(index);
        state.ops[index].status = OpStatus::Running;
        state.cancellable = true;
        on_update(state);

        let target =
            PartitionTarget { disk_number: plan.target_disk, offset: dst.offset, size: dst.size };
        let result = verify_partition_sectors(
            &plan.image_path,
            sidecar,
            *src_index,
            target,
            cancel,
            |pr| {
                let percent = if pr.blocks_total > 0 {
                    (pr.blocks_done * 100 / pr.blocks_total) as u32
                } else {
                    0
                };
                state.ops[index].percent = percent;
                state.ops[index].bytes_done = pr.bytes_written;
                on_update(state);
                Ok(())
            },
        );

        match result {
            Ok(v) if v.ok() => {
                state.ops[index].status = OpStatus::Done;
                state.ops[index].percent = 100;
                state.ops[index].message =
                    format!("Сверено блоков: {}", v.blocks_checked);
                info!(part = dst.index, blocks = v.blocks_checked, "verification passed");
            }
            Ok(v) if v.cancelled => {
                state.ops[index].status = OpStatus::Skipped;
                state.ops[index].message = "Проверка прервана.".into();
            }
            Ok(v) => {
                state.ops[index].status = OpStatus::Failed;
                let mut parts = Vec::new();
                if !v.image_corrupt.is_empty() {
                    parts.push(format!(
                        "в самой копии повреждено блоков: {} (восстановление честно записало то,                          что в ней лежит)",
                        v.image_corrupt.len()
                    ));
                }
                if !v.mismatches.is_empty() {
                    parts.push(format!(
                        "не совпало блоков на диске: {} — запись не легла (проверьте накопитель)",
                        v.mismatches.len()
                    ));
                }
                let text = parts.join("; ");
                error!(part = dst.index, %text, "verification failed");
                state.ops[index].message = text.clone();
                state.follow_ups.push(format!("Раздел {}: {text}.", dst.index));
            }
            Err(e) => {
                state.ops[index].status = OpStatus::Failed;
                state.ops[index].message = e.to_string();
                state
                    .follow_ups
                    .push(format!("Раздел {}: проверку выполнить не удалось: {e}", dst.index));
            }
        }
        on_update(state);
    }
    state.current = None;
}

/// Anything an operation wants to tell the user afterwards.
#[cfg(target_os = "windows")]
#[derive(Default)]
struct OpNote {
    message: String,
    follow_ups: Vec<String>,
}

#[cfg(target_os = "windows")]
fn run_op(
    op: &RestoreOp,
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    session: Option<&mut own_backup_backup::disk_session_win::DiskSession>,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(u32, u64),
) -> Result<OpNote> {
    use own_backup_backup::format_win::format_volume;
    use own_backup_backup::letters_win::{assign_letter, extend_volume, TempDriveLetter};
    use own_backup_backup::restore_win::{
        restore_partition_sectors, PartitionTarget, SectorRestoreOptions,
    };
    use own_backup_backup::volume_win::volume_root_at_offset;

    match op {
        RestoreOp::WritePartitionTable { table } => {
            let session = session.ok_or_else(|| {
                OwnBackupError::Device(
                    "нет открытой сессии диска для записи разметки — внутренняя ошибка".into(),
                )
            })?;
            session.write_partition_table(table)?;
            Ok(OpNote::default())
        }

        RestoreOp::RestorePartition { src_index, dst, mode: RestoreMode::Sectors, wipe_free_space } => {
            let target = PartitionTarget {
                disk_number: plan.target_disk,
                offset: dst.offset,
                size: dst.size,
            };
            let opts = SectorRestoreOptions {
                verify_checksums: true,
                wipe_free_space: *wipe_free_space,
                // When we hold a session the disk is offline and there is
                // nothing left to dismount.
                disk_already_offline: session.is_some(),
            };
            let summary = restore_partition_sectors(
                &plan.image_path,
                sidecar,
                *src_index,
                target,
                opts,
                cancel,
                |pr| {
                    let percent = if pr.blocks_total > 0 {
                        (pr.blocks_done * 100 / pr.blocks_total) as u32
                    } else {
                        0
                    };
                    on_progress(percent, pr.bytes_written);
                    Ok(())
                },
            )?;
            if summary.cancelled {
                return Err(OwnBackupError::Cancelled);
            }
            Ok(OpNote::default())
        }

        RestoreOp::RestorePartition { src_index, dst, mode: RestoreMode::Files, .. } => {
            use own_backup_backup::file_restore_win::{copy_tree, long_path, measure_tree};
            use own_backup_backup::restore_win::MountedImage;

            // The copy is read through a read-only mount of the image, so the
            // filesystem driver does the reading and every file's metadata
            // comes along. Detached when `image` drops, including on failure.
            let image = MountedImage::open(&plan.image_path)?;
            let src_root = image.volume_root_of(sidecar, *src_index)?;
            let dst_root = volume_root_at_offset(plan.target_disk, dst.offset).ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "Windows не показала том раздела {} — копировать файлы некуда",
                    dst.index
                ))
            })?;

            // Counting first costs one metadata pass and buys a progress bar
            // that means something. On a restore that runs for an hour, a
            // spinner is not good enough.
            let total = measure_tree(&long_path(std::path::Path::new(&src_root)), cancel);
            let summary = copy_tree(
                &long_path(std::path::Path::new(&src_root)),
                &long_path(std::path::Path::new(&dst_root)),
                total,
                cancel,
                |pr| {
                    let percent = if pr.bytes_total > 0 {
                        (pr.bytes_done * 100 / pr.bytes_total) as u32
                    } else {
                        0
                    };
                    on_progress(percent, pr.bytes_done);
                    Ok(())
                },
            )?;

            if summary.cancelled {
                return Err(OwnBackupError::Cancelled);
            }

            let mut follow_ups = vec![
                "Серийный номер тома восстановить нельзя — он назначается при форматировании, \
                 и API для его смены не существует. Программы, чьи лицензии к нему привязаны, \
                 потребуют повторной активации; ссылки на том в BCD придётся пересоздать."
                    .into(),
            ];
            if !summary.failures.is_empty() {
                // Naming a few beats a bare count: a user can tell at a glance
                // whether it was a locked log file or their documents.
                let sample: Vec<String> =
                    summary.failures.iter().take(5).map(|(p, e)| format!("{p} ({e})")).collect();
                follow_ups.push(format!(
                    "Не удалось скопировать объектов: {}. Первые из них: {}",
                    summary.failures.len(),
                    sample.join("; ")
                ));
            }
            Ok(OpNote {
                message: format!(
                    "Файлов: {}, каталогов: {}, жёстких ссылок: {}, точек соединения: {}",
                    summary.files_copied,
                    summary.dirs_created,
                    summary.hard_links,
                    summary.reparse_points
                ),
                follow_ups,
            })
        }

        RestoreOp::Format { part, fs, label, quick } => {
            let volume = volume_root_at_offset(plan.target_disk, part.offset).ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "Windows не показала том для раздела {} — форматировать нечего",
                    part.index
                ))
            })?;
            // FormatEx needs a mount point; a partition that has just appeared
            // has none, and should not keep one it did not ask for.
            let temp = TempDriveLetter::acquire(&volume)?;
            format_volume(temp.mount_point(), *fs, label, *quick)?;
            Ok(OpNote::default())
        }

        RestoreOp::ExtendFilesystem { part } => {
            let volume = volume_root_at_offset(plan.target_disk, part.offset).ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "Windows не показала том для раздела {} — расширять нечего",
                    part.index
                ))
            })?;
            let sector = plan
                .table()
                .map(|t| t.sector_size.max(512) as u64)
                .unwrap_or(512);
            extend_volume(&volume, part.size / sector)?;
            Ok(OpNote::default())
        }

        RestoreOp::AssignLetter { part, letter } => {
            let volume = volume_root_at_offset(plan.target_disk, part.offset).ok_or_else(|| {
                OwnBackupError::Device(format!(
                    "Windows не показала том для раздела {} — назначать букву нечему",
                    part.index
                ))
            })?;
            assign_letter(&volume, *letter)?;
            Ok(OpNote::default())
        }

        RestoreOp::FinalizeBoot { windows_part, esp, uefi } => {
            use own_backup_backup::engine_win::finalize_boot;
            let win_volume = volume_root_at_offset(plan.target_disk, windows_part.offset)
                .ok_or_else(|| {
                    OwnBackupError::Device(
                        "Windows не показала том системного раздела — загрузчик не пересоздать"
                            .into(),
                    )
                })?;
            let esp_volume = match esp {
                Some(p) => Some(volume_root_at_offset(plan.target_disk, p.offset).ok_or_else(
                    || OwnBackupError::Device("Windows не показала том раздела EFI".into()),
                )?),
                None => None,
            };
            finalize_boot(&win_volume, esp_volume.as_deref(), *uefi)?;
            Ok(OpNote {
                message: String::new(),
                follow_ups: vec![
                    "Загрузчик пересоздан командой bcdboot. Если система не стартует, \
                     проверьте порядок загрузки в UEFI."
                        .into(),
                ],
            })
        }
    }
}

/// Mark the run cancelled at `index`, leaving the record honest about what was
/// already written.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn finish_cancelled(state: &mut RestoreRunState, index: usize) {
    for later in state.ops.iter_mut().skip(index) {
        later.status = OpStatus::Skipped;
    }
    state.status = RunStatus::Cancelled;
    state.cancellable = false;
    state.current = None;
    state.finished_at = Some(Utc::now().to_rfc3339());
    let destroyed = state.ops.iter().any(|o| o.status == OpStatus::Done && o.destroys_data);
    if destroyed {
        state.follow_ups.push(
            "Восстановление прервано после того, как данные на диске уже были перезаписаны. \
             Отката нет: затронутые разделы непригодны, пока восстановление не будет \
             выполнено заново."
                .into(),
        );
    }
    warn!(cancelled_at = index, destroyed, "restore plan cancelled");
}

/// After a failure, spell out the consequences rather than leaving the user to
/// infer them from a list of statuses.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn add_failure_follow_ups(state: &mut RestoreRunState) {
    let destroyed: Vec<&OpRecord> =
        state.ops.iter().filter(|o| o.status == OpStatus::Done && o.destroys_data).collect();
    if destroyed.is_empty() {
        state
            .follow_ups
            .push("Ничего перезаписать не успели — данные на диске не тронуты.".into());
        return;
    }
    let parts: Vec<String> = destroyed
        .iter()
        .filter_map(|o| o.part_index)
        .map(|i| i.to_string())
        .collect();
    state.follow_ups.push(if parts.is_empty() {
        "Разметка диска уже перезаписана. Прежнюю таблицу разделов вернуть нельзя — \
         повторите восстановление целиком."
            .into()
    } else {
        format!(
            "Данные уже перезаписаны на разделах: {}. Отката нет — эти разделы непригодны, \
             пока восстановление не будет выполнено заново.",
            parts.join(", ")
        )
    });
}

/// Linux has no restore engine yet (D15). Refusing outright is the honest
/// answer; a stub that returned success would be worse than useless.
#[cfg(not(target_os = "windows"))]
pub fn execute_plan(
    plan: &RestorePlan,
    _cancel: &CancelToken,
    mut on_update: impl FnMut(&RestoreRunState),
) -> Result<RestoreRunState> {
    let state = initial_state(plan);
    on_update(&state);
    Err(OwnBackupError::NotImplemented(
        "восстановление по плану реализовано только для Windows".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::{
        PartRef, PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec, RestoreMode,
        RestoreOp,
    };

    fn table() -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: 512 * 1024 * 1024 * 1024,
            disk_id: String::new(),
            entries: vec![PartitionSpec {
                index: 1,
                first_lba: 2048,
                last_lba: 2048 + 1024 * 1024 * 1024 / 512 - 1,
                kind: PartitionKind::Gpt {
                    type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                    part_guid: String::new(),
                    attributes: 0,
                    name: "p1".into(),
                },
            }],
        }
    }

    fn part() -> PartRef {
        PartRef { index: 1, offset: 1024 * 1024, size: 1024 * 1024 * 1024 }
    }

    #[test]
    fn the_initial_state_lists_every_operation_in_execution_order() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::AssignLetter { part: part(), letter: 'D' },
                RestoreOp::WritePartitionTable { table: table() },
            ],
        );
        let state = initial_state(&plan);
        assert_eq!(state.ops.len(), 2);
        assert_eq!(state.ops[0].kind, "write_partition_table", "the table is written first");
        assert_eq!(state.ops[1].kind, "assign_letter");
        assert!(state.ops.iter().all(|o| o.status == OpStatus::Pending));
        assert_eq!(state.status, RunStatus::Running);
    }

    #[test]
    fn a_run_that_has_not_started_is_still_cancellable() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::WritePartitionTable { table: table() }],
        );
        assert!(initial_state(&plan).cancellable);
    }

    #[test]
    fn cancelling_before_anything_destructive_says_nothing_was_touched() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::WritePartitionTable { table: table() }],
        );
        let mut state = initial_state(&plan);
        finish_cancelled(&mut state, 0);
        assert_eq!(state.status, RunStatus::Cancelled);
        assert!(state.follow_ups.is_empty(), "nothing was written, so nothing to warn about");
    }

    #[test]
    fn cancelling_after_a_destructive_operation_says_so_plainly() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::AssignLetter { part: part(), letter: 'D' },
            ],
        );
        let mut state = initial_state(&plan);
        state.ops[0].status = OpStatus::Done;
        finish_cancelled(&mut state, 1);
        assert_eq!(state.status, RunStatus::Cancelled);
        assert_eq!(state.ops[1].status, OpStatus::Skipped);
        assert!(state.follow_ups[0].contains("Отката нет"), "{:?}", state.follow_ups);
    }

    #[test]
    fn a_failure_before_any_write_reassures_rather_than_alarms() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::WritePartitionTable { table: table() }],
        );
        let mut state = initial_state(&plan);
        state.ops[0].status = OpStatus::Failed;
        add_failure_follow_ups(&mut state);
        assert!(state.follow_ups[0].contains("не тронуты"), "{:?}", state.follow_ups);
    }

    #[test]
    fn a_failure_after_a_partition_was_written_names_the_partition() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                },
            ],
        );
        let mut state = initial_state(&plan);
        state.ops[1].status = OpStatus::Done;
        state.ops[0].status = OpStatus::Failed;
        add_failure_follow_ups(&mut state);
        assert!(state.follow_ups[0].contains(": 1"), "{:?}", state.follow_ups);
    }

    #[test]
    fn verification_is_queued_only_when_asked_for() {
        let restore = RestoreOp::RestorePartition {
            src_index: 1,
            dst: part(),
            mode: RestoreMode::Sectors,
            wipe_free_space: false,
        };
        let mut plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::WritePartitionTable { table: table() }, restore],
        );
        assert_eq!(initial_state(&plan).ops.len(), 2);

        plan.verify_after = true;
        let state = initial_state(&plan);
        assert_eq!(state.ops.len(), 3, "one verify op per sector restore");
        assert_eq!(state.ops[2].kind, "verify_sectors");
        // Reading back destroys nothing, so cancelling it costs the user only
        // the check.
        assert!(!state.ops[2].destroys_data);
        assert!(state.ops[2].cancellable);
    }

    #[test]
    fn a_file_mode_restore_gets_no_verify_op() {
        // File-by-file verification compares counts and sizes, not blocks —
        // a different check, and not this one (R4b).
        let mut plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: part(),
                mode: RestoreMode::Files,
                wipe_free_space: false,
            }],
        );
        plan.verify_after = true;
        assert_eq!(initial_state(&plan).ops.len(), 1);
    }

    #[test]
    fn percent_counts_completed_operations() {
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::AssignLetter { part: part(), letter: 'D' },
            ],
        );
        let mut state = initial_state(&plan);
        assert_eq!(state.percent(), 0);
        state.ops[0].status = OpStatus::Done;
        assert_eq!(state.percent(), 50);
    }
}
