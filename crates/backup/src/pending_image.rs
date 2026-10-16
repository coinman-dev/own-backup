//! An unfinished VHDX or qcow2 stays outside the restore-point namespace.
//!
//! Until 2026-10-16 these two containers were written under their final name.
//! From the end of the data copy the image carried a readable provisional
//! sidecar, while the exclusion pass was still deleting and merging inside it —
//! minutes on a big volume. A service killed in that window left a half-merged
//! image that the next run listed as the newest version and parented its
//! increment on. Killed anywhere else, it left a file the strict retention scan
//! could not read, and retention for that job stopped for good, saying so only
//! in the log. The encrypted container already wrote privately
//! (`seal::obk::new_image`); this is the same rule for the other two.
//!
//! The private name keeps the container's extension: the Windows virtual-disk
//! API the exclusion pass opens the image with refuses any other. Everything a
//! run creates beside its image (the exclusion pass's scratch children) starts
//! with the same private name, so a leftover of either is never a version.
//!
//! What a killed run leaves is kept, not reclaimed: a plain image has no key to
//! prove whose it is, and a stranger's file is not deleted by guesswork.
use std::path::Path;

use own_backup_common::{OwnBackupError, Result};
use tempfile::TempPath;

const PREFIX: &str = ".own-backup-new-";
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // the Windows engine writes them
const EXTENSIONS: [&str; 3] = ["vhdx", "avhdx", "qcow2"];

/// A name a run writes privately: the image itself or anything beside it.
pub fn is_pending_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once(".tmp."))
        .is_some_and(|(id, _)| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// [`is_pending_name`] for a path.
pub fn is_pending_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_pending_name)
}

/// The private name of one new image, deleted with this value unless published.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct PendingImage {
    path: TempPath,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl PendingImage {
    /// A name beside `target` that does not exist yet: the writers create the
    /// file themselves, and `CreateVirtualDisk` refuses one that is there.
    pub(crate) fn new(target: &Path) -> Result<Self> {
        let ext = target
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|e| EXTENSIONS.contains(&e.as_str()))
            .ok_or_else(|| {
                OwnBackupError::Config(format!(
                    "not a VHDX or qcow2 image name: {}",
                    target.display()
                ))
            })?;
        let name = format!("{PREFIX}{}.tmp.{ext}", uuid::Uuid::new_v4().simple());
        Ok(Self {
            path: TempPath::try_from_path(target.with_file_name(name))?,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The last step of a run: the finished image takes its public name, which
    /// must not exist — a version already there is never replaced.
    pub(crate) fn publish_as(self, target: &Path) -> Result<()> {
        self.path
            .persist_noclobber(target)
            .map_err(|e| OwnBackupError::Io(e.error))?;
        #[cfg(unix)]
        if let Some(folder) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::File::open(folder)?.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_and_its_scratch_files_are_private_names() {
        let root = tempfile::tempdir().unwrap();
        let pending = PendingImage::new(&root.path().join("job_C_inc_20261016-120000.avhdx")).unwrap();
        let name = pending.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.ends_with(".tmp.avhdx"), "{name}");
        assert!(is_pending_name(&name));
        assert!(is_pending_name(&format!("{name}.excl.avhdx")));
        assert!(is_pending_name(&format!("{name}.excl.scratch")));
        assert!(!is_pending_name("job_C_full_20261016-120000.vhdx"));
        assert!(!is_pending_name(".own-backup-new-notanid.tmp.vhdx"));
        assert!(PendingImage::new(&root.path().join("x.img")).is_err());
    }

    #[test]
    fn an_unpublished_image_is_removed_and_a_published_one_never_replaces() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("job_C_full_20261016-120000.vhdx");

        let dropped = PendingImage::new(&target).unwrap();
        std::fs::write(dropped.path(), b"half written").unwrap();
        let dropped_path = dropped.path().to_path_buf();
        drop(dropped);
        assert!(!dropped_path.exists());

        std::fs::write(&target, b"existing version").unwrap();
        let clash = PendingImage::new(&target).unwrap();
        std::fs::write(clash.path(), b"new").unwrap();
        assert!(clash.publish_as(&target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"existing version");

        let other = root.path().join("job_C_full_20261016-120001.vhdx");
        let fine = PendingImage::new(&other).unwrap();
        std::fs::write(fine.path(), b"finished").unwrap();
        fine.publish_as(&other).unwrap();
        assert_eq!(std::fs::read(&other).unwrap(), b"finished");
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| is_pending_name(n))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
