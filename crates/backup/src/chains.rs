//! Backup-chain discovery and retention planning (plan v2, phase 5).
//!
//! A *chain* is one full backup plus every incremental/differential image
//! that (transitively) depends on it. Chains are discovered from the `.obkm`
//! sidecars in the destination folder — the sidecars are the source of truth,
//! so retention keeps working even if the GUI job history is lost.
//!
//! Safety rules:
//! - only sidecars carrying a matching `job_id` are ever considered — images
//!   from other jobs, CLI runs or older builds are never touched;
//! - chains are deleted whole (base + children); individual increments are
//!   never deleted from the middle of a chain;
//! - the chain containing the just-created image is always exempt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use own_backup_common::Result;

use crate::sidecar::{read_sidecar_header, ChainRole};

/// One image (VHDX/AVHDX/qcow2 + sidecar) inside a chain.
#[derive(Debug, Clone)]
pub struct ChainImage {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub role: ChainRole,
    /// Container format from the sidecar (`vhdx` / `qcow2`). A job's format
    /// must never change over existing archives (decision 2026-07-08) — the
    /// runners check every chain image against the job's format.
    pub container: String,
    /// RFC3339 creation timestamp from the manifest.
    pub created_at: String,
    pub parent_file_name: Option<String>,
    /// The stable identity of the volume this image came from, when the copy
    /// recorded one. See [`crate::sidecar::SidecarInfo::source_id`].
    pub source_id: Option<String>,
    /// Increments consolidation has already folded into this image (base only).
    /// The run planner adds it to the images it can see, so "a full version
    /// after every N versions" keeps counting versions that no longer exist as
    /// files — see [`crate::sidecar::SidecarInfo::consolidated_increments`].
    pub consolidated_increments: u32,
    /// When the chain this image belongs to was started (base only, `None`
    /// until the base has been consolidated at least once).
    pub chain_started_at: Option<String>,
}

/// A full backup with all images depending on it, oldest first.
#[derive(Debug, Clone)]
pub struct Chain {
    pub images: Vec<ChainImage>,
}

impl Chain {
    /// How many increments this chain has held over its life: the ones still
    /// on disk plus the ones consolidation folded into the base. This — not
    /// `images.len()` — is what "a full version after every N versions" counts,
    /// because a sliding window deletes files without the chain having got any
    /// younger.
    pub fn increments_ever(&self) -> u32 {
        let present = self.images.len().saturating_sub(1) as u32;
        present + self.images.first().map_or(0, |i| i.consolidated_increments)
    }

    /// When this chain was STARTED — the day its full backup was taken, even
    /// if consolidation has since moved that image forward in time.
    ///
    /// Falls back to the base's own `created_at`, which is exactly right for a
    /// chain that has never been consolidated (including every chain written
    /// before the stamp existed).
    pub fn started_at(&self) -> &str {
        match self.images.first() {
            Some(base) => base.chain_started_at.as_deref().unwrap_or(&base.created_at),
            None => "",
        }
    }

    /// Whole days between the chain's start and `now`; 0 if the stamp will not
    /// parse (a chain we cannot date is never treated as old — the age rule
    /// starts a NEW full backup, and doing that on unreadable metadata would
    /// be churn, not safety).
    pub fn age_days(&self, now: chrono::DateTime<chrono::Utc>) -> i64 {
        match chrono::DateTime::parse_from_rfc3339(self.started_at()) {
            Ok(started) => (now - started.with_timezone(&chrono::Utc)).num_days(),
            Err(_) => 0,
        }
    }

    /// Creation time of the chain's newest image (age-based retention keys on
    /// this: a chain still receiving increments is not "old").
    pub fn newest_created_at(&self) -> &str {
        self.images
            .iter()
            .map(|i| i.created_at.as_str())
            .max()
            .unwrap_or("")
    }

    fn oldest_created_at(&self) -> &str {
        self.images
            .iter()
            .map(|i| i.created_at.as_str())
            .min()
            .unwrap_or("")
    }

    pub fn contains(&self, image_path: &Path) -> bool {
        self.images.iter().any(|i| i.image_path == image_path)
    }
}

/// The chains of one job — see [`list_chains`].
pub fn list_job_chains(folder: &Path, job_id: &str) -> Result<Vec<Chain>> {
    list_chains(folder, Some(job_id))
}

/// Scan `folder` for sidecars and group them into chains, oldest chain first.
/// Unreadable sidecars are skipped with a warning. Children whose parent
/// sidecar is missing (broken chains) are grouped into their own chain so
/// retention can still clean them up.
///
/// `job_id` says whose images to look at, and the distinction matters because
/// one folder can hold both kinds: `Some(id)` takes a job's own images, `None`
/// takes the ones that belong to no job — what a copy made from the command
/// line writes. Mixing them would let a hand-made image become the parent of a
/// scheduled job's increment, or the other way round.
pub fn list_chains(folder: &Path, job_id: Option<&str>) -> Result<Vec<Chain>> {
    let mut entries: Vec<ChainImage> = Vec::new();

    for dir_entry in std::fs::read_dir(folder)? {
        let dir_entry = match dir_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = dir_entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };
        if ext != "vhdx" && ext != "avhdx" && ext != "qcow2" {
            continue;
        }

        // Try reading embedded sidecar first, fallback to external obkm sidecar
        let (info, manifest) = match crate::sidecar::read_embedded_sidecar_header(&path, &ext) {
            Ok(h) => h,
            Err(_) => {
                let sidecar_path = crate::sidecar::sidecar_path_for(&path);
                match read_sidecar_header(&sidecar_path) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "Skipping unreadable image (no embedded sidecar and external sidecar failed)"
                        );
                        continue;
                    }
                }
            }
        };

        if info.job_id.as_deref() != job_id {
            continue;
        }
        let sidecar_path = crate::sidecar::sidecar_path_for(&path);
        entries.push(ChainImage {
            image_path: path,
            sidecar_path,
            role: info.chain_role,
            container: info.container,
            created_at: manifest.created_at,
            parent_file_name: info.parent_image_file_name,
            source_id: info.source_id,
            consolidated_increments: info.consolidated_increments,
            chain_started_at: info.chain_started_at,
        });
    }

    // Group by walking parent links up to the chain root. Key = the root's
    // image file name (or the missing parent's name for broken chains).
    let by_name: HashMap<String, &ChainImage> = entries
        .iter()
        .filter_map(|e| {
            e.image_path
                .file_name()
                .map(|n| (n.to_string_lossy().to_lowercase(), e))
        })
        .collect();

    let mut groups: HashMap<String, Vec<ChainImage>> = HashMap::new();
    for entry in &entries {
        let mut current = entry;
        // Bounded walk to survive parent-link cycles in corrupted metadata.
        for _ in 0..entries.len() + 1 {
            match &current.parent_file_name {
                None => break,
                Some(parent) => match by_name.get(&parent.to_lowercase()) {
                    Some(parent_entry) => current = parent_entry,
                    None => break, // broken chain — group under the missing name
                },
            }
        }
        let key = match &current.parent_file_name {
            Some(missing) => format!("orphan:{}", missing.to_lowercase()),
            None => current
                .image_path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default(),
        };
        groups.entry(key).or_default().push(entry.clone());
    }

    let mut chains: Vec<Chain> = groups
        .into_values()
        .map(|mut images| {
            images.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            Chain { images }
        })
        .collect();
    chains.sort_by(|a, b| a.oldest_created_at().cmp(b.oldest_created_at()).then(
        a.newest_created_at().cmp(b.newest_created_at()),
    ));
    Ok(chains)
}

/// Retention rules from the job scheme.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetentionRules {
    /// Keep at most this many chains (oldest deleted first).
    pub keep_chains: Option<u32>,
    /// Delete chains whose newest image is older than this many days.
    pub cleanup_older_than_days: Option<u32>,
    /// Never delete the very first (oldest) chain.
    pub keep_first: bool,
}

/// Decide which chains to delete. Returns indices into `chains` (which must be
/// oldest-first, as produced by [`list_job_chains`]). `protect_image` is the
/// image just created — its chain is never deleted. `now` is passed in for
/// testability.
pub fn plan_retention(
    chains: &[Chain],
    rules: &RetentionRules,
    now: chrono::DateTime<chrono::Utc>,
    protect_image: &Path,
) -> Vec<usize> {
    let exempt = |idx: usize| -> bool {
        (rules.keep_first && idx == 0) || chains[idx].contains(protect_image)
    };

    let mut delete = vec![false; chains.len()];

    if let Some(days) = rules.cleanup_older_than_days {
        let cutoff = now - chrono::Duration::days(days as i64);
        for (idx, chain) in chains.iter().enumerate() {
            if exempt(idx) {
                continue;
            }
            if let Ok(newest) =
                chrono::DateTime::parse_from_rfc3339(chain.newest_created_at())
            {
                if newest.with_timezone(&chrono::Utc) < cutoff {
                    delete[idx] = true;
                }
            }
        }
    }

    if let Some(keep) = rules.keep_chains {
        let mut remaining = delete.iter().filter(|&&d| !d).count();
        for idx in 0..chains.len() {
            if remaining <= keep as usize {
                break;
            }
            if !delete[idx] && !exempt(idx) {
                delete[idx] = true;
                remaining -= 1;
            }
        }
    }

    delete
        .iter()
        .enumerate()
        .filter_map(|(idx, &d)| d.then_some(idx))
        .collect()
}

/// One step of the sliding increment window: fold `victim` into `base`, then
/// repoint `next` (the increment that used to sit on `victim`) at `base`.
///
/// `next` is `None` when the victim is the chain's leaf, which happens when
/// the window is shrunk in the job settings rather than overflowed by a new
/// run. The step is still valid: the base simply moves forward to the leaf's
/// point in time and the chain becomes a lone full backup.
#[derive(Debug, Clone)]
pub struct ConsolidationStep {
    pub base: ChainImage,
    pub victim: ChainImage,
    pub next: Option<ChainImage>,
}

/// Decide whether `chain` has more increments than `keep_increments` allows,
/// and if so name the oldest one to fold into the base.
///
/// Returns ONE step. The caller re-plans after each committed step, so a
/// window that shrank by several notches is closed one crash-safe step at a
/// time instead of one long operation with no safe point in the middle.
///
/// Refuses (returns `None`) anything it does not positively understand — the
/// operation rewrites a base that every other image depends on, so "not sure"
/// has to mean "don't":
/// - a root that is not a full backup, or a chain whose files disagree about
///   who their parent is (`created_at` order must match the parent links);
/// - **differential chains**. Their children all hang off the base as
///   siblings, each holding "everything that changed since the base". Folding
///   one in moves the base forward, and every remaining sibling then rebases
///   onto a point in time it was never computed against — silently wrong data,
///   not a broken link that fails loudly;
/// - a chain that mixes container formats.
pub fn plan_consolidation(chain: &Chain, keep_increments: u32) -> Option<ConsolidationStep> {
    let images = &chain.images;
    if images.len() < 2 {
        return None;
    }
    if images.len() - 1 <= keep_increments as usize {
        return None;
    }

    let base = &images[0];
    if base.role != ChainRole::Full || base.parent_file_name.is_some() {
        warn!(
            image = %base.image_path.display(),
            role = ?base.role,
            "Consolidation: chain root is not a full backup — skipping"
        );
        return None;
    }

    for (idx, image) in images.iter().enumerate().skip(1) {
        if image.role != ChainRole::Incremental {
            warn!(
                image = %image.image_path.display(),
                role = ?image.role,
                "Consolidation: only incremental chains can be consolidated — skipping"
            );
            return None;
        }
        if !image.container.eq_ignore_ascii_case(&base.container) {
            warn!(
                image = %image.image_path.display(),
                "Consolidation: chain mixes container formats — skipping"
            );
            return None;
        }
        // The chain must be a straight line in the same order the timestamps
        // put it: consolidation trusts `images[1]` to be the base's only child.
        let expected = images[idx - 1]
            .image_path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase());
        let actual = image.parent_file_name.as_ref().map(|n| n.to_lowercase());
        if expected.is_none() || expected != actual {
            warn!(
                image = %image.image_path.display(),
                parent = ?image.parent_file_name,
                "Consolidation: chain order does not match the parent links — skipping"
            );
            return None;
        }
    }

    Some(ConsolidationStep {
        base: base.clone(),
        victim: images[1].clone(),
        next: images.get(2).cloned(),
    })
}

/// What deleting a chain actually did.
///
/// A count was not enough. The old `delete_chain` returned one, logged
/// "deleted backup version" for every image whether or not it had gone, and
/// left the caller with no way to tell a clean sweep from a chain that is now
/// half there (audit AUD-05).
#[derive(Debug, Default, Clone)]
pub struct DeletionReport {
    /// Files that are gone now.
    pub deleted: Vec<PathBuf>,
    /// Images still on disk, each with the reason it is.
    pub kept: Vec<(PathBuf, String)>,
    /// Sidecars whose image was deleted but which could not be removed
    /// themselves. Not a danger to anybody's data — nothing depends on a
    /// sidecar — but they describe images that are gone, so say so.
    pub orphan_sidecars: Vec<PathBuf>,
}

impl DeletionReport {
    /// Did everything the chain consisted of actually go?
    pub fn complete(&self) -> bool {
        self.kept.is_empty() && self.orphan_sidecars.is_empty()
    }

    pub fn files_deleted(&self) -> u32 {
        self.deleted.len() as u32
    }
}

/// Delete every image + sidecar of a chain, children before parents.
///
/// Several `remove_file` calls do not add up to one transaction, and nothing
/// here pretends otherwise. What this does guarantee is the property that
/// matters when one of them fails: **no image left on disk ever loses a parent
/// it needs.** A differencing child without its base is not a smaller backup,
/// it is an unreadable one — and that is exactly what the old version produced,
/// because an error deleting a child only got logged and the parents went
/// anyway (audit AUD-05).
///
/// What changed:
/// * order comes from the parent links, not the timestamps — a differential
///   chain is a star, not a line, and a wrong `created_at` must not be able to
///   reorder a dependency;
/// * a failure propagates UP. If a child could not be deleted, its ancestors
///   are left alone; its siblings and their own descendants are not affected;
/// * the external sidecar goes AFTER its image, never before. The old order
///   could leave a data file with nothing describing it;
/// * the chain is refused outright while somebody is using it, or while an
///   interrupted consolidation still has a marker on it. Finishing that
///   recovery is a different operation's job, and deleting the files under it
///   would take away the only thing it can finish from.
pub fn delete_chain(chain: &Chain) -> Result<DeletionReport> {
    let images: Vec<&Path> = chain.images.iter().map(|i| i.image_path.as_path()).collect();
    crate::consolidate::ensure_idle(&images)?;
    if let Some(base) = chain.images.first() {
        let marker = crate::consolidate::marker_path(&base.image_path);
        if marker.exists() {
            return Err(own_backup_common::OwnBackupError::ChainBroken(format!(
                "цепочка {} не дошла до конца предыдущей консолидации ({}) — \
                 пока это не завершено, удалять её нельзя",
                base.image_path.display(),
                marker.display()
            )));
        }
    }

    let n = chain.images.len();
    let parent = parent_links(chain);
    let mut order: Vec<usize> = (0..n).collect();
    let depth = depths(&parent);
    order.sort_by_key(|&i| std::cmp::Reverse(depth[i]));

    let mut report = DeletionReport::default();
    let mut blocked = vec![false; n];

    for &i in &order {
        let image = &chain.images[i];
        if blocked[i] {
            report.kept.push((
                image.image_path.clone(),
                "оставлен: от него всё ещё зависит образ, который не удалось удалить".into(),
            ));
            block_parent(&parent, &mut blocked, i);
            continue;
        }

        // The image first. Until it is gone, its parents must stay.
        match std::fs::remove_file(&image.image_path) {
            Ok(()) => report.deleted.push(image.image_path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %image.image_path.display(), error = %e, "Failed to delete chain image");
                report.kept.push((image.image_path.clone(), e.to_string()));
                block_parent(&parent, &mut blocked, i);
                continue;
            }
        }

        // Only now the sidecar. Failing here leaves a description of something
        // that no longer exists — untidy, but it endangers no data, so the
        // parents may still go.
        match std::fs::remove_file(&image.sidecar_path) {
            Ok(()) => report.deleted.push(image.sidecar_path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %image.sidecar_path.display(), error = %e, "Failed to delete sidecar");
                report.orphan_sidecars.push(image.sidecar_path.clone());
            }
        }
        info!(image = %image.image_path.display(), "Retention: deleted backup version");
    }

    Ok(report)
}

/// Index of each image's parent inside the chain, by the file name its sidecar
/// names. `None` for the root and for a link that points outside the chain.
fn parent_links(chain: &Chain) -> Vec<Option<usize>> {
    let by_name: HashMap<String, usize> = chain
        .images
        .iter()
        .enumerate()
        .filter_map(|(i, img)| {
            img.image_path.file_name().map(|n| (n.to_string_lossy().to_lowercase(), i))
        })
        .collect();
    chain
        .images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            img.parent_file_name
                .as_ref()
                .and_then(|p| by_name.get(&p.to_lowercase()).copied())
                .filter(|&p| p != i) // an image naming itself is not its own parent
        })
        .collect()
}

/// How far each image is below the root, so children are always handled before
/// their parents. A sidecar set that somehow forms a cycle is given the
/// greatest depth rather than being followed forever.
fn depths(parent: &[Option<usize>]) -> Vec<usize> {
    let n = parent.len();
    (0..n)
        .map(|i| {
            let mut cur = i;
            let mut d = 0;
            while let Some(p) = parent[cur] {
                d += 1;
                cur = p;
                if d > n {
                    return usize::MAX; // a cycle: treat it as the deepest thing there is
                }
            }
            d
        })
        .collect()
}

fn block_parent(parent: &[Option<usize>], blocked: &mut [bool], i: usize) {
    if let Some(p) = parent[i] {
        blocked[p] = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{sidecar_path_for, write_sidecar, SidecarInfo};
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::image::ImageManifest;
    use own_backup_common::types::*;
    use uuid::Uuid;

    fn manifest(created_at: &str) -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::new_v4(),
            created_at: created_at.into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\E:".into(),
                model: String::new(),
                serial: String::new(),
                size_bytes: 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 512,
            total_blocks: 2,
            used_blocks: 2,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    /// Write a sidecar (and a dummy image file) into `dir`.
    fn put_image(
        dir: &Path,
        name: &str,
        role: ChainRole,
        parent: Option<&str>,
        job: Option<&str>,
        created_at: &str,
    ) {
        let image = dir.join(name);
        std::fs::write(&image, b"img").unwrap();
        let mut info = match parent {
            None => SidecarInfo::new_full(name, 0),
            Some(p) => SidecarInfo::new_child(name, role, p, 0),
        };
        info.job_id = job.map(|s| s.to_string());
        write_sidecar(&sidecar_path_for(&image), &info, &manifest(created_at), &[]).unwrap();
    }

    fn chain_names(chain: &Chain) -> Vec<String> {
        chain
            .images
            .iter()
            .map(|i| i.image_path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn groups_incremental_chain_to_its_full() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "a_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "a_inc1.avhdx", ChainRole::Incremental, Some("a_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        put_image(dir.path(), "a_inc2.avhdx", ChainRole::Incremental, Some("a_inc1.avhdx"), Some("j1"), "2026-01-03T00:00:00Z");
        put_image(dir.path(), "b_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-02-01T00:00:00Z");
        // Foreign job + no-job images must be invisible.
        put_image(dir.path(), "x_full.vhdx", ChainRole::Full, None, Some("other"), "2026-01-05T00:00:00Z");
        put_image(dir.path(), "y_full.vhdx", ChainRole::Full, None, None, "2026-01-06T00:00:00Z");

        let chains = list_job_chains(dir.path(), "j1").unwrap();
        assert_eq!(chains.len(), 2);
        assert_eq!(chain_names(&chains[0]), vec!["a_full.vhdx", "a_inc1.avhdx", "a_inc2.avhdx"]);
        assert_eq!(chain_names(&chains[1]), vec!["b_full.vhdx"]);
        assert_eq!(chains[0].newest_created_at(), "2026-01-03T00:00:00Z");
    }

    /// A copy made from the command line belongs to no job, and those images
    /// form chains of their own. Asked for `None`, the listing must see them
    /// and NOT see a job's — otherwise a hand-made image could become the
    /// parent of a scheduled job's increment.
    #[test]
    fn images_without_a_job_are_a_chain_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "cli_full.vhdx", ChainRole::Full, None, None, "2026-03-01T00:00:00Z");
        put_image(
            dir.path(),
            "cli_inc1.avhdx",
            ChainRole::Incremental,
            Some("cli_full.vhdx"),
            None,
            "2026-03-02T00:00:00Z",
        );
        put_image(dir.path(), "job_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-03-03T00:00:00Z");

        let chains = list_chains(dir.path(), None).unwrap();
        assert_eq!(chains.len(), 1, "the job's image is not ours");
        assert_eq!(chain_names(&chains[0]), vec!["cli_full.vhdx", "cli_inc1.avhdx"]);
        // And the job still sees only its own.
        let job = list_chains(dir.path(), Some("j1")).unwrap();
        assert_eq!(chain_names(&job[0]), vec!["job_full.vhdx"]);
    }

    #[test]
    fn chain_images_carry_their_container() {
        let dir = tempfile::tempdir().unwrap();
        // A qcow2 full: sidecar container says "qcow2".
        let image = dir.path().join("q_full.qcow2");
        std::fs::write(&image, b"img").unwrap();
        let mut info = SidecarInfo::new_full("q_full.qcow2", 0);
        info.container = "qcow2".into();
        info.job_id = Some("j1".into());
        write_sidecar(&sidecar_path_for(&image), &info, &manifest("2026-01-01T00:00:00Z"), &[])
            .unwrap();
        put_image(dir.path(), "v_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-02-01T00:00:00Z");

        let chains = list_job_chains(dir.path(), "j1").unwrap();
        let containers: Vec<&str> = chains
            .iter()
            .flat_map(|c| c.images.iter())
            .map(|i| i.container.as_str())
            .collect();
        assert!(containers.contains(&"qcow2") && containers.contains(&"vhdx"));
    }

    #[test]
    fn orphans_group_into_own_chain() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "lost_inc.avhdx", ChainRole::Incremental, Some("gone_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chain_names(&chains[0]), vec!["lost_inc.avhdx"]);
    }

    fn make_chains(stamps: &[&str]) -> Vec<Chain> {
        stamps
            .iter()
            .enumerate()
            .map(|(i, s)| Chain {
                images: vec![ChainImage {
                    image_path: PathBuf::from(format!("c{i}.vhdx")),
                    sidecar_path: PathBuf::from(format!("c{i}.vhdx.obkm")),
                    role: ChainRole::Full,
                    container: "vhdx".into(),
                    created_at: s.to_string(),
                    parent_file_name: None,
                    source_id: None,
                    consolidated_increments: 0,
                    chain_started_at: None,
                }],
            })
            .collect()
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn retention_keeps_last_n() {
        let chains = make_chains(&[
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
            "2026-04-01T00:00:00Z",
        ]);
        let rules = RetentionRules { keep_chains: Some(2), cleanup_older_than_days: None, keep_first: false };
        let del = plan_retention(&chains, &rules, now(), Path::new("c3.vhdx"));
        assert_eq!(del, vec![0, 1]);
    }

    #[test]
    fn retention_keep_first_exempts_oldest() {
        let chains = make_chains(&[
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
            "2026-04-01T00:00:00Z",
        ]);
        let rules = RetentionRules { keep_chains: Some(2), cleanup_older_than_days: None, keep_first: true };
        let del = plan_retention(&chains, &rules, now(), Path::new("c3.vhdx"));
        // First is exempt; c1 deleted to get down to 3... then stop? keep=2 →
        // remaining must be ≤ 2, but first + current are both exempt, so only
        // c1 and c2 are deletable; c2 is not exempt → both go.
        assert_eq!(del, vec![1, 2]);
    }

    #[test]
    fn retention_never_deletes_current_chain() {
        let chains = make_chains(&["2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z"]);
        let rules = RetentionRules { keep_chains: Some(1), cleanup_older_than_days: None, keep_first: false };
        // Current run went into the OLDEST chain (incremental) — it survives.
        let del = plan_retention(&chains, &rules, now(), Path::new("c0.vhdx"));
        assert_eq!(del, vec![1]);
    }

    #[test]
    fn retention_by_age_uses_newest_image() {
        let chains = make_chains(&[
            "2026-01-01T00:00:00Z", // ~180 days old → delete
            "2026-06-25T00:00:00Z", // fresh → keep
        ]);
        let rules = RetentionRules { keep_chains: None, cleanup_older_than_days: Some(30), keep_first: false };
        let del = plan_retention(&chains, &rules, now(), Path::new("c1.vhdx"));
        assert_eq!(del, vec![0]);
    }

    #[test]
    fn retention_deletes_files() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "old_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "old_inc.avhdx", ChainRole::Incremental, Some("old_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();
        let report = delete_chain(&chains[0]).unwrap();
        assert!(report.complete(), "nothing should have been left: {report:?}");
        assert_eq!(report.files_deleted(), 4);
        assert!(!dir.path().join("old_full.vhdx").exists());
        assert!(!dir.path().join("old_inc.avhdx").exists());
        assert!(!dir.path().join("old_full.vhdx.obkm").exists());
    }

    // --- Sliding increment window ---------------------------------------

    /// Build a chain of `n` increments on a full backup, oldest first.
    fn window_chain(dir: &Path, n: usize, role: ChainRole) -> Chain {
        put_image(dir, "w_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        for i in 1..=n {
            let parent = match role {
                ChainRole::Differential => "w_full.vhdx".to_string(),
                _ if i == 1 => "w_full.vhdx".to_string(),
                _ => format!("w_inc{}.avhdx", i - 1),
            };
            put_image(
                dir,
                &format!("w_inc{i}.avhdx"),
                role,
                Some(&parent),
                Some("j1"),
                &format!("2026-01-{:02}T00:00:00Z", i + 1),
            );
        }
        list_job_chains(dir, "j1").unwrap().remove(0)
    }

    #[test]
    fn window_within_limit_needs_no_consolidation() {
        let dir = tempfile::tempdir().unwrap();
        let chain = window_chain(dir.path(), 3, ChainRole::Incremental);
        assert!(plan_consolidation(&chain, 3).is_none(), "3 increments, limit 3");
    }

    #[test]
    fn window_overflow_folds_the_oldest_increment() {
        let dir = tempfile::tempdir().unwrap();
        let chain = window_chain(dir.path(), 4, ChainRole::Incremental);
        let step = plan_consolidation(&chain, 3).expect("4 increments, limit 3");
        assert_eq!(step.base.image_path.file_name().unwrap(), "w_full.vhdx");
        assert_eq!(step.victim.image_path.file_name().unwrap(), "w_inc1.avhdx");
        assert_eq!(
            step.next.as_ref().unwrap().image_path.file_name().unwrap(),
            "w_inc2.avhdx",
            "inc2 is the one that has to be repointed at the base"
        );
    }

    /// Shrinking the window in the job settings leaves several increments to
    /// fold. Each call plans ONE step, so every merge commits on its own.
    #[test]
    fn window_shrunk_to_zero_folds_one_step_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let chain = window_chain(dir.path(), 2, ChainRole::Incremental);
        let step = plan_consolidation(&chain, 0).expect("2 increments, limit 0");
        assert_eq!(step.victim.image_path.file_name().unwrap(), "w_inc1.avhdx");
        assert!(step.next.is_some());

        // After that step the chain is full + inc2; the leaf then has no next.
        std::fs::remove_file(dir.path().join("w_inc1.avhdx")).unwrap();
        std::fs::remove_file(sidecar_path_for(&dir.path().join("w_inc1.avhdx"))).unwrap();
        put_image(dir.path(), "w_inc2.avhdx", ChainRole::Incremental, Some("w_full.vhdx"), Some("j1"), "2026-01-03T00:00:00Z");
        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        let step = plan_consolidation(&chain, 0).expect("1 increment, limit 0");
        assert_eq!(step.victim.image_path.file_name().unwrap(), "w_inc2.avhdx");
        assert!(step.next.is_none(), "the leaf has nothing to repoint after it");
    }

    /// Differential siblings each hold "everything since the base". Moving the
    /// base forward would silently rebase the remaining ones onto a point they
    /// were never computed against — refuse, loudly and early.
    #[test]
    fn differential_chains_are_never_consolidated() {
        let dir = tempfile::tempdir().unwrap();
        let chain = window_chain(dir.path(), 4, ChainRole::Differential);
        assert!(plan_consolidation(&chain, 1).is_none());
    }

    #[test]
    fn broken_parent_links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "b_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "b_inc1.avhdx", ChainRole::Incremental, Some("b_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        // inc2 claims the FULL as its parent while sitting after inc1 in time:
        // the timestamp order and the links disagree.
        put_image(dir.path(), "b_inc2.avhdx", ChainRole::Incremental, Some("b_full.vhdx"), Some("j1"), "2026-01-03T00:00:00Z");
        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        assert_eq!(chain.images.len(), 3);
        assert!(plan_consolidation(&chain, 1).is_none());
    }

    #[test]
    fn mixed_container_formats_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "m_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "m_inc1.avhdx", ChainRole::Incremental, Some("m_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        let image = dir.path().join("m_inc2.qcow2");
        std::fs::write(&image, b"img").unwrap();
        let mut info = SidecarInfo::new_child("m_inc2.qcow2", ChainRole::Incremental, "m_inc1.avhdx", 0);
        info.container = "qcow2".into();
        info.job_id = Some("j1".into());
        write_sidecar(&sidecar_path_for(&image), &info, &manifest("2026-01-03T00:00:00Z"), &[]).unwrap();

        let chain = list_job_chains(dir.path(), "j1").unwrap().remove(0);
        assert!(plan_consolidation(&chain, 1).is_none());
    }

    // --- Удаление цепочки: что остаётся, когда файл не удалился (AUD-05) ----
    //
    // Несколько remove_file не складываются в одну транзакцию, и обещать этого
    // нельзя. Обещать нужно другое: ни один оставшийся на диске образ не должен
    // потерять родителя, без которого он не читается. Разностный ребёнок без
    // базы — это не копия поменьше, это копия, которую не открыть.

    /// Модель отказа из аудита: на месте дочернего файла каталог, поэтому
    /// remove_file по нему не проходит. Раньше ошибка просто попадала в журнал,
    /// база уезжала следом, и на диске оставался файл без зависимостей — да
    /// ещё и со строкой «deleted backup version» про него.
    #[test]
    fn a_child_that_cannot_be_deleted_keeps_its_base() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "b_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "b_inc.avhdx", ChainRole::Incremental, Some("b_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();

        // Делаем ребёнка неудаляемым: каталог вместо файла.
        let leaf = dir.path().join("b_inc.avhdx");
        std::fs::remove_file(&leaf).unwrap();
        std::fs::create_dir(&leaf).unwrap();

        let report = delete_chain(&chains[0]).unwrap();
        assert!(!report.complete(), "отчёт обязан сказать, что цепочка осталась");
        assert!(
            dir.path().join("b_full.vhdx").exists(),
            "база должна остаться: от неё всё ещё зависит ребёнок"
        );
        assert!(
            dir.path().join("b_inc.avhdx.obkm").exists(),
            "sidecar ребёнка тоже остаётся — образ-то на месте"
        );
        let kept: Vec<_> = report.kept.iter().map(|(p, _)| p.clone()).collect();
        assert!(kept.contains(&dir.path().join("b_full.vhdx")), "{report:?}");
        assert!(kept.contains(&leaf), "{report:?}");
    }

    /// Разностная цепочка — это звезда, а не линия: дети висят на базе
    /// сёстрами. Застрявший ребёнок держит базу, но не должен мешать другим
    /// детям уйти.
    #[test]
    fn a_stuck_child_does_not_hold_its_siblings() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "c_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "c_d1.avhdx", ChainRole::Differential, Some("c_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        put_image(dir.path(), "c_d2.avhdx", ChainRole::Differential, Some("c_full.vhdx"), Some("j1"), "2026-01-03T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();

        let stuck = dir.path().join("c_d1.avhdx");
        std::fs::remove_file(&stuck).unwrap();
        std::fs::create_dir(&stuck).unwrap();

        let report = delete_chain(&chains[0]).unwrap();
        assert!(!dir.path().join("c_d2.avhdx").exists(), "сестра не виновата и должна уйти");
        assert!(dir.path().join("c_full.vhdx").exists(), "база держится застрявшим ребёнком");
        assert!(!report.complete());
    }

    /// Sidecar удаляется ПОСЛЕ своего образа. В прежнем порядке отказ на
    /// образе оставлял файл данных, про который уже ничего не написано.
    #[test]
    fn the_sidecar_never_goes_before_its_image() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "d_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();

        let base = dir.path().join("d_full.vhdx");
        std::fs::remove_file(&base).unwrap();
        std::fs::create_dir(&base).unwrap();

        let report = delete_chain(&chains[0]).unwrap();
        assert!(
            dir.path().join("d_full.vhdx.obkm").exists(),
            "образ не удалился — его описание обязано остаться при нём"
        );
        assert!(report.deleted.is_empty(), "{report:?}");
    }

    /// Встроенный sidecar — обычное дело у qcow2: отдельного .obkm рядом нет.
    /// Его отсутствие не ошибка.
    #[test]
    fn a_missing_sidecar_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "e_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();
        std::fs::remove_file(dir.path().join("e_full.vhdx.obkm")).unwrap();

        let report = delete_chain(&chains[0]).unwrap();
        assert!(report.complete(), "{report:?}");
        assert_eq!(report.files_deleted(), 1);
    }

    /// Незавершённая консолидация — единственное, что может её закончить, это
    /// оставшиеся файлы. Удалять цепочку из-под неё нельзя.
    #[test]
    fn a_chain_with_an_unfinished_consolidation_is_refused_whole() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "f_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-01-01T00:00:00Z");
        put_image(dir.path(), "f_inc.avhdx", ChainRole::Incremental, Some("f_full.vhdx"), Some("j1"), "2026-01-02T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();

        let marker = crate::consolidate::marker_path(&dir.path().join("f_full.vhdx"));
        std::fs::write(&marker, b"{}").unwrap();

        assert!(delete_chain(&chains[0]).is_err());
        assert!(dir.path().join("f_full.vhdx").exists(), "ни один файл не должен быть тронут");
        assert!(dir.path().join("f_inc.avhdx").exists());
    }

    /// Порядок берётся из родительских ссылок, а не из дат: подделанная дата
    /// не должна переставить зависимость. Здесь ребёнок «старше» базы.
    #[test]
    fn the_order_comes_from_the_parent_links_not_the_dates() {
        let dir = tempfile::tempdir().unwrap();
        put_image(dir.path(), "g_full.vhdx", ChainRole::Full, None, Some("j1"), "2026-06-01T00:00:00Z");
        put_image(dir.path(), "g_inc.avhdx", ChainRole::Incremental, Some("g_full.vhdx"), Some("j1"), "2026-01-01T00:00:00Z");
        let chains = list_job_chains(dir.path(), "j1").unwrap();

        let stuck = dir.path().join("g_inc.avhdx");
        std::fs::remove_file(&stuck).unwrap();
        std::fs::create_dir(&stuck).unwrap();

        let report = delete_chain(&chains[0]).unwrap();
        assert!(
            dir.path().join("g_full.vhdx").exists(),
            "ребёнок помечен более ранней датой, но зависит от базы — база остаётся"
        );
        assert!(!report.complete());
    }
}
