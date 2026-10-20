//! Executing a restore plan on Linux (PLAN §6, stage 3).
//!
//! The same plan, order, run state and report as on Windows
//! ([`super::execute_plan`] there); what differs is how the disk is held.
//! Windows takes the disk offline. Linux opens it `O_EXCL` and takes the
//! udev lock (`disk_session_linux`), so every sector of the run — the table,
//! each partition's data, the read-back that checks it — goes through that
//! one descriptor, and the disk is let go only once the sectors are checked:
//! the kernel re-reads the table and udev settles at that point, under a
//! temporary udev hold (`udev_hold_linux`) that keeps udisks, LVM, md and
//! systemd away until the run ends. Steps that need a mounted filesystem (the
//! fallback loader, Windows' boot status) come after, through the partitions'
//! own device nodes.
//!
//! A disk the running system stands on cannot be held whole. Then the plan
//! may only write into partitions nothing uses — each opened `O_EXCL` by
//! itself — and may not change the table.
//!
//! Filesystems are created and grown with the Linux tools after the disk was
//! let go (`restore_fs_linux`).
//!
//! Not done on Linux, and refused before anything is written
//! ([`linux_refusals`]): ReFS, drive letters, file-by-file restore.

use chrono::Utc;
use tracing::{error, info, warn};

use own_backup_backup::disk_session_linux::DiskIo;
use own_backup_backup::restore_disk_linux::{
    open_disk, partition_node, DiskSession, DiskWindow, ReadWindow, UDEV_TIMEOUT,
};
use own_backup_backup::sector_restore::{
    container_fitting, verify_partition_blocks, write_container_blocks, write_partition_blocks,
    ReencryptRequest, SectorRestoreOptions, SectorRestoreProgress, SectorTarget,
};
use own_backup_common::cancel::CancelToken;
use own_backup_common::restore_check::{CheckCode, CheckLevel, CheckResult};
use own_backup_common::restore_plan::{PartRef, RestoreMode, RestoreOp, RestorePlan};
use own_backup_common::restore_state::{OpStatus, RestoreRunState, RunStatus};
use own_backup_common::{tr, OwnBackupError, Result};

use super::{
    add_failure_follow_ups, encryption_follow_up, execution_slots, finish_cancelled, initial_state,
    luks_header_for, with_captured_gpt_fields, IdentitiesInUse,
};

/// Anything an operation wants to tell the user afterwards.
#[derive(Default)]
pub(super) struct OpNote {
    pub(super) message: String,
    pub(super) follow_ups: Vec<String>,
    /// Identities the step changed, and files of the restored system it
    /// edited ([`RestoreOp::ChangeIdentities`]).
    pub(super) identity_changes: Vec<own_backup_common::engine_dto::IdentityChangeDto>,
    pub(super) edited_files: Vec<String>,
}

/// What a plan asks that this engine does not do, as refusals for the plan
/// checks — so the preview says it, not a failure halfway through the run.
pub fn linux_refusals(plan: &RestorePlan) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if plan
        .target_device
        .as_deref()
        .is_none_or(|d| d.trim().is_empty())
    {
        out.push(CheckResult {
            level: CheckLevel::Error,
            code: CheckCode::OpUnsupportedHere,
            message: tr!("restore_exec_linux.no_target_device").into_owned(),
            part_index: None,
        });
    }
    for op in &plan.ops {
        let what = match op {
            RestoreOp::Format {
                fs: own_backup_common::restore_plan::FormatFs::ReFs,
                ..
            } => Some(tr!("restore_exec_linux.no_format_refs")),
            RestoreOp::AssignLetter { .. } => Some(tr!("restore_exec_linux.no_letters")),
            RestoreOp::RestorePartition {
                mode: RestoreMode::Files,
                ..
            } => Some(tr!("restore_exec_linux.no_file_mode")),
            _ => None,
        };
        if let Some(message) = what {
            out.push(CheckResult {
                level: CheckLevel::Error,
                code: CheckCode::OpUnsupportedHere,
                message: message.into_owned(),
                part_index: op.part().map(|p| p.index),
            });
        }
    }
    out
}

/// Partition numbers the disk at `device` holds right now (sysfs). A
/// partition the plan leaves out of the new table is deleted, which is as
/// destructive as writing over it (audit AUD-01, point 4).
fn current_partition_numbers(device: &str) -> Vec<u32> {
    use own_backup_common::linux_block as lb;
    let sys = std::path::Path::new(lb::SYS_CLASS_BLOCK);
    let Some(disk) = lb::kernel_name(std::path::Path::new(device)) else {
        return Vec::new();
    };
    lb::partitions_of(sys, &disk)
        .iter()
        .filter_map(|p| {
            std::fs::read_to_string(sys.join(p).join("partition"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .collect()
}

/// Kernel names the run's udev hold covers. A disk held whole is held with
/// all its partitions. On a disk the running system stands on, only the
/// partitions the plan writes: a hold tells udev, systemd, LVM and md to
/// ignore what it covers, and the partitions the system runs from must not be
/// among them.
fn held_device_names(plan: &RestorePlan, device: &str, live_disk: bool) -> Vec<String> {
    let kernel =
        |path: &str| own_backup_common::linux_block::kernel_name(std::path::Path::new(path));
    if !live_disk {
        return kernel(device).into_iter().collect();
    }
    written_partitions(plan)
        .into_iter()
        .filter_map(|index| partition_node(device, index))
        .filter_map(|node| kernel(&node))
        .collect()
}

/// Partitions whose sectors the plan writes.
fn written_partitions(plan: &RestorePlan) -> Vec<u32> {
    let mut out: Vec<u32> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { dst, .. } => Some(dst.index),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Where a run writes: the whole disk it holds, or — on a disk the running
/// system stands on — one partition at a time.
enum Hold {
    Disk(Box<DiskSession>),
    Partitions,
}

/// Execute `plan` on Linux; the contract is the Windows one: an operation
/// failing is a `Failed` state with a report, `Err` means nothing started.
pub fn execute_plan(
    plan: &RestorePlan,
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    cancel: &CancelToken,
    mut on_update: impl FnMut(&RestoreRunState),
) -> Result<RestoreRunState> {
    let device = plan
        .target_device
        .clone()
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| {
            OwnBackupError::RestoreIncompatible(
                tr!("restore_exec_linux.no_target_device").into_owned(),
            )
        })?;
    info!(
        plan = %plan.id,
        target = %device,
        image = %plan.image_path.display(),
        ops = plan.ops.len(),
        verify_after = plan.verify_after,
        touches_table = plan.touches_partition_table(),
        "=== restore plan starting (Linux) ==="
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
    // A plan can reach the service by a route that skipped the checks.
    if let Some(refusal) = linux_refusals(plan).into_iter().next() {
        return Err(OwnBackupError::RestoreIncompatible(refusal.message));
    }

    let guard = own_backup_restore::protection_of_device(&device);
    let current_parts = current_partition_numbers(&device);
    let touched = plan.touched_partitions_with(&current_parts);
    info!(
        target = %device,
        protected = ?guard.parts,
        cannot_tell = ?guard.cannot_tell,
        ?current_parts,
        ?touched,
        boot_media = own_backup_restore::is_boot_media(),
        "restore target checked against the running system"
    );
    if let Some(why) = &guard.cannot_tell {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_exec.cannot_tell_live_system", why = why).into_owned(),
        ));
    }
    let hit: Vec<String> = touched
        .iter()
        .filter(|i| guard.parts.contains(i))
        .map(|i| i.to_string())
        .collect();
    if !hit.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_exec_linux.touches_live_system",
                parts = hit.join(", ")
            )
            .into_owned(),
        ));
    }
    let live_disk = !guard.parts.is_empty();
    if live_disk && plan.touches_partition_table() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_exec_linux.table_on_live_disk",
                disk = device.as_str()
            )
            .into_owned(),
        ));
    }

    let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(&plan.image_path, archive)?;
    info!(
        image = %plan.image_path.display(),
        made_by = %sidecar.info.app_version,
        created = %sidecar.manifest.created_at,
        block_size = sidecar.manifest.block_size,
        blocks = sidecar.checksums.len(),
        whole_disk = sidecar.disk_manifest.is_some(),
        "copy opened"
    );

    // Installed before the disk is touched and kept to the end of the run;
    // without it the restore still works, the desktop just reacts to it.
    let held = held_device_names(plan, &device, live_disk);
    let _hold = (!held.is_empty())
        .then(|| {
            let names: Vec<&str> = held.iter().map(String::as_str).collect();
            own_backup_backup::udev_hold_linux::UdevHold::install(&names)
                .map_err(|e| warn!(error = %e, "udev hold not installed"))
                .ok()
        })
        .flatten();

    let mut hold = if live_disk {
        Hold::Partitions
    } else {
        let s = open_disk(&device)?;
        info!(
            disk = %device,
            length = s.length(),
            sector = s.sector_size(),
            "disk held exclusively for the restore"
        );
        Hold::Disk(Box::new(s))
    };
    let mut expected: Option<Vec<own_backup_backup::disk_session_linux::KernelPartition>> = None;

    let mut state = initial_state(plan);
    let slots = execution_slots(&state);
    on_update(&state);
    let ordered: Vec<RestoreOp> = plan.ordered().into_iter().cloned().collect();
    let mut released = false;
    let mut sectors_verified = false;

    for (ordered_index, op) in ordered.iter().enumerate() {
        let index = slots[ordered_index];
        if cancel.is_cancelled() {
            finish_cancelled(&mut state, index);
            release(&mut hold, &mut expected, &mut state);
            on_update(&state);
            return Ok(state);
        }
        // The disk is let go at the first step that needs a mounted
        // filesystem — after the sectors written so far have been read back
        // through the same descriptor.
        if !released && op.needs_disk_online() {
            if plan.verify_after && !sectors_verified {
                verify_sectors(
                    plan,
                    &sidecar,
                    &ordered,
                    passphrases,
                    archive,
                    &device,
                    &mut hold,
                    &mut state,
                    cancel,
                    &mut on_update,
                );
                sectors_verified = true;
                if cancel.is_cancelled() {
                    finish_cancelled(&mut state, index);
                    release(&mut hold, &mut expected, &mut state);
                    on_update(&state);
                    return Ok(state);
                }
            }
            release(&mut hold, &mut expected, &mut state);
            released = true;
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
        let outcome = run_op(
            op,
            plan,
            &sidecar,
            passphrases,
            archive,
            &device,
            &mut hold,
            &mut expected,
            cancel,
            &mut |percent, bytes| {
                state.ops[index].percent = percent;
                state.ops[index].bytes_done = bytes;
                on_update(&state);
            },
        );
        match outcome {
            Ok(note) => {
                state.ops[index].status = OpStatus::Done;
                state.ops[index].percent = 100;
                state.ops[index].message = note.message;
                state.follow_ups.extend(note.follow_ups);
                state.identity_changes.extend(note.identity_changes);
                state.edited_files.extend(note.edited_files);
                info!(op = %op.kind(), index, note = %state.ops[index].message, "restore operation done");
            }
            Err(OwnBackupError::Cancelled) => {
                finish_cancelled(&mut state, index);
                release(&mut hold, &mut expected, &mut state);
                on_update(&state);
                return Ok(state);
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
                release(&mut hold, &mut expected, &mut state);
                on_update(&state);
                return Ok(state);
            }
        }
        on_update(&state);
    }

    if plan.verify_after && !sectors_verified {
        verify_sectors(
            plan,
            &sidecar,
            &ordered,
            passphrases,
            archive,
            &device,
            &mut hold,
            &mut state,
            cancel,
            &mut on_update,
        );
    }
    release(&mut hold, &mut expected, &mut state);

    if cancel.is_cancelled() {
        let after_last = state.ops.len();
        finish_cancelled(&mut state, after_last);
        on_update(&state);
        return Ok(state);
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

/// Let the disk go: close it, and when a table was written wait until the
/// kernel and udev show it. Idempotent. A udev that never settles is reported,
/// not raised — every sector is written and checked by then.
fn release(
    hold: &mut Hold,
    expected: &mut Option<Vec<own_backup_backup::disk_session_linux::KernelPartition>>,
    state: &mut RestoreRunState,
) {
    let Hold::Disk(_) = hold else {
        return;
    };
    let Hold::Disk(session) = std::mem::replace(hold, Hold::Partitions) else {
        unreachable!()
    };
    let wanted = expected.take();
    if let Err(e) = session.finish(wanted.as_deref(), UDEV_TIMEOUT, std::thread::sleep) {
        warn!(error = %e, "the disk was written, but udev did not settle on it");
        state
            .follow_ups
            .push(tr!("restore_exec_linux.udev_not_settled", error = e).into_owned());
    }
}

/// A partition's device: written by the restore, read back by the check.
trait PartitionDevice: SectorTarget + own_backup_backup::fat_map::RawVolume {}
impl<T: SectorTarget + own_backup_backup::fat_map::RawVolume> PartitionDevice for T {}

/// How a step uses a partition's sectors.
#[derive(Clone, Copy)]
struct TargetUse {
    /// The plan writes no table of its own, so the partition must already
    /// sit where the plan writes: the kernel's start and size are checked
    /// against the plan's offset and size.
    existing_slot: bool,
    /// The step reads back what was written: the device's cache is dropped
    /// first, or the reads are answered by the page cache the writes filled.
    read_back: bool,
}

impl TargetUse {
    fn for_plan(plan: &RestorePlan, read_back: bool) -> Self {
        Self {
            existing_slot: !plan.touches_partition_table(),
            read_back,
        }
    }
}

/// The target of one partition's sectors under the current hold.
fn with_target<T>(
    hold: &mut Hold,
    device: &str,
    dst: PartRef,
    usage: TargetUse,
    f: impl FnOnce(&mut dyn PartitionDevice) -> Result<T>,
) -> Result<T> {
    match hold {
        Hold::Disk(session) => {
            // A table this plan wrote is the plan's own; without one, the
            // window's offsets are only right if the partition is still
            // where the plan was checked against.
            if usage.existing_slot {
                check_partition_slot(&existing_node(device, dst)?, dst)?;
            }
            let io = session.device_mut();
            if usage.read_back {
                io.drop_cache()?;
            }
            f(&mut DiskWindow::new(io, dst.offset, dst.size))
        }
        Hold::Partitions => {
            let node = existing_node(device, dst)?;
            check_partition_slot(&node, dst)?;
            own_backup_restore::guard_target_volume(&node)?;
            let mut dev = own_backup_backup::linux_device::LinuxRawDevice::open_write(&node)?;
            if usage.read_back {
                dev.drop_cache()?;
            }
            f(&mut dev)
        }
    }
}

/// The kernel's node for the plan's partition `dst` on `device`.
fn existing_node(device: &str, dst: PartRef) -> Result<String> {
    partition_node(device, dst.index).ok_or_else(|| {
        OwnBackupError::Device(
            tr!("restore_exec_linux.no_partition_node", part = dst.index).into_owned(),
        )
    })
}

/// The node of partition `dst` for a step that works on it through the
/// kernel (create or grow a filesystem): it must sit where the plan was
/// checked and nothing may use it — the guard refuses it mounted, swapped on
/// or held by LUKS/LVM/md.
fn free_partition_node(device: &str, dst: PartRef) -> Result<String> {
    let node = existing_node(device, dst)?;
    check_partition_slot(&node, dst)?;
    own_backup_restore::guard_target_volume(&node)?;
    Ok(node)
}

/// The partition node must sit exactly where the plan writes: the plan was
/// checked against offsets, and a number that now names another stretch of
/// the disk is not the partition that was checked.
pub(super) fn check_partition_slot(node: &str, dst: PartRef) -> Result<()> {
    use own_backup_common::linux_block as lb;
    let name = lb::kernel_name(std::path::Path::new(node)).unwrap_or_default();
    check_slot_in(std::path::Path::new(lb::SYS_CLASS_BLOCK), &name, node, dst)
}

/// [`check_partition_slot`] against the sysfs block class at `sys`, for the
/// partition with kernel name `name`.
fn check_slot_in(sys: &std::path::Path, name: &str, node: &str, dst: PartRef) -> Result<()> {
    let read = |f: &str| -> Option<u64> {
        if name.is_empty() {
            return None;
        }
        std::fs::read_to_string(sys.join(name).join(f))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    let (start, size) = (read("start"), read("size"));
    if start.map(|s| s * 512) == Some(dst.offset) && size.map(|s| s * 512) == Some(dst.size) {
        return Ok(());
    }
    Err(OwnBackupError::RestoreIncompatible(
        tr!(
            "restore_exec_linux.partition_moved",
            part = dst.index,
            node = node,
            offset = dst.offset,
            size = dst.size
        )
        .into_owned(),
    ))
}

fn percent_of(pr: &SectorRestoreProgress) -> u32 {
    (pr.blocks_done * 100)
        .checked_div(pr.blocks_total)
        .unwrap_or(0) as u32
}

#[allow(clippy::too_many_arguments)]
fn run_op(
    op: &RestoreOp,
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    device: &str,
    hold: &mut Hold,
    expected: &mut Option<Vec<own_backup_backup::disk_session_linux::KernelPartition>>,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(u32, u64),
) -> Result<OpNote> {
    match op {
        RestoreOp::WritePartitionTable { table } => {
            let Hold::Disk(session) = hold else {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("restore_exec_linux.table_on_live_disk", disk = device).into_owned(),
                ));
            };
            // Linux keeps the copy's GUIDs as they are: a PARTUUID may be
            // what `root=` names, and a duplicate on this machine was put to
            // the person before the run (`identity_check_linux`).
            let (table, identity) =
                with_captured_gpt_fields(table, plan, sidecar, &IdentitiesInUse::default());
            let written = session.write_partition_table(&table)?;
            info!(
                disk = %device,
                boot_code = ?written.boot_code,
                kernel = ?written.kernel,
                disk_guid_preserved = identity.disk_guid_preserved,
                "partition table written"
            );
            *expected = Some(written.expected);
            let mut note = OpNote::default();
            if identity.disk_guid_preserved {
                note.message = tr!("restore_exec.disk_ids_preserved").into_owned();
            }
            if !identity.partitions_moved.is_empty() {
                note.follow_ups.push(
                    tr!(
                        "restore_exec.partitions_moved",
                        parts = format!("{:?}", identity.partitions_moved)
                    )
                    .into_owned(),
                );
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
            info!(
                disk = %device,
                part = dst.index,
                offset = dst.offset,
                size = dst.size,
                src_index,
                wipe_free_space,
                reencrypt,
                "writing sectors into {}:{}:{}",
                device,
                dst.offset,
                dst.size
            );
            let opts = SectorRestoreOptions {
                verify_checksums: true,
                wipe_free_space: *wipe_free_space,
                disk_already_offline: true,
            };
            let label = format!("{device}:{}:{}", dst.offset, dst.size);
            let mut report = |pr: &SectorRestoreProgress| {
                on_progress(percent_of(pr), pr.bytes_written);
                Ok(())
            };
            let summary = if *reencrypt {
                let header = luks_header_for(sidecar, *src_index).ok_or_else(|| {
                    OwnBackupError::RestoreIncompatible(
                        tr!("restore_exec.no_luks_header", part = src_index).into_owned(),
                    )
                })?;
                let passphrase = passphrases.get(src_index).ok_or_else(|| {
                    OwnBackupError::Config(
                        tr!("restore_exec.no_passphrase", part = src_index).into_owned(),
                    )
                })?;
                let req = ReencryptRequest { header, passphrase };
                let fit = container_fitting(sidecar, *src_index, dst.size, &req)?;
                let (disk_path, is_disk) = match hold {
                    Hold::Disk(s) => (s.device_mut().path().to_string(), true),
                    Hold::Partitions => (String::new(), false),
                };
                let dst = *dst;
                with_target(hold, device, dst, TargetUse::for_plan(plan, false), |w| {
                    // Re-reads the header just written: from the disk, not
                    // from the cache (`ReadWindow::open` drops it too).
                    let mut read_back =
                        || -> Result<Box<dyn own_backup_backup::fat_map::RawVolume>> {
                            if is_disk {
                                Ok(Box::new(ReadWindow::open(
                                    &disk_path, dst.offset, dst.size,
                                )?))
                            } else {
                                let node = existing_node(device, dst)?;
                                let dev =
                                    own_backup_backup::linux_device::LinuxRawDevice::open_read(
                                        &node,
                                    )?;
                                dev.drop_cache()?;
                                Ok(Box::new(dev))
                            }
                        };
                    write_container_blocks(
                        &plan.image_path,
                        archive,
                        sidecar,
                        *src_index,
                        fit,
                        w,
                        &mut read_back,
                        &label,
                        opts,
                        &req,
                        cancel,
                        &mut report,
                    )
                })?
            } else {
                with_target(hold, device, *dst, TargetUse::for_plan(plan, false), |w| {
                    write_partition_blocks(
                        &plan.image_path,
                        archive,
                        sidecar,
                        *src_index,
                        dst.size,
                        w,
                        &label,
                        opts,
                        cancel,
                        &mut report,
                    )
                })?
            };
            if summary.cancelled {
                return Err(OwnBackupError::Cancelled);
            }
            if *reencrypt {
                return Ok(OpNote {
                    message: tr!("restore_exec.container_restored").into_owned(),
                    follow_ups: Vec::new(),
                    ..Default::default()
                });
            }
            Ok(OpNote {
                message: String::new(),
                follow_ups: encryption_follow_up(sidecar, dst.index),
                ..Default::default()
            })
        }

        RestoreOp::ChangeIdentities {
            parts,
            fix_references,
        } => change_identities(plan, passphrases, device, parts, *fix_references),

        RestoreOp::RefreshDiskState => {
            // The session's release already waited for the kernel and udev
            // to show the table; there is nothing left to re-read.
            Ok(OpNote {
                message: tr!("restore_exec_linux.disk_reread", disk = device).into_owned(),
                follow_ups: Vec::new(),
                ..Default::default()
            })
        }

        RestoreOp::EnsureFallbackBootloader { .. }
        | RestoreOp::RestoreBootEntries
        | RestoreOp::FinalizeBoot { .. } => {
            super::boot_linux::run_boot_op(op, plan, sidecar, device).map(|n| OpNote {
                message: n.message,
                follow_ups: n.follow_ups,
                ..Default::default()
            })
        }

        RestoreOp::Format {
            part,
            fs,
            label,
            quick,
        } if !matches!(fs, own_backup_common::restore_plan::FormatFs::ReFs) => {
            let node = free_partition_node(device, *part)?;
            let message = super::fs_linux::format(
                &mut own_backup_backup::linux_tools::SystemTools,
                &node,
                *fs,
                label,
                *quick,
            )?;
            Ok(OpNote {
                message,
                follow_ups: Vec::new(),
                ..Default::default()
            })
        }

        RestoreOp::ExtendFilesystem { part } => {
            let node = free_partition_node(device, *part)?;
            let message = super::fs_linux::extend(
                &mut own_backup_backup::linux_tools::SystemTools,
                &node,
                || own_backup_backup::identity_check_linux::btrfs_mount_safe(&node),
            )?;
            Ok(OpNote {
                message,
                follow_ups: Vec::new(),
                ..Default::default()
            })
        }

        RestoreOp::Format { .. }
        | RestoreOp::AssignLetter { .. }
        | RestoreOp::RestorePartition {
            mode: RestoreMode::Files,
            ..
        } => Err(OwnBackupError::RestoreIncompatible(
            linux_refusals(&RestorePlan {
                ops: vec![op.clone()],
                target_device: Some(device.to_string()),
                ..plan.clone()
            })
            .into_iter()
            .next()
            .map(|r| r.message)
            .unwrap_or_default(),
        )),
    }
}

/// Give what the plan restored on `parts` identities of its own
/// (`identity_change_linux`), and with `fix_references` edit the restored
/// system to match. Only partitions this plan wrote are touched, each free
/// of mounts and holders; the machine's identities are compared without them.
fn change_identities(
    plan: &RestorePlan,
    passphrases: &std::collections::HashMap<u32, String>,
    device: &str,
    parts: &[PartRef],
    fix_references: bool,
) -> Result<OpNote> {
    use own_backup_backup::identity_change_linux as change;

    let written = written_partitions(plan);
    let mut devices = Vec::new();
    for part in parts.iter().filter(|p| written.contains(&p.index)) {
        let node = free_partition_node(device, *part)?;
        let passphrase = plan.ops.iter().find_map(|op| match op {
            RestoreOp::RestorePartition {
                src_index,
                dst,
                reencrypt: true,
                ..
            } if dst.index == part.index => passphrases.get(src_index).cloned(),
            _ => None,
        });
        devices.push(change::RestoredDevice {
            node,
            part_index: Some(part.index),
            passphrase,
        });
    }
    let kernel =
        |path: &str| own_backup_common::linux_block::kernel_name(std::path::Path::new(path));
    let mut restored: Vec<String> = devices.iter().filter_map(|d| kernel(&d.node)).collect();
    if plan.touches_partition_table() {
        restored.extend(kernel(device));
    }
    let present = change::present_except(&restored);
    let nodes: Vec<&str> = devices.iter().map(|d| d.node.as_str()).collect();
    info!(
        ?nodes,
        fix_references, "changing the restored copy's duplicate identities"
    );
    let outcome = change::change_identities(
        &mut own_backup_backup::linux_tools::SystemTools,
        &mut change::SystemHost,
        &present,
        &devices,
        fix_references,
    );
    if let Some(error) = &outcome.failed {
        let done = outcome.describe();
        return Err(OwnBackupError::Device(
            tr!(
                "restore_exec_linux.identity_change_failed",
                error = error.as_str(),
                done = if done.is_empty() {
                    "—".to_string()
                } else {
                    done
                }
            )
            .into_owned(),
        ));
    }
    let message = if outcome.changed.is_empty() {
        tr!("restore_exec_linux.identities_unchanged").into_owned()
    } else {
        tr!(
            "restore_exec_linux.identities_changed",
            count = outcome.changed.len(),
            list = outcome.describe()
        )
        .into_owned()
    };
    Ok(OpNote {
        message,
        follow_ups: outcome.notes.clone(),
        identity_changes: outcome.dtos(),
        edited_files: outcome.edited.clone(),
    })
}

/// Read back every sector-restored partition through the current hold and
/// check it against the copy (R9, D10). The cache is dropped before each
/// read-back, so the bytes come from the disk.
#[allow(clippy::too_many_arguments)]
fn verify_sectors(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    ordered: &[RestoreOp],
    passphrases: &std::collections::HashMap<u32, String>,
    archive: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    device: &str,
    hold: &mut Hold,
    state: &mut RestoreRunState,
    cancel: &CancelToken,
    on_update: &mut impl FnMut(&RestoreRunState),
) {
    for op in ordered {
        let RestoreOp::RestorePartition {
            src_index,
            dst,
            mode: RestoreMode::Sectors,
            reencrypt,
            ..
        } = op
        else {
            continue;
        };
        let Some(index) = state
            .ops
            .iter()
            .position(|o| o.kind == "verify_sectors" && o.part_index == Some(dst.index))
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

        let label = format!("{device}:{}:{}", dst.offset, dst.size);
        let result = with_target(
            hold,
            device,
            *dst,
            TargetUse::for_plan(plan, true),
            |reader| {
                let crypt = if *reencrypt {
                    match passphrases.get(src_index) {
                        Some(pw) => match own_backup_backup::luks::open_luks(reader, pw.as_bytes())
                        {
                            Ok(p) => Some(p),
                            Err(e) => {
                                warn!(part = dst.index, error = %e, "verification: cannot open the restored container");
                                None
                            }
                        },
                        None => None,
                    }
                } else {
                    None
                };
                if *reencrypt && crypt.is_none() {
                    return Ok(None);
                }
                let mut tick = |pr: &SectorRestoreProgress| {
                    state.ops[index].percent = percent_of(pr);
                    state.ops[index].bytes_done = pr.bytes_written;
                    on_update(state);
                    Ok(())
                };
                verify_partition_blocks(
                    &plan.image_path,
                    archive,
                    sidecar,
                    *src_index,
                    reader,
                    &label,
                    crypt.as_ref(),
                    cancel,
                    &mut tick,
                )
                .map(Some)
            },
        );

        record_verification(state, index, dst.index, result, cancel.is_cancelled());
        on_update(state);
    }
    state.current = None;
    state.cancellable = false;
    on_update(state);
}

/// Put the outcome of one partition's read-back into the run state.
///
/// A mismatch is always a failure here. Windows excuses a partition its boot
/// steps rewrote after the copy, because there the check can run after them;
/// on Linux it runs before any boot step, so the bytes it reads are the ones
/// the restore wrote and nothing else has changed them on purpose.
fn record_verification(
    state: &mut RestoreRunState,
    index: usize,
    part: u32,
    result: Result<Option<own_backup_backup::sector_restore::VerifySummary>>,
    cancelled: bool,
) {
    match result {
        Ok(None) => {
            state.ops[index].status = OpStatus::Done;
            state.ops[index].percent = 100;
            state.ops[index].message =
                tr!("restore_exec.verify_not_checked_reencrypted").into_owned();
        }
        Ok(Some(v)) if v.cancelled || cancelled => {
            state.ops[index].status = OpStatus::Skipped;
            state.ops[index].message = tr!("restore_exec.verify_interrupted").into_owned();
        }
        Ok(Some(v)) if v.ok() => {
            state.ops[index].status = OpStatus::Done;
            state.ops[index].percent = 100;
            state.ops[index].message =
                tr!("restore_exec.verify_blocks_ok", count = v.blocks_checked).into_owned();
            info!(part, blocks = v.blocks_checked, "verification passed");
        }
        Ok(Some(v)) => {
            state.ops[index].status = OpStatus::Failed;
            let mut parts = Vec::new();
            if !v.image_corrupt.is_empty() {
                parts.push(
                    tr!(
                        "restore_exec.verify_image_corrupt",
                        count = v.image_corrupt.len()
                    )
                    .into_owned(),
                );
            }
            if !v.mismatches.is_empty() {
                parts.push(
                    tr!("restore_exec.verify_mismatch", count = v.mismatches.len()).into_owned(),
                );
            }
            let text = parts.join("; ");
            error!(part, %text, "verification failed");
            state.ops[index].message = text.clone();
            state.follow_ups.push(
                tr!("restore_exec.follow_up_partition", part = part, text = text).into_owned(),
            );
        }
        Err(e) => {
            state.ops[index].status = OpStatus::Failed;
            state.ops[index].message = e.to_string();
            state.follow_ups.push(
                tr!(
                    "restore_exec.follow_up_verify_failed",
                    part = part,
                    error = e
                )
                .into_owned(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::FormatFs;

    fn part(index: u32) -> PartRef {
        PartRef {
            index,
            offset: index as u64 * 1024 * 1024,
            size: 1024 * 1024,
        }
    }

    #[test]
    fn a_plan_without_a_device_and_with_windows_only_steps_is_refused_up_front() {
        let mut plan = RestorePlan::new(
            0,
            "/tmp/copy.qcow2".into(),
            vec![
                RestoreOp::Format {
                    part: part(1),
                    fs: FormatFs::ReFs,
                    label: String::new(),
                    quick: true,
                },
                RestoreOp::AssignLetter {
                    part: part(2),
                    letter: 'E',
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(3),
                    mode: RestoreMode::Files,
                    wipe_free_space: false,
                    reencrypt: false,
                },
                RestoreOp::RestorePartition {
                    src_index: 2,
                    dst: part(4),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
            ],
        );
        let found = linux_refusals(&plan);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found
            .iter()
            .all(|r| r.level == CheckLevel::Error && r.code == CheckCode::OpUnsupportedHere));
        assert_eq!(found[0].part_index, None, "the missing device comes first");
        assert_eq!(
            found
                .iter()
                .filter_map(|r| r.part_index)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        plan.target_device = Some("/dev/disk/by-id/virtio-target".into());
        plan.ops.clear();
        assert!(linux_refusals(&plan).is_empty());
    }

    /// A plan that restores partition 1 and then puts the fallback loader on
    /// it, with the read-back asked for.
    fn plan_with_boot_step_on(index: u32) -> RestorePlan {
        let mut plan = RestorePlan::new(
            0,
            "/tmp/copy.qcow2".into(),
            vec![
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst: part(index),
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
                RestoreOp::EnsureFallbackBootloader { esp: part(index) },
            ],
        );
        plan.verify_after = true;
        plan
    }

    #[test]
    fn a_mismatch_fails_even_on_a_partition_a_boot_step_changes_later() {
        let plan = plan_with_boot_step_on(1);
        let mut state = initial_state(&plan);
        let index = state
            .ops
            .iter()
            .position(|o| o.kind == "verify_sectors")
            .expect("the read-back is queued");
        let summary = own_backup_backup::sector_restore::VerifySummary {
            blocks_checked: 4,
            mismatches: vec![2],
            ..Default::default()
        };
        record_verification(&mut state, index, 1, Ok(Some(summary)), false);
        assert_eq!(state.ops[index].status, OpStatus::Failed);
        assert_eq!(state.follow_ups.len(), 1, "{:?}", state.follow_ups);

        // A clean read-back is still a pass.
        let mut state = initial_state(&plan);
        let clean = own_backup_backup::sector_restore::VerifySummary {
            blocks_checked: 4,
            ..Default::default()
        };
        record_verification(&mut state, index, 1, Ok(Some(clean)), false);
        assert_eq!(state.ops[index].status, OpStatus::Done);
        assert!(state.follow_ups.is_empty());
    }

    #[test]
    fn a_partition_slot_must_match_the_plan_exactly() {
        let sys = tempfile::tempdir().unwrap();
        let p = sys.path().join("sdz2");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("start"), "2048\n").unwrap();
        std::fs::write(p.join("size"), "4096\n").unwrap();
        let planned = PartRef {
            index: 2,
            offset: 2048 * 512,
            size: 4096 * 512,
        };
        check_slot_in(sys.path(), "sdz2", "/dev/sdz2", planned).unwrap();
        let moved = PartRef {
            offset: 4096 * 512,
            ..planned
        };
        assert!(matches!(
            check_slot_in(sys.path(), "sdz2", "/dev/sdz2", moved),
            Err(OwnBackupError::RestoreIncompatible(_))
        ));
        let resized = PartRef {
            size: 2048 * 512,
            ..planned
        };
        assert!(check_slot_in(sys.path(), "sdz2", "/dev/sdz2", resized).is_err());
        // No such partition, or a node the kernel does not name: refused.
        assert!(check_slot_in(sys.path(), "sdz3", "/dev/sdz3", planned).is_err());
        assert!(check_slot_in(sys.path(), "", "/dev/sdz2", planned).is_err());
    }

    #[test]
    fn only_the_partitions_written_are_held_on_a_live_disk() {
        let mut plan = plan_with_boot_step_on(3);
        plan.ops.push(RestoreOp::RestorePartition {
            src_index: 2,
            dst: part(5),
            mode: RestoreMode::Sectors,
            wipe_free_space: false,
            reencrypt: false,
        });
        assert_eq!(written_partitions(&plan), vec![3, 5]);
        // Without a device the kernel names, nothing is held — not "all".
        assert!(held_device_names(&plan, "/nonexistent/disk", true).is_empty());
        assert!(held_device_names(&plan, "/nonexistent/disk", false).is_empty());
    }

    #[test]
    fn no_device_means_nothing_starts() {
        let plan = RestorePlan::new(0, "/tmp/copy.qcow2".into(), Vec::new());
        let err = execute_plan(
            &plan,
            &Default::default(),
            None,
            &CancelToken::new(uuid::Uuid::new_v4()),
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, OwnBackupError::RestoreIncompatible(_)));
    }

    // --- on a real loop disk (root; run explicitly with --ignored) ---------

    const MIB: u64 = 1024 * 1024;

    fn run(cmd: &str, args: &[&str]) -> String {
        let out = std::process::Command::new(cmd).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{cmd} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An ext4 volume with known content and a qcow2 copy of it made by the
    /// Linux engine.
    fn copy_of_ext4(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use own_backup_backup::engine_linux::{backup_volume, VolumeBackupRequest};
        use own_backup_common::types::{BackupType, ImageFormat};

        let content = dir.join("content");
        std::fs::create_dir_all(content.join("nested")).unwrap();
        let payload: Vec<u8> = (0..700_000u32).flat_map(|i| i.to_le_bytes()).collect();
        std::fs::write(content.join("payload.bin"), &payload).unwrap();
        std::fs::write(content.join("nested/hello.txt"), b"restored by the plan\n").unwrap();
        let src = dir.join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(64 * MIB)
            .unwrap();
        run(
            "mkfs.ext4",
            &[
                "-q",
                "-F",
                "-L",
                "planned",
                "-d",
                content.to_str().unwrap(),
                src.to_str().unwrap(),
            ],
        );
        let image = dir.join("copy.qcow2");
        let mut snapshot = own_backup_snapshot::Snapshot {
            provider_name: "linux".into(),
            source_device: src.to_str().unwrap().into(),
            snapshot_device_path: src.to_str().unwrap().into(),
            created_at: Utc::now(),
            metadata: [("mode".to_string(), "file".to_string())].into(),
        };
        own_backup_snapshot::linux::assert_file_frozen(&mut snapshot);
        backup_volume(
            &VolumeBackupRequest {
                source: src.to_str().unwrap().into(),
                image_path: image.clone(),
                full_image_path: None,
                block_size: own_backup_backup::engine_linux::DEFAULT_BLOCK_SIZE,
                all_sectors: false,
                use_vss: true,
                kind: BackupType::Full,
                parent_image: None,
                exclusion_patterns: Vec::new(),
                exclusion_patterns_linux: Vec::new(),
                job_id: None,
                passphrase: None,
                image_format: ImageFormat::Qcow2,
                source_disk: None,
                encryption: None,
                zstd_level: own_backup_backup::qcow2::DEFAULT_ZSTD_LEVEL,
            },
            &snapshot,
            |_| Ok(()),
        )
        .unwrap();
        (src, image)
    }

    struct Loop(String);
    impl Drop for Loop {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup")
                .args(["-d", &self.0])
                .status();
        }
    }

    fn plan_onto(device: &str, image: &std::path::Path) -> RestorePlan {
        use own_backup_common::restore_plan::{
            PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec,
        };
        let dst = PartRef {
            index: 1,
            offset: MIB,
            size: 80 * MIB,
        };
        let mut plan = RestorePlan::new(
            0,
            image.to_path_buf(),
            vec![
                RestoreOp::WritePartitionTable {
                    table: PartitionTableSpec {
                        scheme: PartitionScheme::Gpt,
                        sector_size: 512,
                        disk_size_bytes: 128 * MIB,
                        disk_id: String::new(),
                        entries: vec![PartitionSpec {
                            index: 1,
                            first_lba: 2048,
                            last_lba: 2048 + 80 * 2048 - 1,
                            kind: PartitionKind::Gpt {
                                type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into(),
                                part_guid: String::new(),
                                attributes: 0,
                                name: "restored".into(),
                            },
                        }],
                    },
                },
                RestoreOp::RestorePartition {
                    src_index: 1,
                    dst,
                    mode: RestoreMode::Sectors,
                    wipe_free_space: true,
                    reencrypt: false,
                },
                RestoreOp::RefreshDiskState,
            ],
        );
        plan.verify_after = true;
        plan.target_device = Some(device.to_string());
        plan
    }

    #[test]
    #[ignore]
    fn real_loop_disk_plan_restore() {
        assert_eq!(libc_geteuid(), 0, "this test needs root");
        let dir = tempfile::tempdir().unwrap();
        let (src, image) = copy_of_ext4(dir.path());
        let disk = dir.path().join("disk.img");
        std::fs::File::create(&disk)
            .unwrap()
            .set_len(128 * MIB)
            .unwrap();
        let lo = Loop(run(
            "losetup",
            &["-f", "--show", "-P", disk.to_str().unwrap()],
        ));

        let plan = plan_onto(&lo.0, &image);
        let state = execute_plan(
            &plan,
            &Default::default(),
            None,
            &CancelToken::new(plan.id),
            |_| {},
        )
        .unwrap();
        assert_eq!(state.status, RunStatus::Completed, "{state:#?}");
        assert!(
            state.ops.iter().all(|o| o.status == OpStatus::Done),
            "{state:#?}"
        );
        assert!(
            state.ops.iter().any(|o| o.kind == "verify_sectors"),
            "the read-back ran"
        );

        // The partition holds the volume byte for byte, and the table is GPT.
        let restored = std::fs::read(&disk).unwrap();
        let original = std::fs::read(&src).unwrap();
        assert_eq!(
            &restored[MIB as usize..(MIB + 64 * MIB) as usize],
            &original[..]
        );
        assert_eq!(&restored[512..520], b"EFI PART");
        // The kernel shows the partition the plan wrote.
        let name = std::path::Path::new(&lo.0)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let start =
            std::fs::read_to_string(format!("/sys/class/block/{name}/{name}p1/start")).unwrap();
        assert_eq!(start.trim(), "2048");

        // Mounted, the partition is the running system's: the same plan is
        // refused before anything is written.
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let node = format!("/dev/{name}p1");
        run("mount", &["-o", "ro", &node, mnt.to_str().unwrap()]);
        let refused = execute_plan(
            &plan,
            &Default::default(),
            None,
            &CancelToken::new(plan.id),
            |_| {},
        );
        run("umount", &[mnt.to_str().unwrap()]);
        let err = refused.unwrap_err().to_string();
        assert!(err.contains('1'), "names the partition: {err}");
    }

    fn libc_geteuid() -> u32 {
        std::fs::metadata("/proc/self")
            .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
            .unwrap_or(u32::MAX)
    }
}
