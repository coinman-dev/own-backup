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
        })
        .collect();

    // Verification is not part of the plan the user assembled — it is a
    // consequence of the "проверять после восстановления" checkbox (D10). It
    // still belongs in the queue, so the user sees up front that it will run
    // and how long the run really is.
    if plan.verify_after {
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
            ops.push(OpRecord {
                index: ops.len(),
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
    passphrases: &std::collections::HashMap<u32, String>,
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

        let outcome = run_op(op, plan, &sidecar, passphrases, session.as_mut(), cancel, &mut |percent, bytes| {
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
    state: &mut RestoreRunState,
    cancel: &CancelToken,
    on_update: &mut impl FnMut(&RestoreRunState),
) {
    use own_backup_backup::restore_win::{verify_partition_sectors, PartitionTarget};

    let restore_ops: Vec<&RestoreOp> = ordered
        .iter()
        .filter(|op| matches!(op, RestoreOp::RestorePartition { .. }))
        .collect();
    let first = ordered.len();

    for (n, op) in restore_ops.iter().enumerate() {
        let RestoreOp::RestorePartition { src_index, dst, mode, .. } = op else { continue };
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

        if *mode == RestoreMode::Files {
            verify_files(plan, sidecar, *src_index, dst.index, dst.offset, index, state);
            on_update(state);
            continue;
        }

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
) -> own_backup_common::restore_plan::PartitionTableSpec {
    use own_backup_common::restore_plan::PartitionKind;

    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return table.clone();
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
                *part_guid = captured.part_guid.clone();
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
fn verify_files(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
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
    let image = match MountedImage::open(&plan.image_path) {
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
            let table = with_captured_gpt_fields(table, plan, sidecar);
            session.write_partition_table(&table)?;
            Ok(OpNote::default())
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
            let opts = SectorRestoreOptions {
                verify_checksums: true,
                wipe_free_space: *wipe_free_space,
                // When we hold a session the disk is offline and there is
                // nothing left to dismount.
                disk_already_offline: session.is_some(),
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
    _passphrases: &std::collections::HashMap<u32, String>,
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
    fn sidecar_with(entry: own_backup_common::gpt::GptPartitionEntry) -> own_backup_backup::sidecar::Sidecar {
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
                    disk_guid: String::new(),
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
        let patched = with_captured_gpt_fields(&table(), &plan, &sidecar_with(captured));
        let PartitionKind::Gpt { attributes, name, type_guid, part_guid } = &patched.entries[0].kind
        else {
            panic!("expected a GPT entry");
        };
        assert_eq!(*attributes, (1u64 << 63) | 1, "required + no-drive-letter survive");
        assert_eq!(name, "Basic data partition");
        assert_eq!(type_guid, "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC");
        assert_eq!(part_guid, "11111111-2222-3333-4444-555555555555", "UEFI references this");
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
        let patched = with_captured_gpt_fields(&table(), &plan, &sidecar_with(captured));
        assert_eq!(patched, table());
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
