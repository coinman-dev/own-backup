//! The Linux halves of the restore RPCs (PLAN §6, stage 3).
//!
//! The order of the checks is the Windows one, plus two Linux steps:
//!
//! 1. polkit — overwriting a volume or a disk needs an administrator
//!    (`com.ownbackup.restore`), the way `systemctl` asks;
//! 2. the running-system guard (`own_backup_restore`), fail-closed;
//! 3. the copy's keys, from what the person typed — before the slot, because
//!    deriving them is seconds of Argon2id;
//! 4. duplicate identities: what the copy would put on this machine that a
//!    device of it already carries (`identity_clash`). Found ones need an
//!    explicit choice ([`IdentityChoice`]); asking is a `FAILED_PRECONDITION`
//!    whose details carry the list as JSON, so the window decides from
//!    structured fields;
//! 5. the machine's one slot for destructive work;
//! 6. the write itself, with the target `O_EXCL` and under a temporary udev
//!    hold (`udev_hold_linux`) so udisks, LVM, md and systemd leave it alone.

// tonic::Status, as every RPC handler returns.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use own_backup_common::engine_dto::{
    EngineProgress, IdentityChangeDto, IdentityChoice, IdentityClashDto, RestoreSummaryDto,
    IDENTITY_CLASHES_DETAIL,
};
use own_backup_common::tr;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use super::proto::{JsonReply, JsonRequest};
use super::{
    emit_engine, json_reply, keep_awake, luks_needs, manual_keys_off_thread, BackupServiceImpl,
    ManualSecret,
};
use own_backup_backup::identity_check_linux;
use own_backup_backup::seal::context::ArchiveKeys;

/// Ask for a choice about `clashes`, or let the one made through.
///
/// Nothing found: nothing to ask. Found and no choice: refused with the list
/// in the status details. `Keep`: the duplicates stay, and are reported
/// back. `Change`: the restore changes them once the data is written
/// (`identity_change_linux`); the caller does that, and reports what a change
/// leaves ([`left_by_change`]).
pub(super) fn identity_gate(
    clashes: Vec<IdentityClashDto>,
    choice: Option<IdentityChoice>,
) -> Result<Vec<IdentityClashDto>, Status> {
    if clashes.is_empty() {
        return Ok(clashes);
    }
    let list = identity_check_linux::describe(&clashes);
    match choice {
        None => {
            warn!(clashes = %list, "duplicate identities: asking for a choice");
            let details = serde_json::json!({ IDENTITY_CLASHES_DETAIL: clashes });
            Err(Status::with_details(
                tonic::Code::FailedPrecondition,
                tr!("restore_linux.identity_choice_needed", clashes = list).into_owned(),
                serde_json::to_vec(&details).unwrap_or_default().into(),
            ))
        }
        Some(IdentityChoice::Change) => {
            info!(clashes = %list, "duplicate identities to be changed, as the person chose");
            Ok(clashes)
        }
        Some(IdentityChoice::Keep) => {
            warn!(clashes = %list, "duplicate identities kept, as the person chose");
            Ok(clashes)
        }
    }
}

/// The duplicates a change leaves as they are — those without a command to
/// change them: a PARTUUID is never changed (`root=PARTUUID=` may name it,
/// and the restore keeps the copy's table), and a btrfs spanning several
/// devices can only change on all of them at once.
pub(super) fn left_by_change(clashes: &[IdentityClashDto]) -> Vec<IdentityClashDto> {
    clashes
        .iter()
        .filter(|c| c.kind == "partuuid" || c.change_command.is_empty())
        .cloned()
        .collect()
}

/// [`identity_gate`] for mounting a copy: a shared filesystem label alone
/// does not stop it. Systems made from one template share their labels
/// (`cloudimg-rootfs`, `UEFI`, `BOOT`), and a mounted copy is reached by its
/// mount point, not by `LABEL=`; the duplicates are logged, and only the
/// other kinds need a choice.
pub(super) fn mount_identity_gate(
    clashes: Vec<IdentityClashDto>,
    choice: Option<IdentityChoice>,
) -> Result<Vec<IdentityClashDto>, Status> {
    let (labels, others): (Vec<_>, Vec<_>) = clashes.into_iter().partition(|c| c.kind == "label");
    if !labels.is_empty() {
        warn!(
            labels = %identity_check_linux::describe(&labels),
            "the mounted copy shares filesystem labels with this machine: /dev/disk/by-label may name either"
        );
    }
    identity_gate(others, choice)
}

/// A mounted copy is read, not changed: `Change` is a restore's choice.
pub(super) fn mount_choice_allowed(choice: Option<IdentityChoice>) -> Result<(), Status> {
    match choice {
        Some(IdentityChoice::Change) => Err(Status::invalid_argument(
            tr!("restore_linux.mount_cannot_change").into_owned(),
        )),
        _ => Ok(()),
    }
}

/// The identity step of a plan is the service's to add: drop one the client
/// sent, and add it when the person chose to change the duplicates found —
/// over every partition the plan restores.
pub(super) fn set_identity_step(
    plan: &mut RestorePlan,
    choice: Option<IdentityChoice>,
    clashes: &[IdentityClashDto],
    fix_references: bool,
) {
    plan.ops
        .retain(|op| !matches!(op, RestoreOp::ChangeIdentities { .. }));
    let changeable = clashes.len() > left_by_change(clashes).len();
    if choice != Some(IdentityChoice::Change) || !changeable {
        return;
    }
    let parts: Vec<_> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { dst, .. } => Some(*dst),
            _ => None,
        })
        .collect();
    if !parts.is_empty() {
        plan.ops.push(RestoreOp::ChangeIdentities {
            parts,
            fix_references,
        });
    }
}

/// Change the duplicates on what a volume or disk restore wrote, while its
/// udev hold still stands; `restored` are the kernel names it wrote.
fn change_restored(
    devices: &[own_backup_backup::identity_change_linux::RestoredDevice],
    restored: &[String],
    fix_references: bool,
) -> Result<own_backup_backup::identity_change_linux::Outcome, String> {
    use own_backup_backup::identity_change_linux as change;
    let present = change::present_except(restored);
    let outcome = change::change_identities(
        &mut own_backup_backup::linux_tools::SystemTools,
        &mut change::SystemHost,
        &present,
        devices,
        fix_references,
    );
    match &outcome.failed {
        None => Ok(outcome),
        Some(error) => {
            let done = outcome.describe();
            Err(tr!(
                "restore_linux.identity_change_failed",
                error = error.as_str(),
                done = if done.is_empty() {
                    "—".to_string()
                } else {
                    done
                }
            )
            .into_owned())
        }
    }
}

/// What restoring `image` onto `target` would duplicate on this machine.
fn volume_clashes(
    image: &Path,
    keys: Option<&ArchiveKeys>,
    target: &str,
) -> own_backup_common::Result<Vec<IdentityClashDto>> {
    let overwritten: Vec<String> = own_backup_common::linux_block::kernel_name(Path::new(target))
        .into_iter()
        .collect();
    volume_clashes_against(image, keys, &overwritten, false)
}

/// Every identity the copy carries against this machine's devices, leaving
/// out the `overwritten` ones; `with_partuuids` for a restore that writes the
/// copy's own partition table.
fn volume_clashes_against(
    image: &Path,
    keys: Option<&ArchiveKeys>,
    overwritten: &[String],
    with_partuuids: bool,
) -> own_backup_common::Result<Vec<IdentityClashDto>> {
    let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(image, keys)?;
    let all: Vec<u32> = sidecar
        .disk_manifest
        .as_ref()
        .map(|d| d.layout.partitions.iter().map(|p| p.index).collect())
        .unwrap_or_default();
    let mut restored =
        identity_check_linux::restored_identities(image, keys, &sidecar, &all, with_partuuids)?;
    // A whole-disk restore puts every container the copy keeps a header of
    // back with the source's LUKS UUID, which the decrypted copy does not
    // show.
    if with_partuuids {
        for h in &sidecar.info.luks_headers {
            if let Some(id) = identity_check_linux::saved_container_identity(h) {
                restored.push((Some(h.part_index), id));
            }
        }
    }
    Ok(identity_check_linux::clashes_here(&restored, overwritten))
}

/// What mounting `image` here would duplicate: everything it carries
/// against every device of the machine.
pub(super) fn image_clashes(image: &Path) -> own_backup_common::Result<Vec<IdentityClashDto>> {
    volume_clashes_against(image, None, &[], false)
}

/// The volume restore's reply: the shared summary, the duplicates left as
/// they are (chosen to keep, or a PARTUUID a change does not touch), and what
/// a change did.
#[derive(serde::Serialize)]
struct VolumeRestoreReply {
    #[serde(flatten)]
    summary: RestoreSummaryDto,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    identity_clashes: Vec<IdentityClashDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    identity_changes: Vec<IdentityChangeDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    edited_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

impl BackupServiceImpl {
    pub(super) async fn restore_volume_linux(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        use own_backup_backup::engine_linux::{restore_volume, LinuxRestoreRequest};

        crate::polkit::authorize(&req, crate::polkit::Action::Restore).await?;

        #[derive(serde::Deserialize)]
        struct In {
            image_path: String,
            target: String,
            #[serde(default)]
            identity_choice: Option<IdentityChoice>,
            /// With `change`: also edit the restored system's fstab, crypttab
            /// and boot configuration.
            #[serde(default)]
            fix_references: bool,
            #[serde(flatten)]
            secret: ManualSecret,
        }
        let mut p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        // The kernel's node, once: the udev hold switches off the
        // persistent-storage rules for the target, so a `/dev/disk/by-*` name
        // of it vanishes during the restore — and a by-uuid one names the
        // copy's UUID afterwards, not the device.
        p.target = own_backup_common::linux_block::kernel_name(Path::new(&p.target))
            .map(|name| format!("/dev/{name}"))
            .ok_or_else(|| {
                Status::failed_precondition(
                    tr!(
                        "restore_exec_linux_service.not_a_device",
                        device = p.target.as_str()
                    )
                    .into_owned(),
                )
            })?;

        // No plan and no table on this path: the guard is all that stands
        // between the request and a mounted volume (audit AUD-01).
        own_backup_restore::guard_target_volume(&p.target)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;

        let image_path = PathBuf::from(&p.image_path);
        let keys = manual_keys_off_thread(image_path.clone(), p.secret).await?;

        let (keys, clashes) = {
            let image = image_path.clone();
            let target = p.target.clone();
            tokio::task::spawn_blocking(move || {
                let found = volume_clashes(&image, keys.as_ref(), &target);
                (keys, found)
            })
            .await
            .map_err(|e| Status::internal(format!("identity check panicked: {e}")))?
        };
        let clashes = clashes.map_err(|e| Status::internal(e.to_string()))?;
        let found = identity_gate(clashes, p.identity_choice)?;
        let change = p.identity_choice == Some(IdentityChoice::Change) && !found.is_empty();
        let kept = if change {
            left_by_change(&found)
        } else {
            found
        };
        let fix_references = p.fix_references;

        let slot = self
            .reserve(&tr!("grpc_server.busy_volume_restore"), &p.target)
            .await?;
        let cancel = slot.cancel();
        let events = self.state.events.clone();
        info!(image = %p.image_path, target = %p.target, "RestoreVolume RPC started");
        let request = LinuxRestoreRequest {
            image_path,
            target: p.target.clone(),
            verify_checksums: true,
            encryption: keys,
        };

        let summary = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let _awake = keep_awake(&tr!("grpc_server.busy_volume_restore"));
            let held: Vec<String> =
                own_backup_common::linux_block::kernel_name(Path::new(&request.target))
                    .into_iter()
                    .collect();
            let held: Vec<&str> = held.iter().map(String::as_str).collect();
            // A hold that cannot be installed costs the desktop's manners,
            // not the restore: O_EXCL still keeps every mount away.
            let _hold = match own_backup_backup::udev_hold_linux::UdevHold::install(&held) {
                Ok(h) => Some(h),
                Err(e) => {
                    warn!(error = %e, "udev hold not installed");
                    None
                }
            };
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let summary = restore_volume(&request, |pr| {
                if cancel.is_cancelled() {
                    return Err(own_backup_common::OwnBackupError::Cancelled);
                }
                let permille = (pr.blocks_done * 1000)
                    .checked_div(pr.blocks_total)
                    .unwrap_or(0);
                if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                    last_permille = permille;
                    last_emit = std::time::Instant::now();
                    emit_engine(
                        &events,
                        "restore",
                        &EngineProgress {
                            percent: permille / 10,
                            blocks_done: pr.blocks_done,
                            blocks_total: pr.blocks_total,
                            bytes_done: pr.bytes_written,
                            speed_mbps: pr.speed_bps as f64 / 1_000_000.0,
                        },
                    );
                }
                Ok(())
            })?;
            // Under the hold still: the machine must not meet the clone
            // under the source's names. Nothing may have taken the volume
            // since it was written (the boot-ESP question was settled before).
            let changed = if change {
                own_backup_restore::guard_plan_volume(&request.target)?;
                let device = own_backup_backup::identity_change_linux::RestoredDevice {
                    node: request.target.clone(),
                    part_index: None,
                    passphrase: None,
                };
                let held: Vec<String> = held.iter().map(|s| s.to_string()).collect();
                Some(change_restored(&[device], &held, fix_references))
            } else {
                None
            };
            Ok::<_, own_backup_common::OwnBackupError>((summary, changed))
        })
        .await
        .map_err(|e| Status::internal(format!("restore task panicked: {e}")))?
        .map_err(|e| match e {
            own_backup_common::OwnBackupError::Cancelled => Status::cancelled(e.to_string()),
            other => Status::internal(other.to_string()),
        })?;
        let (summary, changed) = summary;
        let outcome = match changed {
            Some(Err(text)) => return Err(Status::internal(text)),
            Some(Ok(o)) => o,
            None => Default::default(),
        };

        json_reply(&VolumeRestoreReply {
            summary: RestoreSummaryDto {
                blocks_restored: summary.blocks_restored,
                bytes_restored: summary.bytes_restored,
                duration_secs: summary.duration.as_secs_f64(),
            },
            identity_clashes: kept,
            identity_changes: outcome.dtos(),
            edited_files: outcome.edited,
            notes: outcome.notes,
        })
    }
}

// --- restore plans and whole-disk restore ------------------------------------

use own_backup_common::restore_check::{CheckCode, CheckContext, CheckLevel, CheckResult};
use own_backup_common::restore_plan::{RestoreOp, RestorePlan, TargetDiskId};

/// Kernel name of the plan's target disk.
fn target_name(plan: &RestorePlan) -> Option<String> {
    plan.target_device
        .as_deref()
        .and_then(|d| own_backup_common::linux_block::kernel_name(Path::new(d)))
}

/// One udev property of a device (`E:<key>=` in its database record).
fn udev_property(name: &str, key: &str) -> Option<String> {
    use own_backup_common::linux_block as lb;
    let (ma, mi) = lb::dev_numbers(Path::new(lb::SYS_CLASS_BLOCK), name)?;
    let text =
        std::fs::read_to_string(Path::new(lb::UDEV_DATA).join(format!("b{ma}:{mi}"))).ok()?;
    let prefix = format!("E:{key}=");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// udev's `\xNN` escapes (`ID_FS_LABEL_ENC`, `ID_PART_ENTRY_NAME`) undone:
/// `MODULATOR\x20WorkDisk-2` → `MODULATOR WorkDisk-2`.
fn udev_unescape(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' && raw.get(i + 1) == Some(&b'x') {
            if let Some(v) = s
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                bytes.push(v);
                i += 4;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Who the disk the plan names is right now: serial and table GUID from the
/// udev database, size from sysfs — the Linux answer to `live_disk_id`.
pub(super) fn live_disk_id(plan: &RestorePlan) -> TargetDiskId {
    let mut id = TargetDiskId::default();
    let Some(name) = target_name(plan) else {
        return id;
    };
    let sys = Path::new(own_backup_common::linux_block::SYS_CLASS_BLOCK);
    id.size_bytes = std::fs::read_to_string(sys.join(&name).join("size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| s * 512)
        .unwrap_or(0);
    id.serial = udev_property(&name, "ID_SERIAL_SHORT")
        .or_else(|| udev_property(&name, "ID_SERIAL"))
        .unwrap_or_default();
    id.disk_guid = udev_property(&name, "ID_PART_TABLE_UUID").unwrap_or_default();
    id
}

/// The machine's half of the plan checks on Linux: what the target disk
/// holds now and which of its partitions the running system stands on — and,
/// for a volume copy, the one volume it holds, under whichever source number
/// the plan gives it (a volume copy has no partition table to number it by,
/// and the executor reads the whole image for it).
pub(super) fn fill_context(plan: &RestorePlan, ctx: &mut CheckContext, keys: Option<&ArchiveKeys>) {
    ctx.linux_host = true;
    if ctx.sources.is_empty() {
        ctx.sources = volume_sources(plan, keys);
    }
    use own_backup_common::linux_block as lb;
    use own_backup_common::restore_check::TargetPartition;

    let sys = Path::new(lb::SYS_CLASS_BLOCK);
    ctx.targets = target_name(plan)
        .map(|disk| {
            lb::partitions_of(sys, &disk)
                .into_iter()
                .filter_map(|p| {
                    let read = |f: &str| -> Option<u64> {
                        std::fs::read_to_string(sys.join(&p).join(f))
                            .ok()?
                            .trim()
                            .parse()
                            .ok()
                    };
                    Some(TargetPartition {
                        index: read("partition")? as u32,
                        offset_bytes: read("start")? * 512,
                        size_bytes: read("size")? * 512,
                        // Knowing how full it is means mounting it; 0 is
                        // "unknown" to the checks.
                        used_bytes: 0,
                        filesystem: own_backup_common::types::FileSystem::Unknown,
                        // What a message about this partition names it by —
                        // its volume label as udev read it («MODULATOR
                        // WorkDisk-2» on the owner's BitLocker partition),
                        // else its GPT name.
                        label: udev_property(&p, "ID_FS_LABEL_ENC")
                            .or_else(|| udev_property(&p, "ID_PART_ENTRY_NAME"))
                            .map(|l| udev_unescape(&l))
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let guard = match plan.target_device.as_deref() {
        Some(device) => own_backup_restore::protection_of_device(device),
        None => own_backup_restore::Protection {
            parts: Vec::new(),
            cannot_tell: Some(tr!("restore_exec_linux_service.no_device").into_owned()),
        },
    };
    ctx.target_is_system_disk = !guard.parts.is_empty();
    ctx.protected_parts = match &guard.cannot_tell {
        None => guard.parts,
        Some(why) => {
            warn!(reason = %why, "the running system's partitions on the target could not be determined — treating the whole disk as protected");
            ctx.targets.iter().map(|t| t.index).collect()
        }
    };
}

fn volume_sources(
    plan: &RestorePlan,
    keys: Option<&ArchiveKeys>,
) -> Vec<own_backup_common::restore_check::SourcePartition> {
    use own_backup_common::restore_check::SourcePartition;
    let Ok(sidecar) = own_backup_backup::sidecar::read_sidecar_with_keys(&plan.image_path, keys)
    else {
        return Vec::new();
    };
    if sidecar.disk_manifest.is_some() {
        return Vec::new();
    }
    let mut indices: Vec<u32> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { src_index, .. } => Some(*src_index),
            _ => None,
        })
        .collect();
    indices.dedup();
    // What the copy's volume holds decides what can be grown or formatted
    // over it; the head of the volume says it.
    let filesystem = identity_check_linux::volume_head(
        &plan.image_path,
        keys,
        &sidecar,
        own_backup_inventory::fs_probe::PROBE_LEN,
    )
    .ok()
    .flatten()
    .and_then(|head| own_backup_inventory::fs_probe::identify(&head))
    .map(|p| p.fs)
    .unwrap_or(own_backup_common::types::FileSystem::Unknown);
    let captured_plain = sidecar
        .manifest
        .volume_encryption
        .as_ref()
        .is_some_and(|v| v.capture_mode == own_backup_common::image::VolumeCaptureMode::Plaintext);
    indices
        .into_iter()
        .map(|index| SourcePartition {
            index,
            size_bytes: sidecar.manifest.source_disk.size_bytes,
            used_bytes: 0,
            filesystem,
            label: String::new(),
            encrypted_at_source: captured_plain,
            luks_header_available: !sidecar.info.luks_headers.is_empty(),
        })
        .collect()
}

/// The plan's disk size from the device itself (see `correct_disk_size`).
pub(super) fn correct_disk_size(plan: &mut RestorePlan) {
    let Some(device) = plan.target_device.clone() else {
        return;
    };
    let Ok(dev) = own_backup_backup::linux_device::LinuxRawDevice::open_read(&device) else {
        return;
    };
    let actual = dev.length();
    for op in plan.ops.iter_mut() {
        if let RestoreOp::WritePartitionTable { table } = op {
            table.disk_size_bytes = actual;
        }
    }
}

/// Kernel names a plan overwrites: the partitions it writes into, and the
/// whole disk when it rewrites the table.
fn overwritten_by(plan: &RestorePlan) -> Vec<String> {
    let mut out = Vec::new();
    let Some(device) = plan.target_device.as_deref() else {
        return out;
    };
    if plan.touches_partition_table() {
        out.extend(target_name(plan));
    }
    for op in &plan.ops {
        if let RestoreOp::RestorePartition { dst, .. } = op {
            if let Some(node) =
                own_backup_backup::restore_disk_linux::partition_node(device, dst.index)
            {
                out.extend(own_backup_common::linux_block::kernel_name(Path::new(
                    &node,
                )));
            }
        }
    }
    out
}

/// What applying `plan` would duplicate on this machine.
pub(super) fn plan_clashes(
    plan: &RestorePlan,
    keys: Option<&ArchiveKeys>,
) -> own_backup_common::Result<Vec<IdentityClashDto>> {
    let sources: Vec<u32> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            RestoreOp::RestorePartition { src_index, .. } => Some(*src_index),
            _ => None,
        })
        .collect();
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(&plan.image_path, keys)?;
    let mut restored = identity_check_linux::restored_identities(
        &plan.image_path,
        keys,
        &sidecar,
        &sources,
        plan.touches_partition_table(),
    )?;
    // A container put back from its saved header carries the source's LUKS
    // UUID, which the decrypted copy does not show.
    for op in &plan.ops {
        if let RestoreOp::RestorePartition {
            src_index,
            reencrypt: true,
            ..
        } = op
        {
            if let Some(id) = own_backup_core::restore_exec::luks_header_for(&sidecar, *src_index)
                .and_then(identity_check_linux::saved_container_identity)
            {
                restored.push((Some(*src_index), id));
            }
        }
    }
    Ok(identity_check_linux::clashes_here(
        &restored,
        &overwritten_by(plan),
    ))
}

/// Warnings for the preview: one per duplicate. The apply asks for the choice.
pub(super) fn clash_findings(clashes: &[IdentityClashDto]) -> Vec<CheckResult> {
    clashes
        .iter()
        .map(|c| CheckResult {
            level: CheckLevel::Warning,
            code: CheckCode::IdentityClash,
            message: tr!(
                "restore_exec_linux_service.identity_clash",
                kind = c.kind.as_str(),
                value = c.value.as_str(),
                on = c.already_on.join(", ")
            )
            .into_owned(),
            part_index: None,
        })
        .collect()
}

impl BackupServiceImpl {
    pub(super) async fn restore_disk_linux(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        use own_backup_backup::restore_disk_linux::{restore_disk, LinuxDiskRestoreRequest};
        use own_backup_common::engine_dto::{DiskRestoreProgressDto, DiskRestoreSummaryDto};

        crate::polkit::authorize(&req, crate::polkit::Action::Restore).await?;
        #[derive(serde::Deserialize)]
        struct In {
            manifest_path: String,
            /// The disk: Linux disks have no stable number.
            target_device: String,
            #[serde(default)]
            identity_choice: Option<IdentityChoice>,
            /// With `change`: also edit the restored system's fstab, crypttab
            /// and boot configuration.
            #[serde(default)]
            fix_references: bool,
            /// The LUKS containers' own passphrases, by source partition
            /// index: each container the copy holds decrypted is put back
            /// under its saved header. Not the copy's password (`secret`).
            #[serde(default)]
            passphrases: std::collections::HashMap<u32, String>,
            #[serde(flatten)]
            secret: ManualSecret,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        own_backup_restore::guard_target_disk_device(&p.target_device)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;

        let image_path = PathBuf::from(&p.manifest_path);
        let keys = manual_keys_off_thread(image_path.clone(), p.secret).await?;
        let disk = own_backup_common::linux_block::kernel_name(Path::new(&p.target_device))
            .ok_or_else(|| {
                Status::failed_precondition(
                    tr!(
                        "restore_exec_linux_service.not_a_device",
                        device = p.target_device.as_str()
                    )
                    .into_owned(),
                )
            })?;
        // The containers to put back and their passphrases come first: a
        // missing or wrong one is asked for (structured details) before
        // anything else, and before the slot is taken. Seconds of key
        // derivation per container, off the runtime.
        let super::LuksSecrets {
            keys,
            passphrases,
            known,
        } = luks_needs(image_path.clone(), keys, p.passphrases).await?;

        let (keys, clashes) = {
            let image = image_path.clone();
            let disk = disk.clone();
            tokio::task::spawn_blocking(move || {
                let found = volume_clashes_against(&image, keys.as_ref(), &[disk], true);
                (keys, found)
            })
            .await
            .map_err(|e| Status::internal(format!("identity check panicked: {e}")))?
        };
        let clashes = clashes.map_err(|e| Status::internal(e.to_string()))?;
        let found = identity_gate(clashes, p.identity_choice)?;
        let change = p.identity_choice == Some(IdentityChoice::Change) && !found.is_empty();
        let kept = if change {
            left_by_change(&found)
        } else {
            found
        };
        let fix_references = p.fix_references;

        let slot = self
            .reserve(&tr!("grpc_server.busy_disk_restore"), &p.target_device)
            .await?;
        let cancel = slot.cancel();
        let events = self.state.events.clone();
        info!(image = %p.manifest_path, target = %p.target_device, "RestoreDisk RPC started");
        let request = LinuxDiskRestoreRequest {
            image_path,
            // The kernel's node: a `/dev/disk/by-id/…` name disappears under
            // the udev hold, and the partitions are found through it after
            // the restore.
            target_device: format!("/dev/{disk}"),
            verify_checksums: true,
            encryption: keys,
            passphrases,
            known_keys: known,
        };
        let summary = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let _awake = keep_awake(&tr!("grpc_server.busy_disk_restore"));
            let _hold = own_backup_backup::udev_hold_linux::UdevHold::install(&[disk.as_str()])
                .map_err(|e| warn!(error = %e, "udev hold not installed"))
                .ok();
            let mut last = String::new();
            let summary = restore_disk(&request, |pr| {
                if cancel.is_cancelled() {
                    return Err(own_backup_common::OwnBackupError::Cancelled);
                }
                let percent = (pr.blocks_done * 100)
                    .checked_div(pr.blocks_total)
                    .unwrap_or(0);
                let key = format!("{}:{}:{}", pr.phase, pr.partition_index, percent);
                if key != last {
                    last = key;
                    emit_engine(
                        &events,
                        "restore-disk",
                        &DiskRestoreProgressDto {
                            phase: pr.phase.clone(),
                            partition_index: pr.partition_index as u64,
                            partition_count: pr.partition_count as u64,
                            part_label: pr.part_label.clone(),
                            percent,
                            bytes_written: pr.bytes_written,
                        },
                    );
                }
                Ok(())
            })?;
            // Under the hold still, on every partition the copy's table put
            // back.
            let changed = if change {
                let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(
                    &request.image_path,
                    request.encryption.as_ref(),
                )?;
                let indices: Vec<u32> = sidecar
                    .disk_manifest
                    .map(|d| d.layout.partitions.iter().map(|p| p.index).collect())
                    .unwrap_or_default();
                let devices: Vec<_> = indices
                    .into_iter()
                    .filter_map(|i| {
                        own_backup_backup::restore_disk_linux::partition_node(
                            &request.target_device,
                            i,
                        )
                        .map(|node| {
                            own_backup_backup::identity_change_linux::RestoredDevice {
                                node,
                                part_index: Some(i),
                                // A container put back opens with its own
                                // passphrase: what is inside changes too.
                                passphrase: summary
                                    .containers
                                    .contains(&i)
                                    .then(|| request.passphrases.get(&i).cloned())
                                    .flatten(),
                            }
                        })
                    })
                    .collect();
                let mut restored: Vec<String> = devices
                    .iter()
                    .filter_map(|d| own_backup_common::linux_block::kernel_name(Path::new(&d.node)))
                    .collect();
                restored.push(disk.clone());
                Some(change_restored(&devices, &restored, fix_references))
            } else {
                None
            };
            Ok::<_, own_backup_common::OwnBackupError>((summary, changed))
        })
        .await
        .map_err(|e| Status::internal(format!("disk restore task panicked: {e}")))?
        .map_err(|e| match e {
            own_backup_common::OwnBackupError::Cancelled => Status::cancelled(e.to_string()),
            other => Status::internal(other.to_string()),
        })?;
        let (summary, changed) = summary;
        let outcome = match changed {
            Some(Err(text)) => return Err(Status::internal(text)),
            Some(Ok(o)) => o,
            None => Default::default(),
        };

        #[derive(serde::Serialize)]
        struct Out {
            #[serde(flatten)]
            summary: DiskRestoreSummaryDto,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            identity_clashes: Vec<IdentityClashDto>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            identity_changes: Vec<IdentityChangeDto>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            edited_files: Vec<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            warnings: Vec<String>,
            /// Partitions put back as the LUKS containers they were.
            #[serde(skip_serializing_if = "Vec::is_empty")]
            luks_containers: Vec<u32>,
        }
        json_reply(&Out {
            summary: DiskRestoreSummaryDto {
                partitions: summary.partitions as u64,
                bytes_written: summary.bytes_written,
                duration_secs: summary.duration.as_secs_f64(),
            },
            luks_containers: summary.containers,
            identity_clashes: kept,
            identity_changes: outcome.dtos(),
            edited_files: outcome.edited,
            warnings: summary.warnings.into_iter().chain(outcome.notes).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::luks_gate;

    #[test]
    fn a_udev_label_reads_as_it_was_written() {
        assert_eq!(
            super::udev_unescape(r"MODULATOR\x20WorkDisk-2\x2030.06.2025"),
            "MODULATOR WorkDisk-2 30.06.2025"
        );
        assert_eq!(
            super::udev_unescape(r"\xd0\x94\xd0\xb0\xd0\xbd\xd0\xbd\xd1\x8b\xd0\xb5"),
            "Данные"
        );
        assert_eq!(super::udev_unescape(r"a\xZZb\"), r"a\xZZb\");
    }
    use super::*;

    fn clash() -> IdentityClashDto {
        IdentityClashDto {
            kind: "lvm_vg_name".into(),
            value: "vg0".into(),
            part_index: Some(3),
            already_on: vec!["sda3".into()],
            severity: "critical".into(),
            change_command: "vgimportclone --basevgname <new-name> <device>".into(),
            mount_hint: String::new(),
        }
    }

    /// Mounting: shared labels alone are logged, not asked about; with
    /// another duplicate the question still comes, without the labels. A
    /// restore still asks about a label.
    #[test]
    fn a_mount_is_not_stopped_by_shared_labels_alone() {
        let label = IdentityClashDto {
            kind: "label".into(),
            value: "cloudimg-rootfs".into(),
            severity: "ambiguous".into(),
            change_command: "e2fsck -f <device> && tune2fs -L <new-label> <device>".into(),
            ..clash()
        };
        assert!(mount_identity_gate(vec![label.clone()], None)
            .unwrap()
            .is_empty());
        let status = mount_identity_gate(vec![label.clone(), clash()], None).unwrap_err();
        let details: serde_json::Value = serde_json::from_slice(status.details()).unwrap();
        let list: Vec<IdentityClashDto> =
            serde_json::from_value(details[IDENTITY_CLASHES_DETAIL].clone()).unwrap();
        assert_eq!(list, vec![clash()]);
        assert!(identity_gate(vec![label], None).is_err());
    }

    #[test]
    fn missing_luks_passphrases_are_asked_for_in_structured_details() {
        use own_backup_common::engine_dto::{LuksPassphraseNeedDto, LUKS_PASSPHRASES_DETAIL};
        assert!(luks_gate(Vec::new()).is_ok());
        let need = LuksPassphraseNeedDto {
            part_index: 3,
            kind: own_backup_common::image::VolumeEncryptionKind::Luks2,
            wrong: true,
        };
        let s = luks_gate(vec![need.clone()]).unwrap_err();
        assert_eq!(s.code(), tonic::Code::FailedPrecondition);
        let v: serde_json::Value = serde_json::from_slice(s.details()).unwrap();
        let back: Vec<LuksPassphraseNeedDto> =
            serde_json::from_value(v[LUKS_PASSPHRASES_DETAIL].clone()).unwrap();
        assert_eq!(back, vec![need]);
        assert!(s.message().contains('3'), "{}", s.message());
    }

    #[test]
    fn nothing_found_needs_no_choice() {
        assert!(identity_gate(Vec::new(), None).unwrap().is_empty());
    }

    #[test]
    fn a_duplicate_without_a_choice_is_asked_about_in_structured_details() {
        let status = identity_gate(vec![clash()], None).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let details: serde_json::Value = serde_json::from_slice(status.details()).unwrap();
        let list: Vec<IdentityClashDto> =
            serde_json::from_value(details[IDENTITY_CLASHES_DETAIL].clone()).unwrap();
        assert_eq!(list, vec![clash()]);
    }

    #[test]
    fn a_choice_lets_the_restore_through_and_a_change_leaves_only_partuuids() {
        let found = identity_gate(vec![clash()], Some(IdentityChoice::Change)).unwrap();
        assert_eq!(found, vec![clash()]);
        let kept = identity_gate(vec![clash()], Some(IdentityChoice::Keep)).unwrap();
        assert_eq!(kept, vec![clash()]);
        let partuuid = IdentityClashDto {
            kind: "partuuid".into(),
            ..clash()
        };
        assert_eq!(left_by_change(&[clash(), partuuid.clone()]), vec![partuuid]);
        // A multi-device btrfs comes without a command: a change keeps it too.
        let btrfs = IdentityClashDto {
            kind: "btrfs".into(),
            change_command: String::new(),
            ..clash()
        };
        assert_eq!(left_by_change(&[clash(), btrfs.clone()]), vec![btrfs]);
    }

    #[test]
    fn a_mount_cannot_change_identities() {
        assert!(mount_choice_allowed(Some(IdentityChoice::Keep)).is_ok());
        assert!(mount_choice_allowed(None).is_ok());
        let status = mount_choice_allowed(Some(IdentityChoice::Change)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    fn pref(index: u32) -> own_backup_common::restore_plan::PartRef {
        own_backup_common::restore_plan::PartRef {
            index,
            offset: index as u64 * 1024 * 1024,
            size: 1024 * 1024,
        }
    }

    fn plan_restoring(parts: &[u32]) -> RestorePlan {
        RestorePlan::new(
            0,
            "/tmp/copy.qcow2".into(),
            parts
                .iter()
                .map(|&i| RestoreOp::RestorePartition {
                    src_index: i,
                    dst: pref(i),
                    mode: own_backup_common::restore_plan::RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                })
                .collect(),
        )
    }

    #[test]
    fn the_identity_step_is_the_services_to_add() {
        // A client's own step is dropped, whatever it asked for.
        let mut plan = plan_restoring(&[1, 2]);
        plan.ops.push(RestoreOp::ChangeIdentities {
            parts: vec![pref(7)],
            fix_references: true,
        });
        set_identity_step(&mut plan, Some(IdentityChoice::Keep), &[clash()], true);
        assert!(!plan
            .ops
            .iter()
            .any(|o| matches!(o, RestoreOp::ChangeIdentities { .. })));

        // Changing adds one step over every restored partition.
        set_identity_step(&mut plan, Some(IdentityChoice::Change), &[clash()], true);
        let steps: Vec<_> = plan
            .ops
            .iter()
            .filter_map(|o| match o {
                RestoreOp::ChangeIdentities {
                    parts,
                    fix_references,
                } => Some((parts.clone(), *fix_references)),
                _ => None,
            })
            .collect();
        assert_eq!(steps, vec![(vec![pref(1), pref(2)], true)]);

        // Only a PARTUUID: nothing a change would do.
        let mut plan = plan_restoring(&[1]);
        let partuuid = IdentityClashDto {
            kind: "partuuid".into(),
            ..clash()
        };
        set_identity_step(&mut plan, Some(IdentityChoice::Change), &[partuuid], false);
        assert_eq!(plan.ops.len(), 1);
    }
}
