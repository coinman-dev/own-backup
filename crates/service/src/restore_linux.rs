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
    EngineProgress, IdentityChoice, IdentityClashDto, RestoreSummaryDto, IDENTITY_CLASHES_DETAIL,
};
use own_backup_common::tr;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use super::proto::{JsonReply, JsonRequest};
use super::{
    emit_engine, json_reply, keep_awake, manual_keys_off_thread, BackupServiceImpl, ManualSecret,
};
use own_backup_backup::identity_check_linux;
use own_backup_backup::seal::context::ArchiveKeys;

/// Ask for a choice about `clashes`, or carry out the one made.
///
/// Nothing found: nothing to ask. Found and no choice: refused with the list
/// in the status details. `Change`: not done yet — re-identifying a restored
/// system also means editing its fstab, crypttab and boot configuration, and
/// a half-done job of that leaves a system that does not boot; the refusal
/// says how to proceed. `Keep`: the duplicates stay, and are reported back.
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
        Some(IdentityChoice::Change) => Err(Status::failed_precondition(
            tr!("restore_linux.identity_change_unsupported", clashes = list).into_owned(),
        )),
        Some(IdentityChoice::Keep) => {
            warn!(clashes = %list, "duplicate identities kept, as the person chose");
            Ok(clashes)
        }
    }
}

/// What restoring `image` onto `target` would duplicate on this machine.
fn volume_clashes(
    image: &Path,
    keys: Option<&ArchiveKeys>,
    target: &str,
) -> own_backup_common::Result<Vec<IdentityClashDto>> {
    let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(image, keys)?;
    let all: Vec<u32> = sidecar
        .disk_manifest
        .as_ref()
        .map(|d| d.layout.partitions.iter().map(|p| p.index).collect())
        .unwrap_or_default();
    let restored = identity_check_linux::restored_identities(image, keys, &sidecar, &all, false)?;
    let overwritten: Vec<String> = own_backup_common::linux_block::kernel_name(Path::new(target))
        .into_iter()
        .collect();
    Ok(identity_check_linux::clashes_here(&restored, &overwritten))
}

/// The volume restore's reply: the shared summary, and the duplicates the
/// person chose to keep.
#[derive(serde::Serialize)]
struct VolumeRestoreReply {
    #[serde(flatten)]
    summary: RestoreSummaryDto,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    identity_clashes: Vec<IdentityClashDto>,
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
            #[serde(flatten)]
            secret: ManualSecret,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

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
        let kept = identity_gate(clashes, p.identity_choice)?;

        let slot = self.reserve(&tr!("grpc_server.busy_volume_restore"), &p.target)?;
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
            restore_volume(&request, |pr| {
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
            })
        })
        .await
        .map_err(|e| Status::internal(format!("restore task panicked: {e}")))?
        .map_err(|e| match e {
            own_backup_common::OwnBackupError::Cancelled => Status::cancelled(e.to_string()),
            other => Status::internal(other.to_string()),
        })?;

        json_reply(&VolumeRestoreReply {
            summary: RestoreSummaryDto {
                blocks_restored: summary.blocks_restored,
                bytes_restored: summary.bytes_restored,
                duration_secs: summary.duration.as_secs_f64(),
            },
            identity_clashes: kept,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clash() -> IdentityClashDto {
        IdentityClashDto {
            kind: "lvm_vg_name".into(),
            value: "vg0".into(),
            part_index: Some(3),
            already_on: vec!["sda3".into()],
            severity: "critical".into(),
            change_command: String::new(),
            mount_hint: String::new(),
        }
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
    fn changing_identities_is_refused_and_keeping_them_reports_them() {
        let status = identity_gate(vec![clash()], Some(IdentityChoice::Change)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.details().is_empty());
        let kept = identity_gate(vec![clash()], Some(IdentityChoice::Keep)).unwrap();
        assert_eq!(kept, vec![clash()]);
    }
}
