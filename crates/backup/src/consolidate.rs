//! Backup-chain consolidation: the sliding increment window.
//!
//! A job that keeps at most N increments behind its full backup folds the
//! oldest one into the base when a new run overflows the window. The base then
//! stands for that increment's point in time, the next increment is repointed
//! at it, and the folded file is deleted. The number of restore points does not
//! change; the deepest one does — the base stops meaning "the day the chain
//! started" and starts meaning "the day of the increment it absorbed".
//!
//! [`crate::chains::plan_consolidation`] decides *whether* and *what*; this
//! module does it, for both containers:
//!
//! | | fold the increment in | repoint the next one |
//! |---|---|---|
//! | VHDX | `MergeVirtualDisk` (Windows) | `SetVirtualDiskInformation` |
//! | qcow2 | [`crate::qcow2::merge_child_into_parent`] | rewrite `backing_file` |
//! | `.obk` | [`crate::seal::obk::fold_child_into_base`] | [`crate::seal::obk::set_backing_file`] |
//!
//! The encrypted arm needs the chain's key, which no other arm does — a sealed
//! record belongs to one image's authenticated index, so folding means reading
//! and re-sealing rather than moving bytes. That key is the job's, opened once
//! at the start of a run and handed down here; a caller without one (the CLI
//! today) gets a refusal naming the image, and the chain is left as it is.
//!
//! # Order, and why it is that order
//!
//! 1. read the increment's own metadata — it becomes the base's;
//! 2. drop a marker beside the base naming the participants;
//! 3. fold the increment's data into the base;
//! 4. write the increment's metadata into the base, promoted to "full";
//! 5. repoint the next increment at the base;
//! 6. **delete the folded increment — last** — and clear the marker.
//!
//! The increment stays on disk until step 6, and the marker allows recovery
//! from interruptions BETWEEN completed steps. New `.obk` images publish
//! through alternate header slots; legacy images stage changes beside the
//! file and publish by rename. Steps 4 and 5 are
//! idempotent only when their individual writes complete.
//!
//! Step 3 is the one that needs care, and the two containers differ:
//!
//! - **qcow2** grafts clusters and swings the header last, so re-running it is
//!   always safe (it costs some dead space, nothing else).
//! - **VHDX** is documented as re-runnable while a merge is *unfinished* ("if a
//!   merge operation is interrupted, the child disk is still usable. The
//!   MergeVirtualDisk function can be rerun to finish the merge"), but a merge
//!   that already **succeeded** leaves the child invalid, and re-running would
//!   fail. So the marker records the merge the moment it returns, and if a
//!   crash beats that record, the failed re-run is cross-checked against the
//!   files themselves — by their DATA: does the base already carry every block
//!   the child holds? Then the fold happened. (The older cross-check asked
//!   whether the child's parent linkage had gone stale, on the theory that a
//!   merge bumps the base's `DataWriteGuid`. That stopped being true when
//!   images moved to userspace writing: nothing bumps the GUID any more, and a
//!   live run on 2026-08-21 showed a whole chain sharing one, unchanged across
//!   a real `MergeVirtualDisk`. It is still consulted, but only as a bonus.)
//!
//! # The window where a chain is not readable
//!
//! Between steps 3 and 5 the next increment still points at a base that has
//! been rewritten underneath it, so the chain does not open. That is precisely
//! why step 5 has to be reachable after a crash, and why the increment is not
//! deleted until it is. Consolidation therefore runs where nothing else is
//! using the images: after a completed backup, inside the job runner, with an
//! explicit check that nothing has them mounted.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use own_backup_common::types::BackupType;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::chains::ConsolidationStep;
use crate::seal::context::ArchiveKeys;
use crate::seal::ChainKey;
use crate::sidecar::{
    read_image_sidecar, sidecar_path_for, write_embedded_sidecar, write_sidecar, ChainRole, Sidecar,
};

/// Suffix of the file that says "a consolidation was in flight for this base".
pub const MARKER_SUFFIX: &str = ".consolidating.json";

const MARKER_VERSION: u32 = 1;

/// What a completed step did.
#[derive(Debug, Clone)]
pub struct ConsolidationOutcome {
    pub base: PathBuf,
    /// The increment that was folded in and deleted.
    pub folded: PathBuf,
    /// The increment that was repointed at the base, if there was one.
    pub repointed: Option<PathBuf>,
    /// Bytes in the base that the fold left unreferenced. They are
    /// free space as far as the format is concerned, but our writers only
    /// append, so they accumulate until the image is rebuilt.
    pub bytes_orphaned: u64,
    pub duration: Duration,
}

/// Crash marker. Holds file NAMES, not paths: a backup folder that has been
/// moved or remounted under a different letter must still resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Marker {
    version: u32,
    started_at: String,
    base: String,
    victim: String,
    next: Option<String>,
    /// Set — and flushed — the instant the fold returns successfully.
    #[serde(default)]
    merged: bool,
    /// The base's consolidation count as it stood BEFORE this step.
    ///
    /// Captured here rather than read when needed, because the fold overwrites
    /// the base's embedded sidecar with the increment's — after it, the base
    /// can no longer say how many increments it had already absorbed. Holding
    /// the number in the marker is also what keeps a resumed step from
    /// counting the same fold twice.
    #[serde(default)]
    folded_before: u32,
    /// When the chain was STARTED, carried for the same reason as
    /// `folded_before`: the fold replaces the base's sidecar with the
    /// increment's, and the increment knows nothing about the chain's birth.
    /// Without this the base's `created_at` keeps moving forward and the chain
    /// never grows old, so an age-based rule would never fire.
    #[serde(default)]
    chain_started_at: Option<String>,
}

/// Marker file for a given base image.
pub fn marker_path(base: &Path) -> PathBuf {
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(MARKER_SUFFIX);
    base.with_file_name(name)
}

fn file_name_of(path: &Path) -> Result<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| OwnBackupError::Config(format!("path has no file name: {}", path.display())))
}

/// Container family of an image, from its extension. A VHDX chain names its
/// children `.avhdx`, so both map to the same handling.
fn container_of(path: &Path) -> Result<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "vhdx" | "avhdx" => Ok("vhdx"),
        "qcow2" => Ok("qcow2"),
        "obk" => Ok("obk"),
        other => Err(OwnBackupError::ImageFormat(format!(
            "cannot consolidate a {other:?} image: {}",
            path.display()
        ))),
    }
}

/// The chain key for an encrypted image, or a refusal that names the file.
///
/// A caller without keys is not a bug — the CLI has none until somebody is
/// asked for a password (stage E5). It is a reason to leave the chain longer
/// than the window asked for, which is a correct chain.
fn chain_key_for(image: &Path, keys: Option<&ArchiveKeys>) -> Result<ChainKey> {
    let keys = keys.ok_or_else(|| {
        OwnBackupError::Encryption(
            tr!(
                "consolidate.encrypted_needs_job_key",
                image = image.display()
            )
            .into_owned(),
        )
    })?;
    Ok(keys.open_chain_of(image)?.key)
}

/// An image's sidecar, wherever that image keeps it: in its meta partition, in
/// a file beside it, or sealed inside the container.
fn read_sidecar_of(image: &Path, keys: Option<&ArchiveKeys>) -> Result<Sidecar> {
    if container_of(image)? == "obk" {
        let key = chain_key_for(image, keys)?;
        return crate::sidecar::read_sealed_sidecar(image, &key);
    }
    read_image_sidecar(image)
}

/// The same, header only — used before a step starts, where a missing sidecar
/// is not fatal.
fn read_header_of(
    image: &Path,
    keys: Option<&ArchiveKeys>,
) -> Option<(
    crate::sidecar::SidecarInfo,
    own_backup_common::image::ImageManifest,
)> {
    if container_of(image).ok()? == "obk" {
        let sidecar = read_sidecar_of(image, keys).ok()?;
        return Some((sidecar.info, sidecar.manifest));
    }
    crate::sidecar::read_image_sidecar_header(image).ok()
}

/// A sidecar in the byte form an encrypted image stores it in.
fn sealed_blob(sidecar: &Sidecar) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    crate::sidecar::write_sidecar_v2_full_to(
        &mut bytes,
        &sidecar.info,
        &sidecar.manifest,
        &sidecar.checksums,
        sidecar.disk_manifest.as_ref(),
    )?;
    Ok(bytes)
}

/// Extension as the sidecar readers want it (they dispatch on the file's own
/// extension, so `.avhdx` must stay `.avhdx`).
fn sidecar_ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// Write the marker so that a power cut leaves either the old one or the new
/// one, never half of either.
///
/// The marker only helps if it survives the crash that made it necessary, and
/// the old version could not promise that (audit AUD-06): `fs::write`
/// truncates the existing file in place, so a cut between truncate and write
/// left a zero-length marker — unreadable, which the resume pass treats as
/// "leave it alone". And the flush that was supposed to make it durable was
/// written as `let _ = file.sync_all()`, so a failing disk reported success.
///
/// Now: a temporary file beside it, flushed with the error CHECKED, then an
/// atomic rename over the old one.
fn write_marker(path: &Path, marker: &Marker) -> Result<()> {
    use std::io::Write;

    let json = serde_json::to_vec_pretty(marker)?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&json)?;
        // Not `let _ =`: if the bytes did not reach the platter, the marker is
        // not a marker, and the caller must not go on believing it has one.
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // The rename has to reach the platter too, or a cut can leave a directory
    // entry pointing at nothing. Windows has no directory handle to sync;
    // there `rename` over an existing file is already atomic and ordered.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Delete a file whose disappearance is part of the operation's meaning, so a
/// failure has to be reported rather than logged.
fn remove_checked(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OwnBackupError::ChainBroken(
            tr!(
                "consolidate.remove_failed",
                path = path.display(),
                error = e
            )
            .into_owned(),
        )),
    }
}

/// Resolve a name out of a marker against the folder the marker sits in.
///
/// A marker holds file NAMES on purpose, so a moved or remounted backup folder
/// still resumes. That only holds if they really are names: `folder.join()` on
/// `..\\..\\something` or on an absolute path walks straight out of the folder
/// (audit AUD-06, point 5).
fn name_inside(folder: &Path, name: &str) -> Result<PathBuf> {
    let candidate = Path::new(name);
    let mut parts = candidate.components();
    let single =
        matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none();
    if !single {
        return Err(OwnBackupError::ChainBroken(
            tr!("consolidate.marker_bad_name", name = format!("{name:?}")).into_owned(),
        ));
    }
    Ok(folder.join(name))
}

fn read_marker(path: &Path) -> Option<Marker> {
    let bytes = std::fs::read(path).ok()?;
    let marker: Marker = serde_json::from_slice(&bytes).ok()?;
    (marker.version == MARKER_VERSION).then_some(marker)
}

/// Refuse to touch images somebody is using. Consolidation rewrites a base
/// that a mounted chain is reading through, which would hand that reader a
/// file that changed under it.
#[cfg(target_os = "windows")]
pub(crate) fn ensure_idle(images: &[&Path]) -> Result<()> {
    for image in images {
        if crate::engine_win::child_path_for(image).exists() {
            return Err(OwnBackupError::Config(format!(
                "{} is mounted read-write — not consolidating while it is in use",
                image.display()
            )));
        }
        if crate::engine_win::qcow2_mounted_here(image) {
            return Err(OwnBackupError::Config(format!(
                "{} is mounted — not consolidating while it is in use",
                image.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn ensure_idle(_images: &[&Path]) -> Result<()> {
    Ok(())
}

/// Fold `victim` into `base` (VHDX arm).
#[cfg(target_os = "windows")]
fn fold_vhdx(base: &Path, victim: &Path) -> Result<()> {
    match crate::vhdx::merge_into_parent(victim) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Ask the FILES, not the error text. Two questions, either of which
            // proves the fold already happened:
            //
            //   a) is the child's recorded parent identity stale? A merge
            //      rewrites the base, and a base whose DataWriteGuid moved no
            //      longer matches what the child stamped at creation.
            //   b) does the base already carry the child's data?
            //
            // (a) alone is NOT enough any more, and that is measured, not
            // assumed: since images are written in userspace (no attach, no VHD
            // driver), nothing bumps DataWriteGuid — on the 2026-08-21 live run
            // every image in the chain carried the SAME one, unchanged across a
            // real MergeVirtualDisk. So (a) can no longer fire, and without (b)
            // a crash between a successful merge and the marker write would
            // leave the chain in its one unreadable state (base folded, next
            // not yet repointed) with the resume refusing to finish it — the
            // exact case step 6 is ordered last to make survivable.
            let stale = matches!(
                crate::vhdx_bat::vhdx_parent_linkage_matches(victim, base),
                Ok(false)
            );
            let applied = stale || merge_already_applied(base, victim).unwrap_or(false);
            if applied {
                warn!(
                    child = %victim.display(),
                    base = %base.display(),
                    stale_linkage = stale,
                    error = %e,
                    "MergeVirtualDisk refused a child whose data the base already carries \
                     — treating the merge as finished and completing the remaining steps"
                );
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Does `base` already carry every block `victim` holds? Then the fold is done.
///
/// Only the blocks the victim CARRIES are compared — everything else it reads
/// through the parent anyway, so it is equal by construction. An increment is
/// small, which is what makes this affordable on the crash path where it runs.
///
/// Reading is portable (`vhdx_bat`), so this does not care whether the victim
/// is still openable through the Windows API after a successful merge.
/// A victim whose blocks already match the base answers `true`, which is the
/// right answer either way: folding them in would change nothing.
fn merge_already_applied(base: &Path, victim: &Path) -> Result<bool> {
    let carried = crate::vhdx_bat::allocated_payload_blocks(victim)?;
    let mut vic = crate::vhdx_bat::VhdxGuestReader::open(victim)?;
    let mut base_reader = crate::vhdx_bat::VhdxGuestReader::open(base)?;
    for &block in &carried.blocks {
        let (start, end) = carried.block_range(block);
        let len = (end - start) as usize;
        let mut from_victim = vec![0u8; len];
        let mut from_base = vec![0u8; len];
        vic.read_at(start, &mut from_victim)?;
        base_reader.read_at(start, &mut from_base)?;
        if from_victim != from_base {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(not(target_os = "windows"))]
fn fold_vhdx(_base: &Path, _victim: &Path) -> Result<()> {
    Err(OwnBackupError::NotImplemented(
        "folding a differencing VHDX needs MergeVirtualDisk, which is Windows-only".into(),
    ))
}

/// Point `next` at `base` (VHDX arm).
#[cfg(target_os = "windows")]
fn repoint_vhdx(next: &Path, base: &Path) -> Result<()> {
    crate::vhdx::set_parent_path(next, base)
}

#[cfg(not(target_os = "windows"))]
fn repoint_vhdx(_next: &Path, _base: &Path) -> Result<()> {
    Err(OwnBackupError::NotImplemented(
        "repointing a differencing VHDX needs SetVirtualDiskInformation, which is Windows-only"
            .into(),
    ))
}

/// Write a sidecar back into an image: embedded when the image carries one,
/// and the adjacent `.obkm` when that exists. Both, when both do — the two
/// must not be allowed to disagree about who the parent is.
fn write_sidecars(image: &Path, sidecar: &Sidecar) -> Result<()> {
    let ext = sidecar_ext(image);
    let embedded = crate::sidecar::read_embedded_sidecar_header(image, &ext).is_ok();
    let external = sidecar_path_for(image);

    if embedded {
        write_embedded_sidecar(image, &ext, sidecar)?;
    }
    if external.exists() {
        write_sidecar(
            &external,
            &sidecar.info,
            &sidecar.manifest,
            &sidecar.checksums,
        )?;
    }
    if !embedded && !external.exists() {
        return Err(OwnBackupError::ImageFormat(format!(
            "{} has neither an embedded nor an adjacent sidecar to update",
            image.display()
        )));
    }
    Ok(())
}

/// Turn the folded increment's metadata into the base's own.
///
/// The checksum table needs no recomputation, and that is not an optimisation
/// but what the format already guarantees: a child's sidecar carries the
/// checksums of the WHOLE state it represents, inherited blocks included (see
/// `engine_win`, "Child sidecars carry the FULL state checksums"). After the
/// fold the base holds exactly that state, so the increment's table describes
/// it exactly. Only the four fields that say "I am a child" have to change.
fn promote(
    base: &Path,
    mut sidecar: Sidecar,
    folded: u32,
    chain_started_at: Option<String>,
) -> Result<Sidecar> {
    sidecar.info.image_file_name = file_name_of(base)?;
    sidecar.info.chain_role = ChainRole::Full;
    sidecar.info.parent_image_file_name = None;
    sidecar.info.consolidated_increments = folded;
    sidecar.info.chain_started_at = chain_started_at;
    sidecar.manifest.backup_type = BackupType::Full;
    sidecar.manifest.parent_backup_id = None;
    Ok(sidecar)
}

/// Repoint the increment that used to sit on the folded one.
///
/// `parent_backup_id` is deliberately left alone throughout: it named the
/// increment we just folded in, and the base has inherited that very id along
/// with its metadata, so the reference is still true. For an encrypted chain
/// that is not a nicety but the mechanism — `ObkChain` follows identities, so
/// an unchanged `parent_backup_id` is what lets the leaf open over the base
/// with nothing re-sealed but its own sidecar.
fn repoint_next(
    next: &Path,
    base: &Path,
    container: &str,
    keys: Option<&ArchiveKeys>,
) -> Result<()> {
    let base_name = file_name_of(base)?;

    if container == "obk" {
        let key = chain_key_for(next, keys)?;
        let mut sidecar = crate::sidecar::read_sealed_sidecar(next, &key)?;
        sidecar.info.parent_image_file_name = Some(base_name.clone());
        return crate::seal::obk::repoint_with_metadata(
            next,
            &key,
            &base_name,
            &sealed_blob(&sidecar)?,
        );
    }

    match container {
        "qcow2" => crate::qcow2::set_backing_file(next, Some(&base_name))?,
        "vhdx" => repoint_vhdx(next, base)?,
        other => {
            return Err(OwnBackupError::ImageFormat(format!(
                "unsupported container: {other}"
            )))
        }
    }

    // The container link and the sidecar's record of it must agree —
    // `list_job_chains` groups by the sidecar, the readers follow the file.
    let mut sidecar = read_image_sidecar(next)?;
    sidecar.info.parent_image_file_name = Some(base_name);
    write_sidecars(next, &sidecar)
}

fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Consolidation: could not delete file")
        }
    }
}

/// Perform one consolidation step, from wherever a previous attempt left off.
fn execute(
    base: &Path,
    victim: &Path,
    next: Option<&Path>,
    keys: Option<&ArchiveKeys>,
) -> Result<ConsolidationOutcome> {
    let started = Instant::now();
    let container = container_of(base)?;
    let marker_file = marker_path(base);

    if !base.exists() {
        return Err(OwnBackupError::ChainBroken(format!(
            "base image is missing: {}",
            base.display()
        )));
    }

    let mut watched: Vec<&Path> = vec![base, victim];
    watched.extend(next);
    ensure_idle(&watched)?;

    // Read what the step needs BEFORE the marker is written. A marker is a
    // promise that this step can be carried to the end, and writing one for a
    // step that cannot even start leaves the chain looking mid-fold to every
    // later run — while recording, for an encrypted chain, a consolidation
    // count and a chain birthday read from an image nobody could open.
    let victim_sidecar = read_sidecar_of(victim, keys)?;

    let mut marker = match read_marker(&marker_file) {
        // A marker naming a different increment means two different steps got
        // mixed up in one folder. Stop; a human should look.
        Some(existing) if existing.victim != file_name_of(victim)? => {
            return Err(OwnBackupError::ChainBroken(format!(
                "{} says a consolidation of {} is still in flight — refusing to start another",
                marker_file.display(),
                existing.victim
            )));
        }
        Some(existing) => existing,
        None => {
            // Read only when a marker has to be made: on a resume these two
            // come out of the marker, and asking the base again would get the
            // promoted answer and count the same fold twice.
            let base_header = read_header_of(base, keys);
            // A plaintext base may predate these fields, so a missing answer
            // there falls back below. An encrypted one always carries them,
            // and the same key just opened its increment — so "cannot read it"
            // means something is wrong, not that the count is zero. Guessing
            // would quietly reset how many increments the chain has absorbed
            // and when it started, and an age-based rule would never fire again.
            if container == "obk" && base_header.is_none() {
                return Err(OwnBackupError::ChainBroken(
                    tr!("consolidate.base_sidecar_unreadable", path = base.display()).into_owned(),
                ));
            }
            Marker {
                version: MARKER_VERSION,
                started_at: chrono::Utc::now().to_rfc3339(),
                base: file_name_of(base)?,
                victim: file_name_of(victim)?,
                next: next.map(file_name_of).transpose()?,
                merged: false,
                // Both read while the base is still describing itself. A base
                // that has never been consolidated has no stamp, and its own
                // creation time IS the chain's start — the same fallback
                // `Chain::started_at` makes.
                folded_before: base_header
                    .as_ref()
                    .map(|(info, _)| info.consolidated_increments)
                    .unwrap_or(0),
                chain_started_at: base_header.as_ref().and_then(|(info, manifest)| {
                    info.chain_started_at
                        .clone()
                        .or_else(|| Some(manifest.created_at.clone()))
                }),
            }
        }
    };

    write_marker(&marker_file, &marker)?;

    // The increment describes the state the base is about to hold.
    let promoted = promote(
        base,
        victim_sidecar,
        marker.folded_before + 1,
        marker.chain_started_at.clone(),
    )?;

    let mut bytes_orphaned = 0u64;
    if !marker.merged {
        match container {
            "qcow2" => {
                let stats = crate::qcow2::merge_child_into_parent(base, victim)?;
                bytes_orphaned = stats.bytes_orphaned;
                info!(
                    base = %base.display(),
                    increment = %victim.display(),
                    clusters = stats.clusters,
                    bytes = stats.bytes_copied,
                    orphaned = stats.bytes_orphaned,
                    "Consolidation: increment folded into the base"
                );
            }
            "vhdx" => {
                fold_vhdx(base, victim)?;
                info!(
                    base = %base.display(),
                    increment = %victim.display(),
                    "Consolidation: increment folded into the base"
                );
            }
            // The fold carries the promotion with it: an encrypted image keeps
            // its sidecar inside the container, and the rebuild that puts the
            // increment's clusters in is the one write that can replace it.
            "obk" => {
                let key = chain_key_for(base, keys)?;
                let stats = crate::seal::obk::fold_child_into_base(
                    base,
                    victim,
                    &key,
                    &sealed_blob(&promoted)?,
                    crate::seal::obk::RebuildIdentity {
                        backup_id: promoted.manifest.backup_id,
                        created_at: promoted.manifest.created_at.clone(),
                        consolidated_increments: promoted.info.consolidated_increments,
                        chain_started_at: promoted.info.chain_started_at.clone(),
                    },
                    // The increment's own level, from its manifest: the job's
                    // setting as it stood when that copy ran.
                    promoted.manifest.compression_level,
                )?;
                bytes_orphaned = stats.bytes_orphaned;
                info!(
                    base = %base.display(),
                    increment = %victim.display(),
                    clusters = stats.clusters,
                    orphaned = stats.bytes_orphaned,
                    already_applied = stats.already_applied,
                    "Consolidation: increment folded into the encrypted base"
                );
            }
            other => {
                return Err(OwnBackupError::ImageFormat(format!(
                    "unsupported container: {other}"
                )))
            }
        }
        marker.merged = true;
        write_marker(&marker_file, &marker)?;
    }

    // Everywhere but `.obk`, writing the promoted sidecar is its own step —
    // idempotent, so a resumed run simply does it again.
    if container != "obk" {
        write_sidecars(base, &promoted)?;
    }

    if let Some(next) = next {
        repoint_next(next, base, container, keys)?;
    }

    if container == "obk" {
        let key = chain_key_for(base, keys)?;
        // Keep the consolidation marker and victim until optional maintenance
        // has published safely. After a crash either complete base is readable;
        // retrying this phase preserves its identity and parent relationships.
        if let Some(compacted) = crate::seal::obk::compact_if_needed(base, &key)? {
            bytes_orphaned = 0;
            info!(base = %base.display(), before = compacted.before_bytes,
                after = compacted.after_bytes, reclaimed = compacted.reclaimed_bytes(),
                "Consolidation: obsolete encrypted records reclaimed");
        }
    }

    // Only now is the increment redundant. Its deletion is the step's last
    // phase and has to be CHECKABLE: reporting a shortened window while the
    // increment is still on disk sends the caller straight back to planning
    // the very same step (audit AUD-06, point 4). Failing here is safe — the
    // fold, the promotion and the repointing have all happened and are
    // idempotent, the marker stays, and the next recovery finishes the
    // deletion.
    remove_checked(victim)?;
    remove_quietly(&sidecar_path_for(victim)); // a description, not data
    remove_checked(&marker_file)?;

    let outcome = ConsolidationOutcome {
        base: base.to_path_buf(),
        folded: victim.to_path_buf(),
        repointed: next.map(|p| p.to_path_buf()),
        bytes_orphaned,
        duration: started.elapsed(),
    };
    info!(
        base = %outcome.base.display(),
        folded = %outcome.folded.display(),
        repointed = ?outcome.repointed.as_ref().map(|p| p.display().to_string()),
        secs = outcome.duration.as_secs(),
        "Consolidation complete"
    );
    Ok(outcome)
}

/// Run one step planned by [`crate::chains::plan_consolidation`].
pub fn consolidate_step(step: &ConsolidationStep) -> Result<ConsolidationOutcome> {
    consolidate_step_with_keys(step, None)
}

/// The same, for a job whose chain may be encrypted.
pub fn consolidate_step_with_keys(
    step: &ConsolidationStep,
    keys: Option<&ArchiveKeys>,
) -> Result<ConsolidationOutcome> {
    execute(
        &step.base.image_path,
        &step.victim.image_path,
        step.next.as_ref().map(|i| i.image_path.as_path()),
        keys,
    )
}

/// Finish whatever a crash left half-done in `folder`, before anything else
/// reads or plans against these chains. Returns how many markers it cleared.
///
/// Called at the start of a job run rather than at service start: the folder
/// may be a removable drive that was not attached when the service came up.
pub fn resume_pending(folder: &Path) -> Result<ResumeReport> {
    resume_pending_with_keys(folder, None)
}

/// The same, for a job whose chain may be encrypted.
pub fn resume_pending_with_keys(folder: &Path, keys: Option<&ArchiveKeys>) -> Result<ResumeReport> {
    let mut report = ResumeReport::default();
    let entries = match std::fs::read_dir(folder) {
        Ok(e) => e,
        Err(_) => return Ok(report),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_marker = path
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(MARKER_SUFFIX))
            .unwrap_or(false);
        if !is_marker {
            continue;
        }
        let Some(marker) = read_marker(&path) else {
            // Kept for a human to look at, never deleted and never guessed at
            // — and reported, so the chain it names is not quietly used as if
            // nothing had happened (audit AUD-06).
            warn!(path = %path.display(), "Consolidation: unreadable marker — leaving it in place");
            report.unfinished.push((
                path.clone(),
                tr!("consolidate.marker_unreadable").into_owned(),
            ));
            continue;
        };

        let resolved: Result<(PathBuf, PathBuf, Option<PathBuf>)> = (|| {
            Ok((
                name_inside(folder, &marker.base)?,
                name_inside(folder, &marker.victim)?,
                marker
                    .next
                    .as_deref()
                    .map(|n| name_inside(folder, n))
                    .transpose()?,
            ))
        })();
        let (base, victim, next) = match resolved {
            Ok(v) => v,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Consolidation: marker names are not usable");
                report.unfinished.push((path.clone(), e.to_string()));
                continue;
            }
        };

        if !victim.exists() {
            // The step got as far as deleting the increment; only the marker
            // outlived it. Everything before that had to have succeeded.
            info!(
                marker = %path.display(),
                "Consolidation: nothing left to finish — clearing the marker"
            );
            remove_quietly(&sidecar_path_for(&victim));
            match remove_checked(&path) {
                Ok(()) => report.finished.push(path.clone()),
                Err(e) => report.unfinished.push((path.clone(), e.to_string())),
            }
            continue;
        }

        warn!(
            base = %base.display(),
            increment = %victim.display(),
            started_at = %marker.started_at,
            merged = marker.merged,
            "Consolidation: finishing a step interrupted earlier"
        );
        match execute(&base, &victim, next.as_deref(), keys) {
            Ok(_) => report.finished.push(path.clone()),
            Err(e) => {
                warn!(
                    base = %base.display(),
                    error = %e,
                    "Consolidation: could not finish the interrupted step — the marker stays, \
                     the chain is left exactly as it was"
                );
                report.unfinished.push((path.clone(), e.to_string()));
            }
        }
    }
    Ok(report)
}

/// What a recovery pass over a folder found and what it managed to finish.
///
/// It used to return a bare count of markers cleared, which made "three
/// finished" and "three finished, one chain in an unknown state" the same
/// answer. The second one must stop the run: planning against a chain that is
/// mid-fold is planning against a lie (audit AUD-06).
#[derive(Debug, Default, Clone)]
pub struct ResumeReport {
    /// Markers whose step is now complete.
    pub finished: Vec<PathBuf>,
    /// Markers still on disk, with the reason. The chains they name must not
    /// be used as parents, and must not be deleted, until this is resolved.
    pub unfinished: Vec<(PathBuf, String)>,
}

impl ResumeReport {
    pub fn all_finished(&self) -> bool {
        self.unfinished.is_empty()
    }

    /// The base image each unfinished marker belongs to, so a caller can ask
    /// whether the chain in question is one of its own.
    pub fn stuck_bases(&self) -> Vec<PathBuf> {
        self.unfinished
            .iter()
            .filter_map(|(marker, _)| {
                let name = marker.file_name()?.to_string_lossy().into_owned();
                let base = name.strip_suffix(MARKER_SUFFIX)?;
                Some(marker.with_file_name(base))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vhdx_format::VhdxWriter;

    /// The crash-path question "did the fold already happen?" must be answered
    /// from the DATA. The old answer — "the child's parent linkage went stale"
    /// — cannot fire any more: images are written in userspace, so nothing
    /// bumps DataWriteGuid, and a live run on 2026-08-21 showed every image in
    /// a chain carrying the same one, unchanged across a real MergeVirtualDisk.
    #[test]
    fn merge_already_applied_answers_from_the_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mb = 1024 * 1024u64;
        let base = dir.path().join("base.vhdx");
        let victim = dir.path().join("victim.vhdx");
        let payload = vec![0x7Eu8; mb as usize];

        // The victim carries block 1; the base still has the old bytes there.
        let mut w = VhdxWriter::create(&victim, 8 * mb, mb, 512).unwrap();
        w.write_at(mb, &payload).unwrap();
        w.finish().unwrap();
        let mut w = VhdxWriter::create(&base, 8 * mb, mb, 512).unwrap();
        w.write_at(mb, &vec![0x11u8; mb as usize]).unwrap();
        w.finish().unwrap();
        assert!(
            !merge_already_applied(&base, &victim).unwrap(),
            "the fold has NOT happened yet"
        );

        // Now the base carries the victim's bytes — the fold is done.
        let mut w = VhdxWriter::create(&base, 8 * mb, mb, 512).unwrap();
        w.write_at(mb, &payload).unwrap();
        w.finish().unwrap();
        assert!(
            merge_already_applied(&base, &victim).unwrap(),
            "the fold IS already applied"
        );
    }

    use crate::chains::{list_job_chains, plan_consolidation};
    use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
    use crate::qcow2::{Qcow2RandomReader, Qcow2StagedWriter, Qcow2WriterOptions};
    use crate::sidecar::{BlockChecksum, SidecarInfo};
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::gpt::{DiskBackupManifest, DiskImageRef, DiskLayout};
    use own_backup_common::image::ImageManifest;
    use own_backup_common::types::*;
    use uuid::Uuid;

    const CS: u64 = 65536;
    /// 1 MiB GPT head + 1 MiB volume + 512 KiB meta + tail for the backup GPT.
    const VOLUME_LEN: u64 = 1024 * 1024;
    const META_SIZE: u64 = 512 * 1024;
    const META_OFFSET: u64 = DEFAULT_PARTITION_OFFSET + VOLUME_LEN;
    const VSIZE: u64 = 4 * 1024 * 1024;

    fn manifest(created_at: &str, kind: BackupType, parent: Option<Uuid>) -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::new_v4(),
            created_at: created_at.into(),
            backup_type: kind,
            parent_backup_id: parent,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\E:".into(),
                model: String::new(),
                serial: String::new(),
                // The volume this fixture actually builds. It used to say 4096
                // bytes while claiming eight 4096-byte blocks and numbering
                // checksums up to block 3 — an image that cannot exist. Nothing
                // read it until the sidecar reader started bounding the
                // checksum table by the size the header itself states.
                size_bytes: VOLUME_LEN,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::Zstd,
            compression_level: 3,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 4096,
            total_blocks: VOLUME_LEN / 4096,
            used_blocks: 8,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    /// Build a qcow2 image with a real GPT holding the OwnBackupMeta partition,
    /// its embedded sidecar, and `fill` written into `data_clusters`.
    #[allow(clippy::too_many_arguments)]
    fn put_image(
        dir: &Path,
        name: &str,
        backing: Option<&str>,
        role: ChainRole,
        created_at: &str,
        kind: BackupType,
        parent_id: Option<Uuid>,
        data_clusters: &[(u64, u8)],
        checksums: &[BlockChecksum],
        disk_manifest: Option<DiskBackupManifest>,
    ) -> (PathBuf, Uuid) {
        let path = dir.join(name);
        let mut writer = Qcow2StagedWriter::create(
            &path,
            Qcow2WriterOptions {
                virtual_size: VSIZE,
                backing_file: backing.map(|s| s.to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        // Only the root of a chain lays down the GPT; children inherit it.
        if backing.is_none() {
            let ops = gpt_write_ops(&GptLayout {
                disk_size: VSIZE,
                sector_size: 512,
                partition_offset: DEFAULT_PARTITION_OFFSET,
                partition_size: VOLUME_LEN,
                disk_guid: Uuid::new_v4(),
                partition_guid: Uuid::new_v4(),
                partition_name: "OwnBackupData".into(),
                meta_partition_size: Some(META_SIZE),
                meta_partition_guid: Some(Uuid::new_v4()),
            })
            .unwrap();
            for op in ops {
                writer.write_at(op.offset, &op.bytes).unwrap();
            }
        }
        for &(index, fill) in data_clusters {
            writer
                .write_at(
                    DEFAULT_PARTITION_OFFSET + index * CS,
                    &vec![fill; CS as usize],
                )
                .unwrap();
        }
        // Reserve the meta partition so the blob write has clusters to land in.
        writer
            .write_at(META_OFFSET, &vec![0xEEu8; META_SIZE as usize])
            .unwrap();
        writer.finish().unwrap();

        let mut info = match backing {
            None => SidecarInfo::new_full(name, DEFAULT_PARTITION_OFFSET),
            Some(parent) => SidecarInfo::new_child(name, role, parent, DEFAULT_PARTITION_OFFSET),
        };
        info.container = "qcow2".into();
        info.job_id = Some("j1".into());
        let manifest = manifest(created_at, kind, parent_id);
        let backup_id = manifest.backup_id;

        let sidecar = Sidecar {
            info,
            manifest,
            checksums: checksums.to_vec(),
            disk_manifest,
        };
        write_embedded_sidecar(&path, "qcow2", &sidecar).unwrap();
        (path, backup_id)
    }

    /// The backed-up volume's bytes — the data partition only.
    ///
    /// Deliberately NOT the whole virtual disk: consolidation is *supposed* to
    /// change the meta partition (the leaf's sidecar now names a different
    /// parent), and comparing that in would assert the opposite of what we
    /// want. What must never change is what the volume reads.
    fn read_volume(path: &Path) -> Vec<u8> {
        let mut r = Qcow2RandomReader::open(path).unwrap();
        let mut buf = vec![0u8; VOLUME_LEN as usize];
        r.read_at(DEFAULT_PARTITION_OFFSET, &mut buf).unwrap();
        buf
    }

    fn sums(pairs: &[(u64, u64)]) -> Vec<BlockChecksum> {
        pairs
            .iter()
            .map(|&(block_number, checksum)| BlockChecksum {
                block_number,
                checksum,
            })
            .collect()
    }

    /// base ← inc1 ← inc2 with real sidecars, one window step applied.
    fn three_image_chain(dir: &Path) -> (PathBuf, PathBuf, PathBuf, Uuid) {
        let (base, base_id) = put_image(
            dir,
            "j_C_full_1.qcow2",
            None,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11), (2, 0x11)],
            &sums(&[(0, 100), (1, 101)]),
            None,
        );
        let (inc1, inc1_id) = put_image(
            dir,
            "j_C_inc_2.qcow2",
            Some("j_C_full_1.qcow2"),
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some(base_id),
            &[(2, 0x22), (3, 0x22)],
            &sums(&[(0, 100), (1, 201), (2, 202)]),
            None,
        );
        let (inc2, _) = put_image(
            dir,
            "j_C_inc_3.qcow2",
            Some("j_C_inc_2.qcow2"),
            ChainRole::Incremental,
            "2026-01-03T00:00:00Z",
            BackupType::Incremental,
            Some(inc1_id),
            &[(3, 0x33), (4, 0x33)],
            &sums(&[(0, 100), (1, 201), (3, 303)]),
            None,
        );
        (base, inc1, inc2, inc1_id)
    }

    /// The whole point: after a step the leaf reads exactly what it did, and
    /// the base has become a full backup of the folded increment's moment —
    /// carrying that increment's identity, checksums and role.
    #[test]
    fn one_window_step_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2, inc1_id) = three_image_chain(dir.path());

        let leaf_before = read_volume(&inc2);
        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        let step = plan_consolidation(&chain, 1).expect("2 increments, limit 1");
        let outcome = consolidate_step(&step).unwrap();

        assert_eq!(outcome.folded, inc1);
        assert_eq!(outcome.repointed.as_deref(), Some(inc2.as_path()));
        assert!(!inc1.exists(), "the folded increment is deleted");
        assert!(!marker_path(&base).exists(), "the marker is cleared");
        assert_eq!(
            read_volume(&inc2),
            leaf_before,
            "the leaf must read unchanged"
        );

        let promoted = read_image_sidecar(&base).unwrap();
        assert_eq!(promoted.info.chain_role, ChainRole::Full);
        assert_eq!(promoted.info.parent_image_file_name, None);
        assert_eq!(promoted.info.image_file_name, "j_C_full_1.qcow2");
        assert_eq!(promoted.manifest.backup_type, BackupType::Full);
        assert_eq!(promoted.manifest.parent_backup_id, None);
        assert_eq!(
            promoted.manifest.backup_id, inc1_id,
            "the base now IS that point in time"
        );
        assert_eq!(
            promoted.checksums,
            sums(&[(0, 100), (1, 201), (2, 202)]),
            "checksums come from the increment, not from re-reading the image"
        );

        // The leaf's own record of its parent follows the container link, and
        // its parent_backup_id still names the id the base has inherited.
        let leaf = read_image_sidecar(&inc2).unwrap();
        assert_eq!(
            leaf.info.parent_image_file_name.as_deref(),
            Some("j_C_full_1.qcow2")
        );
        assert_eq!(leaf.manifest.parent_backup_id, Some(inc1_id));

        // And the shortened chain is what the planner now sees.
        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        assert_eq!(chain.images.len(), 2);
        assert!(plan_consolidation(&chain, 1).is_none());
    }

    /// A crash between folding the data and repointing the next increment is
    /// the dangerous moment — the chain does not open until step 5 lands.
    /// Resuming from the marker must finish it.
    #[test]
    fn interrupted_after_the_fold_is_finished_by_resume() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, inc2, _) = three_image_chain(dir.path());
        let leaf_before = read_volume(&inc2);

        // Exactly the state a crash leaves: marker says merged, data folded,
        // metadata and the link not yet updated.
        let marker = Marker {
            version: MARKER_VERSION,
            started_at: "2026-01-03T01:00:00Z".into(),
            base: "j_C_full_1.qcow2".into(),
            victim: "j_C_inc_2.qcow2".into(),
            next: Some("j_C_inc_3.qcow2".into()),
            merged: true,
            folded_before: 0,
            chain_started_at: Some("2026-01-01T00:00:00Z".into()),
        };
        write_marker(&marker_path(&base), &marker).unwrap();
        crate::qcow2::merge_child_into_parent(&base, &inc1).unwrap();

        let report = resume_pending(dir.path()).unwrap();
        assert!(report.all_finished(), "{report:?}");
        assert_eq!(report.finished.len(), 1);
        assert!(!inc1.exists());
        assert!(!marker_path(&base).exists());
        assert_eq!(
            read_volume(&inc2),
            leaf_before,
            "the leaf survived the interruption"
        );
        assert_eq!(
            read_image_sidecar(&base).unwrap().info.chain_role,
            ChainRole::Full
        );
    }

    /// A marker whose increment is already gone means every destructive step
    /// had finished; resume just clears it.
    #[test]
    fn stale_marker_without_its_increment_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _, _, _) = three_image_chain(dir.path());
        let marker = Marker {
            version: MARKER_VERSION,
            started_at: "2026-01-03T01:00:00Z".into(),
            base: "j_C_full_1.qcow2".into(),
            victim: "gone.qcow2".into(),
            next: None,
            merged: true,
            folded_before: 0,
            chain_started_at: Some("2026-01-01T00:00:00Z".into()),
        };
        write_marker(&marker_path(&base), &marker).unwrap();
        let report = resume_pending(dir.path()).unwrap();
        assert!(report.all_finished(), "{report:?}");
        assert_eq!(report.finished.len(), 1);
        assert!(!marker_path(&base).exists());
    }

    /// Consolidating the LEAF (the window shrank to zero) leaves a lone full
    /// backup — and must not try to repoint anything.
    #[test]
    fn folding_the_leaf_leaves_a_standalone_full() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = put_image(
            dir.path(),
            "k_C_full_1.qcow2",
            None,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11)],
            &sums(&[(0, 1)]),
            None,
        );
        let base_id = read_image_sidecar(&base).unwrap().manifest.backup_id;
        let (inc, _) = put_image(
            dir.path(),
            "k_C_inc_2.qcow2",
            Some("k_C_full_1.qcow2"),
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some(base_id),
            &[(2, 0x22)],
            &sums(&[(0, 1), (1, 2)]),
            None,
        );
        let expected = read_volume(&inc);

        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        let step = plan_consolidation(&chain, 0).unwrap();
        let outcome = consolidate_step(&step).unwrap();

        assert!(outcome.repointed.is_none());
        assert!(!inc.exists());
        assert_eq!(
            read_volume(&base),
            expected,
            "the base now reads like the leaf did"
        );
        let sidecar = read_image_sidecar(&base).unwrap();
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
        assert_eq!(sidecar.checksums, sums(&[(0, 1), (1, 2)]));
    }

    /// A whole-disk image keeps its restore manifest through the promotion.
    /// Losing it would leave an image that verifies, mounts, and cannot be
    /// restored to bare metal — the failure nobody notices until they need it.
    #[test]
    fn promotion_keeps_the_whole_disk_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (base, base_id) = put_image(
            dir.path(),
            "d_disk0_full_1.qcow2",
            None,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11)],
            &sums(&[(0, 1)]),
            None,
        );
        let disk_manifest = DiskBackupManifest {
            format_version: DiskBackupManifest::FORMAT_VERSION,
            created_at: "2026-01-02T00:00:00Z".into(),
            job_id: Some("j1".into()),
            disk_number: 0,
            disk_model: "TEST".into(),
            layout: DiskLayout {
                disk_guid: Uuid::nil().to_string(),
                sector_size: 512,
                size_bytes: VSIZE,
                partitions: vec![],
            },
            images: vec![DiskImageRef {
                part_index: 1,
                token: "C".into(),
                image_file_name: "d_disk0_inc_2.qcow2".into(),
                filesystem: "ntfs".into(),
                encryption: None,
            }],
            source_disk: None,
        };
        let (inc, _) = put_image(
            dir.path(),
            "d_disk0_inc_2.qcow2",
            Some("d_disk0_full_1.qcow2"),
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some(base_id),
            &[(2, 0x22)],
            &sums(&[(0, 1), (1, 2)]),
            Some(disk_manifest.clone()),
        );
        assert!(inc.exists());

        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        let step = plan_consolidation(&chain, 0).unwrap();
        consolidate_step(&step).unwrap();

        let promoted = read_image_sidecar(&base).unwrap();
        let kept = promoted
            .disk_manifest
            .expect("whole-disk manifest must survive promotion");
        assert_eq!(kept.images.len(), 1);
        assert_eq!(kept.images[0].image_file_name, "d_disk0_inc_2.qcow2");
    }

    // --- Восстановление после прерывания (аудит AUD-06) --------------------

    /// Маркер, который не читается, — это не «нечего делать». Пока в него не
    /// заглянул человек, состояние цепочки неизвестно, и проход обязан это
    /// сказать, а не вернуть число, похожее на успех.
    #[test]
    fn an_unreadable_marker_is_reported_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _, _, _) = three_image_chain(dir.path());
        let marker = marker_path(&base);
        std::fs::write(&marker, b"{ this is not json").unwrap();

        let report = resume_pending(dir.path()).unwrap();
        assert!(!report.all_finished(), "{report:?}");
        assert!(
            marker.exists(),
            "повреждённый маркер сохраняется для разбора"
        );
        assert_eq!(report.stuck_bases(), vec![base], "{report:?}");
    }

    /// Пустой маркер — это ровно то, что оставлял прежний `fs::write`, если
    /// питание пропадало между усечением файла и его заполнением.
    #[test]
    fn a_truncated_marker_is_reported_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _, _, _) = three_image_chain(dir.path());
        let marker = marker_path(&base);
        std::fs::write(&marker, b"").unwrap();

        let report = resume_pending(dir.path()).unwrap();
        assert!(!report.all_finished(), "{report:?}");
        assert!(marker.exists());
    }

    /// Маркер хранит ИМЕНА файлов, чтобы переехавшая папка копий всё равно
    /// доводилась до конца. «Имя», уводящее из папки, таковым не является.
    #[test]
    fn a_marker_name_that_leaves_the_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (base, inc1, _, _) = three_image_chain(dir.path());
        let marker = Marker {
            version: MARKER_VERSION,
            started_at: "2026-01-03T01:00:00Z".into(),
            base: "../j_C_full_1.qcow2".into(),
            victim: "j_C_inc_2.qcow2".into(),
            next: None,
            merged: false,
            folded_before: 0,
            chain_started_at: None,
        };
        write_marker(&marker_path(&base), &marker).unwrap();

        let report = resume_pending(dir.path()).unwrap();
        assert!(!report.all_finished(), "{report:?}");
        assert!(inc1.exists(), "ни один файл не должен быть тронут");
        assert!(marker_path(&base).exists());
    }

    /// Запись маркера идёт через временный файл и переименование, поэтому
    /// момента, когда на диске лежит ни то ни другое, не существует. Здесь
    /// видно следствие: ничего лишнего рядом не остаётся, а перезапись
    /// заменяет содержимое целиком.
    #[test]
    fn writing_a_marker_leaves_nothing_half_written_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("x_full.qcow2");
        let path = marker_path(&base);

        let mut marker = Marker {
            version: MARKER_VERSION,
            started_at: "2026-01-03T01:00:00Z".into(),
            base: "x_full.qcow2".into(),
            victim: "x_inc.qcow2".into(),
            next: None,
            merged: false,
            folded_before: 0,
            chain_started_at: None,
        };
        write_marker(&path, &marker).unwrap();
        marker.merged = true;
        write_marker(&path, &marker).unwrap();

        assert!(
            read_marker(&path).unwrap().merged,
            "перезапись видна целиком"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остался временный файл: {leftovers:?}"
        );
    }

    /// Удаление свёрнутого инкремента — последняя фаза шага, и её результат
    /// должен быть проверяемым. Раньше оно шло через remove_quietly, и шаг
    /// отчитывался об укороченном окне, когда инкремент ещё лежал на месте —
    /// после чего вызывающий планировал ровно тот же шаг снова.
    #[test]
    fn deleting_the_increment_gives_a_checkable_result() {
        let dir = tempfile::tempdir().unwrap();
        // Уже нет — смысл фазы выполнен.
        assert!(remove_checked(&dir.path().join("never-existed")).is_ok());
        // Не удаляется — это ошибка, а не строчка в журнале.
        std::fs::create_dir(dir.path().join("stuck")).unwrap();
        assert!(remove_checked(&dir.path().join("stuck")).is_err());
    }

    // --- Зашифрованная цепочка (§7.4) ------------------------------------

    /// One job's keys, with the test KDF profile — the production one takes
    /// tens of seconds by design and proves nothing extra here.
    fn obk_keys() -> crate::seal::context::ArchiveKeys {
        use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
        use crate::seal::recovery::RecoveryKey;

        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let envelope = SealedPrivateKey::seal(
            "j1",
            &pair,
            "пароль архива",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        crate::seal::context::ArchiveKeys::from_private(envelope, pair.private).unwrap()
    }

    const OBK_CLUSTER: u64 = 1 << crate::seal::context::OBK_CLUSTER_BITS;

    /// One encrypted image of a chain, with a real sealed sidecar.
    #[allow(clippy::too_many_arguments)]
    fn put_obk(
        dir: &Path,
        name: &str,
        keys: &crate::seal::context::ArchiveKeys,
        session: &crate::seal::context::ChainSession,
        role: ChainRole,
        created_at: &str,
        kind: BackupType,
        parent: Option<(Uuid, &str)>,
        clusters: &[(u64, u8)],
        checksums: &[BlockChecksum],
    ) -> (PathBuf, Uuid) {
        use crate::seal::obk::{ObkCompression, ObkWriter};

        let path = dir.join(name);
        let manifest = manifest(created_at, kind, parent.map(|(id, _)| id));
        let backup_id = manifest.backup_id;
        let options = keys.writer_options(
            session,
            crate::seal::context::ImagePlan {
                backup_id,
                role,
                parent,
                created_at: created_at.into(),
                virtual_size: VSIZE,
                source_id: Some("volume-under-test".into()),
                consolidated_increments: 0,
                chain_started_at: None,
                compression: ObkCompression::Zstd { level: 3 },
            },
        );
        let mut writer = ObkWriter::create(&path, &session.key, options).unwrap();
        for &(index, fill) in clusters {
            writer
                .write_cluster(index, &vec![fill; OBK_CLUSTER as usize])
                .unwrap();
        }

        let mut info = match parent {
            None => SidecarInfo::new_full(name, DEFAULT_PARTITION_OFFSET),
            Some((_, parent_name)) => {
                SidecarInfo::new_child(name, role, parent_name, DEFAULT_PARTITION_OFFSET)
            }
        };
        info.container = "obk".into();
        info.job_id = Some("j1".into());
        let sidecar = Sidecar {
            info,
            manifest,
            checksums: checksums.to_vec(),
            disk_manifest: None,
        };
        let blob = sealed_blob(&sidecar).unwrap();
        // Room to rewrite it later: repointing the next increment replaces its
        // sidecar in place, and the file name of a parent can grow.
        let reserve = blob.len() + 4096;
        writer.finish_with_reserve(&blob, reserve).unwrap();
        (path, backup_id)
    }

    /// Every cluster of an encrypted chain, read through the chain itself.
    fn read_obk(path: &Path, keys: &crate::seal::context::ArchiveKeys) -> Vec<Vec<u8>> {
        let key = keys.open_chain_of(path).unwrap().key;
        let mut chain = crate::seal::obk::ObkChain::open(path, &key).unwrap();
        (0..VSIZE / OBK_CLUSTER)
            .map(|c| chain.read_cluster(c).unwrap().to_vec())
            .collect()
    }

    fn obk_sidecar(path: &Path, keys: &crate::seal::context::ArchiveKeys) -> Sidecar {
        let key = keys.open_chain_of(path).unwrap().key;
        crate::sidecar::read_sealed_sidecar(path, &key).unwrap()
    }

    /// The whole step for an encrypted chain, judged by the same criterion as
    /// the plaintext one: the leaf reads exactly what it read before, and the
    /// base has become a full backup of the folded increment's moment.
    #[test]
    fn one_window_step_end_to_end_on_an_encrypted_chain() {
        let dir = tempfile::tempdir().unwrap();
        let keys = obk_keys();
        let session = keys.start_chain().unwrap();

        let (base, base_id) = put_obk(
            dir.path(),
            "e_C_full_1.obk",
            &keys,
            &session,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11), (2, 0x11)],
            &sums(&[(0, 100), (1, 101)]),
        );
        let (inc1, inc1_id) = put_obk(
            dir.path(),
            "e_C_inc_2.obk",
            &keys,
            &session,
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some((base_id, "e_C_full_1.obk")),
            &[(2, 0x22), (3, 0x22)],
            &sums(&[(0, 100), (1, 201), (2, 202)]),
        );
        let (inc2, _) = put_obk(
            dir.path(),
            "e_C_inc_3.obk",
            &keys,
            &session,
            ChainRole::Incremental,
            "2026-01-03T00:00:00Z",
            BackupType::Incremental,
            Some((inc1_id, "e_C_inc_2.obk")),
            &[(3, 0x33), (4, 0x33)],
            &sums(&[(0, 100), (1, 201), (3, 303)]),
        );

        let leaf_before = read_obk(&inc2, &keys);
        // Model the unreachable tail left by older generations. The marked
        // coordinator must reclaim it before retiring the increment/marker.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&base)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        assert_eq!(chain.images.len(), 3, "the listing needs no key");
        let step = plan_consolidation(&chain, 1).expect("2 increments, limit 1");
        let outcome = consolidate_step_with_keys(&step, Some(&keys)).unwrap();

        assert_eq!(outcome.folded, inc1);
        assert_eq!(outcome.repointed.as_deref(), Some(inc2.as_path()));
        assert!(!inc1.exists(), "the folded increment is deleted");
        assert!(!marker_path(&base).exists(), "the marker is cleared");
        assert!(
            std::fs::metadata(&base).unwrap().len() < 8 * 1024 * 1024,
            "automatic cleanup must discard the obsolete tail"
        );
        assert_eq!(outcome.bytes_orphaned, 0);
        assert_eq!(
            read_obk(&inc2, &keys),
            leaf_before,
            "the leaf must read unchanged"
        );

        // The base is that point in time now, and still encrypted.
        let promoted = obk_sidecar(&base, &keys);
        assert_eq!(promoted.info.chain_role, ChainRole::Full);
        assert_eq!(promoted.info.parent_image_file_name, None);
        assert_eq!(promoted.info.consolidated_increments, 1);
        assert_eq!(
            promoted.info.chain_started_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(promoted.manifest.backup_type, BackupType::Full);
        assert_eq!(promoted.manifest.backup_id, inc1_id);
        assert_eq!(promoted.checksums, sums(&[(0, 100), (1, 201), (2, 202)]));
        assert!(
            crate::qcow2::Qcow2RandomReader::open(&base).is_err(),
            "a folded base is still an encrypted image"
        );

        // The leaf follows the base by name and by identity.
        let leaf = obk_sidecar(&inc2, &keys);
        assert_eq!(
            leaf.info.parent_image_file_name.as_deref(),
            Some("e_C_full_1.obk")
        );
        assert_eq!(leaf.manifest.parent_backup_id, Some(inc1_id));
        let listing = crate::seal::obk::read_listing(&inc2).unwrap();
        assert_eq!(listing.parent_file_name.as_deref(), Some("e_C_full_1.obk"));

        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        assert_eq!(chain.images.len(), 2);
        assert!(plan_consolidation(&chain, 1).is_none());

        // What retention counts, for an encrypted chain, comes out of the open
        // header — and consolidation is the one thing that can make the files
        // on disk disagree with the chain's history. Two increments have been
        // taken over this chain's life; one of them is now inside the base.
        assert_eq!(
            chain.increments_ever(),
            2,
            "a folded increment still counts"
        );
        assert_eq!(
            chain.started_at(),
            "2026-01-01T00:00:00Z",
            "the chain did not get younger by being consolidated"
        );
    }

    /// The dangerous moment, for an encrypted chain: the base has been folded
    /// and the leaf has not been repointed, so the chain genuinely does not
    /// open — by design, since the leaf's recorded parent identity is now the
    /// base's and the *name* it follows still points at a file about to be
    /// deleted. Resuming from the marker has to finish it.
    #[test]
    fn an_interrupted_encrypted_fold_is_finished_by_resume() {
        let dir = tempfile::tempdir().unwrap();
        let keys = obk_keys();
        let session = keys.start_chain().unwrap();

        let (base, base_id) = put_obk(
            dir.path(),
            "e_C_full_1.obk",
            &keys,
            &session,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11), (2, 0x11)],
            &sums(&[(0, 100), (1, 101)]),
        );
        let (inc1, inc1_id) = put_obk(
            dir.path(),
            "e_C_inc_2.obk",
            &keys,
            &session,
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some((base_id, "e_C_full_1.obk")),
            &[(2, 0x22), (3, 0x22)],
            &sums(&[(0, 100), (1, 201), (2, 202)]),
        );
        let (inc2, _) = put_obk(
            dir.path(),
            "e_C_inc_3.obk",
            &keys,
            &session,
            ChainRole::Incremental,
            "2026-01-03T00:00:00Z",
            BackupType::Incremental,
            Some((inc1_id, "e_C_inc_2.obk")),
            &[(3, 0x33), (4, 0x33)],
            &sums(&[(0, 100), (1, 201), (3, 303)]),
        );
        let leaf_before = read_obk(&inc2, &keys);

        // Exactly the state a crash leaves: the fold done and recorded, the
        // link not yet moved and the increment still on disk.
        let marker = Marker {
            version: MARKER_VERSION,
            started_at: "2026-01-03T01:00:00Z".into(),
            base: "e_C_full_1.obk".into(),
            victim: "e_C_inc_2.obk".into(),
            next: Some("e_C_inc_3.obk".into()),
            merged: true,
            folded_before: 0,
            chain_started_at: Some("2026-01-01T00:00:00Z".into()),
        };
        write_marker(&marker_path(&base), &marker).unwrap();
        let promoted = promote(
            &base,
            obk_sidecar(&inc1, &keys),
            1,
            Some("2026-01-01T00:00:00Z".into()),
        )
        .unwrap();
        let key = keys.open_chain_of(&base).unwrap().key;
        crate::seal::obk::fold_child_into_base(
            &base,
            &inc1,
            &key,
            &sealed_blob(&promoted).unwrap(),
            crate::seal::obk::RebuildIdentity {
                backup_id: inc1_id,
                created_at: "2026-01-02T00:00:00Z".into(),
                consolidated_increments: 1,
                chain_started_at: Some("2026-01-01T00:00:00Z".into()),
            },
            promoted.manifest.compression_level,
        )
        .unwrap();

        // This is the window the ordering exists for: the leaf does not open.
        assert!(
            crate::seal::obk::ObkChain::open(&inc2, &key).is_err(),
            "between the fold and the repoint the chain is not readable — \
             that is why step 5 has to survive a crash"
        );

        let report = resume_pending_with_keys(dir.path(), Some(&keys)).unwrap();
        assert!(report.all_finished(), "{report:?}");
        assert_eq!(report.finished.len(), 1);
        assert!(!inc1.exists());
        assert!(!marker_path(&base).exists());
        assert_eq!(
            read_obk(&inc2, &keys),
            leaf_before,
            "the leaf survived the interruption"
        );
        let promoted = obk_sidecar(&base, &keys);
        assert_eq!(promoted.info.chain_role, ChainRole::Full);
        assert_eq!(
            promoted.info.consolidated_increments, 1,
            "the fold is counted once"
        );
    }

    /// Without the job's key nothing can be folded — and nothing may be
    /// touched while saying so. A chain left longer than the window asked for
    /// is a correct chain; a half-rewritten one is not.
    #[test]
    fn an_encrypted_chain_is_left_alone_when_there_is_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys = obk_keys();
        let session = keys.start_chain().unwrap();
        let (base, base_id) = put_obk(
            dir.path(),
            "e_C_full_1.obk",
            &keys,
            &session,
            ChainRole::Full,
            "2026-01-01T00:00:00Z",
            BackupType::Full,
            None,
            &[(1, 0x11)],
            &sums(&[(0, 1)]),
        );
        let (inc, _) = put_obk(
            dir.path(),
            "e_C_inc_2.obk",
            &keys,
            &session,
            ChainRole::Incremental,
            "2026-01-02T00:00:00Z",
            BackupType::Incremental,
            Some((base_id, "e_C_full_1.obk")),
            &[(2, 0x22)],
            &sums(&[(0, 1), (1, 2)]),
        );
        let before = std::fs::read(&base).unwrap();

        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        let step = plan_consolidation(&chain, 0).unwrap();
        let error = consolidate_step(&step).unwrap_err().to_string();
        assert!(error.contains("ключ задания"), "unexpected: {error}");

        assert!(inc.exists(), "the increment stays");
        assert_eq!(
            std::fs::read(&base).unwrap(),
            before,
            "the base is untouched"
        );
        assert!(!marker_path(&base).exists(), "and no marker is left behind");
    }

    /// Имя внутри папки — ровно одна часть пути и ничего больше.
    #[test]
    fn only_a_plain_file_name_resolves_inside_the_folder() {
        let folder = Path::new("/backups");
        assert!(name_inside(folder, "a_full.qcow2").is_ok());
        for bad in ["../a.qcow2", "sub/a.qcow2", "/etc/passwd", "", "."] {
            assert!(
                name_inside(folder, bad).is_err(),
                "{bad:?} должно быть отвергнуто"
            );
        }
    }
}
