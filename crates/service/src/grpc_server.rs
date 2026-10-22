//! gRPC surface over the orchestrator core (plan v2, phase 4).
//!
//! Handlers are thin: they delegate to [`own_backup_core`] (jobs, queue,
//! activity) and serialize the shared Rust types to JSON envelopes. Progress
//! and lifecycle events are fanned out to `StreamEvents` subscribers via a
//! broadcast channel fed by [`GrpcEventSink`].

use std::pin::Pin;
use std::sync::Arc;

use own_backup_common::tr;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::app_state::AppState;

/// The engine of this system: verification is the shared one on both.
#[cfg(target_os = "linux")]
use own_backup_backup::engine_linux as platform_engine;
#[cfg(target_os = "windows")]
use own_backup_backup::engine_win as platform_engine;

pub mod proto {
    tonic::include_proto!("ownbackup");
}

/// Restore and verify on Linux: the handlers' Linux halves (PLAN §6, stage 3).
#[cfg(target_os = "linux")]
#[path = "restore_linux.rs"]
mod restore_linux;

use proto::backup_service_server::{BackupService, BackupServiceServer};
use proto::*;

pub struct BackupServiceImpl {
    state: Arc<AppState>,
    shutdown: crate::shutdown::ShutdownSignal,
    /// Expected bearer token. Empty string means auth is disabled (dev/TCP mode).
    service_token: String,
}

impl BackupServiceImpl {
    pub fn new(
        state: Arc<AppState>,
        service_token: String,
        shutdown: crate::shutdown::ShutdownSignal,
    ) -> Self {
        Self {
            state,
            service_token,
            shutdown,
        }
    }

    pub fn into_service(self) -> BackupServiceServer<BackupServiceImpl> {
        BackupServiceServer::new(self)
            .max_decoding_message_size(own_backup_common::MAX_RPC_MESSAGE)
            .max_encoding_message_size(own_backup_common::MAX_RPC_MESSAGE)
    }

    /// Take the machine for one destructive operation, or turn the caller away
    /// naming who has it (audit AUD-02).
    ///
    /// `failed_precondition` and not `unavailable`: nothing is broken, the
    /// answer is simply "not now", and a client can retry when the thing that
    /// is running has ended.
    fn reserve(
        &self,
        what: &str,
        which: &str,
    ) -> Result<own_backup_core::busy::Reservation, Status> {
        if self.shutdown.is_shutdown_requested() {
            return Err(Status::unavailable("service is stopping"));
        }
        let slot = self
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::new(what, which))
            .map_err(|held| Status::failed_precondition(held.refusal()))?;
        if self.shutdown.is_shutdown_requested() {
            return Err(Status::unavailable("service is stopping"));
        }
        Ok(slot)
    }

    /// The same, for an operation the client already has an id for — a restore
    /// plan is cancelled by its plan id, and that is what the window knows.
    fn reserve_with_id(
        &self,
        what: &str,
        which: &str,
        id: uuid::Uuid,
    ) -> Result<own_backup_core::busy::Reservation, Status> {
        if self.shutdown.is_shutdown_requested() {
            return Err(Status::unavailable("service is stopping"));
        }
        let slot = self
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::with_id(what, which, id))
            .map_err(|held| Status::failed_precondition(held.refusal()))?;
        if self.shutdown.is_shutdown_requested() {
            return Err(Status::unavailable("service is stopping"));
        }
        Ok(slot)
    }

    /// Run every plan check against a service-built context and return the
    /// refusals, in the user's words. An empty list means the plan may run.
    ///
    /// The plan is `&mut` because two of the preparations change it: drive
    /// letters that collide are moved to free ones, and a disk size that
    /// disagrees with the device is corrected. Both were already done for the
    /// preview; doing them here too is what makes the executed plan the
    /// checked one.
    async fn validate_restore(
        &self,
        plan: &mut own_backup_common::restore_plan::RestorePlan,
        keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    ) -> Result<Vec<String>, Status> {
        use own_backup_common::restore_check::{check_plan, CheckContext, CheckLevel};

        // Which device is under that number NOW. `PhysicalDriveN` is a slot:
        // unplug a USB disk and plug another in, and the number moves (audit
        // AUD-01, point 5).
        #[cfg(not(target_os = "linux"))]
        let now = live_disk_id(plan.target_disk).await;
        #[cfg(target_os = "linux")]
        let now = restore_linux::live_disk_id(plan);
        match plan
            .target_disk_id
            .as_ref()
            .and_then(|was| was.matches(&now))
        {
            Some(true) => {}
            Some(false) => {
                let was = plan.target_disk_id.clone().unwrap_or_default();
                tracing::warn!(
                    disk = plan.target_disk,
                    device = ?plan.target_device,
                    expected = ?was,
                    found = ?now,
                    "the target disk is not the one the plan was made for"
                );
                return Ok(vec![tr!(
                    "grpc_server.target_disk_changed",
                    disk = plan_disk_name(plan),
                    was_serial = was.serial,
                    was_size = was.size_bytes,
                    now_serial = now.serial,
                    now_size = now.size_bytes
                )
                .into_owned()]);
            }
            // Nothing recorded, or nothing comparable: it cannot be checked,
            // and saying so is all that is honestly available. It is not a
            // refusal — a plan built by a client that predates this field is
            // still a plan the rest of the checks can judge.
            None => tracing::warn!(
                disk = plan.target_disk,
                "the plan carries no target identity, so it cannot be confirmed \
                 that this is still the same device"
            ),
        }

        let ctx = self.restore_context(plan, CheckContext::default(), keys);
        let owned = target_owned_letters(plan);
        let moved = own_backup_common::restore_check::resolve_letter_conflicts_with_owners(
            plan,
            &ctx.taken_letters,
            &owned,
        );
        for m in &moved {
            tracing::info!(
                part = m.part_index,
                asked = %m.requested,
                given = ?m.assigned,
                "restore: the drive letter asked for was taken"
            );
        }
        correct_disk_size(plan);

        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut results = check_plan(plan, &ctx);
        #[cfg(target_os = "linux")]
        results.extend(own_backup_core::restore_exec::linux_refusals(plan));
        for r in &results {
            match r.level {
                CheckLevel::Error => tracing::warn!(
                    plan = %plan.id, code = ?r.code, part = ?r.part_index,
                    "restore refused: {}", r.message
                ),
                _ => tracing::info!(
                    plan = %plan.id, code = ?r.code, part = ?r.part_index,
                    "restore note: {}", r.message
                ),
            }
        }
        Ok(results
            .into_iter()
            .filter(|r| r.level == CheckLevel::Error)
            .map(|r| r.message)
            .collect())
    }

    /// Everything the plan checks need, gathered on this side of the wire.
    ///
    /// The client's context is taken only for the things the client alone
    /// knows — which filesystem the user picked in a dialog that has not been
    /// sent yet. Everything describing the MACHINE is asked of the machine
    /// (audit AUD-01, point 2): what a client sends about disks is a picture it
    /// drew some time ago, and the layout can have changed since it drew it.
    /// These facts are the input to every refusal below, so a client able to
    /// supply them is a client able to switch them off.
    fn restore_context(
        &self,
        plan: &own_backup_common::restore_plan::RestorePlan,
        from_client: own_backup_common::restore_check::CheckContext,
        keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    ) -> own_backup_common::restore_check::CheckContext {
        use own_backup_common::restore_check::CheckContext;

        let mut ctx = CheckContext {
            // The user's own choices in the designer: not facts about the
            // machine, and there is nowhere else to get them.
            requested_formats: from_client.requested_formats,
            // What the image holds. The client read the same sidecar, but the
            // service can read it again and this is the sizes every "will it
            // fit" answer is computed from.
            sources: image_sources(&plan.image_path, keys).unwrap_or_else(|| {
                // A copy made from the command line has no snapshot of the
                // live disk. The window's list, when it sent one, says more;
                // without it (a plan applied from elsewhere) the copy's own
                // partition table still gives every size.
                if from_client.sources.is_empty() {
                    image_layout_sources(&plan.image_path, keys).unwrap_or_default()
                } else {
                    from_client.sources
                }
            }),
            source_sector_size: image_sector_size(&plan.image_path, keys)
                .unwrap_or(from_client.source_sector_size),
            ..Default::default()
        };
        ctx.taken_letters = taken_drive_letters();
        ctx.target_is_system_disk = is_system_disk(plan.target_disk);
        ctx.targets = current_target_partitions(plan.target_disk);

        // Being on the system disk is no longer a veto — a spare partition
        // there is an ordinary target. What is refused is touching the
        // partitions the running system stands on.
        //
        // A protection that could not be worked out is NOT an empty list: an
        // empty list reads as "nothing to protect here", and that was the
        // default a failed check used to land on. Here it becomes a protection
        // over every partition the plan could touch, so the checks refuse and
        // say why, rather than allowing everything.
        let guard = own_backup_restore::protection(plan.target_disk);
        ctx.protected_parts = match &guard.cannot_tell {
            None => guard.parts,
            Some(why) => {
                tracing::warn!(
                    disk = plan.target_disk,
                    reason = %why,
                    "the running system's partitions on this disk could not be determined — \
                     treating the whole disk as protected"
                );
                ctx.targets.iter().map(|t| t.index).collect()
            }
        };
        // Linux disks are named by device, not number: the facts above came
        // back empty or unsure, and these replace them.
        #[cfg(target_os = "linux")]
        restore_linux::fill_context(plan, &mut ctx, keys);
        ctx
    }
}

/// The image's partitions as the checks want them, read from the copy's own
/// sidecar rather than taken from the client.
///
/// This is where every "will it fit" answer comes from, so it has to come from
/// the file the restore will actually read. `None` when the copy is not a
/// whole-disk image or carries no source snapshot — then the client's list is
/// all there is, and the executor's own size check is the backstop.
fn image_sources(
    image: &std::path::Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> Option<Vec<own_backup_common::restore_check::SourcePartition>> {
    use own_backup_common::image::VolumeCaptureMode;
    use own_backup_common::restore_check::SourcePartition;

    let sidecar = read_sidecar_for_checks(image, keys)?;
    let disk = sidecar.disk_manifest.as_ref()?;
    let source = disk.source_disk.as_ref()?;

    // A copy that kept the container's header can be restored still encrypted.
    let headers: Vec<u32> = sidecar
        .info
        .luks_headers
        .iter()
        .map(|h| h.part_index)
        .collect();

    Some(
        source
            .partitions
            .iter()
            .map(|p| {
                // "Encrypted at source, and the copy holds it in the clear" —
                // a container captured verbatim comes back exactly as it was
                // and is not this case at all.
                let captured_plain = disk
                    .images
                    .iter()
                    .find(|i| i.part_index == p.index)
                    .and_then(|i| i.encryption.as_ref())
                    .is_some_and(|e| e.capture_mode == VolumeCaptureMode::Plaintext);
                SourcePartition {
                    index: p.index,
                    size_bytes: p.size_bytes,
                    used_bytes: p.used_bytes,
                    filesystem: p.filesystem,
                    label: p.volume_label.clone(),
                    encrypted_at_source: captured_plain,
                    luks_header_available: headers.contains(&p.index),
                }
            })
            .collect(),
    )
}

/// A whole-disk copy's partitions from its partition table — how full each
/// one is stays unknown (0). `None` when the copy is not a whole-disk image.
fn image_layout_sources(
    image: &std::path::Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> Option<Vec<own_backup_common::restore_check::SourcePartition>> {
    let sidecar = read_sidecar_for_checks(image, keys)?;
    let disk = sidecar.disk_manifest.as_ref()?;
    Some(layout_sources(disk, &sidecar.info.luks_headers))
}

/// Each partition of `disk`'s table, with the filesystem its image recorded.
fn layout_sources(
    disk: &own_backup_common::gpt::DiskBackupManifest,
    luks_headers: &[own_backup_common::image::LuksHeaderBackup],
) -> Vec<own_backup_common::restore_check::SourcePartition> {
    use own_backup_common::image::VolumeCaptureMode;
    use own_backup_common::types::FileSystem;
    let sector = u64::from(disk.layout.sector_size.max(1));
    disk.layout
        .partitions
        .iter()
        .map(|p| {
            let image = disk.images.iter().find(|i| i.part_index == p.index);
            let filesystem = match image.map(|i| i.filesystem.to_ascii_lowercase()).as_deref() {
                Some("ntfs") => FileSystem::Ntfs,
                Some("fat32" | "fat" | "vfat") => FileSystem::Fat32,
                Some("exfat") => FileSystem::ExFat,
                Some("ext2" | "ext3" | "ext4") => FileSystem::Ext4,
                Some("xfs") => FileSystem::Xfs,
                Some("btrfs") => FileSystem::Btrfs,
                Some("refs") => FileSystem::ReFs,
                _ => FileSystem::Unknown,
            };
            own_backup_common::restore_check::SourcePartition {
                index: p.index,
                size_bytes: (p.last_lba + 1).saturating_sub(p.first_lba) * sector,
                used_bytes: 0,
                filesystem,
                label: p.name.clone(),
                encrypted_at_source: image
                    .and_then(|i| i.encryption.as_ref())
                    .is_some_and(|e| e.capture_mode == VolumeCaptureMode::Plaintext),
                luks_header_available: luks_headers.iter().any(|h| h.part_index == p.index),
            }
        })
        .collect()
}

/// The copy's own description of itself, for the plan checks — with the key
/// when there is one.
///
/// An ENCRYPTED copy says nothing without it. Read keyless, it came back as
/// "not a whole-disk image", the service fell through to whatever the client
/// had sent, and `apply_restore_plan` sends no context at all — so every
/// operation was refused with «в копии нет раздела с номером N» about a copy
/// that plainly had them. The dry run passed, because the window did have the
/// key and its list filled the gap: the preview said yes and the restore said
/// no, which is the worst way for these two to disagree.
fn read_sidecar_for_checks(
    image: &std::path::Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> Option<own_backup_backup::Sidecar> {
    match own_backup_backup::sidecar::read_sidecar_with_keys(image, keys) {
        Ok(sidecar) => Some(sidecar),
        Err(e) => {
            // No falling back to a keyless read when a key was given and failed:
            // that answers for a different file shape and would let the checks
            // judge a copy nobody has opened.
            tracing::warn!(image = %image.display(), error = %e,
                "plan checks: the copy's description could not be read");
            None
        }
    }
}

/// Logical sector size the image's layout was captured with.
fn image_sector_size(
    image: &std::path::Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
) -> Option<u32> {
    if keys.is_some() {
        return read_sidecar_for_checks(image, keys)
            .map(|s| s.manifest.source_disk.sector_size_logical)
            .filter(|s| *s > 0);
    }
    own_backup_backup::sidecar::read_image_sidecar_header(image)
        .ok()
        .map(|(_, manifest)| manifest.source_disk.sector_size_logical)
        .filter(|s| *s > 0)
}

/// Who the disk under `PhysicalDriveN` is right now.
///
/// Read straight off the device rather than from the inventory, so it is the
/// same question the restore itself will ask a moment later, answered without
/// a scan of every disk in the machine.
#[cfg(target_os = "windows")]
async fn live_disk_id(disk: u32) -> own_backup_common::restore_plan::TargetDiskId {
    use own_backup_common::restore_plan::TargetDiskId;
    use own_backup_inventory::DiskInventory;

    let mut id = TargetDiskId::default();
    if let Ok(layout) = own_backup_backup::gpt_reader::read_disk_layout(disk) {
        id.disk_guid = layout.disk_guid;
        id.size_bytes = layout.size_bytes;
    }
    // The serial is the one fact that survives repartitioning, so it is worth
    // the enumeration; a machine with no inventory answer still has the two
    // facts above.
    if let Ok(inv) = own_backup_inventory::WindowsInventory::new() {
        if let Ok(disks) = inv.enumerate_disks().await {
            let want = format!(r"\\.\PhysicalDrive{disk}");
            if let Some(info) = disks
                .iter()
                .find(|d| d.device_id.eq_ignore_ascii_case(&want))
            {
                id.serial = info.serial.clone();
                if id.size_bytes == 0 {
                    id.size_bytes = info.size_bytes;
                }
            }
        }
    }
    id
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
async fn live_disk_id(_disk: u32) -> own_backup_common::restore_plan::TargetDiskId {
    own_backup_common::restore_plan::TargetDiskId::default()
}

/// What the target disk holds right now.
#[cfg(target_os = "windows")]
fn current_target_partitions(disk: u32) -> Vec<own_backup_common::restore_check::TargetPartition> {
    use own_backup_common::restore_check::TargetPartition;

    let Ok(layout) = own_backup_backup::gpt_reader::read_disk_layout(disk) else {
        return Vec::new();
    };
    layout
        .partitions
        .iter()
        .map(|p| TargetPartition {
            index: p.index,
            offset_bytes: p.offset_bytes(layout.sector_size),
            size_bytes: p.size_bytes(layout.sector_size),
            // Knowing how full a partition is means mounting it, which is not
            // something a safety check should do. Zero means "unknown" to the
            // checks, which is the honest answer.
            used_bytes: 0,
            filesystem: own_backup_common::types::FileSystem::Unknown,
            label: p.name.trim().to_string(),
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn current_target_partitions(_disk: u32) -> Vec<own_backup_common::restore_check::TargetPartition> {
    Vec::new()
}

/// Only the live volume at a planned target offset may keep its letter.
/// These facts come from the service's disk, never from client-supplied context.
#[cfg(target_os = "windows")]
fn target_owned_letters(plan: &own_backup_common::restore_plan::RestorePlan) -> Vec<(u32, char)> {
    use own_backup_common::restore_plan::RestoreOp;
    let roots = own_backup_backup::volume_win::all_volume_roots_on_disk(plan.target_disk);
    plan.ops
        .iter()
        .filter_map(|op| {
            let RestoreOp::AssignLetter { part, letter } = op else {
                return None;
            };
            roots
                .iter()
                .find(|root| {
                    own_backup_backup::volume_win::volume_start_on_disk(root)
                        == Some((plan.target_disk, part.offset))
                })
                .and_then(|root| {
                    own_backup_backup::volume_win::volume_drive_letters(root)
                        .into_iter()
                        .find(|current| current.eq_ignore_ascii_case(&format!("{letter}:")))
                        .map(|_| (part.index, *letter))
                })
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn target_owned_letters(_plan: &own_backup_common::restore_plan::RestorePlan) -> Vec<(u32, char)> {
    Vec::new()
}

/// Bridges core orchestration events onto the broadcast channel that
/// `StreamEvents` reads. `send` errors only when there are no subscribers,
/// which is fine — the events are simply dropped until a client connects.
pub struct GrpcEventSink {
    pub tx: broadcast::Sender<proto::Event>,
}

impl own_backup_core::EventSink for GrpcEventSink {
    fn job_progress(&self, progress: own_backup_core::JobProgress) {
        let _ = self.tx.send(Event {
            kind: "progress".into(),
            json: serde_json::to_string(&progress).unwrap_or_default(),
        });
    }
    fn queue_changed(&self, snapshot: own_backup_core::QueueSnapshot) {
        let _ = self.tx.send(Event {
            kind: "queue".into(),
            json: serde_json::to_string(&snapshot).unwrap_or_default(),
        });
    }
    fn job_finished(&self, run: own_backup_core::JobRun) {
        let _ = self.tx.send(Event {
            kind: "finished".into(),
            json: serde_json::to_string(&run).unwrap_or_default(),
        });
    }
}

/// The hidden system partitions on the disks attached right now.
#[cfg(target_os = "windows")]
async fn hidden_volumes_now() -> Result<Vec<own_backup_common::disk::HiddenVolume>, Status> {
    use own_backup_inventory::DiskInventory;
    let disks = own_backup_inventory::WindowsInventory::new()
        .map_err(|e| Status::internal(e.to_string()))?
        .enumerate_disks()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    Ok(own_backup_common::disk::hidden_volumes(&disks))
}

#[cfg(not(target_os = "windows"))]
async fn hidden_volumes_now() -> Result<Vec<own_backup_common::disk::HiddenVolume>, Status> {
    Err(Status::unimplemented(
        "drive letters for hidden partitions are a Windows feature",
    ))
}

/// Assign `letter` to `volume`, or remove its letter when `letter` is `None`.
#[cfg(target_os = "windows")]
fn change_volume_letter(
    volume: &own_backup_common::disk::HiddenVolume,
    letter: Option<&str>,
) -> Result<(), Status> {
    use own_backup_backup::volume_win::{
        assign_drive_letter, drive_letter_in_use, remove_drive_letter,
    };
    match letter {
        Some(text) => {
            // D–Z: A and B are the floppy letters Windows still reserves, and
            // C is the system's.
            let letter = match text
                .trim()
                .trim_end_matches(':')
                .chars()
                .collect::<Vec<_>>()[..]
            {
                [c] if c.is_ascii_alphabetic() && c.to_ascii_uppercase() >= 'D' => {
                    c.to_ascii_uppercase()
                }
                _ => {
                    return Err(Status::invalid_argument(
                        tr!("grpc_server.letter_bad", letter = text).into_owned(),
                    ))
                }
            };
            if let Some(have) = volume.letter {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.letter_already_set", letter = have).into_owned(),
                ));
            }
            if drive_letter_in_use(letter) {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.letter_busy", letter = letter).into_owned(),
                ));
            }
            assign_drive_letter(&volume.device, letter).map_err(|e| {
                Status::internal(
                    tr!(
                        "grpc_server.letter_assign_failed",
                        letter = letter,
                        error = e
                    )
                    .into_owned(),
                )
            })
        }
        None => {
            let Some(have) = volume.letter else {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.letter_none").into_owned(),
                ));
            };
            remove_drive_letter(have).map_err(|e| {
                Status::internal(
                    tr!("grpc_server.letter_remove_failed", letter = have, error = e).into_owned(),
                )
            })
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn change_volume_letter(
    _volume: &own_backup_common::disk::HiddenVolume,
    _letter: Option<&str>,
) -> Result<(), Status> {
    Err(Status::unimplemented(
        "drive letters for hidden partitions are a Windows feature",
    ))
}

/// Authenticate an incoming request via bearer token. Empty expected token =
/// auth disabled (dev/TCP mode). Constant-time comparison avoids a timing oracle.
/// A call over the Unix socket must also come from root or a user with an
/// active local session (`peer_access`): the token alone admits every member
/// of the access group, from anywhere.
fn check_auth<T>(req: &Request<T>, expected: &str) -> Result<(), Status> {
    #[cfg(unix)]
    crate::peer_access::check(
        req.extensions()
            .get::<tonic::transport::server::UdsConnectInfo>(),
    )?;
    if expected.is_empty() {
        return Ok(());
    }
    let provided = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        tracing::warn!(target: "security", "gRPC request rejected: invalid bearer token");
        return Err(Status::unauthenticated("Invalid or missing token"));
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Serialize any value into a `JsonReply`.
fn json_reply<T: serde::Serialize>(value: &T) -> Result<Response<JsonReply>, Status> {
    let json = serde_json::to_string(value)
        .map_err(|e| Status::internal(format!("serialize failed: {e}")))?;
    Ok(Response::new(JsonReply { json }))
}

/// What a manual operation was given to open an encrypted copy with.
///
/// Both are optional because most copies are not encrypted; when one is, the
/// caller must have supplied exactly one of these.
#[derive(serde::Deserialize, Default)]
struct ManualSecret {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    recovery_key: Option<String>,
}

/// Keys for a MANUAL mount, restore or verify, from what the person typed.
///
/// **Never the service's own key store**, and that is the line §7.4 draws: a
/// scheduled copy runs with nobody present and therefore must be able to open
/// its key by itself, but handing data BACK — to whoever happens to be at the
/// console, or to whoever reached this socket — costs a password every time.
/// The service holding a key for its own copies is not consent to decrypt.
///
/// `Ok(None)` means the image is not encrypted and needs nothing.
/// `Err(unauthenticated)` means it is, and nothing was supplied — the code is
/// how a screen knows to ask rather than to show an error.
///
/// Slow: opening the envelope is Argon2id at the production profile, seconds.
/// [`manual_keys_off_thread`] is what callers use, so that the machine's single
/// busy slot is not held for the length of a key derivation.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn manual_keys(
    image: &std::path::Path,
    secret: &ManualSecret,
) -> Result<Option<own_backup_backup::seal::context::ArchiveKeys>, Status> {
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::obk::OBK_EXTENSION;
    use own_backup_backup::seal::recovery::RecoveryKey;

    let container = own_backup_backup::sidecar::detect_container_format(image)
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    if container != OBK_EXTENSION {
        return Ok(None);
    }
    match (&secret.password, &secret.recovery_key) {
        (Some(_), Some(_)) => Err(Status::invalid_argument(
            tr!("grpc_server.password_or_recovery_key").into_owned(),
        )),
        (Some(password), None) => ArchiveKeys::from_image_password(image, password)
            .map(Some)
            .map_err(refused_secret),
        (None, Some(typed)) => {
            let key =
                RecoveryKey::parse(typed).map_err(|e| Status::invalid_argument(e.to_string()))?;
            ArchiveKeys::from_image_recovery(image, &key)
                .map(Some)
                .map_err(refused_secret)
        }
        (None, None) => Err(Status::unauthenticated(
            tr!(
                "grpc_server.image_encrypted_needs_secret",
                image = image.display()
            )
            .into_owned(),
        )),
    }
}

/// A refused password or recovery key, in the words it was refused with.
///
/// The key file reports it as a `Config` error, whose `Display` puts an English
/// «Configuration error:» in front — and that is what the unlock dialog showed
/// under a mistyped password.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn refused_secret(e: own_backup_common::OwnBackupError) -> Status {
    match e {
        own_backup_common::OwnBackupError::Config(m) => Status::permission_denied(m),
        other => Status::permission_denied(other.to_string()),
    }
}

/// [`manual_keys`] on a blocking thread, **before** the operation reserves the
/// machine.
///
/// Deriving a key is seconds of Argon2id. Doing it while holding the single
/// busy slot would make every mistyped password block the next backup for as
/// long as it takes to fail.
async fn manual_keys_off_thread(
    image: std::path::PathBuf,
    secret: ManualSecret,
) -> Result<Option<own_backup_backup::seal::context::ArchiveKeys>, Status> {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        tokio::task::spawn_blocking(move || manual_keys(&image, &secret))
            .await
            .map_err(|e| Status::internal(format!("key task panicked: {e}")))?
    }
    // No engine here, so nothing can use a key anyway; the operation that
    // follows refuses with its own message.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (image, secret);
        Ok(None)
    }
}

/// Drive letters in use on this machine. A dry run needs them to resolve
/// conflicts (D11), and only the service can see them — the GUI runs
/// unelevated and would be guessing.
fn taken_drive_letters() -> Vec<char> {
    #[cfg(target_os = "windows")]
    {
        own_backup_backup::letters_win::taken_letters()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// Whether `disk` is the one the running OS boots from.
///
/// Informational now, not a refusal: what gets refused is a plan that touches
/// the partitions that system is standing on (`protected_partitions`).
fn is_system_disk(disk: u32) -> bool {
    own_backup_restore::system_disk_number() == Some(disk) && !own_backup_restore::is_boot_media()
}

/// Replace the plan's idea of the target disk's size with the device's own.
///
/// The designer gets the figure from the inventory, and an inventory figure can
/// be a little short of the truth. Every bound in the checks — where the backup
/// GPT goes, whether the last partition fits — is measured against it, so a
/// disk reported a few megabytes small makes a perfectly valid layout look as
/// if its last partition runs off the end.
///
/// The executor already resolves this the same way when it writes the table
/// (`DiskSession::write_partition_table`). Doing it here too means the dry run
/// checks the same geometry that will actually be written, rather than a second
/// opinion about it.
fn correct_disk_size(plan: &mut own_backup_common::restore_plan::RestorePlan) {
    #[cfg(target_os = "windows")]
    {
        use own_backup_common::restore_plan::RestoreOp;
        let path = format!(r"\\.\PhysicalDrive{}", plan.target_disk);
        let Ok(dev) = own_backup_backup::win_device::WinRawDevice::open_read(&path) else {
            return; // not reachable right now; the checks fall back to the plan's figure
        };
        let actual = dev.length();
        for op in plan.ops.iter_mut() {
            if let RestoreOp::WritePartitionTable { table } = op {
                if table.disk_size_bytes != actual {
                    tracing::debug!(
                        disk = plan.target_disk,
                        planned = table.disk_size_bytes,
                        actual,
                        "corrected the plan's disk size from the device"
                    );
                    table.disk_size_bytes = actual;
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    restore_linux::correct_disk_size(plan);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = plan;
    }
}

/// How a message names a plan's target disk: by number on Windows, by its
/// device on Linux, where `target_disk` carries no meaning (always 0).
fn plan_disk_name(plan: &own_backup_common::restore_plan::RestorePlan) -> String {
    if cfg!(target_os = "linux") {
        plan.target_device
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| plan.target_disk.to_string())
    } else {
        plan.target_disk.to_string()
    }
}

/// Message returned by the direct engine RPCs when built off Windows (the VHDX
/// engine is Windows-only for now — see plan phase 8).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn engine_msg() -> String {
    tr!("grpc_server.engine_windows_only").into_owned()
}

/// Broadcast one engine-progress event (`restore` / `verify` / `restore-disk`)
/// to `StreamEvents` subscribers. Called from the blocking engine thread.
fn emit_engine<T: serde::Serialize>(
    events: &broadcast::Sender<proto::Event>,
    kind: &str,
    payload: &T,
) {
    let _ = events.send(Event {
        kind: kind.into(),
        json: serde_json::to_string(payload).unwrap_or_default(),
    });
}

/// `CheckDestination`: the destination question of [`validate_job`], asked
/// before there is a job to save. The same two checks in the same order, so
/// the wizard says what the save would say. `placement` is decided on, the
/// texts are only shown.
fn destination_answer(sources: &[String], dest_folder: &str) -> serde_json::Value {
    let sources: Vec<String> = sources
        .iter()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    let mut answer = placement_answer(&sources, dest_folder);
    answer["key_store_unknown"] = key_store_unknown(&sources);
    answer
}

/// The sources on which it cannot be told whether the service's key
/// directory lies (ZFS, overlayfs): the engine treats them as holding it
/// (decision 6/E4) — a copy that cannot leave it out is refused — and the
/// wizard says so before the run. `[{ source, reason }]`; empty off Linux.
fn key_store_unknown(sources: &[String]) -> serde_json::Value {
    #[cfg(target_os = "linux")]
    {
        use own_backup_backup::seal::local::{key_store_placement, KeyStorePlacement};
        // Linux sources are device paths; anything else is not one to place.
        sources
            .iter()
            .filter(|s| s.starts_with('/'))
            .filter_map(|s| match key_store_placement(s) {
                KeyStorePlacement::CannotTell(why) => {
                    Some(serde_json::json!({ "source": s, "reason": why }))
                }
                KeyStorePlacement::Clear | KeyStorePlacement::Here => None,
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sources;
        serde_json::json!([])
    }
}

/// Where the destination sits: `{ placement, source, reason }`.
fn placement_answer(sources: &[String], dest_folder: &str) -> serde_json::Value {
    let answer = |placement: &str, source: &str, reason: &str| serde_json::json!({ "placement": placement, "source": source, "reason": reason });
    let sources = sources.to_vec();
    if sources.is_empty() || dest_folder.trim().is_empty() {
        return answer("clear", "", "");
    }
    let job = own_backup_core::Job {
        sources,
        ..Default::default()
    };
    if let Some(letter) = own_backup_core::dest_collision_letter(&job, dest_folder) {
        return answer("on_source", &format!("{letter}:"), "");
    }
    match own_backup_core::dest_placement(&job, dest_folder) {
        own_backup_core::DestPlacement::Clear => answer("clear", "", ""),
        own_backup_core::DestPlacement::OnSource(source) => answer("on_source", &source, ""),
        own_backup_core::DestPlacement::CannotTell(why) => answer("cannot_tell", "", &why),
    }
}

/// Shared save/run validation: name + at least one source, dest non-empty, and
/// the destination not on ANY of the job's source volumes.
///
/// `encrypted` — whether the job WILL write encrypted copies once this
/// operation lands. It cannot be read off `job` alone: an ordinary save may
/// arrive without the authorization (the store puts the stored one back), and
/// the encryption wizard validates the job before the authorization exists.
fn validate_job(job: &own_backup_core::Job, encrypted: bool) -> Result<(), Status> {
    if job.name.trim().is_empty() {
        return Err(Status::invalid_argument(
            tr!("grpc_server.job_name_missing").into_owned(),
        ));
    }
    if job.source_list().is_empty() {
        return Err(Status::invalid_argument(
            tr!("grpc_server.job_source_missing").into_owned(),
        ));
    }
    if job.dest_folder.trim().is_empty() {
        return Err(Status::invalid_argument(
            tr!("grpc_server.job_dest_missing").into_owned(),
        ));
    }
    if let Some(letter) = own_backup_core::dest_collision_letter(job, &job.dest_folder) {
        return Err(Status::invalid_argument(
            tr!("grpc_server.job_dest_on_source_letter", letter = letter).into_owned(),
        ));
    }
    // And the same question asked properly: by the sectors each volume
    // occupies, so a source with no drive letter is seen too (audit AUD-10).
    // Saving a job writes nothing, so an unanswerable case is noted here and
    // refused where it matters — immediately before the copy.
    match own_backup_core::dest_placement(job, &job.dest_folder) {
        own_backup_core::DestPlacement::Clear => {}
        own_backup_core::DestPlacement::OnSource(source) => {
            return Err(Status::invalid_argument(
                tr!("grpc_server.job_dest_on_source_volume", source = source).into_owned(),
            ))
        }
        own_backup_core::DestPlacement::CannotTell(why) => {
            tracing::warn!(job = %job.name, dest = %job.dest_folder, reason = %why,
                "could not confirm the destination is not on a source volume")
        }
    }
    // The image format must never change over existing archives (decision
    // 2026-07-08): reject the save right away when the destination already
    // holds this job's backups in a different container. The runners repeat
    // the same check before every run (defence in depth).
    //
    // The container is what the RUNNER will write, and for an encrypted job
    // that is `.obk`, whatever `image_format` says (it stays qcow2 underneath).
    // Comparing against `image_format` alone refused every save of every
    // encrypted job that had made a single copy: the check saw the job's own
    // `.obk` as "another format" — ticking «проверять после создания» was
    // refused with a message about changing a format nobody had touched.
    let folder = std::path::Path::new(&job.dest_folder);
    if folder.is_dir() {
        if let Ok(chains) = own_backup_backup::chains::list_job_chains(folder, &job.id) {
            let want = if encrypted {
                own_backup_backup::seal::obk::OBK_EXTENSION
            } else {
                job.image_format.extension()
            };
            let mut images = chains.iter().flat_map(|c| c.images.iter());
            if let Some(other) = images.find(|i| i.container != want) {
                // Turning encryption on over plaintext copies IS a format
                // change, and it used to pass here — then every run after it
                // failed the runner's own check, at night, with nobody told
                // why. Said now, while the person is looking at the screen.
                let msg = if encrypted
                    && other.container != own_backup_backup::seal::obk::OBK_EXTENSION
                {
                    tr!("grpc_server.job_encrypt_over_plain_copies")
                } else {
                    tr!("grpc_server.job_format_change")
                };
                return Err(Status::invalid_argument(msg.into_owned()));
            }
        }
    }
    Ok(())
}

#[tonic::async_trait]
impl BackupService for BackupServiceImpl {
    // --- Jobs -------------------------------------------------------------

    async fn list_jobs(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.list())
    }

    async fn get_job(&self, req: Request<IdRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.get(&req.into_inner().id))
    }

    async fn save_job(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let job: own_backup_core::Job = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad job JSON: {e}")))?;
        // Encrypted if the edit says so or the stored job already is — the
        // store keeps a stored authorization when a client omits the field, so
        // the check has to judge the job the store is about to write.
        let encrypted = job.archive_encryption.is_some()
            || self
                .state
                .store
                .get(&job.id)
                .is_some_and(|stored| stored.archive_encryption.is_some());
        validate_job(&job, encrypted)?;
        // Reported as saved only once it is on disk: a full destination or a
        // read-only data folder used to give a job that lived until the next
        // restart and then vanished (audit AUD-08).
        let saved = self.state.store.upsert(job).map_err(|e| {
            Status::internal(tr!("grpc_server.job_not_saved", error = e).into_owned())
        })?;
        json_reply(&saved)
    }

    async fn delete_job(
        &self,
        req: Request<DeleteJobRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // Deleting the job's images is deleting backups; the job alone is not.
        #[cfg(target_os = "linux")]
        if req.get_ref().delete_files {
            crate::polkit::authorize(&req, crate::polkit::Action::DeleteBackups).await?;
        }
        let DeleteJobRequest { id, delete_files } = req.into_inner();

        // A run in flight is writing into the very files this is about to
        // delete. Deleting them under it is not "best effort", it is pulling
        // the floor out (audit AUD-09). The user is told to stop it first;
        // cancelling on their behalf would throw away a backup they may be
        // minutes from finishing.
        let queue = self.state.queue.snapshot();
        if queue.current.as_deref() == Some(id.as_str()) {
            return Err(Status::failed_precondition(
                tr!("grpc_server.job_running_cannot_delete").into_owned(),
            ));
        }
        // Waiting in the queue is a different matter — nothing has started, so
        // it is simply taken out before the job goes.
        if self.state.queue.cancel_pending(&id) {
            tracing::info!(job = %id, "delete_job: taken out of the queue first");
        }

        let report = if delete_files {
            match self.state.store.get(&id) {
                Some(job) => {
                    // Deleting a job's images is destructive work on the same
                    // folders a backup writes and a restore reads, so it takes
                    // the machine's slot like the rest (audit AUD-02).
                    let _slot = self.reserve(&tr!("grpc_server.busy_delete_copies"), &job.name)?;
                    own_backup_core::delete_job_files(&self.state.store, &job)
                }
                None => own_backup_core::JobFilesDeleted::default(),
            }
        } else {
            own_backup_core::JobFilesDeleted::default()
        };
        for (path, why) in &report.kept {
            tracing::warn!(job = %id, path = %path, reason = %why, "delete_job: left on disk");
        }

        self.state.store.delete(&id).map_err(|e| {
            Status::internal(tr!("grpc_server.job_not_deleted", error = e).into_owned())
        })?;
        json_reply(&report)
    }

    async fn new_job_template(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let name = std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("HOSTNAME")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| tr!("grpc_server.default_job_name").into_owned());
        json_reply(&own_backup_core::Job {
            name,
            ..Default::default()
        })
    }

    async fn default_exclusions(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // JSON: { "windows": [string], "linux": [string] } — one default list
        // per OS family for the Windows/Linux exclusion tabs.
        json_reply(&serde_json::json!({
            "windows": own_backup_core::default_exclusions(),
            "linux": own_backup_core::default_linux_exclusions(),
        }))
    }

    // --- Hidden system partitions ---------------------------------------

    async fn list_hidden_volumes(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&hidden_volumes_now().await?)
    }

    /// Give a hidden system partition a drive letter, or take it away.
    ///
    /// Only for what [`own_backup_common::disk::hidden_volumes`] lists: the
    /// letters of ordinary volumes are what jobs and restores are built on,
    /// and Disk Management changes those. Re-checked here against the disks
    /// as they are now — never from what the window last saw.
    async fn set_volume_letter(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            device: String,
            #[serde(default)]
            letter: Option<String>,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        let _slot = self.reserve(&tr!("grpc_server.busy_volume_letter"), &p.device)?;
        let volumes = hidden_volumes_now().await?;
        let Some(volume) = volumes
            .iter()
            .find(|v| v.device.eq_ignore_ascii_case(p.device.trim()))
        else {
            return Err(Status::failed_precondition(
                tr!("grpc_server.letter_not_hidden_volume", device = p.device).into_owned(),
            ));
        };
        change_volume_letter(volume, p.letter.as_deref())?;
        tracing::info!(device = %p.device, letter = ?p.letter, "gRPC: SetVolumeLetter done");
        json_reply(&hidden_volumes_now().await?)
    }

    // --- Queue / runs -----------------------------------------------------

    async fn run_job(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let job = self
            .state
            .store
            .get(&id)
            .ok_or_else(|| Status::not_found(tr!("grpc_server.job_not_found").into_owned()))?;
        // The guard asks where the sources are, so it has to ask about the
        // partitions as they are now: after Windows Update re-creates the
        // recovery partition, the saved device names a volume that no longer
        // exists and "cannot tell" refused every run. The worker saves the
        // updated job before it copies; here it is only looked at.
        #[cfg(target_os = "windows")]
        let job = match own_backup_core::source_refresh::reconcile_with_live_disks(&job).await {
            own_backup_core::source_refresh::Refreshed::Updated(r) => r.job,
            own_backup_core::source_refresh::Refreshed::Missing(why) => {
                return Err(Status::failed_precondition(why))
            }
            own_backup_core::source_refresh::Refreshed::Unchanged => job,
        };
        // Guard against writing onto any source volume (mirrors save-time
        // check). This is the one that runs immediately before anything is
        // written, so here "cannot tell" is a refusal: not knowing where the
        // copy will land is not knowing that it lands somewhere safe (audit
        // AUD-10, point 2).
        if let Some(letter) = own_backup_core::dest_collision_letter(&job, &job.dest_folder) {
            return Err(Status::failed_precondition(
                tr!("grpc_server.run_dest_on_source_letter", letter = letter).into_owned(),
            ));
        }
        match own_backup_core::dest_placement(&job, &job.dest_folder) {
            own_backup_core::DestPlacement::Clear => {}
            own_backup_core::DestPlacement::OnSource(source) => {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.run_dest_on_source_volume", source = source).into_owned(),
                ))
            }
            own_backup_core::DestPlacement::CannotTell(why) => {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.run_dest_cannot_tell", reason = why).into_owned(),
                ))
            }
        }
        // Asked and added in one step: two requests arriving together used to
        // be able to both find it absent and both add it.
        if !self.state.queue.enqueue(&id) {
            return Ok(Response::new(Empty {}));
        }
        if let Err(e) = self.state.job_tx.send(id.clone()) {
            // Nobody will ever come for it — leaving it waiting would show a
            // job queued forever.
            self.state.queue.cancel_pending(&id);
            return Err(Status::internal(format!("enqueue failed: {e}")));
        }
        Ok(Response::new(Empty {}))
    }

    async fn cancel_queued(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        self.state.queue.cancel_pending(&id);
        Ok(Response::new(Empty {}))
    }

    async fn cancel_running(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        self.state
            .cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Response::new(Empty {}))
    }

    async fn get_queue(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.queue.snapshot())
    }

    async fn list_activity(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        json_reply(&self.state.store.activity())
    }

    type StreamEventsStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send>>;

    async fn stream_events(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        check_auth(&req, &self.service_token)?;
        // Drop "lagged" notices silently; a slow client just misses old events.
        let mut events =
            BroadcastStream::new(self.state.events.subscribe()).filter_map(|r| r.ok().map(Ok));
        let mut shutdown = self.shutdown.clone();
        let busy = self.state.busy.clone();
        let stream = async_stream::stream! {
            let drained = async move {
                shutdown.wait_for_shutdown().await;
                busy.wait_until_idle().await;
            };
            tokio::pin!(drained);
            loop {
                tokio::select! {
                    // A subscriber must release its HTTP/2 stream before tonic
                    // can finish graceful shutdown, even if it stays connected.
                    _ = &mut drained => break,
                    event = events.next() => match event {
                        Some(event) => yield event,
                        None => break,
                    },
                }
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    // --- Encrypted volumes ------------------------------------------------

    async fn provide_volume_password(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        use own_backup_common::engine_dto::VolumeUnlockDto;
        #[derive(serde::Deserialize)]
        struct Payload {
            volume: String,
            password: String,
            #[serde(default)]
            remember: bool,
        }
        let p: Payload = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

        // Unlock + probe HERE (LocalSystem/root has raw-disk access; the GUI does
        // not — doing it there fails with CreateFileW access-denied on the disk).
        #[cfg(target_os = "windows")]
        let dto = {
            let (vol, pw) = (p.volume.clone(), p.password.clone());
            let probe = tokio::task::spawn_blocking(move || {
                own_backup_backup::engine_win::unlock_and_probe(&vol, &pw)
            })
            .await
            .map_err(|e| Status::internal(format!("probe task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;
            VolumeUnlockDto {
                ok: probe.ok,
                filesystem: probe.filesystem,
                label: probe.label,
                used_bytes: probe.used_bytes,
                linux_last_mounted: probe.linux_last_mounted,
            }
        };
        #[cfg(target_os = "linux")]
        let dto = {
            let (vol, pw) = (p.volume.clone(), p.password.clone());
            let probe = tokio::task::spawn_blocking(move || {
                own_backup_backup::engine_linux::unlock_and_probe(&vol, &pw)
            })
            .await
            .map_err(|e| Status::internal(format!("probe task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;
            VolumeUnlockDto {
                ok: probe.ok,
                filesystem: probe.filesystem,
                label: probe.label,
                used_bytes: probe.used_bytes,
                linux_last_mounted: probe.linux_last_mounted,
            }
        };
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let dto: VolumeUnlockDto = {
            let _ = &p;
            return Err(Status::unimplemented(
                "volume unlock is not available on this platform",
            ));
        };

        // Only cache a password that actually unlocked the volume. Key by the
        // same token the backup path resolves against.
        if dto.ok {
            let token = own_backup_core::source_token(&p.volume);
            self.state
                .passwords
                .remember(&token, p.password, p.remember)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        json_reply(&dto)
    }

    // --- Inventory --------------------------------------------------------

    async fn list_disks(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        info!("gRPC: ListDisks");
        use own_backup_inventory::DiskInventory;

        #[cfg(target_os = "windows")]
        let mut disks = own_backup_inventory::WindowsInventory::new()
            .map_err(|e| Status::internal(e.to_string()))?
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Ask each encrypted volume how it can be opened. The inventory cannot
        // do this itself — the parser lives in the backup crate, which depends
        // on the inventory — so it is filled in here, where both are in reach.
        // A few sector reads per encrypted partition, and no password.
        #[cfg(target_os = "windows")]
        for disk in disks.iter_mut() {
            let Some(number) = disk
                .device_id
                .rsplit("PhysicalDrive")
                .next()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            for part in disk.partitions.iter_mut().filter(|p| p.is_encrypted) {
                let (methods, unlockable, recovery_key_only) =
                    own_backup_backup::engine_win::describe_encryption(
                        number,
                        part.offset_bytes,
                        part.size_bytes,
                    );
                part.unlock_methods = methods;
                part.unlockable = unlockable;
                part.recovery_key_only = recovery_key_only;
            }
        }

        #[cfg(target_os = "linux")]
        let disks = own_backup_inventory::LinuxInventory::new()
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let disks = own_backup_inventory::MockInventory::default()
            .enumerate_disks()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        json_reply(&disks)
    }

    async fn get_service_status(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let internal_mounts = self.state.active_mounts.lock().unwrap().clone();

        // Off the runtime: on Windows this asks `diskpart` for disks attached
        // outside the program, a child process that takes seconds — and the
        // «Инструменты» screen asks every five. Run inline, it held a worker
        // thread of the gRPC server for the whole time.
        #[cfg(target_os = "windows")]
        let active_mounts = tokio::task::spawn_blocking(move || {
            own_backup_backup::engine_win::get_all_active_mounts(&internal_mounts)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        #[cfg(target_os = "linux")]
        let active_mounts =
            own_backup_backup::engine_linux::get_all_active_mounts(&internal_mounts);

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let active_mounts = internal_mounts;

        let status = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": self.state.uptime_seconds(),
            "queue": self.state.queue.snapshot(),
            "active_mounts": active_mounts,
        });
        Ok(Response::new(JsonReply {
            json: status.to_string(),
        }))
    }

    async fn backup_capabilities(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let caps = own_backup_core::capabilities::current();
        // Linux: encryption is offered only where this machine has a key
        // store, and the window is told what protects it (TPM2 or host key).
        #[cfg(target_os = "linux")]
        let caps = {
            let store = self.state.store.clone();
            let protection = tokio::task::spawn_blocking(move || store.key_store_protection())
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            own_backup_core::capabilities::with_key_store(caps, protection)
        };
        let json = serde_json::to_string(&caps).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(JsonReply { json }))
    }

    async fn check_destination(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            sources: Vec<String>,
            dest_folder: String,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        // Off the runtime: the answer reads the mount table and sysfs on Linux
        // and opens volumes on Windows, and the wizard asks as the user types.
        let answer =
            tokio::task::spawn_blocking(move || destination_answer(&p.sources, &p.dest_folder))
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(JsonReply {
            json: answer.to_string(),
        }))
    }

    // --- Settings (config.toml) -------------------------------------------

    /// The config as this service reads it. On Linux the window cannot always
    /// read root's data folder itself, and this is the copy that counts.
    async fn get_settings(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let config =
            tokio::task::spawn_blocking(own_backup_common::config::AppConfig::load_or_default)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
        json_reply(&config)
    }

    /// Change the fields present in the request (`SettingsPatch`), checked
    /// for this caller (`settings`), and return what is stored now.
    async fn save_settings(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let caller = crate::settings::caller_of(&req);
        let patch: own_backup_common::config::SettingsPatch =
            serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad settings JSON: {e}")))?;
        let saved = tokio::task::spawn_blocking(move || {
            crate::settings::save(
                &own_backup_common::config::AppConfig::default_path(),
                &patch,
                caller,
            )
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        json_reply(&saved)
    }

    // --- Direct engine operations ----------------------------------------

    async fn restore_volume(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::{restore_volume, VolumeRestoreRequest};
            use own_backup_common::engine_dto::{EngineProgress, RestoreSummaryDto};

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                target: String,
                #[serde(flatten)]
                secret: ManualSecret,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            // This path writes sectors straight onto a device with no plan and
            // no table, so none of the plan checks apply to it — and it had no
            // guard of its own at all, which made the running system's volume
            // a perfectly acceptable target (audit AUD-01).
            own_backup_restore::guard_target_volume(&p.target)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;

            // The machine's one slot. Writing a volume back cannot share the
            // machine with a backup, another restore or a verify (audit
            // AUD-02); the guard travels into the blocking closure so it is
            // released even if the engine panics there.
            let slot = self.reserve(&tr!("grpc_server.busy_volume_restore"), &p.target)?;
            let cancel = slot.cancel();
            let events = self.state.events.clone();
            let image_path = std::path::PathBuf::from(&p.image_path);
            let request = VolumeRestoreRequest {
                image_path: image_path.clone(),
                target: p.target,
                verify_checksums: true,
                // From what the person typed, never from the service's own key
                // store (see `manual_keys`).
                encryption: manual_keys_off_thread(image_path, p.secret).await?,
            };

            let summary = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _awake = keep_awake(&tr!("grpc_server.busy_volume_restore"));
                let mut last_permille = u64::MAX;
                let mut last_emit = std::time::Instant::now();
                restore_volume(&request, |pr| {
                    if cancel.is_cancelled() {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let permille = if pr.blocks_total > 0 {
                        pr.blocks_done * 1000 / pr.blocks_total
                    } else {
                        0
                    };
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
            .map_err(|e| Status::internal(e.to_string()))?;

            json_reply(&RestoreSummaryDto {
                blocks_restored: summary.blocks_restored,
                bytes_restored: summary.bytes_restored,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(target_os = "linux")]
        {
            self.restore_volume_linux(req).await
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    async fn verify_image(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            use own_backup_common::engine_dto::{EngineProgress, VerifySummaryDto};
            use platform_engine::verify_image_with_keys;

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                #[serde(flatten)]
                secret: ManualSecret,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            // A verify only reads — but it reads a chain that a consolidation
            // or a retention pass may be rewriting, and reading one of those
            // mid-change is how a good image gets reported as damaged.
            let path = std::path::PathBuf::from(&p.image_path);
            let keys = manual_keys_off_thread(path.clone(), p.secret).await?;
            let slot = self.reserve(&tr!("grpc_server.busy_validation"), &p.image_path)?;
            info!(image = %p.image_path, "VerifyImage RPC started");
            let cancel = slot.cancel();
            let events = self.state.events.clone();

            let summary = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let mut last_permille = u64::MAX;
                let mut last_emit = std::time::Instant::now();
                verify_image_with_keys(&path, keys.as_ref(), |done, total| {
                    if cancel.is_cancelled() {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let permille = if total > 0 { done * 1000 / total } else { 0 };
                    if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                        last_permille = permille;
                        last_emit = std::time::Instant::now();
                        emit_engine(
                            &events,
                            "verify",
                            &EngineProgress {
                                percent: permille / 10,
                                blocks_done: done,
                                blocks_total: total,
                                bytes_done: 0,
                                speed_mbps: 0.0,
                            },
                        );
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| Status::internal(format!("verify task panicked: {e}")))?
            .map_err(|e| {
                if matches!(e, own_backup_common::OwnBackupError::Cancelled) {
                    info!("VerifyImage RPC returning Cancelled");
                    Status::cancelled(e.to_string())
                } else {
                    Status::internal(e.to_string())
                }
            })?;

            json_reply(&VerifySummaryDto {
                valid: summary.valid(),
                blocks_checked: summary.blocks_checked,
                mismatched_blocks: summary.mismatched_blocks,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    /// The copies one job has on disk, for the menu of the job: «очистить
    /// версии», and choosing a version to mount or to save.
    async fn list_job_versions(
        &self,
        req: Request<IdRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let job = self
            .state
            .store
            .get(&id)
            .ok_or_else(|| Status::not_found(tr!("grpc_server.job_not_found").into_owned()))?;
        let store = self.state.store.clone();
        // Reading every image's description is file I/O on a folder that may
        // be a slow disk; it does not belong on a reactor thread.
        let versions =
            tokio::task::spawn_blocking(move || own_backup_core::job_versions(&store, &job))
                .await
                .map_err(|e| Status::internal(format!("listing task panicked: {e}")))?;
        json_reply(&versions)
    }

    /// Delete the copies ticked in «очистить версии».
    async fn delete_job_versions(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "linux")]
        crate::polkit::authorize(&req, crate::polkit::Action::DeleteBackups).await?;
        #[derive(serde::Deserialize)]
        struct In {
            job_id: String,
            images: Vec<String>,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        let job = self
            .state
            .store
            .get(&p.job_id)
            .ok_or_else(|| Status::not_found(tr!("grpc_server.job_not_found").into_owned()))?;

        // The slot first, the queue second: a run that is waiting for the slot
        // already calls itself the current one, and it would start writing an
        // increment onto the very image this is about to delete the moment the
        // slot is let go.
        let slot = self.reserve(&tr!("grpc_server.busy_delete_copies"), &job.name)?;
        if self.state.queue.snapshot().current.as_deref() == Some(p.job_id.as_str()) {
            return Err(Status::failed_precondition(
                tr!("grpc_server.job_running_cannot_delete").into_owned(),
            ));
        }

        let store = self.state.store.clone();
        let images: Vec<std::path::PathBuf> = p.images.iter().map(Into::into).collect();
        let job_id = p.job_id.clone();
        let report = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            own_backup_core::delete_job_versions(&store, &job, &images)
        })
        .await
        .map_err(|e| Status::internal(format!("delete task panicked: {e}")))?;
        info!(job = %job_id, deleted = report.deleted.len(), kept = report.kept.len(), "DeleteJobVersions");
        for (path, why) in &report.kept {
            tracing::warn!(job = %job_id, path = %path, reason = %why, "delete_job_versions: left on disk");
        }
        json_reply(&report)
    }

    /// «Проверить последнюю версию» / «проверить все версии» of a job.
    async fn verify_job(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            use own_backup_common::engine_dto::{
                JobVerifyImage, JobVerifyProgress, JobVerifySummary,
            };
            use platform_engine::{verify_image_scoped, VerifyScope};

            #[derive(serde::Deserialize)]
            struct In {
                job_id: String,
                #[serde(default)]
                all_versions: bool,
                #[serde(flatten)]
                secret: ManualSecret,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let job =
                self.state.store.get(&p.job_id).ok_or_else(|| {
                    Status::not_found(tr!("grpc_server.job_not_found").into_owned())
                })?;

            let store = self.state.store.clone();
            let listed = job.clone();
            let all = p.all_versions;
            let targets = tokio::task::spawn_blocking(move || {
                own_backup_core::job_verify_targets(&store, &listed, all)
            })
            .await
            .map_err(|e| Status::internal(format!("listing task panicked: {e}")))?
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let Some(first) = targets.first() else {
                return Err(Status::failed_precondition(
                    tr!("grpc_server.job_has_no_copies").into_owned(),
                ));
            };
            // One key opens every copy of a job, so it is derived once, from
            // the first — and before the slot is taken, like any manual verify.
            let keys = manual_keys_off_thread(first.0.clone(), p.secret).await?;
            let slot = self.reserve(&tr!("grpc_server.busy_validation"), &job.name)?;
            info!(job = %p.job_id, images = targets.len(), all_versions = p.all_versions, "VerifyJob RPC started");
            let cancel = slot.cancel();
            let events = self.state.events.clone();

            let summary = tokio::task::spawn_blocking(
                move || -> own_backup_common::Result<JobVerifySummary> {
                    let _slot = slot;
                    let started = std::time::Instant::now();
                    let count = targets.len() as u32;
                    let mut images = Vec::new();
                    for (index, (path, own_blocks_only)) in targets.iter().enumerate() {
                        let file_name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let scope = if *own_blocks_only {
                            VerifyScope::LatestCopyOnly
                        } else {
                            VerifyScope::Everything
                        };
                        let mut last_permille = u64::MAX;
                        let mut last_emit = std::time::Instant::now();
                        let outcome =
                            verify_image_scoped(path, keys.as_ref(), scope, |done, total| {
                                if cancel.is_cancelled() {
                                    return Err(own_backup_common::OwnBackupError::Cancelled);
                                }
                                let permille = if total > 0 { done * 1000 / total } else { 0 };
                                if permille != last_permille
                                    || last_emit.elapsed().as_millis() >= 400
                                {
                                    last_permille = permille;
                                    last_emit = std::time::Instant::now();
                                    emit_engine(
                                        &events,
                                        "verify-job",
                                        &JobVerifyProgress {
                                            index: index as u32,
                                            count,
                                            file_name: file_name.clone(),
                                            percent: (index as u64 * 1000 + permille)
                                                / (count as u64 * 10),
                                            blocks_done: done,
                                            blocks_total: total,
                                        },
                                    );
                                }
                                Ok(())
                            });
                        match outcome {
                            Ok(found) => images.push(JobVerifyImage {
                                image_path: path.display().to_string(),
                                file_name,
                                valid: found.valid(),
                                blocks_checked: found.blocks_checked,
                                mismatched_blocks: found.mismatched_blocks.len() as u64,
                                error: String::new(),
                            }),
                            Err(own_backup_common::OwnBackupError::Cancelled) => {
                                return Err(own_backup_common::OwnBackupError::Cancelled)
                            }
                            // One copy that cannot be read is a finding about
                            // that copy, not a reason to say nothing about the
                            // others.
                            Err(e) => images.push(JobVerifyImage {
                                image_path: path.display().to_string(),
                                file_name,
                                valid: false,
                                blocks_checked: 0,
                                mismatched_blocks: 0,
                                error: e.to_string(),
                            }),
                        }
                    }
                    Ok(JobVerifySummary {
                        valid: images.iter().all(|i| i.valid),
                        images,
                        duration_secs: started.elapsed().as_secs_f64(),
                    })
                },
            )
            .await
            .map_err(|e| Status::internal(format!("verify task panicked: {e}")))?
            .map_err(|e| {
                if matches!(e, own_backup_common::OwnBackupError::Cancelled) {
                    info!("VerifyJob RPC returning Cancelled");
                    Status::cancelled(e.to_string())
                } else {
                    Status::internal(e.to_string())
                }
            })?;
            json_reply(&summary)
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    /// «Сохранить как VHDX»: one copy, through its chain, into a file that
    /// stands on its own.
    async fn export_image(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        use own_backup_common::engine_dto::{EngineProgress, ExportSummaryDto};

        #[derive(serde::Deserialize)]
        struct In {
            image_path: String,
            target_path: String,
            #[serde(flatten)]
            secret: ManualSecret,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        let image = std::path::PathBuf::from(&p.image_path);
        let target = std::path::PathBuf::from(&p.target_path);
        // The service writes this file as LocalSystem, so where it goes has to
        // be said in full: a relative path would land in System32.
        let folder_exists = target.parent().is_some_and(|f| f.is_dir());
        if !target.is_absolute() || !folder_exists {
            return Err(Status::invalid_argument(
                tr!("grpc_server.export_bad_target", path = target.display()).into_owned(),
            ));
        }

        let keys = manual_keys_off_thread(image.clone(), p.secret).await?;
        let slot = self.reserve(&tr!("grpc_server.busy_export"), &p.image_path)?;
        info!(image = %p.image_path, target = %p.target_path, "ExportImage RPC started");
        let cancel = slot.cancel();
        let events = self.state.events.clone();

        let (summary, took) = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let started = std::time::Instant::now();
            let mut last_permille = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let result = own_backup_backup::export::export_to_vhdx(
                &image,
                keys.as_ref(),
                &target,
                &cancel,
                |done, total| {
                    let permille = if total > 0 { done * 1000 / total } else { 0 };
                    if permille != last_permille || last_emit.elapsed().as_millis() >= 400 {
                        last_permille = permille;
                        last_emit = std::time::Instant::now();
                        let secs = started.elapsed().as_secs_f64().max(0.001);
                        emit_engine(
                            &events,
                            "export",
                            &EngineProgress {
                                percent: permille / 10,
                                blocks_done: done >> 20,
                                blocks_total: total >> 20,
                                bytes_done: done,
                                speed_mbps: done as f64 / 1_048_576.0 / secs,
                            },
                        );
                    }
                    Ok(())
                },
            );
            (result, started.elapsed())
        })
        .await
        .map_err(|e| Status::internal(format!("export task panicked: {e}")))?;
        let summary = summary.map_err(|e| {
            if matches!(e, own_backup_common::OwnBackupError::Cancelled) {
                info!("ExportImage RPC returning Cancelled");
                Status::cancelled(e.to_string())
            } else {
                Status::internal(e.to_string())
            }
        })?;
        info!(target = %summary.target.display(), stored = summary.bytes_stored, "ExportImage RPC done");
        json_reply(&ExportSummaryDto {
            target_path: summary.target.display().to_string(),
            virtual_size: summary.virtual_size,
            bytes_stored: summary.bytes_stored,
            duration_secs: took.as_secs_f64(),
        })
    }

    async fn mount_image(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::mount_image_with_keys;
            use own_backup_common::engine_dto::MountResultDto;

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                read_write: Option<bool>,
                #[serde(flatten)]
                secret: ManualSecret,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let path = std::path::PathBuf::from(&p.image_path);
            let rw = p.read_write.unwrap_or(false);
            let image_path = p.image_path.clone();
            // Attaching an image while a chain of ours is being consolidated or
            // deleted would hand the mount a file that changes underneath it
            // (audit AUD-02). The slot is held for the duration of the ATTACH
            // only — a mount lives on afterwards, and blocking every backup for
            // as long as a window is open would be worse than the problem.
            // What protects a live mount is the check consolidation already
            // makes: it refuses an image that has a mount beside it.
            let keys = manual_keys_off_thread(path.clone(), p.secret).await?;
            let slot = self.reserve(&tr!("grpc_server.busy_mount"), &p.image_path)?;
            let mount_point = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                mount_image_with_keys(&path, rw, keys.as_ref())
            })
            .await
            .map_err(|e| Status::internal(format!("mount task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
                mounts.push(own_backup_common::engine_dto::ActiveMountDto {
                    image_path,
                    physical_path: mount_point.clone(),
                    read_write: rw,
                    volume_letters: Vec::new(),
                });
            }

            json_reply(&MountResultDto { mount_point })
        }
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::engine_linux::mount_image_with_options;
            use own_backup_common::engine_dto::MountResultDto;

            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
                read_write: Option<bool>,
                /// What to do about identities the mounted copy shares with
                /// this machine's own devices (`restore_linux::identity_gate`).
                #[serde(default)]
                identity_choice: Option<own_backup_common::engine_dto::IdentityChoice>,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            restore_linux::mount_choice_allowed(p.identity_choice)?;
            let path = std::path::PathBuf::from(&p.image_path);
            let rw = p.read_write.unwrap_or(false);
            let image_path = p.image_path.clone();
            let probed = path.clone();
            let clashes =
                tokio::task::spawn_blocking(move || restore_linux::image_clashes(&probed))
                    .await
                    .map_err(|e| Status::internal(format!("identity check panicked: {e}")))?
                    .map_err(|e| Status::internal(e.to_string()))?;
            let kept = restore_linux::identity_gate(clashes, p.identity_choice)?;
            // XFS refuses a second filesystem of one UUID unless told.
            let extra = own_backup_backup::identity_check_linux::mount_options_for(&kept);
            let mount_point =
                tokio::task::spawn_blocking(move || mount_image_with_options(&path, rw, &extra))
                    .await
                    .map_err(|e| Status::internal(format!("mount task panicked: {e}")))?
                    .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
                mounts.push(own_backup_common::engine_dto::ActiveMountDto {
                    image_path,
                    physical_path: mount_point.clone(),
                    read_write: rw,
                    volume_letters: Vec::new(),
                });
            }

            json_reply(&MountResultDto { mount_point })
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    async fn find_base_images(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_common::engine_dto::BaseImageDto;
            // Reads several files off disk; keep it off the async runtime.
            let found =
                tokio::task::spawn_blocking(own_backup_backup::winre_find::find_base_images)
                    .await
                    .map_err(|e| Status::internal(format!("base image search panicked: {e}")))?;
            let out: Vec<BaseImageDto> = found.into_iter().map(base_image_dto).collect();
            info!(count = out.len(), "base images offered");
            return json_reply(&out);
        }
        #[cfg(not(target_os = "windows"))]
        Err(Status::unimplemented(
            tr!("grpc_server.media_windows_only").into_owned(),
        ))
    }

    async fn inspect_base_image(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            #[derive(serde::Deserialize)]
            struct Chosen {
                path: String,
            }
            let chosen: Chosen = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let path = std::path::PathBuf::from(chosen.path.trim());
            let judged = tokio::task::spawn_blocking(move || {
                own_backup_backup::winre_find::inspect_chosen(&path)
            })
            .await
            .map_err(|e| Status::internal(format!("base image check panicked: {e}")))?;
            return match judged {
                Ok(base) => json_reply(&base_image_dto(base)),
                Err(why) => {
                    info!(%why, "chosen base image refused");
                    Err(Status::failed_precondition(why))
                }
            };
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_windows_only").into_owned(),
            ))
        }
    }

    async fn list_drivers(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_common::engine_dto::DriverPackageDto;
            // «Не смог опросить систему» и «драйверов нет» — разные ответы, и
            // мастер обязан показать первый как причину, а не как пустой список.
            let list = tokio::task::spawn_blocking(
                own_backup_backup::media_build_win::list_driver_packages,
            )
            .await
            .map_err(|e| Status::internal(format!("driver enumeration panicked: {e}")))?
            .map_err(|e| {
                Status::failed_precondition(
                    tr!("grpc_server.drivers_list_failed", error = e).into_owned(),
                )
            })?;
            let out: Vec<DriverPackageDto> = list
                .into_iter()
                .map(|d| DriverPackageDto {
                    inf: d.inf,
                    original_name: d.original_name,
                    class_name: d.class_name,
                    provider: d.provider,
                    version: d.version,
                    size_bytes: d.size_bytes,
                    recommended: d.recommended,
                    role: d.role,
                    device: d.device,
                    inbox: d.inbox,
                })
                .collect();
            return json_reply(&out);
        }
        #[cfg(not(target_os = "windows"))]
        Err(Status::unimplemented(
            tr!("grpc_server.drivers_windows_only").into_owned(),
        ))
    }

    async fn list_media_tools(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_common::engine_dto::MediaToolDto;
            // Не нашли ничего — это обычный ответ, а не отказ: машина без
            // Total Commander и Notepad++ ничего не теряет.
            let found = tokio::task::spawn_blocking(own_backup_backup::media_tools_win::find_tools)
                .await
                .map_err(|e| Status::internal(format!("tool search panicked: {e}")))?;
            let out: Vec<MediaToolDto> = found
                .into_iter()
                .map(|t| MediaToolDto {
                    id: t.id,
                    name: t.name,
                    source: t.source.display().to_string(),
                    size_bytes: t.size_bytes,
                    files: t.files,
                })
                .collect();
            info!(
                count = out.len(),
                "third-party tools offered for the medium"
            );
            return json_reply(&out);
        }
        #[cfg(not(target_os = "windows"))]
        Err(Status::unimplemented(
            tr!("grpc_server.tools_windows_only").into_owned(),
        ))
    }

    async fn build_media(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::media_build_win::{
                build_media, clean_up, clean_up_with_progress, BuildStage, MediaBuildRequest,
            };
            use own_backup_common::engine_dto::{
                MediaBuildOptionsDto, MediaBuildReportDto, MediaProgressDto, MediaStageDto,
            };

            let opts: MediaBuildOptionsDto = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            if opts.output_iso.trim().is_empty() {
                return Err(Status::invalid_argument(
                    tr!("grpc_server.iso_path_missing").into_owned(),
                ));
            }

            // The base is either what the user picked or the best candidate —
            // the search has already sorted them, best first.
            let base = if opts.base_wim.trim().is_empty() {
                tokio::task::spawn_blocking(own_backup_backup::winre_find::find_base_images)
                    .await
                    .map_err(|e| Status::internal(format!("base image search panicked: {e}")))?
                    .into_iter()
                    .next()
                    .map(|b| b.path)
                    .ok_or_else(|| {
                        Status::failed_precondition(tr!("grpc_server.winre_not_found").into_owned())
                    })?
            } else {
                // Judged like the found ones before anything is unpacked: the
                // build itself only checks that the file is a WIM.
                let chosen = std::path::PathBuf::from(opts.base_wim.trim());
                tokio::task::spawn_blocking(move || {
                    own_backup_backup::winre_find::inspect_chosen(&chosen)
                })
                .await
                .map_err(|e| Status::internal(format!("base image check panicked: {e}")))?
                .map_err(Status::failed_precondition)?
                .path
            };

            let program_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .ok_or_else(|| {
                    Status::internal(tr!("grpc_server.program_dir_unknown").into_owned())
                })?;
            let webview2 = find_webview2_runtime(&program_dir).ok_or_else(|| {
                Status::failed_precondition(tr!("grpc_server.webview2_not_found").into_owned())
            })?;

            let out_iso = std::path::PathBuf::from(&opts.output_iso);
            let work_root = own_backup_common::config::AppConfig::load_or_default()
                .data_dir
                .join("media");
            let events = self.state.events.clone();
            let extra = (!opts.extra_drivers.trim().is_empty())
                .then(|| std::path::PathBuf::from(&opts.extra_drivers));

            let report = tokio::task::spawn_blocking(move || -> own_backup_common::Result<_> {
                let staging = work_root.join("out");
                let work = work_root.join("work");
                let stages: Vec<MediaStageDto> =
                    BuildStage::plan(!opts.drivers.is_empty(), !opts.tools.is_empty())
                        .into_iter()
                        .map(|s| MediaStageDto {
                            id: s.id().into(),
                            title: s.title().into(),
                        })
                        .collect();
                let index_of = |id: &str| stages.iter().position(|s| s.id == id).unwrap_or(0);
                let emit = |stage_id: &str, percent: u64, detail: &str, finished: bool| {
                    emit_engine(
                        &events,
                        "media",
                        &MediaProgressDto {
                            stages: stages.clone(),
                            stage_index: index_of(stage_id),
                            percent,
                            detail: detail.to_string(),
                            finished,
                        },
                    );
                };

                let build = MediaBuildRequest {
                    base_wim: base,
                    program_dir,
                    webview2_dir: webview2,
                    out_dir: staging.clone(),
                    work_dir: work.clone(),
                    drivers: opts.drivers.clone(),
                    extra_drivers: extra,
                    tools: opts.tools.clone(),
                    user_appdata: (!opts.user_appdata.trim().is_empty())
                        .then(|| std::path::PathBuf::from(opts.user_appdata.trim())),
                    classic_console: opts.classic_console,
                    runtime_in_image: opts.runtime_in_image,
                };

                // A build that fails still leaves gigabytes behind, so the
                // clean-up runs either way — the first run left 3 GB in
                // ProgramData because it only ran on success.
                let built = match build_media(&build, |r| {
                    emit(r.stage.id(), r.percent, r.detail, false);
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        emit("clean", 0, &tr!("grpc_server.media_after_error"), false);
                        clean_up(&work, &staging);
                        return Err(e);
                    }
                };

                emit("iso", 0, &tr!("grpc_server.media_building_image"), false);
                let iso_bytes = match write_media_iso(&built.media_dir, &out_iso, &mut |pct| {
                    emit("iso", pct, &tr!("grpc_server.media_writing_files"), false);
                }) {
                    Ok(n) => n,
                    Err(e) => {
                        emit("clean", 0, &tr!("grpc_server.media_after_error"), false);
                        clean_up(&work, &staging);
                        return Err(e);
                    }
                };
                emit("iso", 100, "", false);

                let freed = clean_up_with_progress(&work, &staging, &mut |pct, detail| {
                    emit("clean", pct, detail, false);
                });
                emit("clean", 100, "", true);

                Ok(MediaBuildReportDto {
                    iso_path: out_iso.display().to_string(),
                    iso_bytes,
                    base_name: built.base_name,
                    base_build: built.base_build,
                    base_language: built.base_language,
                    unpacked: built.unpacked,
                    drivers_added: built.drivers_added,
                    tools_added: built.tools_added,
                    tools_bytes: built.tools_bytes,
                    runtime_files_trimmed: built.runtime_files_trimmed,
                    temp_bytes_freed: freed,
                    warnings: built.warnings,
                })
            })
            .await
            .map_err(|e| Status::internal(format!("media build panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            info!(
                iso = %report.iso_path,
                mb = report.iso_bytes / 1_048_576,
                drivers = report.drivers_added,
                tools = report.tools_added,
                freed_mb = report.temp_bytes_freed / 1_048_576,
                "boot medium built"
            );
            return json_reply(&report);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_windows_only").into_owned(),
            ))
        }
    }

    async fn list_media_sticks(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "linux")]
        {
            let sticks =
                tokio::task::spawn_blocking(own_backup_backup::media_stick_linux::list_sticks)
                    .await
                    .map_err(|e| Status::internal(format!("stick listing panicked: {e}")))?;
            json_reply(&sticks)
        }
        #[cfg(not(target_os = "linux"))]
        Err(Status::unimplemented(
            tr!("grpc_server.media_stick_linux_only").into_owned(),
        ))
    }

    /// Erase a USB stick and write the Linux medium's ISO onto it. The same
    /// gates as a whole-disk restore: an administrator's yes, the one slot the
    /// machine has, and the running system's disks out of reach.
    async fn write_media_stick(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::media_stick_linux::{write_stick, StickStage};
            use own_backup_common::engine_dto::{
                MediaProgressDto, MediaStageDto, MediaStickReportDto, MediaStickWriteDto,
            };

            crate::polkit::authorize(&req, crate::polkit::Action::Restore).await?;
            let p: MediaStickWriteDto = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            // Which stick the person chose, as listed: without it a name like
            // /dev/sdb may mean a disk plugged in since. An older client does
            // not send it; it is refused, not trusted.
            let expect = p.expect.clone().ok_or_else(|| {
                Status::failed_precondition(tr!("grpc_server.media_stick_list_again").into_owned())
            })?;
            // The stick's own refusals first ("not a stick", "eject it"): the
            // guard would call a mounted stick part of the running system.
            let device = p.device.clone();
            tokio::task::spawn_blocking(move || {
                own_backup_backup::media_stick_linux::check_stick(&device)?;
                own_backup_restore::guard_target_disk_device(&device)
            })
            .await
            .map_err(|e| Status::internal(format!("stick check panicked: {e}")))?
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let slot = self.reserve(
                &tr!("grpc_server.busy_media_stick"),
                &tr!("grpc_server.busy_which_stick", device = p.device.as_str()),
            )?;
            let cancel = slot.cancel();
            let events = self.state.events.clone();
            let stages: Vec<MediaStageDto> = StickStage::ALL
                .iter()
                .map(|s| MediaStageDto {
                    id: s.id().into(),
                    title: s.title(),
                })
                .collect();
            let device = p.device.clone();
            let report = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _awake = keep_awake(&tr!("grpc_server.busy_media_stick"));
                let mut emit = |stage: StickStage, percent: u64| {
                    let index = StickStage::ALL.iter().position(|s| *s == stage);
                    emit_engine(
                        &events,
                        "media",
                        &MediaProgressDto {
                            stages: stages.clone(),
                            stage_index: index.unwrap_or(0),
                            percent,
                            detail: String::new(),
                            finished: stage == StickStage::Logs && percent == 100,
                        },
                    );
                };
                write_stick(
                    std::path::Path::new(&p.iso_path),
                    &p.device,
                    &expect,
                    &cancel,
                    &mut emit,
                )
            })
            .await
            .map_err(|e| Status::internal(format!("stick write panicked: {e}")))?
            .map_err(|e| match e {
                own_backup_common::OwnBackupError::Cancelled => {
                    Status::cancelled(tr!("grpc_server.media_stick_cancelled").into_owned())
                }
                other => Status::failed_precondition(other.to_string()),
            })?;
            info!(
                device = %device,
                mb = report.iso_bytes / 1_048_576,
                log = ?report.log_partition,
                "boot medium written onto a stick"
            );
            json_reply(&MediaStickReportDto {
                device,
                iso_bytes: report.iso_bytes,
                volume_id: report.volume_id,
                log_partition: report.log_partition,
                log_skipped: report.log_skipped,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_stick_linux_only").into_owned(),
            ))
        }
    }

    /// Linux: can a medium be built now, and what an earlier build left in
    /// the cache.
    async fn linux_media_status(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "linux")]
        {
            use own_backup_common::engine_dto::LinuxMediaStatusDto;
            let s = tokio::task::spawn_blocking(own_backup_backup::media_linux_build::status)
                .await
                .map_err(|e| Status::internal(format!("media status panicked: {e}")))?;
            json_reply(&LinuxMediaStatusDto {
                installed_version: s.installed_version.unwrap_or_default(),
                cache_dir: s.cache_dir.display().to_string(),
                cache_bytes: s.cache_bytes,
                free_bytes: s.free_bytes,
                need_bytes: s.need_bytes,
                problems: s.problems,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_linux_only").into_owned(),
            ))
        }
    }

    /// Linux: build the boot medium from the installed package, save the ISO
    /// into the caller's folder and, when asked, write it onto a stick. The
    /// destructive-operation slot is held throughout: the build is long, runs
    /// as root, and its stick write erases a disk.
    async fn build_linux_media(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        media_build_unavailable()?;
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::media_linux::LinuxStage;
            use own_backup_backup::media_linux_build::{
                build, open_callers_file, open_output_dir, BuildRequest,
            };
            use own_backup_common::engine_dto::{
                LinuxMediaBuildDto, LinuxMediaReportDto, MediaProgressDto, MediaStageDto,
                MediaStickReportDto,
            };
            use own_backup_common::OwnBackupError;

            // The person's words, without the error kind's English prefix.
            fn refused(e: OwnBackupError) -> Status {
                match e {
                    OwnBackupError::Cancelled => {
                        Status::cancelled(tr!("grpc_server.media_linux_cancelled").into_owned())
                    }
                    OwnBackupError::Config(m)
                    | OwnBackupError::Device(m)
                    | OwnBackupError::RestoreIncompatible(m) => Status::failed_precondition(m),
                    other => Status::failed_precondition(other.to_string()),
                }
            }

            let info = req
                .extensions()
                .get::<tonic::transport::server::UdsConnectInfo>();
            let over_socket = info.is_some();
            let (uid, gid) = crate::peer_access::caller_ids(info)?;
            let p: LinuxMediaBuildDto = serde_json::from_str(&req.get_ref().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let with_stick = !p.stick_device.trim().is_empty();
            // A .deb of the caller's runs its maintainer scripts as root in
            // the build: root's (the CLI under sudo) or a foreground
            // development run's only, and refused before any password is asked.
            if !p.deb_path.trim().is_empty() {
                own_backup_backup::media_linux_build::given_deb_allowed(over_socket, uid)
                    .map_err(|e| {
                        tracing::warn!(target: "security", uid, "a .deb for the medium refused from a non-root caller");
                        match e {
                            OwnBackupError::Config(m) => Status::permission_denied(m),
                            other => Status::permission_denied(other.to_string()),
                        }
                    })?;
            }
            // The build itself, then — with a stick — erasing it, which is
            // the restore question. polkit answers one action per check and
            // keeps an `auth_admin_keep` answer per action, so without a
            // kept answer the person is asked twice; an administrator's
            // polkit rule may grant both at once.
            for action in crate::polkit::media_build_actions(with_stick) {
                crate::polkit::authorize(&req, *action).await?;
            }

            let stick = if with_stick {
                // Which stick the person chose, as listed (see write_media_stick).
                let expect = p.stick_expect.clone().ok_or_else(|| {
                    Status::failed_precondition(
                        tr!("grpc_server.media_stick_list_again").into_owned(),
                    )
                })?;
                let device = p.stick_device.trim().to_string();
                let checked = device.clone();
                tokio::task::spawn_blocking(move || {
                    own_backup_backup::media_stick_linux::check_stick(&checked)?;
                    own_backup_restore::guard_target_disk_device(&checked)
                })
                .await
                .map_err(|e| Status::internal(format!("stick check panicked: {e}")))?
                .map_err(refused)?;
                Some((device, expect))
            } else {
                None
            };
            let dir = p.output_dir.clone();
            let deb_path = p.deb_path.trim().to_string();
            let (out, deb) = tokio::task::spawn_blocking(move || {
                let out = open_output_dir(&dir, uid, gid)?;
                let deb = if deb_path.is_empty() {
                    None
                } else {
                    Some(open_callers_file(&deb_path, uid)?)
                };
                Ok::<_, OwnBackupError>((out, deb))
            })
            .await
            .map_err(|e| Status::internal(format!("folder check panicked: {e}")))?
            .map_err(refused)?;

            let slot = self.reserve(
                &tr!("grpc_server.busy_media_build"),
                &out.path.display().to_string(),
            )?;
            let cancel = slot.cancel();
            let events = self.state.events.clone();
            let plan = LinuxStage::plan(with_stick);
            let stages: Vec<MediaStageDto> = plan
                .iter()
                .map(|s| MediaStageDto {
                    id: s.id().into(),
                    title: s.title(),
                })
                .collect();
            info!(
                folder = %out.path.display(),
                uid,
                stick = %p.stick_device,
                given_deb = !p.deb_path.trim().is_empty(),
                "building the Linux boot medium"
            );
            let report = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _awake = keep_awake(&tr!("grpc_server.busy_media_build"));
                let last = *plan.last().unwrap_or(&LinuxStage::Iso);
                let mut emit = |stage: LinuxStage, percent: u64, detail: &str| {
                    emit_engine(
                        &events,
                        "media",
                        &MediaProgressDto {
                            stages: stages.clone(),
                            stage_index: plan.iter().position(|s| *s == stage).unwrap_or(0),
                            percent,
                            detail: detail.to_string(),
                            finished: stage == last && percent == 100,
                        },
                    );
                };
                build(BuildRequest { out, deb, stick }, &cancel, &mut emit)
            })
            .await
            .map_err(|e| Status::internal(format!("medium build panicked: {e}")))?
            .map_err(|e| {
                tracing::warn!(error = %e, "the Linux boot medium was not built");
                refused(e)
            })?;
            info!(
                iso = %report.iso_path.display(),
                mb = report.iso_bytes >> 20,
                version = %report.version,
                stick = report.stick.is_some(),
                "Linux boot medium built"
            );
            let device = p.stick_device.trim().to_string();
            json_reply(&LinuxMediaReportDto {
                iso_path: report.iso_path.display().to_string(),
                iso_bytes: report.iso_bytes,
                version: report.version,
                from_installed: report.from_installed,
                warnings: report.warnings,
                stick: report.stick.map(|s| MediaStickReportDto {
                    device,
                    iso_bytes: s.iso_bytes,
                    volume_id: s.volume_id,
                    log_partition: s.log_partition,
                    log_skipped: s.log_skipped,
                }),
                cache_dir: report.cache_dir.display().to_string(),
                cache_bytes: report.cache_bytes,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_linux_only").into_owned(),
            ))
        }
    }

    /// Linux: remove the medium's builder and package cache. Not while a
    /// build (or anything else) holds the slot.
    async fn clear_linux_media_cache(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "linux")]
        {
            use own_backup_common::engine_dto::LinuxMediaCacheClearedDto;
            crate::polkit::authorize(&req, crate::polkit::Action::BuildMedia).await?;
            let slot = self.reserve(
                &tr!("grpc_server.busy_media_cache"),
                own_backup_backup::media_linux_build::CACHE_DIR,
            )?;
            let freed = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                own_backup_backup::media_linux_build::clear_cache()
            })
            .await
            .map_err(|e| Status::internal(format!("cache removal panicked: {e}")))?
            .map_err(|e| match e {
                own_backup_common::OwnBackupError::Config(m) => Status::failed_precondition(m),
                other => Status::failed_precondition(other.to_string()),
            })?;
            info!(mb = freed >> 20, "the medium's build cache was removed");
            json_reply(&LinuxMediaCacheClearedDto { freed_bytes: freed })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = req;
            Err(Status::unimplemented(
                tr!("grpc_server.media_linux_only").into_owned(),
            ))
        }
    }

    async fn unmount_image(&self, req: Request<JsonRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::unmount_image;
            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let image_path = p.image_path.clone();
            let path = std::path::PathBuf::from(&p.image_path);
            tokio::task::spawn_blocking(move || unmount_image(&path))
                .await
                .map_err(|e| Status::internal(format!("unmount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
            }

            Ok(Response::new(Empty {}))
        }
        #[cfg(target_os = "linux")]
        {
            use own_backup_backup::engine_linux::unmount_image;
            #[derive(serde::Deserialize)]
            struct In {
                image_path: String,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
            let image_path = p.image_path.clone();
            let path = std::path::PathBuf::from(&p.image_path);
            tokio::task::spawn_blocking(move || unmount_image(&path))
                .await
                .map_err(|e| Status::internal(format!("unmount task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;

            {
                let mut mounts = self.state.active_mounts.lock().unwrap();
                mounts.retain(|m| m.image_path != image_path);
            }

            Ok(Response::new(Empty {}))
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    async fn restore_disk(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::{restore_disk, DiskRestoreRequest};
            use own_backup_common::engine_dto::{DiskRestoreProgressDto, DiskRestoreSummaryDto};

            #[derive(serde::Deserialize)]
            struct In {
                manifest_path: String,
                target_disk: u32,
                #[serde(default)]
                force: bool,
                #[serde(flatten)]
                secret: ManualSecret,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

            // A client flag cannot override protection of the running system.
            let _ = p.force;
            own_backup_restore::guard_target_disk(p.target_disk)
                .map_err(|e| Status::failed_precondition(e.to_string()))?;

            let slot = self.reserve(
                &tr!("grpc_server.busy_disk_restore"),
                &tr!("grpc_server.busy_which_disk", disk = p.target_disk),
            )?;
            let cancel = slot.cancel();
            let events = self.state.events.clone();
            let manifest_path = std::path::PathBuf::from(&p.manifest_path);
            let request = DiskRestoreRequest {
                manifest_path: manifest_path.clone(),
                target_disk: p.target_disk,
                verify_checksums: true,
                // From what the person typed, never from the service's own
                // store: that key is for scheduled COPIES (see `manual_keys`).
                encryption: manual_keys_off_thread(manifest_path, p.secret).await?,
            };

            let summary = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _awake = keep_awake(&tr!("grpc_server.busy_disk_restore"));
                let mut last = String::new();
                restore_disk(&request, |pr| {
                    if cancel.is_cancelled() {
                        return Err(own_backup_common::OwnBackupError::Cancelled);
                    }
                    let percent = if pr.blocks_total > 0 {
                        pr.blocks_done * 100 / pr.blocks_total
                    } else {
                        0
                    };
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
                })
            })
            .await
            .map_err(|e| Status::internal(format!("disk restore task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;

            json_reply(&DiskRestoreSummaryDto {
                partitions: summary.partitions as u64,
                bytes_written: summary.bytes_written,
                duration_secs: summary.duration.as_secs_f64(),
            })
        }
        #[cfg(target_os = "linux")]
        {
            self.restore_disk_linux(req).await
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = req;
            Err(Status::unimplemented(engine_msg()))
        }
    }

    async fn cancel_engine_op(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        // Whatever is in the slot. There is no id here — this is the button
        // that stops what the user can see — but the token belongs to that one
        // run and is thrown away with it, so a late press cannot reach the
        // next one (audit AUD-02).
        self.state.busy.cancel_whatever_is_running();
        Ok(Response::new(Empty {}))
    }

    // --- Archive encryption (plan §7.5) ----------------------------------

    /// Prepare encryption for a job and hand back its recovery key. **Writes
    /// nothing** — see [`own_backup_core::archive_setup`] for why the order
    /// matters.
    async fn begin_archive_encryption(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            job: own_backup_core::Job,
            password: String,
        }
        #[derive(serde::Serialize)]
        struct Out {
            token: String,
            recovery_key: String,
            fingerprint: String,
            job_id: String,
            /// What will keep this machine's authorization: `dpapi`, `tpm2`
            /// or `host`.
            key_protection: &'static str,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        // The job as it will be once encrypted: over plaintext copies that is a
        // format change, and it must be refused now rather than by every run.
        validate_job(&p.job, true)?;
        // Without a local key store the authorization can never be confirmed:
        // handing out a recovery key for it would be a promise nobody keeps.
        // Linux has one where `systemd-creds` can protect its machine key.
        #[cfg(target_os = "linux")]
        let key_protection = {
            let store = self.state.store.clone();
            tokio::task::spawn_blocking(move || store.key_store_protection())
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .map_err(|e| {
                    Status::failed_precondition(
                        tr!("grpc_server.archive_key_store_unavailable", error = e).into_owned(),
                    )
                })?
        };
        #[cfg(target_os = "windows")]
        let key_protection = "dpapi";
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        return Err(Status::failed_precondition(
            tr!("grpc_server.archive_key_store_missing").into_owned(),
        ));

        // Sealing the envelope is Argon2id at the production profile — seconds,
        // not milliseconds, and it must not sit on the async runtime.
        let setup = self.state.archive_setup.clone();
        let begun = tokio::task::spawn_blocking(move || setup.begin(p.job, &p.password))
            .await
            .map_err(|e| Status::internal(format!("key setup task panicked: {e}")))?
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        info!(job = %begun.job_id, fingerprint = %begun.fingerprint, key_protection,
            "Archive encryption prepared — waiting for the recovery key to be confirmed");
        json_reply(&Out {
            token: begun.token,
            recovery_key: begun.recovery_key.to_string(),
            fingerprint: begun.fingerprint,
            job_id: begun.job_id,
            key_protection,
        })
    }

    /// Commit it, once the operator has typed the recovery key back.
    async fn confirm_archive_encryption(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            token: String,
            recovery_key: String,
        }
        // Commits a key that opens the job's archives without a password.
        #[cfg(target_os = "linux")]
        crate::polkit::authorize(&req, crate::polkit::Action::ManageKeys).await?;
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

        let setup = self.state.archive_setup.clone();
        let prepared = setup
            .pending_job(&p.token)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let slot = self.reserve(&tr!("grpc_server.busy_archive_setup"), &prepared.name)?;
        let queue = self.state.queue.snapshot();
        if queue.current.as_deref() == Some(prepared.id.as_str())
            || queue.pending.iter().any(|id| id == &prepared.id)
        {
            return Err(Status::failed_precondition(
                tr!("grpc_server.encryption_job_queued").into_owned(),
            ));
        }
        // The folder may have gained a plaintext copy after Begin, so the
        // format check must run under the same slot as the key-store commit.
        validate_job(&prepared, true)?;
        let store = self.state.store.clone();
        let job = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            setup.confirm(&store, &p.token, &p.recovery_key)
        })
        .await
        .map_err(|e| Status::internal(format!("key commit task panicked: {e}")))?
        .map_err(|e| Status::invalid_argument(e.to_string()))?;

        info!(job = %job.id, name = %job.name, "Archive encryption enabled");
        json_reply(&job)
    }

    async fn reauthorize_archive(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // New keys replace the ones the archive opens with.
        #[cfg(target_os = "linux")]
        crate::polkit::authorize(&req, crate::polkit::Action::ManageKeys).await?;
        #[derive(serde::Deserialize)]
        struct In {
            job_id: String,
            #[serde(flatten)]
            secret: ManualSecret,
        }
        let body = zeroize::Zeroizing::new(req.into_inner().json);
        let p: In = serde_json::from_str(&body)
            .map_err(|_| Status::invalid_argument("bad archive authorization request"))?;
        let job = self
            .state
            .store
            .get(&p.job_id)
            .ok_or_else(|| Status::not_found(tr!("grpc_server.job_not_found").into_owned()))?;
        let slot = self.reserve(&tr!("grpc_server.busy_archive_reauthorize"), &job.name)?;
        let queue = self.state.queue.snapshot();
        if queue.current.as_deref() == Some(job.id.as_str())
            || queue.pending.iter().any(|id| id == &job.id)
        {
            return Err(Status::failed_precondition(
                tr!("grpc_server.encryption_job_queued").into_owned(),
            ));
        }
        let store = self.state.store.clone();
        let saved = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let password = p.secret.password.map(zeroize::Zeroizing::new);
            let recovery = p.secret.recovery_key.map(zeroize::Zeroizing::new);
            store.reauthorize_archive(
                job,
                password.as_ref().map(|value| value.as_str()),
                recovery.as_ref().map(|value| value.as_str()),
            )
        })
        .await
        .map_err(|_| Status::internal("archive authorization task failed"))?
        .map_err(|error| match error {
            own_backup_common::OwnBackupError::Config(message) => {
                Status::permission_denied(message)
            }
            other => Status::failed_precondition(other.to_string()),
        })?;
        info!(job = %saved.id, "Local archive authorization restored");
        json_reply(&saved)
    }

    async fn cancel_archive_encryption(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            token: String,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        self.state.archive_setup.cancel(&p.token);
        Ok(Response::new(Empty {}))
    }

    /// What a copy is, without any key.
    ///
    /// A screen has to know an image is encrypted **before** it offers to mount
    /// or restore it, so that it asks for a password instead of showing a
    /// failure. Everything here comes out of the open header, which is why it
    /// needs nothing: it is also why none of it is a fact about the contents
    /// (`seal/obk/listing.rs`).
    async fn inspect_image(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[derive(serde::Deserialize)]
        struct In {
            image_path: String,
        }
        #[derive(serde::Serialize, Default)]
        struct Out {
            container: String,
            encrypted: bool,
            job_id: String,
            fingerprint: String,
            role: String,
            created_at: String,
            chain_id: String,
            virtual_size: u64,
        }
        let p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;
        let path = std::path::PathBuf::from(&p.image_path);

        let container = own_backup_backup::sidecar::detect_container_format(&path)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        if container != own_backup_backup::seal::obk::OBK_EXTENSION {
            return json_reply(&Out {
                container: container.to_string(),
                ..Default::default()
            });
        }
        let listing = own_backup_backup::seal::obk::read_listing(&path)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let header = own_backup_backup::seal::obk::read_open_header(&path)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        json_reply(&Out {
            container: container.to_string(),
            encrypted: true,
            job_id: listing.job_id,
            fingerprint: header.key_file.fingerprint.clone(),
            role: format!("{:?}", listing.role).to_lowercase(),
            created_at: listing.created_at,
            chain_id: listing.chain_id.to_string(),
            virtual_size: listing.virtual_size,
        })
    }

    // --- Restore plans (the partition designer) --------------------------

    async fn dry_run_restore_plan(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // No plan executor here: previewing a plan that can never be applied
        // answers a question nobody asked.
        if cfg!(not(any(target_os = "windows", target_os = "linux"))) {
            return Err(Status::unimplemented(
                tr!("grpc_server.engine_windows_only").into_owned(),
            ));
        }
        use own_backup_common::restore_check::{check_plan, CheckContext};
        use own_backup_common::restore_plan::RestorePlan;

        #[derive(serde::Deserialize)]
        struct In {
            plan: RestorePlan,
            #[serde(default)]
            context: CheckContext,
            /// What opens the copy, when it is encrypted. The preview must read
            /// the same bytes the restore will, or the two disagree — and the
            /// direction they disagreed in was the bad one: preview yes,
            /// restore no.
            #[serde(default, flatten)]
            secret: ManualSecret,
        }
        let mut p: In = serde_json::from_str(&req.into_inner().json)
            .map_err(|e| Status::invalid_argument(format!("bad JSON: {e}")))?;

        // Seconds of Argon2id, and this runs before anything is reserved.
        let archive = manual_keys_off_thread(p.plan.image_path.clone(), p.secret).await?;

        // The context is built HERE, from the machine and the image, and the
        // client's copy only fills gaps it alone can know about (audit AUD-01,
        // point 2). What a client sends about disks is a picture it drew some
        // time ago; the partition table can have changed since, and a
        // `CheckContext` is the input to every safety decision below.
        let ctx = self.restore_context(&p.plan, p.context, archive.as_ref());

        // Resolving letters mutates the plan, so the caller gets the adjusted
        // plan back and applies *that* — otherwise the fix would be lost
        // between the dry run and the real one.
        let owned = target_owned_letters(&p.plan);
        let letter_changes = own_backup_common::restore_check::resolve_letter_conflicts_with_owners(
            &mut p.plan,
            &ctx.taken_letters,
            &owned,
        );
        correct_disk_size(&mut p.plan);
        // Stamp the plan with WHICH DEVICE the user is looking at. The client
        // sends this plan back to be applied, and by then `PhysicalDriveN` may
        // point somewhere else entirely (audit AUD-01, point 5).
        #[cfg(not(target_os = "linux"))]
        {
            p.plan.target_disk_id = Some(live_disk_id(p.plan.target_disk).await);
        }
        #[cfg(target_os = "linux")]
        {
            p.plan.target_disk_id = Some(restore_linux::live_disk_id(&p.plan));
        }
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut results = check_plan(&p.plan, &ctx);
        // What this engine does not do, and what the restore would duplicate
        // on this machine — said by the preview, not halfway through the run.
        #[cfg(target_os = "linux")]
        let identity_clashes = {
            results.extend(own_backup_core::restore_exec::linux_refusals(&p.plan));
            let plan = p.plan.clone();
            let (archive, found) = tokio::task::spawn_blocking(move || {
                let found = restore_linux::plan_clashes(&plan, archive.as_ref());
                (archive, found)
            })
            .await
            .map_err(|e| Status::internal(format!("identity check panicked: {e}")))?;
            let _ = archive;
            let found = found.map_err(|e| Status::internal(e.to_string()))?;
            results.extend(restore_linux::clash_findings(&found));
            found
        };
        #[cfg(not(target_os = "linux"))]
        let identity_clashes: Vec<own_backup_common::engine_dto::IdentityClashDto> = Vec::new();

        // The dry run is where a restore usually stops, and until now it left no
        // trace at all: the user saw a red box in a dialog and the log had
        // nothing to say about it. The plan that was checked and every finding
        // go in, so the refusal can be read afterwards.
        tracing::info!(
            plan = %p.plan.id,
            target_disk = p.plan.target_disk,
            image = %p.plan.image_path.display(),
            ops = p.plan.ops.len(),
            findings = results.len(),
            "dry run: checking a restore plan"
        );
        if let Some(t) = p.plan.table() {
            tracing::info!(
                disk_size = t.disk_size_bytes,
                sector = t.sector_size,
                partitions = t.entries.len(),
                "dry run: proposed table"
            );
            for line in own_backup_backup::table_merge::describe_table(t) {
                tracing::info!("dry run: {line}");
            }
        }
        for r in &results {
            match r.level {
                own_backup_common::restore_check::CheckLevel::Error => {
                    tracing::warn!(code = ?r.code, part = ?r.part_index, "dry run refuses: {}", r.message)
                }
                _ => {
                    tracing::info!(code = ?r.code, part = ?r.part_index, "dry run notes: {}", r.message)
                }
            }
        }

        json_reply(&serde_json::json!({
            "plan": p.plan,
            "results": results,
            "letter_changes": letter_changes,
            "identity_clashes": identity_clashes,
        }))
    }

    async fn apply_restore_plan(
        &self,
        req: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // No plan executor here.
        if cfg!(not(any(target_os = "windows", target_os = "linux"))) {
            return Err(Status::unimplemented(
                tr!("grpc_server.engine_windows_only").into_owned(),
            ));
        }
        #[cfg(target_os = "linux")]
        crate::polkit::authorize(&req, crate::polkit::Action::Restore).await?;
        use own_backup_common::restore_plan::RestorePlan;
        use own_backup_common::restore_state::RunStatus;

        // Passphrases arrive with the request, not inside the plan: the plan is
        // persisted to the restore history, and a password written to disk in
        // the clear is not a trade worth making (D19).
        #[derive(serde::Deserialize)]
        struct In {
            plan: RestorePlan,
            #[serde(default)]
            passphrases: std::collections::HashMap<u32, String>,
            /// What opens the COPY, when it is encrypted. Different thing from
            /// `passphrases`, which put a source volume's own container back.
            #[serde(default, flatten)]
            secret: ManualSecret,
            /// Linux: what to do about identities the restore would duplicate
            /// on this machine (`restore_linux::identity_gate`).
            #[serde(default)]
            #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
            identity_choice: Option<own_backup_common::engine_dto::IdentityChoice>,
            /// Linux, with `change`: also edit the restored system's fstab,
            /// crypttab and boot configuration.
            #[serde(default)]
            #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
            fix_references: bool,
        }
        let raw = req.into_inner().json;
        // A bare plan is still accepted — nothing needs a passphrase unless it
        // asks to put a container back.
        let In {
            mut plan,
            passphrases,
            secret,
            identity_choice,
            fix_references,
        } = serde_json::from_str::<In>(&raw)
            .or_else(|_| {
                serde_json::from_str::<RestorePlan>(&raw).map(|plan| In {
                    plan,
                    passphrases: Default::default(),
                    secret: Default::default(),
                    identity_choice: None,
                    fix_references: false,
                })
            })
            .map_err(|e| Status::invalid_argument(format!("bad plan JSON: {e}")))?;
        // The identity step is the service's to add (below), never the client's.
        #[cfg(target_os = "linux")]
        restore_linux::set_identity_step(&mut plan, None, &[], false);

        // Before the machine is reserved: deriving the key is seconds, and a
        // mistyped password must not hold the one slot the machine has.
        let archive = manual_keys_off_thread(plan.image_path.clone(), secret).await?;

        // THE gate. `dry_run_restore_plan` checks a plan; `apply_restore_plan`
        // used to accept a different one and hand it straight to the executor
        // (audit AUD-01). A client could therefore send a plan the preview
        // would have refused — restore a partition by sectors and then format
        // it, or name one partition's number with another's offset — and the
        // only thing between that and the disk was whatever the executor
        // happened to notice on its way past.
        //
        // The same checks now run here, on the plan that is actually about to
        // be executed, against a context built on this side of the wire. A
        // preview is still useful; it is not a permission slip.
        let refusals = self.validate_restore(&mut plan, archive.as_ref()).await?;
        if !refusals.is_empty() {
            return Err(Status::failed_precondition(
                tr!("grpc_server.plan_refused", refusals = refusals.join("\n")).into_owned(),
            ));
        }
        // What the restore would duplicate here needs the person's choice,
        // asked before the machine is reserved.
        #[cfg(target_os = "linux")]
        let archive = {
            let checked = plan.clone();
            let (archive, found) = tokio::task::spawn_blocking(move || {
                let found = restore_linux::plan_clashes(&checked, archive.as_ref());
                (archive, found)
            })
            .await
            .map_err(|e| Status::internal(format!("identity check panicked: {e}")))?;
            let found = found.map_err(|e| Status::internal(e.to_string()))?;
            let found = restore_linux::identity_gate(found, identity_choice)?;
            restore_linux::set_identity_step(&mut plan, identity_choice, &found, fix_references);
            archive
        };
        #[cfg(not(target_os = "linux"))]
        let _ = (identity_choice, fix_references);

        // One destructive operation at a time, and the check IS the taking.
        // The old version read `restore_run` under one lock and wrote the new
        // state under another, so two requests arriving together could both
        // find nothing running and both start (audit AUD-02). It also only
        // knew about other RESTORES — a backup writing the same images was
        // invisible to it.
        //
        // The token comes with the slot, addressed by plan id, so a cancel
        // that arrives late cannot hit the run that started after it (D18).
        let slot = self.reserve_with_id(
            &tr!("grpc_server.busy_restore"),
            &tr!("grpc_server.busy_which_disk", disk = plan_disk_name(&plan)),
            plan.id,
        )?;
        let cancel = slot.cancel();
        let initial = own_backup_core::restore_exec::initial_state(&plan);
        *self.state.restore_run.lock().unwrap() = Some(initial.clone());

        let state = self.state.clone();
        let events = self.state.events.clone();
        let plan_id = plan.id;
        let task = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let _awake = keep_awake(&tr!("grpc_server.busy_restore"));
            let mut last_emit = std::time::Instant::now();
            let outcome = own_backup_core::restore_exec::execute_plan(
                &plan,
                &passphrases,
                archive.as_ref(),
                &cancel,
                |run| {
                    *state.restore_run.lock().unwrap() = Some(run.clone());
                    // Throttle the wire, never the stored state: the state is
                    // what a reconnecting window reads, and it must always be
                    // current.
                    if run.is_finished() || last_emit.elapsed().as_millis() >= 300 {
                        last_emit = std::time::Instant::now();
                        let _ = events.send(Event {
                            kind: "restore-plan".into(),
                            json: serde_json::to_string(run).unwrap_or_default(),
                        });
                    }
                },
            );
            // Persisted as soon as it ends: a service restart must not erase
            // the record of what was done to the disk (D5).
            if let Ok(final_state) = &outcome {
                own_backup_core::restore_exec::append_history(&state.config.data_dir, final_state);
            }
            if let Err(e) = outcome {
                // The plan never started, so there is no run state describing
                // it — say so instead of leaving the last run's state on screen.
                tracing::error!(error = %e, "restore plan could not start");
                let mut slot = state.restore_run.lock().unwrap();
                if let Some(run) = slot.as_mut() {
                    run.status = RunStatus::Failed;
                    run.cancellable = false;
                    run.follow_ups
                        .push(tr!("grpc_server.restore_did_not_start", error = e).into_owned());
                    let _ = events.send(Event {
                        kind: "restore-plan".into(),
                        json: serde_json::to_string(&*run).unwrap_or_default(),
                    });
                }
            }
        });

        // The handle used to be dropped here, so if the engine panicked the
        // run stayed `Running` on screen until the service was restarted — and
        // nothing ever said why (audit AUD-02, point 5). The slot itself is
        // freed by the guard unwinding with the panic; what still needs saying
        // is that the run ended, and how.
        let watcher_state = self.state.clone();
        let watcher_events = self.state.events.clone();
        tokio::spawn(async move {
            let Err(e) = task.await else { return };
            tracing::error!(error = %e, plan = %plan_id, "the restore task did not finish normally");
            let mut slot = watcher_state.restore_run.lock().unwrap();
            if let Some(run) = slot
                .as_mut()
                .filter(|r| r.plan_id == plan_id && !r.is_finished())
            {
                run.status = RunStatus::Failed;
                run.cancellable = false;
                run.follow_ups
                    .push(tr!("grpc_server.restore_aborted_inside").into_owned());
                let _ = watcher_events.send(Event {
                    kind: "restore-plan".into(),
                    json: serde_json::to_string(&*run).unwrap_or_default(),
                });
            }
        });

        json_reply(&initial)
    }

    async fn get_restore_state(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let run = self.state.restore_run.lock().unwrap().clone();
        // Nothing in memory means the service was restarted, not that nothing
        // ever happened — the last run comes back off disk so a reopened window
        // still shows what the disk went through.
        let run = run.or_else(|| {
            own_backup_core::restore_exec::read_history(&self.state.config.data_dir)
                .into_iter()
                .next()
        });
        // …but only until it has been read. After that the report belongs to
        // the history, not to the restore page.
        let run = run.filter(|r| !r.acknowledged);
        json_reply(&run)
    }

    async fn get_restore_history(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let history = own_backup_core::restore_exec::read_history(&self.state.config.data_dir);
        json_reply(&history)
    }

    async fn ack_restore_run(&self, req: Request<IdRequest>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let plan_id = uuid::Uuid::parse_str(id.trim())
            .map_err(|e| Status::invalid_argument(format!("bad plan id {id:?}: {e}")))?;

        // Both copies, because either one alone would bring the report back:
        // the in-memory run is what a window reads first, the history is what
        // answers after a service restart.
        if let Some(run) = self.state.restore_run.lock().unwrap().as_mut() {
            if run.plan_id == plan_id {
                run.acknowledged = true;
            }
        }
        let found = own_backup_core::restore_exec::acknowledge_in_history(
            &self.state.config.data_dir,
            plan_id,
        );
        tracing::info!(plan = %plan_id, in_history = found, "restore report acknowledged");
        Ok(Response::new(Empty {}))
    }

    async fn cancel_restore_plan(
        &self,
        req: Request<IdRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        let id = req.into_inner().id;
        let plan_id = uuid::Uuid::parse_str(&id)
            .map_err(|e| Status::invalid_argument(format!("bad plan id: {e}")))?;

        // Refuse honestly while a non-cancellable operation runs (D12/D18).
        // Accepting the request and doing nothing is how a user ends up
        // pressing a dead button and believing the restore is stopping.
        let cancellable = self
            .state
            .restore_run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.cancellable)
            .unwrap_or(false);
        if !cancellable {
            return json_reply(&serde_json::json!({
                "cancelled": false,
                "reason": tr!("grpc_server.cancel_not_now").into_owned(),
            }));
        }

        let matched = self.state.busy.cancel_run(plan_id);

        json_reply(&serde_json::json!({
            "cancelled": matched,
            "reason": if matched { String::new() } else { tr!("grpc_server.run_already_finished").into_owned() },
        }))
    }

    // --- What mounting a backup image needs here ---

    async fn mount_backend_status(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        // The GUI reads `platform` to pick the right card. Neither platform
        // installs a driver of ours any more: Windows serves the image over an
        // iSCSI target of ours to its own initiator, Linux uses in-tree NBD.
        #[cfg(target_os = "windows")]
        {
            let st = own_backup_backup::iscsi_win::status();
            json_reply(&serde_json::json!({ "platform": "windows", "iscsi": st }))
        }
        #[cfg(target_os = "linux")]
        {
            json_reply(&serde_json::json!({
                "platform": "linux",
                "linux": own_backup_backup::engine_linux::mount_support_status(),
            }))
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Err(Status::unimplemented(engine_msg()))
        }
    }

    async fn prepare_mount_backend(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            // Nothing is installed: the initiator service is simply started.
            // Policy can hold it disabled, and that failure carries the one
            // command that fixes it.
            tokio::task::spawn_blocking(own_backup_backup::iscsi_win::ensure_initiator_service)
                .await
                .map_err(|e| Status::internal(format!("initiator task panicked: {e}")))?
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let st = own_backup_backup::iscsi_win::status();
            json_reply(&serde_json::json!({ "platform": "windows", "iscsi": st }))
        }
        #[cfg(target_os = "linux")]
        {
            tokio::task::spawn_blocking(own_backup_backup::engine_linux::ensure_nbd_loaded)
                .await
                .map_err(|e| Status::internal(format!("modprobe task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;
            json_reply(&serde_json::json!({
                "platform": "linux",
                "linux": own_backup_backup::engine_linux::mount_support_status(),
            }))
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Err(Status::unimplemented(engine_msg()))
        }
    }

    // --- boot entries -----------------------------------------------------

    /// Everything about how this machine boots, read in one pass.
    ///
    /// Entries and EFI-partition contents travel together because the answer
    /// the screen needs is the comparison between them, and two calls would
    /// compare two different moments.
    async fn boot_status(&self, req: Request<Empty>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::{efivars_win, esp_scan_win};

            let value = tokio::task::spawn_blocking(|| -> own_backup_common::Result<_> {
                if !efivars_win::is_uefi() {
                    return Ok(serde_json::json!({
                        "uefi": false,
                        "message": tr!("grpc_server.boot_bios_machine").into_owned(),
                    }));
                }
                let cfg = efivars_win::read_config()?;
                let esps = esp_scan_win::scan(&cfg.entries)?;
                Ok(serde_json::json!({
                    "uefi": true,
                    "order": cfg.order.iter().map(|n| format!("{n:04X}")).collect::<Vec<_>>(),
                    "current": cfg.current.map(|n| format!("{n:04X}")),
                    "next": cfg.next.map(|n| format!("{n:04X}")),
                    "timeout_secs": cfg.timeout,
                    "found_by": cfg.found_by,
                    "dangling": cfg.dangling.iter().map(|n| format!("{n:04X}")).collect::<Vec<_>>(),
                    "entries": cfg.entries.iter().map(|e| serde_json::json!({
                        "number": format!("{:04X}", e.number),
                        "name": e.option.description,
                        "in_menu": e.order_index.map(|i| i + 1),
                        "active": e.option.is_active(),
                        "current": e.is_current,
                        "next": e.is_next,
                        "device_entry": e.is_device_entry(),
                        "file": e.option.file_path(),
                        "partition_guid": e.option.hard_drive().and_then(|h| h.partition_guid()),
                        "partition_number": e.option.hard_drive().map(|h| h.partition_number),
                        // An entry whose device path we did not fully read is
                        // shown and not offered for editing.
                        "editable": e.option.fully_understood(),
                    })).collect::<Vec<_>>(),
                    "esps": esps.iter().map(|p| serde_json::json!({
                        "disk": p.disk,
                        "partition": p.partition,
                        "guid": p.partition_guid,
                        "size_bytes": p.size_bytes,
                        "unreadable": p.unreadable,
                        "loaders": p.loaders.iter().map(|l| serde_json::json!({
                            "path": l.path,
                            "size": l.size,
                            "kind": l.kind,
                            "generic": l.generic,
                            "fallback": l.fallback,
                            "second_stage":
                                l.role == own_backup_backup::esp_scan::Role::SecondStage,
                            "named_by": l.named_by.map(|n| format!("{n:04X}")),
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                }))
            })
            .await
            .map_err(|e| Status::internal(format!("boot status task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;
            json_reply(&value)
        }
        // Not read here yet: answering "not UEFI" on a UEFI machine is a wrong
        // fact, not a missing feature (`.ai/PLAN.md` §6, stage 3).
        #[cfg(not(target_os = "windows"))]
        {
            Err(Status::unimplemented(
                tr!("grpc_server.boot_read_windows_only").into_owned(),
            ))
        }
    }

    /// Change one thing about it.
    ///
    /// The safety copy and the guard on the currently-booted entry live in
    /// `efivars_win::apply_action`, not here: the command line goes through the
    /// same door, and rules that each caller has to remember are rules that one
    /// of them eventually will not.
    async fn boot_action(&self, req: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        check_auth(&req, &self.service_token)?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::efivars_win::{apply_action, Action};

            #[derive(serde::Deserialize)]
            struct In {
                action: String,
                entry: Option<String>,
                name: Option<String>,
                on: Option<bool>,
                to: Option<usize>,
                force: Option<bool>,
                disk: Option<u32>,
                partition: Option<u32>,
                file: Option<String>,
                position: Option<usize>,
                secs: Option<u16>,
                clear: Option<bool>,
            }
            let p: In = serde_json::from_str(&req.into_inner().json)
                .map_err(|e| Status::invalid_argument(format!("boot_action: {e}")))?;

            let entry = || -> Result<u16, Status> {
                let s = p.entry.as_deref().unwrap_or_default();
                u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).map_err(|_| {
                    Status::invalid_argument(
                        tr!("grpc_server.boot_not_entry_number", value = s).into_owned(),
                    )
                })
            };
            let want = |o: Option<String>, what: &str| -> Result<String, Status> {
                o.ok_or_else(|| {
                    Status::invalid_argument(
                        tr!("grpc_server.boot_not_given", what = what).into_owned(),
                    )
                })
            };

            let action = match p.action.as_str() {
                "rename" => Action::Rename {
                    entry: entry()?,
                    name: want(p.name.clone(), &tr!("grpc_server.boot_field_name"))?,
                },
                "set_active" => Action::SetActive {
                    entry: entry()?,
                    on: p.on.unwrap_or(true),
                    force: p.force.unwrap_or(false),
                },
                "move" => Action::Move {
                    entry: entry()?,
                    to: p.to.unwrap_or(0).saturating_sub(1),
                },
                "delete" => Action::Delete {
                    entry: entry()?,
                    force: p.force.unwrap_or(false),
                },
                "create" => Action::Create {
                    name: want(p.name.clone(), &tr!("grpc_server.boot_field_name"))?,
                    disk: p.disk.ok_or_else(|| {
                        Status::invalid_argument(
                            tr!("grpc_server.boot_disk_not_given").into_owned(),
                        )
                    })?,
                    partition: p.partition.ok_or_else(|| {
                        Status::invalid_argument(
                            tr!("grpc_server.boot_partition_not_given").into_owned(),
                        )
                    })?,
                    file: want(p.file.clone(), &tr!("grpc_server.boot_field_loader_path"))?,
                    position: p.position.map(|x| x.saturating_sub(1)),
                },
                "boot_next" => Action::BootNext {
                    entry: if p.clear.unwrap_or(false) {
                        None
                    } else {
                        Some(entry()?)
                    },
                },
                "timeout" => Action::Timeout {
                    secs: p.secs.ok_or_else(|| {
                        Status::invalid_argument(
                            tr!("grpc_server.boot_secs_not_given").into_owned(),
                        )
                    })?,
                },
                other => {
                    return Err(Status::invalid_argument(
                        tr!("grpc_server.boot_unknown_action", action = other).into_owned(),
                    ))
                }
            };

            let done = tokio::task::spawn_blocking(move || apply_action(action))
                .await
                .map_err(|e| Status::internal(format!("boot action task panicked: {e}")))?
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            tracing::info!(message = %done.message, "boot configuration changed");
            json_reply(&serde_json::json!({
                "message": done.message,
                "safety_copy": done.safety_copy.path.display().to_string(),
                // На носителе копия может лежать на временном диске: экран
                // обязан это сказать, а не показать путь и промолчать.
                "safety_copy_durable": done.safety_copy.durable,
                "created": done.created.map(|n| format!("{n:04X}")),
            }))
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(Status::unimplemented(
                tr!("grpc_server.boot_change_windows_only").into_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    /// A command-line whole-disk copy has no live-disk snapshot; its own
    /// partition table still names every partition the plan restores.
    #[test]
    fn a_whole_disk_copy_without_a_snapshot_is_described_by_its_table() {
        use own_backup_common::gpt::{
            DiskBackupManifest, DiskImageRef, DiskLayout, GptPartitionEntry,
        };
        use own_backup_common::types::FileSystem;
        let entry = |index, first, last| GptPartitionEntry {
            index,
            type_guid: String::new(),
            part_guid: String::new(),
            first_lba: first,
            last_lba: last,
            attributes: 0,
            name: format!("p{index}"),
        };
        let image = |index, fs: &str| DiskImageRef {
            part_index: index,
            token: String::new(),
            image_file_name: String::new(),
            filesystem: fs.into(),
            encryption: None,
        };
        let disk = DiskBackupManifest {
            format_version: 1,
            created_at: String::new(),
            job_id: None,
            disk_number: 0,
            disk_model: String::new(),
            layout: DiskLayout {
                disk_guid: String::new(),
                sector_size: 512,
                size_bytes: 1 << 32,
                partitions: vec![
                    entry(1, 2048, 4095),
                    entry(14, 4096, 4097),
                    entry(15, 8192, 16383),
                ],
            },
            images: vec![image(1, "ext4"), image(15, "FAT32")],
            source_disk: None,
        };
        let sources = super::layout_sources(&disk, &[]);
        let by = |i: u32| sources.iter().find(|s| s.index == i).unwrap();
        assert_eq!(sources.len(), 3);
        assert_eq!(by(1).size_bytes, 2048 * 512);
        assert_eq!(by(1).filesystem, FileSystem::Ext4);
        assert_eq!(by(14).filesystem, FileSystem::Unknown);
        assert_eq!(by(14).size_bytes, 2 * 512);
        assert_eq!(by(15).filesystem, FileSystem::Fat32);
        assert_eq!(by(15).label, "p15");
    }

    /// The English text of this refusal once belonged to another message (the
    /// destination-on-source one, REPORT-cloud-gui2). It must speak of what
    /// it refuses.
    #[test]
    fn encrypting_over_plain_copies_is_explained_in_both_languages() {
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("../locales/service.json")).unwrap();
        let entry = &catalog["grpc_server.job_encrypt_over_plain_copies"];
        assert!(entry["ru"].as_str().unwrap().contains("Шифрование"));
        let en = entry["en"].as_str().unwrap();
        assert!(en.starts_with("Encryption cannot be turned on"), "{en}");
        assert!(!en.contains("destination"), "{en}");
    }

    use super::*;

    #[test]
    fn a_plans_disk_is_named_by_its_device_on_linux() {
        let mut plan =
            own_backup_common::restore_plan::RestorePlan::new(3, "/x.qcow2".into(), Vec::new());
        assert_eq!(plan_disk_name(&plan), "3");
        plan.target_device = Some("/dev/disk/by-id/usb-Disk".into());
        let want = if cfg!(target_os = "linux") {
            "/dev/disk/by-id/usb-Disk"
        } else {
            "3"
        };
        assert_eq!(plan_disk_name(&plan), want);
    }

    fn make_service(dir: &std::path::Path) -> BackupServiceImpl {
        let (_, shutdown) = crate::shutdown::create_shutdown_channel();
        make_service_with_shutdown(dir, shutdown)
    }

    fn make_service_with_shutdown(
        dir: &std::path::Path,
        shutdown: crate::shutdown::ShutdownSignal,
    ) -> BackupServiceImpl {
        let mut config = own_backup_common::config::AppConfig::default();
        config.data_dir = dir.to_path_buf();
        let store = own_backup_core::JobStore::load(dir);
        let queue = own_backup_core::RunQueue::new();
        let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (events, _r) = broadcast::channel(16);
        let passwords =
            std::sync::Arc::new(own_backup_core::PasswordStore::new(dir.join("meta.db")));
        let busy = own_backup_core::busy::Busy::new();
        let state = AppState::new(
            config, store, queue, busy, job_tx, cancel, events, passwords,
        );
        BackupServiceImpl::new(state, String::new(), shutdown)
    }

    /// The wizard's destination check says what the save would: a letter
    /// collision first, by name, before anything is looked up.
    #[test]
    fn destination_answer_is_the_save_check() {
        let on = destination_answer(&["C:".into()], r"C:\Backups");
        assert_eq!(on["placement"], "on_source");
        assert_eq!(on["source"], "C:");
        // A drive letter is not a Linux device: nothing to warn about.
        assert_eq!(on["key_store_unknown"], serde_json::json!([]));
        // Nothing to compare yet: no answer to give, and no refusal either.
        assert_eq!(destination_answer(&[], r"C:\Backups")["placement"], "clear");
        assert_eq!(
            destination_answer(&["C:".into()], "  ")["placement"],
            "clear"
        );
        assert_eq!(
            destination_answer(&["  ".into()], r"C:\Backups")["placement"],
            "clear"
        );
    }

    #[tokio::test]
    async fn capabilities_are_served_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let reply = svc
            .backup_capabilities(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner();
        let v: serde_json::Value = serde_json::from_str(&reply.json).unwrap();
        assert_eq!(v["os"], own_backup_core::capabilities::current().os);
        assert!(v["methods"].as_array().unwrap().contains(&"full".into()));
    }

    #[tokio::test]
    async fn shutdown_refuses_new_destructive_operations() {
        let dir = tempfile::tempdir().unwrap();
        let (controller, shutdown) = crate::shutdown::create_shutdown_channel();
        let svc = make_service_with_shutdown(dir.path(), shutdown);
        controller.trigger_shutdown();
        assert_eq!(
            svc.reserve("verify", "image").unwrap_err().code(),
            tonic::Code::Unavailable
        );
        assert_eq!(
            svc.reserve_with_id("restore", "plan", uuid::Uuid::new_v4())
                .unwrap_err()
                .code(),
            tonic::Code::Unavailable
        );
        assert!(svc.state.busy.current().is_none());
    }

    /// Keep a real HTTP/2 subscriber connected across shutdown. The service
    /// must drain the operation and close the stream without needing the GUI
    /// to disconnect first (H02).
    #[tokio::test]
    async fn shutdown_drains_operation_and_closes_connected_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let (controller, mut shutdown) = crate::shutdown::create_shutdown_channel();
        let svc = make_service_with_shutdown(dir.path(), shutdown.clone());
        let busy = svc.state.busy.clone();
        let events = svc.state.events.clone();
        let held = busy
            .reserve(own_backup_core::busy::Doing::new("verify", "image"))
            .unwrap();
        let cancel = held.cancel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(svc.into_service())
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async move {
                        shutdown.wait_for_shutdown().await;
                        busy.cancel_whatever_is_running();
                        let _ = stopping_tx.send(());
                        busy.wait_until_idle().await;
                    },
                )
                .await
        });
        let mut client =
            proto::backup_service_client::BackupServiceClient::connect(format!("http://{addr}"))
                .await
                .unwrap();
        let mut stream = client.stream_events(Empty {}).await.unwrap().into_inner();
        events
            .send(Event {
                kind: "verify".into(),
                json: "{}".into(),
            })
            .unwrap();
        assert_eq!(stream.message().await.unwrap().unwrap().kind, "verify");
        controller.trigger_shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(5), stopping_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(cancel.is_cancelled());
        assert!(
            !server.is_finished(),
            "transport closed before the operation drained"
        );
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// A job list past gRPC's default 4 MiB still travels both ways through
    /// the real client: on the stand (2026-10-17) a 6 MB list failed every
    /// request that read the jobs.
    #[tokio::test]
    async fn a_job_list_larger_than_four_megabytes_still_travels() {
        let dir = tempfile::tempdir().unwrap();
        let (_controller, shutdown) = crate::shutdown::create_shutdown_channel();
        let svc = make_service_with_shutdown(dir.path(), shutdown);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(svc.into_service())
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        // Straight to this server: `connect` tries the service socket first,
        // and on a machine with the package installed that is another service.
        let mut client =
            own_backup_client::BackupClient::connect_tcp_only(format!("http://{addr}"), "")
                .await
                .unwrap();
        let mut big = own_backup_core::Job {
            name: "big".into(),
            source: "C:".into(),
            dest_folder: dir.path().display().to_string(),
            ..Default::default()
        };
        big.exclusions.patterns = (0..6000)
            .map(|i| format!("{i}-{}", "x".repeat(1000)))
            .collect();
        let saved = client.save_job(&big).await.unwrap();
        let listed = client.list_jobs().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, saved.id);
        assert_eq!(listed[0].exclusions.patterns.len(), 6000);
    }

    fn job(name: &str, source: &str, dest: &str) -> String {
        serde_json::to_string(&own_backup_core::Job {
            name: name.into(),
            source: source.into(),
            dest_folder: dest.into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn jobs_roundtrip_over_grpc() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Empty to start.
        let json = svc
            .list_jobs(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
            .json;
        assert_eq!(json, "[]");

        // Save a job, get a generated id back.
        let saved = svc
            .save_job(Request::new(JsonRequest {
                json: job("T", "E:", r"D:\bk"),
            }))
            .await
            .unwrap()
            .into_inner()
            .json;
        let saved: own_backup_core::Job = serde_json::from_str(&saved).unwrap();
        assert!(!saved.id.is_empty());

        // It is listed and fetchable.
        let listed: Vec<own_backup_core::Job> = serde_json::from_str(
            &svc.list_jobs(Request::new(Empty {}))
                .await
                .unwrap()
                .into_inner()
                .json,
        )
        .unwrap();
        assert_eq!(listed.len(), 1);
        let got = svc
            .get_job(Request::new(IdRequest {
                id: saved.id.clone(),
            }))
            .await
            .unwrap()
            .into_inner()
            .json;
        assert!(got.contains(&saved.id));

        // dest-on-source is rejected at the boundary.
        let err = svc
            .save_job(Request::new(JsonRequest {
                json: job("B", "E:", r"E:\bk"),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Delete it.
        svc.delete_job(Request::new(DeleteJobRequest {
            id: saved.id,
            delete_files: false,
        }))
        .await
        .unwrap();
        let json = svc
            .list_jobs(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
            .json;
        assert_eq!(json, "[]");
    }

    #[tokio::test]
    async fn run_missing_job_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let err = svc
            .run_job(Request::new(IdRequest { id: "nope".into() }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn status_and_template_and_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let status = svc
            .get_service_status(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
            .json;
        assert!(status.contains("version"));

        let tmpl: own_backup_core::Job = serde_json::from_str(
            &svc.new_job_template(Request::new(Empty {}))
                .await
                .unwrap()
                .into_inner()
                .json,
        )
        .unwrap();
        assert!(!tmpl.name.is_empty());

        let excl: serde_json::Value = serde_json::from_str(
            &svc.default_exclusions(Request::new(Empty {}))
                .await
                .unwrap()
                .into_inner()
                .json,
        )
        .unwrap();
        assert!(excl["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().contains("Temp")));
        assert!(excl["linux"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().contains("tmp")));
    }

    // --- Один разрушительный ход за раз (аудит AUD-02) ---------------------

    #[tokio::test]
    async fn reauthorization_refuses_busy_and_queued_jobs_before_touching_keys() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let saved = svc
            .state
            .store
            .upsert(serde_json::from_str(&job("Repair", "E:", r"D:\bk")).unwrap())
            .unwrap();
        let request = || {
            Request::new(JsonRequest {
                json: serde_json::json!({"job_id":saved.id,"password":"synthetic secret"})
                    .to_string(),
            })
        };
        let before = std::fs::read(dir.path().join("jobs.json")).unwrap();
        let held = svc
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::new("backup", "disk"))
            .unwrap();
        assert_eq!(
            svc.reauthorize_archive(request()).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        drop(held);
        assert!(svc.state.queue.enqueue(&saved.id));
        assert_eq!(
            svc.reauthorize_archive(request()).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(std::fs::read(dir.path().join("jobs.json")).unwrap(), before);
        assert!(!dir.path().join(".own-backup-service-keys").exists());
    }

    #[tokio::test]
    async fn malformed_reauthorization_does_not_echo_secrets_and_missing_jobs_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let secret = "never echo this synthetic secret";
        let error = svc
            .reauthorize_archive(Request::new(JsonRequest {
                json: serde_json::json!({"job_id":{},"password":secret}).to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(!error.message().contains(secret));
        let error = svc
            .reauthorize_archive(Request::new(JsonRequest {
                json: serde_json::json!({"job_id":"missing","password":secret}).to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::NotFound);
        assert!(!dir.path().join("jobs.json").exists());
    }

    /// Проверка «занято?» и захват — одно действие. Раньше план восстановления
    /// читал restore_run под одним замком и писал новое состояние под другим,
    /// так что два запроса, пришедшие вместе, оба проходили проверку.
    #[tokio::test]
    async fn a_second_destructive_request_is_refused_while_one_holds_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let saved: own_backup_core::Job = serde_json::from_str(
            &svc.save_job(Request::new(JsonRequest {
                json: job("T", "E:", r"D:\bk"),
            }))
            .await
            .unwrap()
            .into_inner()
            .json,
        )
        .unwrap();

        let held = svc
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::new(
                "восстановление",
                "диск 1",
            ))
            .expect("свободно");

        let refused = svc
            .delete_job(Request::new(DeleteJobRequest {
                id: saved.id.clone(),
                delete_files: true,
            }))
            .await
            .expect_err("удаление копий не должно начаться поверх восстановления");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(
            refused.message().contains("восстановление"),
            "{}",
            refused.message()
        );

        // Задание при этом не удалено: отказ должен случиться ДО изменений.
        assert!(svc.state.store.get(&saved.id).is_some());

        drop(held);
        assert!(svc
            .delete_job(Request::new(DeleteJobRequest {
                id: saved.id,
                delete_files: true
            }))
            .await
            .is_ok());
    }

    /// Удалять задание, которое прямо сейчас пишет в свои файлы, нельзя
    /// (аудит AUD-09, пункт 1).
    #[tokio::test]
    async fn a_job_that_is_running_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let saved: own_backup_core::Job = serde_json::from_str(
            &svc.save_job(Request::new(JsonRequest {
                json: job("T", "E:", r"D:\bk"),
            }))
            .await
            .unwrap()
            .into_inner()
            .json,
        )
        .unwrap();

        assert!(svc.state.queue.enqueue(&saved.id));
        assert!(svc.state.queue.start(&saved.id));

        let refused = svc
            .delete_job(Request::new(DeleteJobRequest {
                id: saved.id.clone(),
                delete_files: false,
            }))
            .await
            .expect_err("выполняющееся задание удалять нельзя");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(svc.state.store.get(&saved.id).is_some());

        svc.state.queue.finish();
        assert!(svc
            .delete_job(Request::new(DeleteJobRequest {
                id: saved.id,
                delete_files: false
            }))
            .await
            .is_ok());
    }

    /// Ожидающее задание не выполняется, поэтому удаляется — но сначала
    /// снимается с очереди, иначе рабочий возьмёт его уже несуществующим.
    #[tokio::test]
    async fn a_job_waiting_in_the_queue_is_taken_out_before_it_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let saved: own_backup_core::Job = serde_json::from_str(
            &svc.save_job(Request::new(JsonRequest {
                json: job("T", "E:", r"D:\bk"),
            }))
            .await
            .unwrap()
            .into_inner()
            .json,
        )
        .unwrap();

        assert!(svc.state.queue.enqueue(&saved.id));
        assert!(svc
            .delete_job(Request::new(DeleteJobRequest {
                id: saved.id.clone(),
                delete_files: false,
            }))
            .await
            .is_ok());
        assert!(
            !svc.state.queue.snapshot().pending.contains(&saved.id),
            "оно не должно остаться в очереди"
        );
    }

    /// Кнопка «остановить» бьёт по тому, что идёт сейчас, и ни по чему больше:
    /// токен принадлежит одному запуску и выбрасывается вместе с ним.
    #[tokio::test]
    async fn stopping_hits_the_run_that_is_going_on_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Ничего не идёт — останавливать нечего, и это не ошибка.
        assert!(svc.cancel_engine_op(Request::new(Empty {})).await.is_ok());

        let first = svc
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::new(
                "проверка образа",
                "a.vhdx",
            ))
            .unwrap();
        svc.cancel_engine_op(Request::new(Empty {})).await.unwrap();
        assert!(first.cancel().is_cancelled());
        drop(first);

        // Следующий запуск начинается с чистого токена: старый флаг не
        // сбрасывается, его просто больше нет.
        let second = svc
            .state
            .busy
            .reserve(own_backup_core::busy::Doing::new(
                "проверка образа",
                "b.vhdx",
            ))
            .unwrap();
        assert!(!second.cancel().is_cancelled());
    }
}

/// A boot medium cannot build a boot medium, and the reasons are not one but
/// several: `X:` is a RAM disk with no room to unpack a multi-gigabyte image
/// into, the driver store the medium would copy from does not exist there (which
/// is why the driver scan comes back empty), and WinPE carries a reduced DISM
/// that cannot service an image at all.
///
/// Refused here rather than only in the interface, because the interface is not
/// the only way in — and because the failure it produced was a DISM error code
/// halfway through the build, which explains nothing.
fn media_build_unavailable() -> Result<(), Status> {
    if own_backup_common::media_mode::is_media_mode() {
        return Err(Status::failed_precondition(
            tr!("grpc_server.media_build_on_media").into_owned(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn base_image_dto(
    b: own_backup_backup::winre_find::BaseImage,
) -> own_backup_common::engine_dto::BaseImageDto {
    own_backup_common::engine_dto::BaseImageDto {
        path: b.path.display().to_string(),
        origin: b.origin.describe().to_string(),
        name: b.info.name,
        build: b.info.build,
        language: b.info.language,
        architecture: b.info.architecture,
        total_bytes: b.info.total_bytes,
        warnings: b.warnings,
    }
}

/// Where the WebView2 runtime lives on this machine.
///
/// The installed Evergreen runtime keeps its version in the folder name, so the
/// newest is taken. A `WebView2` folder beside the program wins over all of it —
/// that is how a machine without Edge is served, and how a specific runtime can
/// be forced.
#[cfg(target_os = "windows")]
fn find_webview2_runtime(program_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let beside = program_dir.join("WebView2");
    if beside.join("msedgewebview2.exe").exists() {
        return Some(beside);
    }
    let mut best: Option<(String, std::path::PathBuf)> = None;
    for base in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(root) = std::env::var(base) else {
            continue;
        };
        let apps = std::path::PathBuf::from(root).join(r"Microsoft\EdgeWebView\Application");
        let Ok(rd) = std::fs::read_dir(&apps) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            if !e.path().join("msedgewebview2.exe").exists() {
                continue;
            }
            if best.as_ref().map(|(n, _)| name > *n).unwrap_or(true) {
                best = Some((name, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Turn a finished medium folder into a bootable ISO.
///
/// The El Torito image carries the loader and the BCD it looks for beside
/// itself; everything else is read from the ISO9660 tree, which is why the whole
/// medium goes in as files as well.
#[cfg(target_os = "windows")]
fn write_media_iso(
    media_dir: &std::path::Path,
    out_iso: &std::path::Path,
    progress: &mut dyn FnMut(u64),
) -> own_backup_common::Result<u64> {
    use own_backup_backup::fat_writer::{build_fat16, FatFile};
    use own_backup_backup::iso9660::{write_iso_with_progress, IsoBoot, IsoFile};

    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<IsoFile>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(IsoFile {
                    path: rel.to_string_lossy().replace('\\', "/"),
                    source: p.clone(),
                });
            }
        }
    }
    let mut files = Vec::new();
    walk(media_dir, media_dir, &mut files);

    let mut boot_files = Vec::new();
    // `boot.stl` travels with the loader, not with the rest of the tree: with
    // Secure Boot on, the loader looks for it on ITS OWN device, and on an ISO
    // that device is this FAT image rather than the ISO9660 filesystem. Its
    // absence stops the boot at `0xc000000f` (2026-09-20, found by bisection).
    for rel in [
        "EFI/BOOT/BOOTX64.EFI",
        "EFI/Microsoft/Boot/BCD",
        "EFI/Microsoft/Boot/boot.stl",
        "boot/boot.sdi",
    ] {
        let src = media_dir.join(rel.replace('/', "\\"));
        if src.exists() {
            boot_files.push(FatFile {
                path: rel.replace('/', "\\"),
                data: std::fs::read(&src)?,
            });
        }
    }
    let efisys = media_dir.join("boot").join("efisys.bin");
    std::fs::write(&efisys, build_fat16(&boot_files, "OWNBOOT")?)?;
    files.push(IsoFile {
        path: "boot/efisys.bin".into(),
        source: efisys.clone(),
    });

    if let Some(parent) = out_iso.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let len = write_iso_with_progress(
        &files,
        "OWNBACKUP",
        Some(IsoBoot {
            image_path: "boot/efisys.bin".into(),
        }),
        out_iso,
        progress,
    )?;
    let _ = std::fs::remove_file(&efisys);
    Ok(len)
}

/// No sleep while a restore writes a disk: a machine that sleeps halfway leaves
/// a target that is neither the old disk nor the copy. (A copy holds the same
/// in `core::run_queue_worker`.) Dropped when the restore ends, however it ends.
fn keep_awake(what: &str) -> Option<Box<dyn Send>> {
    #[cfg(target_os = "windows")]
    {
        own_backup_backup::power_win::KeepAwake::new(&format!("Own Backup: {what}"))
            .map(|k| Box::new(k) as Box<dyn Send>)
    }
    #[cfg(target_os = "linux")]
    {
        own_backup_backup::power_linux::KeepAwake::new(&format!("Own Backup: {what}"))
            .map(|k| Box::new(k) as Box<dyn Send>)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = what;
        None
    }
}
