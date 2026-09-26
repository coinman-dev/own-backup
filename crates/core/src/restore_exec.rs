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

/// Where finished restore runs are kept.
///
/// The run state lived only in the service's memory, so a service restart —
/// or a crash mid-restore — erased the record of what had been done to the
/// disk. That is the one thing a user needs after a restore goes wrong, so it
/// is written to disk as soon as the run ends (D5).
pub const HISTORY_FILE: &str = "restore-history.json";

/// How many runs to keep. A restore is a rare event; a hundred of them is more
/// history than anyone will read and still a trivial file.
const HISTORY_CAP: usize = 100;

/// Append `state` to the persisted history, newest first.
pub fn append_history(data_dir: &std::path::Path, state: &RestoreRunState) {
    let path = data_dir.join(HISTORY_FILE);
    let mut history: Vec<RestoreRunState> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // Re-running the same plan replaces its entry rather than accumulating
    // near-duplicates.
    history.retain(|r| r.plan_id != state.plan_id);
    history.insert(0, state.clone());
    history.truncate(HISTORY_CAP);
    if let Err(e) = write_history(&path, &history) {
        warn!(path = %path.display(), error = %e, "could not persist the restore history");
    }
}

/// Finished runs, newest first.
pub fn read_history(data_dir: &std::path::Path) -> Vec<RestoreRunState> {
    std::fs::read(data_dir.join(HISTORY_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Mark the run `plan_id` as read, in the history on disk.
///
/// Without this the report comes back from the history every time the restore
/// page is opened, which reads as "it ran again". Returns whether an entry was
/// found — a run that never reached the history (the service is still holding
/// it) is not an error, and the caller marks its in-memory copy instead.
pub fn acknowledge_in_history(data_dir: &std::path::Path, plan_id: uuid::Uuid) -> bool {
    let path = data_dir.join(HISTORY_FILE);
    let mut history = read_history(data_dir);
    let Some(entry) = history.iter_mut().find(|r| r.plan_id == plan_id) else {
        return false;
    };
    if entry.acknowledged {
        return true;
    }
    entry.acknowledged = true;
    if let Err(e) = write_history(&path, &history) {
        warn!(path = %path.display(), error = %e, "could not record that the run was read");
    }
    true
}

fn write_history(path: &std::path::Path, history: &[RestoreRunState]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Written through a temporary file: a half-written history is worse than
    // none, because it looks readable and is not.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(history)?)?;
    std::fs::rename(&tmp, path)
}

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
            hidden: op.is_housekeeping(),
        })
        .collect();

    // Verification is not part of the plan the user assembled — it is a
    // consequence of the "проверять после восстановления" checkbox (D10). It
    // still belongs in the queue, so the user sees up front that it will run
    // and how long the run really is.
    //
    // Where it belongs in that queue is where it RUNS. The sector checks read
    // the disk back while it is still offline — before the loader is written,
    // before Windows sees a volume — so they stand right after the last sector
    // restore. Listing them at the end made the screen contradict itself: the
    // checks ticked off one by one while "Пересоздать загрузчик" sat above them
    // saying "ожидает" (2026-09-20). File checks really are last: they need a
    // mounted volume.
    let after_sector_restores = plan
        .ordered()
        .iter()
        .rposition(|op| {
            matches!(op, RestoreOp::RestorePartition { mode: RestoreMode::Sectors, .. })
        })
        .map(|i| i + 1)
        .unwrap_or(ops.len());

    if plan.verify_after {
        let mut sector_checks: Vec<OpRecord> = Vec::new();
        for op in plan.ordered() {
            let RestoreOp::RestorePartition { dst, mode, src_index, .. } = op else {
                continue;
            };
            // Two different checks: a sector restore is verified block by block
            // against the copy's checksums, a file restore by comparing what
            // arrived with what was there. Neither substitutes for the other.
            let (kind, what) = match mode {
                RestoreMode::Sectors => ("verify_sectors", "записанное"),
                RestoreMode::Files => ("verify_files", "перенесённые файлы"),
            };
            let record = OpRecord {
                // Filled in below: these records are placed by where they run,
                // not by the order they were built in.
                index: 0,
                kind: kind.into(),
                title: format!("Проверить {what} в разделе {} (из раздела {src_index})", dst.index),
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
                hidden: false,
            };
            match mode {
                RestoreMode::Sectors => sector_checks.push(record),
                RestoreMode::Files => ops.push(record),
            }
        }
        // Inserted as a block, so they keep their own order among themselves.
        for (n, record) in sector_checks.into_iter().enumerate() {
            ops.insert(after_sector_restores + n, record);
        }
        for (i, op) in ops.iter_mut().enumerate() {
            op.index = i;
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
        acknowledged: false,
        image_file: plan
            .image_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Execute `plan`, calling `on_update` whenever the state changes.
///
/// Returns the final state. An operation failing is *not* an `Err` — it is a
/// state with `RunStatus::Failed` and a report. `Err` is reserved for the plan
/// never starting at all (a refused target, an unreadable image), where there
/// is nothing to report because nothing happened.
#[cfg(target_os = "windows")]
/// `archive` carries the keys of an ENCRYPTED copy, from what the person
/// typed. A manual restore never borrows the service's own key (§7.4).
pub fn execute_plan(
    plan: &RestorePlan,
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    cancel: &CancelToken,
    mut on_update: impl FnMut(&RestoreRunState),
) -> Result<RestoreRunState> {
    use own_backup_backup::disk_session_win::DiskSession;
    use own_backup_backup::engine_win::load_sidecar_with_keys;

    // The running system's own partitions are refused here too, not only in the
    // checks: a plan can reach the service by a route that skipped the dry run.
    // The *disk* is no longer refused — a spare partition on it is an ordinary
    // target — only the partitions the system is standing on.
    // Logged before anything happens, and in full. A restore is the one
    // operation whose log has to survive the machine it ran on: if it goes
    // wrong the disk may not boot, and this record is then the only account of
    // what was written where.
    info!(
        plan = %plan.id,
        target_disk = plan.target_disk,
        image = %plan.image_path.display(),
        ops = plan.ops.len(),
        verify_after = plan.verify_after,
        touches_table = plan.touches_partition_table(),
        "=== restore plan starting ==="
    );
    for (n, op) in plan.ordered().iter().enumerate() {
        match op.part() {
            Some(p) => info!(
                step = n,
                kind = op.kind(),
                part = p.index,
                offset = p.offset,
                size = p.size,
                "plan step: {}",
                op.title()
            ),
            None => info!(step = n, kind = op.kind(), "plan step: {}", op.title()),
        }
    }
    if let Some(t) = plan.table() {
        for line in own_backup_backup::table_merge::describe_table(t) {
            info!(plan = %plan.id, "planned table: {line}");
        }
    }

    // Asked once and kept: the answer cannot change between these two uses of
    // it, and both callers below want the same one.
    let system_disk = own_backup_restore::system_disk_number();
    let boot_media = own_backup_restore::is_boot_media();
    let guard = own_backup_restore::protection(plan.target_disk);
    // What the disk holds RIGHT NOW. Without it, a partition the plan simply
    // leaves out of the new table — which deletes it — counted as untouched,
    // so a protected partition could be removed rather than overwritten and
    // the check below would not notice (audit AUD-01, point 4).
    let current_parts = current_partition_indices(plan.target_disk);
    let touched = plan.touched_partitions_with(&current_parts);
    info!(
        target_disk = plan.target_disk,
        protected = ?guard.parts,
        cannot_tell = ?guard.cannot_tell,
        ?current_parts,
        ?touched,
        ?system_disk,
        boot_media,
        "restore target checked against the running system"
    );
    // An answer that could not be worked out is not an all-clear. It used to
    // be spelled as an empty list, which reads exactly like "nothing to
    // protect here".
    if let Some(why) = &guard.cannot_tell {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "Нельзя проверить, не стоит ли на этом диске работающая система: {why}. \
             Загрузитесь с носителя восстановления — там этот вопрос не стоит."
        )));
    }
    if !guard.parts.is_empty() {
        let hit: Vec<u32> = touched.iter().copied().filter(|i| guard.parts.contains(i)).collect();
        if !hit.is_empty() {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "план затрагивает разделы, на которых стоит работающая Windows: {}. \
                 Загрузитесь с Boot CD и повторите.",
                hit.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let live_system = system_disk == Some(plan.target_disk) && !boot_media;

    let sidecar = load_sidecar_with_keys(&plan.image_path, archive)?;
    info!(
        image = %plan.image_path.display(),
        // Which build wrote this copy — the first thing worth knowing when a
        // restore from it turns out badly.
        made_by = %if sidecar.info.app_version.is_empty() {
            "<до 0.1.1>".to_string()
        } else {
            format!("Own Backup {}", sidecar.info.app_version)
        },
        created = %sidecar.manifest.created_at,
        block_size = sidecar.manifest.block_size,
        blocks = sidecar.checksums.len(),
        source_size = sidecar.manifest.source_disk.size_bytes,
        whole_disk = sidecar.disk_manifest.is_some(),
        data_offset = sidecar.info.volume_data_offset,
        "copy opened"
    );
    let mut state = initial_state(plan);
    on_update(&state);

    let ordered: Vec<RestoreOp> = plan.ordered().into_iter().cloned().collect();

    // The session is only needed when the table is rewritten. Opening it always
    // would take the disk offline for a plan that merely assigns a drive letter.
    let mut session = if plan.touches_partition_table() {
        let mut s = DiskSession::open(plan.target_disk, live_system)?;
        info!(
            disk = plan.target_disk,
            live_system,
            length = s.length(),
            sector = s.sector_size(),
            "disk session opened"
        );
        s.take_offline()?;
        Some(s)
    } else {
        None
    };

    // A live system disk never went offline, so there is no transition to make
    // and its volumes were never taken away.
    let mut online = session.as_ref().map(|s| s.is_live_system()).unwrap_or(true);
    // Was the disk ours alone, from the first sector written to the check? Only
    // then does a mismatch mean the write did not stick.
    let held_offline = !online;
    let mut sectors_verified = false;

    for (index, op) in ordered.iter().enumerate() {
        if cancel.is_cancelled() {
            finish_cancelled(&mut state, index);
            on_update(&state);
            return Ok(state);
        }

        // One transition, at the first operation that needs a mounted volume.
        if !online && op.needs_disk_online() {
            // The last moment the disk is still ours. Everything past this
            // point — bcdboot, the fallback loader, Windows itself the instant
            // a volume appears — writes to it, and those writes are not in the
            // copy: a Recovery partition came back with three blocks changed
            // and the check called it "запись не легла (проверьте накопитель)"
            // about a perfectly good disk (2026-09-20). Read it back now.
            if plan.verify_after && !sectors_verified {
                run_verification(
                    plan,
                    &sidecar,
                    &ordered,
                    passphrases,
                    archive,
                    VerifyPhase::Sectors,
                    !held_offline,
                    &mut state,
                    cancel,
                    &mut on_update,
                );
                sectors_verified = true;
            }
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
        info!(
            step = index,
            kind = op.kind(),
            destroys_data = op.destroys_data(),
            "restore operation starting: {}",
            op.title()
        );

        let outcome = run_op(op, plan, &sidecar, passphrases, archive, session.as_mut(), cancel, &mut |percent, bytes| {
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
                info!(op = %op.kind(), index, note = %state.ops[index].message, "restore operation done");
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

    // A plan that never needed a mounted volume gets its check here — still
    // before the disk goes back, for the same reason.
    if plan.verify_after && !sectors_verified {
        run_verification(
            plan,
            &sidecar,
            &ordered,
            passphrases,
            archive,
            VerifyPhase::Sectors,
            !held_offline,
            &mut state,
            cancel,
            &mut on_update,
        );
    }

    // Bring the disk back before declaring success, so the user sees their
    // volumes rather than a disk that is technically restored and invisible.
    if let Some(s) = session.as_mut() {
        s.update_properties()?;
        s.bring_online()?;
    }
    drop(session);

    // Files went INTO a mounted volume, so they can only be checked with the
    // volume in place — which is now.
    if plan.verify_after {
        run_verification(
            plan,
            &sidecar,
            &ordered,
            passphrases,
            archive,
            VerifyPhase::Files,
            true,
            &mut state,
            cancel,
            &mut on_update,
        );
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

/// Which half of the check runs now.
///
/// A sector restore is checked against the copy's checksums and wants a disk
/// nobody has touched since we wrote it — so it runs while the disk is still
/// offline, before anything can mount a volume. A file restore copied files
/// INTO a mounted volume and can only be checked with that volume in place, so
/// it waits until the disk is back.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifyPhase {
    Sectors,
    Files,
}

/// Read back every sector-restored partition and check it against the copy
/// (R9, D10).
///
/// A verification failure marks the whole run failed, and deliberately so: the
/// bytes on the disk are not the bytes in the copy, which is what a restore was
/// supposed to deliver. Reporting "Завершено" with a failed check buried inside
/// would invite exactly the wrong conclusion.
///
/// The follow-up still separates the two causes — a damaged backup, or a target
/// that did not take the write — because they call for different actions.
#[cfg(target_os = "windows")]
fn run_verification(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    ordered: &[RestoreOp],
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    phase: VerifyPhase,
    // Could anything have written to these partitions after the copy landed?
    // True only when the disk was never ours to hold offline — a live system
    // disk. With the disk offline a mismatch means exactly one thing, and
    // excusing it would be excusing the failure this check exists to find.
    writes_possible_after_copy: bool,
    state: &mut RestoreRunState,
    cancel: &CancelToken,
    on_update: &mut impl FnMut(&RestoreRunState),
) {
    use own_backup_backup::restore_win::{verify_partition_sectors, PartitionTarget};

    let restore_ops: Vec<&RestoreOp> = ordered
        .iter()
        .filter(|op| matches!(op, RestoreOp::RestorePartition { .. }))
        .collect();

    for op in restore_ops.iter() {
        let RestoreOp::RestorePartition { src_index, dst, mode, .. } = op else { continue };
        // Not this pass: leave it Pending for the other one, untouched.
        let belongs_here = match mode {
            RestoreMode::Files => phase == VerifyPhase::Files,
            RestoreMode::Sectors => phase == VerifyPhase::Sectors,
        };
        if !belongs_here {
            continue;
        }
        // The record sits where the check RUNS, not at a fixed offset from the
        // end of the plan — so it is found by what it is, not by arithmetic.
        let kind = match mode {
            RestoreMode::Sectors => "verify_sectors",
            RestoreMode::Files => "verify_files",
        };
        let Some(index) =
            state.ops.iter().position(|o| o.kind == kind && o.part_index == Some(dst.index))
        else {
            continue;
        };
        if cancel.is_cancelled() {
            state.ops[index].status = OpStatus::Skipped;
            continue;
        }

        state.current = Some(index);
        state.ops[index].status = OpStatus::Running;
        state.cancellable = true;
        on_update(state);

        if *mode == RestoreMode::Files {
            verify_files(plan, sidecar, archive, *src_index, dst.index, dst.offset, index, state);
            on_update(state);
            continue;
        }

        let target =
            PartitionTarget { disk_number: plan.target_disk, offset: dst.offset, size: dst.size };
        // A re-encrypted container holds ciphertext past its header: to compare
        // it with the copy the check has to open it the same way the restore
        // wrote it. Without this every block read as different — 12691 of 12691
        // on a partition whose restored system booted fine.
        let reencrypted = matches!(op, RestoreOp::RestorePartition { reencrypt: true, .. });
        let crypt = if reencrypted {
            match passphrases.get(src_index) {
                Some(pw) => {
                    let path = format!("part:{}:{}:{}", plan.target_disk, dst.offset, dst.size);
                    match own_backup_backup::win_device::WinRawDevice::open_read(&path)
                        .and_then(|mut d| own_backup_backup::luks::open_luks(&mut d, pw.as_bytes()))
                    {
                        Ok(p) => Some(p),
                        Err(e) => {
                            warn!(part = dst.index, error = %e, "verification: cannot open the restored container");
                            None
                        }
                    }
                }
                None => None,
            }
        } else {
            None
        };
        if reencrypted && crypt.is_none() {
            // Better to say the check did not happen than to report a failure
            // that only means we could not read what we wrote.
            state.ops[index].status = OpStatus::Done;
            state.ops[index].percent = 100;
            state.ops[index].message =
                "Не проверено: контейнер перешифрован, а открыть его не удалось".into();
            on_update(state);
            continue;
        }
        let result = verify_partition_sectors(
            &plan.image_path,
            archive,
            sidecar,
            *src_index,
            target,
            crypt.as_ref(),
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
            // The restore itself writes to the ESP after the copy is in place:
            // bcdboot, and the fallback loader at \EFI\BOOT\BOOTX64.EFI. Those
            // blocks differ from the copy BECAUSE the restore changed them, and
            // reporting that as "запись не легла (проверьте накопитель)" sends
            // the user to look for a failing disk that is working perfectly.
            Ok(v)
                if v.image_corrupt.is_empty()
                    && writes_possible_after_copy
                    && touched_after_copy(ordered, dst.index) =>
            {
                state.ops[index].status = OpStatus::Done;
                state.ops[index].percent = 100;
                state.ops[index].message = format!(
                    "Сверено блоков: {}; отличаются {} — в этот раздел после копирования \
                     дописан загрузчик, это ожидаемо",
                    v.blocks_checked,
                    v.mismatches.len()
                );
                info!(
                    part = dst.index,
                    blocks = v.blocks_checked,
                    changed = v.mismatches.len(),
                    "verification passed (bootloader written after the copy)"
                );
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

/// Did the plan write into this partition after its data was copied?
///
/// Only the boot steps do: `bcdboot` rebuilds the Windows loader, and the
/// fallback step drops `\EFI\BOOT\BOOTX64.EFI` on the ESP. Both are asked for
/// deliberately, and both make the partition differ from the copy on purpose.
#[cfg(target_os = "windows")]
fn touched_after_copy(ordered: &[RestoreOp], part_index: u32) -> bool {
    ordered.iter().any(|op| match op {
        RestoreOp::EnsureFallbackBootloader { esp } => esp.index == part_index,
        RestoreOp::FinalizeBoot { esp, windows_part, .. } => {
            esp.map(|e| e.index == part_index).unwrap_or(false) || windows_part.index == part_index
        }
        _ => false,
    })
}

/// Restore the GPT fields the designer cannot know, from the copy's own
/// captured layout.
///
/// The partition map in the interface is built out of `PartitionInfo`, which
/// describes *volumes*: label, filesystem, size. It has no room for a GPT
/// entry's attribute flags or its entry name, so a table rebuilt from it comes
/// out with attributes zeroed — and those flags are not decoration:
///
/// - bit 0 (Required Partition) marks the ESP and the Microsoft Reserved
///   partition; clearing it invites tools to treat them as ordinary volumes;
/// - bit 63 (no drive letter) is what keeps a Recovery partition from
///   appearing as a drive the user can open and empty.
///
/// For every partition that came out of the copy, the values are taken from the
/// image's own captured GPT, which is exactly the layout being reproduced.
/// Hand-created partitions keep what the designer gave them.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn with_captured_gpt_fields(
    table: &own_backup_common::restore_plan::PartitionTableSpec,
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    taken: &IdentitiesInUse,
) -> (own_backup_common::restore_plan::PartitionTableSpec, IdentityOutcome) {
    use own_backup_common::restore_plan::{PartitionKind, PartitionScheme};

    let mut outcome = IdentityOutcome::default();
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return (table.clone(), outcome);
    };
    // Which target partition receives which source partition.
    let src_of: std::collections::HashMap<u32, u32> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { src_index, dst, .. } => Some((dst.index, *src_index)),
            _ => None,
        })
        .collect();

    let mut out = table.clone();
    for e in out.entries.iter_mut() {
        let Some(src_index) = src_of.get(&e.index) else { continue };
        let Some(captured) = dm.layout.partitions.iter().find(|p| p.index == *src_index) else {
            continue;
        };
        let index = e.index;
        // `ReAgent.xml` records where WinRE lives as disk GUID + partition
        // BYTE OFFSET, so a partition that lands somewhere else takes the
        // recovery environment with it even when every GUID is reproduced.
        if captured.first_lba != e.first_lba {
            outcome.partitions_moved.push(index);
        }
        if let PartitionKind::Gpt { type_guid, part_guid, attributes, name } = &mut e.kind {
            *attributes = captured.attributes;
            *name = captured.name.clone();
            // The type GUID and the unique GUID also come from the copy: the
            // BCD and UEFI reference partitions by the unique GUID, and losing
            // it is what turns a bootable restore into an unbootable one.
            if !captured.type_guid.is_empty() {
                *type_guid = captured.type_guid.clone();
            }
            if !captured.part_guid.is_empty() {
                if taken.holds_partition(&captured.part_guid) {
                    // Another disk in this machine already answers to that GUID
                    // — most often the source disk itself, still attached. Two
                    // partitions with one identity is what makes the mount
                    // manager hand a drive letter to the wrong volume, so this
                    // one gets a fresh GUID (empty = the writer generates one).
                    part_guid.clear();
                    outcome.partitions_regenerated.push(index);
                } else {
                    *part_guid = captured.part_guid.clone();
                }
            }
        }
    }

    // The disk's own GUID, and with it the second half of every BCD device
    // element: those name a partition by *both* its GUID and its disk's. A
    // restored disk that reproduces both boots on the copy's own boot entries —
    // WinRE and `ReAgent.xml` (which records the disk GUID) included — and
    // `bcdboot` then updates that entry instead of adding a second one. When
    // the source disk is still attached, reproducing its GUID would put two
    // disks with one identity in the machine and Windows takes one of them
    // offline, so the target keeps a fresh GUID and the boot chain is rebuilt.
    if out.scheme == PartitionScheme::Gpt && !dm.layout.disk_guid.is_empty() {
        if taken.holds_disk(&dm.layout.disk_guid) {
            outcome.disk_guid_taken = Some(dm.layout.disk_guid.clone());
        } else {
            out.disk_id = dm.layout.disk_guid.clone();
            outcome.disk_guid_preserved = true;
        }
    }
    (out, outcome)
}

/// GPT identities that disks attached to this machine already answer to.
///
/// A restored disk may reproduce the source's GUIDs only while nothing else
/// holds them: duplicates make Windows take a disk offline (the disk GUID) or
/// hand a remembered drive letter to the wrong volume (a partition GUID).
#[derive(Debug, Default, Clone)]
pub struct IdentitiesInUse {
    disk_guids: std::collections::BTreeSet<String>,
    part_guids: std::collections::BTreeSet<String>,
    /// Disks the scan could read, for the log (Windows-only; the scan does not
    /// exist elsewhere).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    scanned: Vec<u32>,
}

impl IdentitiesInUse {
    fn normalize(guid: &str) -> String {
        guid.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_uppercase()
    }

    fn holds_disk(&self, guid: &str) -> bool {
        self.disk_guids.contains(&Self::normalize(guid))
    }

    fn holds_partition(&self, guid: &str) -> bool {
        self.part_guids.contains(&Self::normalize(guid))
    }
}

/// What the identity decision came to, for the log and the restore report.
#[derive(Debug, Default)]
pub struct IdentityOutcome {
    /// The restored disk carries the source disk's own GUID.
    disk_guid_preserved: bool,
    /// The source disk's GUID is in use here, so a fresh one was used instead.
    disk_guid_taken: Option<String>,
    /// Target partitions that had to get a fresh GUID because the source's is
    /// in use on this machine.
    partitions_regenerated: Vec<u32>,
    /// Target partitions that start at a different offset than on the source.
    /// Identity by GUID survives that; `ReAgent.xml`'s offset-based reference
    /// to WinRE does not.
    partitions_moved: Vec<u32>,
}

/// Read the GPT identities of every disk attached to this machine except
/// `except_disk` (the restore target, whose table is about to be replaced).
///
/// Disks are probed by number rather than asked of the inventory: this runs in
/// the middle of a restore and wants one fact per disk, not a full sweep. A
/// disk that cannot be opened or has no GPT contributes nothing —
/// which is correct, it holds no GUID we could collide with.
/// The partition numbers the target disk holds right now.
///
/// Needed so that a partition the plan simply LEAVES OUT of the new table is
/// counted as touched: leaving it out deletes it, which is every bit as
/// destructive as writing over it (audit AUD-01, point 4). Empty when the disk
/// has no readable GPT — a disk with nothing on it has nothing to lose.
#[cfg(target_os = "windows")]
fn current_partition_indices(disk: u32) -> Vec<u32> {
    own_backup_backup::gpt_reader::read_disk_layout(disk)
        .map(|l| l.partitions.iter().map(|p| p.index).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn identities_in_use(except_disk: u32) -> IdentitiesInUse {
    const MAX_DISKS: u32 = 32;
    let mut out = IdentitiesInUse::default();
    for n in 0..MAX_DISKS {
        if n == except_disk {
            continue;
        }
        let Ok(layout) = own_backup_backup::gpt_reader::read_disk_layout(n) else { continue };
        out.scanned.push(n);
        if !layout.disk_guid.is_empty() {
            out.disk_guids.insert(IdentitiesInUse::normalize(&layout.disk_guid));
        }
        for p in &layout.partitions {
            if !p.part_guid.is_empty() {
                out.part_guids.insert(IdentitiesInUse::normalize(&p.part_guid));
            }
        }
    }
    out
}

/// Compare what arrived with what the copy holds, for a file-by-file restore.
///
/// Block checksums mean nothing here — the bytes are laid out differently by
/// construction. What can be compared is the count and the total size, which
/// catches the failure that matters: files that did not make it. Individual
/// copy failures are already reported by the restore operation itself; this is
/// the independent second look.
#[cfg(target_os = "windows")]
// Eight: the plan, what the image says about itself, the key that opens it,
// which partition on each side, and where to put the answer.
#[allow(clippy::too_many_arguments)]
fn verify_files(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    src_index: u32,
    dst_index: u32,
    dst_offset: u64,
    index: usize,
    state: &mut RestoreRunState,
) {
    use own_backup_backup::file_restore_win::{long_path, measure_tree};
    use own_backup_backup::restore_win::MountedImage;
    use own_backup_backup::volume_win::volume_root_at_offset;
    use own_backup_common::cancel::CancelToken;

    let fail = |state: &mut RestoreRunState, msg: String| {
        state.ops[index].status = OpStatus::Failed;
        state.ops[index].message = msg;
    };

    let quiet = CancelToken::default();
    let image = match MountedImage::open(&plan.image_path, archive) {
        Ok(i) => i,
        Err(e) => return fail(state, format!("не удалось подключить копию для сверки: {e}")),
    };
    let src_root = match image.volume_root_of(sidecar, src_index) {
        Ok(r) => r,
        Err(e) => return fail(state, e.to_string()),
    };
    let Some(dst_root) = volume_root_at_offset(plan.target_disk, dst_offset) else {
        return fail(state, format!("Windows не показала том раздела {dst_index} — сверять не с чем"));
    };

    let want = measure_tree(&long_path(std::path::Path::new(&src_root)), &quiet);
    let got = measure_tree(&long_path(std::path::Path::new(&dst_root)), &quiet);

    if want.files == got.files && want.bytes == got.bytes {
        state.ops[index].status = OpStatus::Done;
        state.ops[index].percent = 100;
        state.ops[index].message =
            format!("Файлов: {}, суммарно {}", got.files, own_backup_common::units::human_bytes(got.bytes));
        info!(part = dst_index, files = got.files, "file verification passed");
        return;
    }

    let text = format!(
        "в копии {} файлов на {}, в разделе {} файлов на {}",
        want.files,
        own_backup_common::units::human_bytes(want.bytes),
        got.files,
        own_backup_common::units::human_bytes(got.bytes)
    );
    error!(part = dst_index, %text, "file verification failed");
    fail(state, text.clone());
    state.follow_ups.push(format!("Раздел {dst_index}: перенос файлов неполный — {text}."));
}

/// The saved container header for one source partition, if the copy has one.
///
/// A per-volume image holds exactly one, and its recorded index is 0 because
/// there was nothing to disambiguate at backup time — so a lone entry answers
/// for whichever partition is asked about.
pub fn luks_header_for(
    sidecar: &own_backup_backup::sidecar::Sidecar,
    src_index: u32,
) -> Option<&own_backup_common::image::LuksHeaderBackup> {
    let headers = &sidecar.info.luks_headers;
    match headers.as_slice() {
        [only] if sidecar.disk_manifest.is_none() => Some(only),
        _ => headers.iter().find(|h| h.part_index == src_index),
    }
}

/// What to tell the user when the copy held a decrypted container (D14).
///
/// The restore is faithful — it writes exactly what the backup holds — and what
/// the backup holds is a plain filesystem where a LUKS container used to be. A
/// Linux system on such a partition mounts fine from a Live CD and refuses to
/// boot on its own, which is a confusing failure unless someone says why.
#[cfg(target_os = "windows")]
fn encryption_follow_up(
    sidecar: &own_backup_backup::sidecar::Sidecar,
    part_index: u32,
) -> Vec<String> {
    let was_encrypted = sidecar
        .manifest
        .volume_encryption
        .as_ref()
        .is_some_and(|v| v.capture_mode == own_backup_common::image::VolumeCaptureMode::Plaintext);
    if !was_encrypted {
        return Vec::new();
    }
    vec![format!(
        "Раздел {part_index}: исходный том был зашифрован, а в копии он лежит расшифрованным — \
         контейнер LUKS не восстанавливается. Данные на месте, но система с этого раздела \
         не загрузится, пока не убрать запись из /etc/crypttab, не поправить /etc/fstab на \
         UUID самого раздела и не пересобрать initramfs (с Live CD)."
    )]
}

/// Anything an operation wants to tell the user afterwards.
#[cfg(target_os = "windows")]
#[derive(Default)]
struct OpNote {
    message: String,
    follow_ups: Vec<String>,
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn run_op(
    op: &RestoreOp,
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    // Passphrase per SOURCE partition, for putting its container back (D19).
    // Kept out of the plan on purpose: the plan is persisted to the restore
    // history, and a password on disk in the clear is not a trade worth making.
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
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
            let taken = identities_in_use(plan.target_disk);
            let (table, identity) = with_captured_gpt_fields(table, plan, sidecar, &taken);
            info!(
                scanned_disks = ?taken.scanned,
                disk_guid_preserved = identity.disk_guid_preserved,
                disk_guid_taken = ?identity.disk_guid_taken,
                partitions_regenerated = ?identity.partitions_regenerated,
                partitions_moved = ?identity.partitions_moved,
                "identity of the source disk on the restored disk"
            );
            // Everything the plan is not acting on is a bystander: its GPT
            // entry belongs to the disk, not to the designer.
            let owned = plan.touched_partitions();
            // A restore into an existing partition does not change the layout,
            // and then the table is left exactly as it is. Saying so matters:
            // "разметка перезаписана" in the report of a run that touched one
            // spare partition is how a user concludes their disk was rebuilt.
            let written = session.write_partition_table(&table, &owned)?;
            let mut note = if written {
                OpNote::default()
            } else {
                OpNote {
                    message: "Разметка уже соответствует плану — таблица разделов не менялась"
                        .into(),
                    follow_ups: Vec::new(),
                }
            };
            if identity.disk_guid_preserved {
                note.message = if note.message.is_empty() {
                    "Идентификаторы исходного диска воспроизведены — записи загрузки и среда \
                     восстановления (WinRE) из копии продолжают работать"
                        .into()
                } else {
                    note.message
                };
                if !identity.partitions_moved.is_empty() {
                    note.follow_ups.push(format!(
                        "Разделы {:?} начинаются не там, где на исходном диске. Записи загрузки \
                         это переживут (они ссылаются на GUID), а среда восстановления WinRE — \
                         нет: ReAgent.xml помнит смещение раздела. Если «Особые варианты \
                         загрузки» не работают, выполните в восстановленной системе: reagentc \
                         /setreimage /path C:\\Recovery\\WindowsRE, затем reagentc /enable.",
                        identity.partitions_moved
                    ));
                }
            } else if let Some(guid) = &identity.disk_guid_taken {
                note.follow_ups.push(format!(
                    "Идентификатор исходного диска ({guid}) уже занят другим подключённым диском, \
                     поэтому восстановленный диск получил новый. Загрузчик пересоздан bcdboot и \
                     система стартует, но записи загрузки из копии (в том числе среда \
                     восстановления WinRE) указывают на прежний диск. Чтобы вернуть WinRE, \
                     выполните в восстановленной системе: reagentc /setreimage /path \
                     C:\\Recovery\\WindowsRE, затем reagentc /enable."
                ));
            }
            if !identity.partitions_regenerated.is_empty() {
                note.follow_ups.push(format!(
                    "Разделам {:?} выданы новые GUID: их идентификаторы из копии уже заняты \
                     другим подключённым диском, а два раздела с одним GUID путают назначение \
                     букв дисков.",
                    identity.partitions_regenerated
                ));
            }
            Ok(note)
        }

        RestoreOp::RestorePartition {
            src_index,
            dst,
            mode: RestoreMode::Sectors,
            wipe_free_space,
            reencrypt,
        } => {
            let target = PartitionTarget {
                disk_number: plan.target_disk,
                offset: dst.offset,
                size: dst.size,
            };
            // Written down before the first byte goes out, not after the last:
            // a log that only reports finished writes says nothing about the
            // one that was in flight when the machine went down.
            info!(
                disk = plan.target_disk,
                part = dst.index,
                offset = dst.offset,
                size = dst.size,
                src_index,
                wipe_free_space,
                reencrypt,
                "writing sectors into part:{}:{}:{}",
                plan.target_disk,
                dst.offset,
                dst.size
            );
            let opts = SectorRestoreOptions {
                verify_checksums: true,
                wipe_free_space: *wipe_free_space,
                // When we hold a session the disk is offline and there is
                // nothing left to dismount.
                // A live system disk stays online, so its volumes are still
                // mounted and the targeted dismount guard is exactly what is
                // needed. Only a disk we took offline has nothing left to guard.
                disk_already_offline: session.as_ref().is_some_and(|s| !s.is_live_system()),
            };
            let report = |pr: &own_backup_backup::restore_win::SectorRestoreProgress,
                          on_progress: &mut dyn FnMut(u32, u64)| {
                let percent = if pr.blocks_total > 0 {
                    (pr.blocks_done * 100 / pr.blocks_total) as u32
                } else {
                    0
                };
                on_progress(percent, pr.bytes_written);
            };

            let summary = if *reencrypt {
                use own_backup_backup::restore_win::{restore_partition_encrypted, ReencryptRequest};
                let header = luks_header_for(sidecar, *src_index).ok_or_else(|| {
                    OwnBackupError::RestoreIncompatible(format!(
                        "в копии нет сохранённого заголовка контейнера для раздела {src_index} — \
                         вернуть шифрование нечем"
                    ))
                })?;
                // Keyed by the SOURCE partition: the passphrase belongs to the
                // container, and the container is identified by where it came
                // from, not by where it is going.
                let passphrase = passphrases.get(src_index).ok_or_else(|| {
                    OwnBackupError::Config(format!(
                        "для раздела {src_index} копии не передан пароль, а план требует \
                         вернуть шифрование"
                    ))
                })?;
                restore_partition_encrypted(
                    &plan.image_path,
                    archive,
                    sidecar,
                    *src_index,
                    target,
                    opts,
                    &ReencryptRequest { header, passphrase },
                    cancel,
                    |pr| {
                        report(pr, on_progress);
                        Ok(())
                    },
                )?
            } else {
                restore_partition_sectors(
                    &plan.image_path,
                    archive,
                    sidecar,
                    *src_index,
                    target,
                    opts,
                    cancel,
                    |pr| {
                        report(pr, on_progress);
                        Ok(())
                    },
                )?
            };
            if summary.cancelled {
                return Err(OwnBackupError::Cancelled);
            }
            if *reencrypt {
                return Ok(OpNote {
                    message: "Контейнер восстановлен вместе с прежним паролем".into(),
                    follow_ups: Vec::new(),
                });
            }
            Ok(OpNote { message: String::new(), follow_ups: encryption_follow_up(sidecar, dst.index) })
        }

        RestoreOp::RestorePartition { src_index, dst, mode: RestoreMode::Files, .. } => {
            use own_backup_backup::file_restore_win::{copy_tree, long_path, measure_tree};
            use own_backup_backup::restore_win::MountedImage;

            // The copy is read through a read-only mount of the image, so the
            // filesystem driver does the reading and every file's metadata
            // comes along. Detached when `image` drops, including on failure.
            let image = MountedImage::open(&plan.image_path, archive)?;
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
            use own_backup_backup::letters_win::TempDriveLetter;
            let win_volume = volume_root_at_offset(plan.target_disk, windows_part.offset)
                .ok_or_else(|| {
                    OwnBackupError::Device(
                        "Windows не показала том системного раздела — загрузчик не пересоздать"
                            .into(),
                    )
                })?;
            // bcdboot takes paths, not volume GUIDs: handed
            // `\\?\Volume{…}\Windows` it fails with exit 123
            // (ERROR_INVALID_NAME) and says nothing on stderr, which is exactly
            // how a restored Windows ended up with no loader and a report that
            // explained nothing. A restored partition normally has no letter —
            // automount is off on purpose — so it gets one for the duration.
            let win_mount = TempDriveLetter::acquire(&win_volume)?;
            let esp_mount = match esp {
                Some(p) => {
                    let vr = volume_root_at_offset(plan.target_disk, p.offset).ok_or_else(|| {
                        OwnBackupError::Device("Windows не показала том раздела EFI".into())
                    })?;
                    Some(TempDriveLetter::acquire(&vr)?)
                }
                None => None,
            };
            // Drive roots, without the trailing backslash bcdboot dislikes.
            let win_arg = win_mount.mount_point().trim_end_matches('\\').to_string();
            let esp_arg = esp_mount
                .as_ref()
                .map(|m| m.mount_point().trim_end_matches('\\').to_string());
            info!(win = %win_arg, esp = ?esp_arg, uefi = *uefi, "bcdboot: recreating the loader");
            let report = finalize_boot(&win_arg, esp_arg.as_deref(), *uefi)?;
            let mut follow_ups = vec![
                "Загрузчик пересоздан командой bcdboot. Если система не стартует, \
                 проверьте порядок загрузки в UEFI."
                    .into(),
            ];
            // The ESP came out of the copy, so its BCD still describes the
            // source disk: those entries point at GPT GUIDs this disk does not
            // have and end in 0xc000000e. They are hidden, not deleted.
            if report.menu_trimmed {
                follow_ups.push(format!(
                    "В скопированном хранилище загрузки было записей Windows: {}. В меню оставлена \
                     одна — созданная bcdboot для восстановленной системы; остальные указывали на \
                     разделы исходного диска и не загрузились бы. Система стартует без вопросов.",
                    report.loaders_found
                ));
            }
            if let Some(note) = report.note {
                follow_ups.push(note);
            }
            // A copy is taken from a running system, so its recorded shutdown
            // never happened and the first boot would stop at the recovery
            // screen (0xc0000001). Removing bootstat.dat is what makes the
            // restored machine start straight into Windows the first time.
            let message = if report.boot_status_cleared {
                "Снят признак незавершённого выключения — первая загрузка пройдёт без экрана \
                 восстановления"
                    .to_string()
            } else {
                String::new()
            };
            Ok(OpNote { message, follow_ups })
        }

        RestoreOp::EnsureFallbackBootloader { esp } => {
            use own_backup_backup::esp_fallback_win::ensure_fallback_bootloader;
            // Which partitions this run wrote: their filesystem UUIDs are what
            // tells one bootloader directory on the ESP from another, and an
            // ESP that has served a machine for years has several.
            let restored: Vec<_> = plan
                .ops
                .iter()
                .filter_map(|op| match op {
                    RestoreOp::RestorePartition { dst, .. } => Some(*dst),
                    _ => None,
                })
                .collect();
            let outcome = ensure_fallback_bootloader(plan.target_disk, *esp, &restored)?;
            // Say something only when something was done. This step is hidden
            // from the queue precisely because it usually has nothing to do,
            // and a paragraph about NVRAM entries under a restore that changed
            // nothing is noise.
            Ok(OpNote {
                message: outcome.message,
                follow_ups: if outcome.copied {
                    vec![format!(
                        "На разделе EFI не было \\EFI\\BOOT\\BOOTX64.EFI — единственного пути, \
                         который UEFI пробует без записи в NVRAM. Скопировали туда загрузчик из \
                         EFI\\{}, иначе на новой машине диск не стартовал бы.",
                        outcome.from
                    )]
                } else {
                    outcome.follow_ups
                },
            })
        }

        RestoreOp::RestoreBootEntries => {
            use own_backup_backup::efivars_win;

            let mut note = OpNote::default();
            // From the sidecar this run already opened — with the archive key
            // when the copy is encrypted. Re-reading the file keyless got
            // "there are none" from a copy that had four.
            let Some(saved) =
                own_backup_backup::sidecar::boot_entries_of(sidecar, &plan.image_path)
            else {
                // The plan asked for them and the copy has none. Not a failure
                // of the restore — everything on the disk is written — but the
                // user was shown a list and is owed the news that it did not go.
                note.message = "Записей загрузки в копии не оказалось".into();
                note.follow_ups.push(
                    "Копия не несёт записей загрузки UEFI, поэтому вернуть их не из чего. \
                     Если машина не предложит загрузиться, добавьте запись в прошивке \
                     вручную или через «Записи загрузки»."
                        .into(),
                );
                return Ok(note);
            };

            // Rule one: a copy of what is there NOW, before anything is
            // written, taken without asking. Firmware has no undo of its own
            // and the file costs a few kilobytes. On a boot medium it lands on
            // the RAM disk and does not outlive the restart — the user is told
            // which of the two it was rather than left to assume.
            match efivars_win::safety_copy() {
                Ok(copy) => {
                    note.follow_ups.push(format!(
                        "Прежние записи загрузки сохранены в {}{}",
                        copy.path.display(),
                        if copy.durable {
                            "."
                        } else {
                            " — это временный диск, файл не переживёт перезагрузку."
                        }
                    ));
                }
                Err(e) => {
                    warn!(error = %e, "restore: could not take a safety copy of the boot entries");
                    note.follow_ups.push(format!(
                        "Не удалось сохранить прежние записи загрузки ({e}) — они будут \
                         заменены без копии."
                    ));
                }
            }

            let here = std::env::var("COMPUTERNAME").unwrap_or_default();
            if !here.is_empty() && !saved.machine.eq_ignore_ascii_case(&here) {
                // The entries name partitions by GUID. From another machine
                // those GUIDs are somebody else's.
                note.follow_ups.push(format!(
                    "Копия снята на машине «{}», а восстановление идёт на «{here}». \
                     Записи указывают на разделы по их GUID, и здесь таких разделов \
                     может не быть — пункты появятся в меню и не заработают.",
                    saved.machine
                ));
            }

            match efivars_win::apply_import(&saved) {
                Ok(written) => {
                    info!(written, machine = %saved.machine, "restore: boot entries written");
                    note.message = format!(
                        "Записи загрузки возвращены: изменено {written} из {}",
                        saved.variables.len()
                    );
                }
                Err(e) => {
                    // The disk is restored; only the menu is not. Raising this
                    // would report a failed restore for a firmware that
                    // refuses writes — which many boards do.
                    warn!(error = %e, "restore: boot entries could not be written");
                    note.message = "Записи загрузки вернуть не удалось".into();
                    note.follow_ups.push(format!(
                        "Прошивка не приняла записи загрузки ({e}). Диск восстановлен \
                         полностью; добавьте пункт загрузки в настройках прошивки или \
                         через «Записи загрузки»."
                    ));
                }
            }
            Ok(note)
        }

        RestoreOp::RefreshDiskState => {
            use own_backup_backup::disk_session_win::DiskSession;
            // partmgr re-reads the table, so the disk stops presenting the
            // layout it cached before the restore rewrote it. A failure here
            // costs nothing that a reboot does not fix, and the data is already
            // written — so it is reported, not raised.
            let mut note = OpNote::default();
            match DiskSession::open(plan.target_disk, false)
                .and_then(|s| s.update_properties())
            {
                Ok(()) => {
                    note.message = format!("Диск {} перечитан", plan.target_disk);
                }
                Err(e) => {
                    warn!(disk = plan.target_disk, error = %e, "restore: disk re-read failed");
                    note.message = "Диск не удалось перечитать".into();
                    note.follow_ups.push(
                        "Windows мог не перечитать разметку восстановленного диска. \
                         Данные записаны полностью; если раздел не виден в проводнике, \
                         откройте «Управление дисками» или перезагрузитесь."
                            .into(),
                    );
                }
            }
            Ok(note)
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
    _passphrases: &std::collections::HashMap<u32, String>,
    _archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
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
                    reencrypt: false,
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
            reencrypt: false,
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
    fn the_check_stands_where_it_runs_and_not_at_the_end_of_the_list() {
        // На экране список идёт сверху вниз, и человек читает его как порядок
        // работ. Посекторная сверка выполняется, пока диск ещё офлайн, — то
        // есть ДО загрузчика; показанная последней, она заставляла экран
        // противоречить себе: галочки шли по проверкам, а «Пересоздать
        // загрузчик» над ними стоял «ожидает».
        let mut plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
                RestoreOp::FinalizeBoot { windows_part: part(), esp: None, uefi: true },
                RestoreOp::RefreshDiskState,
            ],
        );
        plan.verify_after = true;
        let state = initial_state(&plan);
        let kinds: Vec<&str> = state.ops.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "write_partition_table",
                "restore_sectors",
                "verify_sectors",
                "finalize_boot",
                "refresh_disk_state",
            ]
        );
        // Вставка в середину не должна оставить старую нумерацию.
        assert!(state.ops.iter().enumerate().all(|(i, o)| o.index == i), "{kinds:?}");
    }

    #[test]
    fn a_file_mode_restore_gets_its_own_kind_of_check() {
        // Block checksums say nothing about a file copy — the bytes are laid
        // out differently by construction — so the check is a different one.
        let mut plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::RestorePartition {
                src_index: 1,
                dst: part(),
                mode: RestoreMode::Files,
                wipe_free_space: false,
                reencrypt: false,
            }],
        );
        plan.verify_after = true;
        let state = initial_state(&plan);
        assert_eq!(state.ops.len(), 2);
        assert_eq!(state.ops[1].kind, "verify_files");
    }

    /// A minimal sidecar whose disk manifest carries one captured partition.
    fn sidecar_with(
        entry: own_backup_common::gpt::GptPartitionEntry,
    ) -> own_backup_backup::sidecar::Sidecar {
        sidecar_with_disk(entry, "")
    }

    fn sidecar_with_disk(
        entry: own_backup_common::gpt::GptPartitionEntry,
        disk_guid: &str,
    ) -> own_backup_backup::sidecar::Sidecar {
        use own_backup_backup::sidecar::SidecarInfo;
        use own_backup_common::gpt::{DiskBackupManifest, DiskLayout};
        use own_backup_common::image::ImageManifest;
        use own_backup_common::types::{BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable};

        let disk = own_backup_common::disk::DiskInfo {
            device_id: String::new(),
            model: String::new(),
            serial: String::new(),
            size_bytes: 0,
            sector_size_logical: 512,
            sector_size_physical: 512,
            media_type: String::new(),
            partition_table: PartitionTable::Gpt,
            disk_guid: String::new(),
            partitions: vec![],
        };
        own_backup_backup::sidecar::Sidecar {
            info: SidecarInfo::new_full(String::from("img.vhdx"), 0),
            manifest: ImageManifest {
                backup_id: uuid::Uuid::nil(),
                created_at: String::new(),
                backup_type: BackupType::Full,
                parent_backup_id: None,
                source_os: String::new(),
                source_disk: disk,
                boot_mode: BootMode::Uefi,
                partition_table: PartitionTable::Gpt,
                compression_algo: CompressionAlgo::None,
                compression_level: 0,
                encryption_algo: None,
                encryption_salt: None,
                block_size: 1024 * 1024,
                total_blocks: 0,
                used_blocks: 0,
                backup_mode: BackupMode::UsedSectors,
                checksum_algorithm: "xxhash64".into(),
                preflight_report: None,
                volume_encryption: None,
            },
            checksums: vec![],
            disk_manifest: Some(DiskBackupManifest {
                format_version: DiskBackupManifest::FORMAT_VERSION,
                created_at: String::new(),
                job_id: None,
                disk_number: 0,
                disk_model: String::new(),
                layout: DiskLayout {
                    disk_guid: disk_guid.to_string(),
                    sector_size: 512,
                    size_bytes: 0,
                    partitions: vec![entry],
                },
                images: vec![],
                source_disk: None,
            }),
        }
    }

    #[test]
    fn gpt_attributes_and_names_come_back_from_the_copy() {
        // The designer builds entries out of PartitionInfo, which has no room
        // for attribute flags — so a table rebuilt from it zeroes them. Bit 63
        // is what keeps a Recovery partition from getting a drive letter, and
        // losing it silently changes what the restored disk does.
        let captured = own_backup_common::gpt::GptPartitionEntry {
            index: 7,
            type_guid: "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC".into(),
            part_guid: "11111111-2222-3333-4444-555555555555".into(),
            first_lba: 2048,
            last_lba: 4095,
            attributes: (1 << 63) | 1,
            name: "Basic data partition".into(),
        };
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::RestorePartition {
                    // Target partition 1 receives source partition 7.
                    src_index: 7,
                    dst: part(),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        );
        let (patched, _) =
            with_captured_gpt_fields(&table(), &plan, &sidecar_with(captured), &IdentitiesInUse::default());
        let PartitionKind::Gpt { attributes, name, type_guid, part_guid } = &patched.entries[0].kind
        else {
            panic!("expected a GPT entry");
        };
        assert_eq!(*attributes, (1u64 << 63) | 1, "required + no-drive-letter survive");
        assert_eq!(name, "Basic data partition");
        assert_eq!(type_guid, "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC");
        assert_eq!(part_guid, "11111111-2222-3333-4444-555555555555", "UEFI references this");
    }

    /// A restore onto a disk of its own reproduces the source's identity whole:
    /// the BCD names a partition by its GUID *and* its disk's, so both have to
    /// come back for the copy's own boot entries — WinRE included — to resolve.
    #[test]
    fn the_source_disks_identity_is_reproduced_when_nothing_holds_it() {
        let captured = own_backup_common::gpt::GptPartitionEntry {
            index: 7,
            type_guid: "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC".into(),
            part_guid: "11111111-2222-3333-4444-555555555555".into(),
            first_lba: 2048,
            last_lba: 4095,
            attributes: 0,
            name: "Basic data partition".into(),
        };
        let plan = restoring_plan();
        let sidecar = sidecar_with_disk(captured, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        let (patched, outcome) =
            with_captured_gpt_fields(&table(), &plan, &sidecar, &IdentitiesInUse::default());
        assert_eq!(patched.disk_id, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        assert!(outcome.disk_guid_preserved);
        assert!(outcome.partitions_regenerated.is_empty());
        assert!(outcome.partitions_moved.is_empty(), "the partition lands where it came from");
    }

    /// A partition that lands at another offset is still the same partition by
    /// GUID — but `ReAgent.xml` points at WinRE by offset, so the run has to
    /// say so rather than let the user find out from a dead recovery menu.
    #[test]
    fn a_partition_that_moved_is_reported_even_when_its_guid_is_kept() {
        let captured = own_backup_common::gpt::GptPartitionEntry {
            index: 7,
            type_guid: "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC".into(),
            part_guid: "11111111-2222-3333-4444-555555555555".into(),
            first_lba: 34, // the copy had it right after the GPT head
            last_lba: 4095,
            attributes: 0,
            name: "Basic data partition".into(),
        };
        let plan = restoring_plan();
        let sidecar = sidecar_with_disk(captured, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        let (_, outcome) =
            with_captured_gpt_fields(&table(), &plan, &sidecar, &IdentitiesInUse::default());
        assert!(outcome.disk_guid_preserved);
        assert_eq!(outcome.partitions_moved, vec![1]);
    }

    /// …and refuses to when the source disk is still attached: two disks with
    /// one GUID make Windows take one of them offline, and two partitions with
    /// one GUID send a remembered drive letter to the wrong volume.
    #[test]
    fn an_identity_another_attached_disk_answers_to_is_not_reproduced() {
        let captured = own_backup_common::gpt::GptPartitionEntry {
            index: 7,
            type_guid: "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC".into(),
            part_guid: "11111111-2222-3333-4444-555555555555".into(),
            first_lba: 2048,
            last_lba: 4095,
            attributes: 0,
            name: "Basic data partition".into(),
        };
        let plan = restoring_plan();
        let sidecar = sidecar_with_disk(captured, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        // Written the way the on-disk GPT spells them — braces and case are
        // noise, and a comparison that trips over them would miss a collision.
        let mut taken = IdentitiesInUse::default();
        taken.disk_guids.insert("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_ascii_uppercase());
        taken.part_guids.insert("11111111-2222-3333-4444-555555555555".into());

        let (patched, outcome) = with_captured_gpt_fields(&table(), &plan, &sidecar, &taken);
        assert_eq!(patched.disk_id, "", "empty means the writer makes a fresh one");
        assert!(!outcome.disk_guid_preserved);
        assert_eq!(outcome.disk_guid_taken.as_deref(), Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"));
        assert_eq!(outcome.partitions_regenerated, vec![1]);
        let PartitionKind::Gpt { part_guid, .. } = &patched.entries[0].kind else {
            panic!("expected a GPT entry");
        };
        assert_eq!(part_guid, "", "the copy's partition GUID is in use here — take a fresh one");
    }

    /// A plan that restores source partition 7 into target partition 1.
    fn restoring_plan() -> RestorePlan {
        RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::WritePartitionTable { table: table() },
                RestoreOp::RestorePartition {
                    src_index: 7,
                    dst: part(),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        )
    }

    #[test]
    fn a_hand_created_partition_keeps_what_the_designer_gave_it() {
        // Nothing restores into it, so there is no captured entry to take from.
        let plan = RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![RestoreOp::WritePartitionTable { table: table() }],
        );
        let captured = own_backup_common::gpt::GptPartitionEntry {
            index: 7,
            type_guid: "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC".into(),
            part_guid: "11111111-2222-3333-4444-555555555555".into(),
            first_lba: 2048,
            last_lba: 4095,
            attributes: 1 << 63,
            name: "Recovery".into(),
        };
        let (patched, outcome) =
            with_captured_gpt_fields(&table(), &plan, &sidecar_with(captured), &IdentitiesInUse::default());
        assert_eq!(patched, table());
        assert!(!outcome.disk_guid_preserved, "the copy carries no disk GUID here");
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
