//! An unfinished backup stays outside the restore-point namespace.
use std::fs::File;
use std::path::{Path, PathBuf};

use own_backup_common::Result;
use tempfile::{NamedTempFile, TempPath};

use crate::seal::context::ArchiveKeys;

pub(crate) struct NewImage {
    _owner: NamedTempFile,
    file: File,
    stage: TempPath,
    target: PathBuf,
    writer_path: PathBuf,
}

/// Private producer names are not restore points, even after their container
/// headers are complete but exclusion/final metadata work is still running.
pub fn is_pending_name(name: &str) -> bool {
    name.strip_prefix(".own-backup-obk-")
        .and_then(|name| name.strip_suffix(".tmp.obk"))
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

impl NewImage {
    pub(crate) fn create(target: &Path, keys: &ArchiveKeys) -> Result<Self> {
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let requested_parent = parent.to_path_buf();
        let parent = std::fs::canonicalize(parent)?;
        let target = parent.join(
            target
                .file_name()
                .ok_or_else(|| super::bad("new image has no name"))?,
        );
        if target.exists() {
            return Err(super::bad("new image target already exists"));
        }
        let key = keys.new_image_owner_key();
        super::staging::cleanup_new_images(&parent, &key)?;
        let (owner, stage) = super::staging::create_new_image(&target, &key)?;
        let (file, stage) = stage.into_parts();
        // Preserve the caller's path spelling for relative parent references.
        // Windows canonicalization adds a \\?\ prefix; comparing it against
        // an ordinary drive path would otherwise make the backing link absolute.
        let writer_path = requested_parent.join(stage.file_name().expect("stage basename"));
        Ok(Self {
            _owner: owner,
            file,
            stage,
            target,
            writer_path,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.writer_path
    }

    pub(crate) fn writer_file(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }

    pub(crate) fn publish(self) -> Result<()> {
        let Self {
            _owner,
            file,
            stage,
            target,
            writer_path: _,
        } = self;
        file.sync_all()?;
        drop(file);
        stage
            .persist_noclobber(&target)
            .map_err(|error| own_backup_common::OwnBackupError::Io(error.error))?;
        super::transaction::sync_parent(target.parent().expect("canonical parent"))?;
        // Keep the authenticated owner locked until publication is complete.
        drop(_owner);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::container_tests::Fixture;
    use crate::seal::obk::{ObkImage, ObkWriter};
    use crate::sidecar::ChainRole;
    use std::io::{BufWriter, Write};
    use uuid::Uuid;

    fn keys(fixture: &Fixture) -> ArchiveKeys {
        let options = fixture.options(Uuid::new_v4(), ChainRole::Full, None);
        ArchiveKeys::from_password(options.key_file, "пароль архива").unwrap()
    }

    fn abandoned(pending: NewImage) -> (PathBuf, PathBuf) {
        let NewImage {
            _owner,
            file,
            stage,
            ..
        } = pending;
        drop(file);
        let stage = stage.keep().unwrap();
        let (file, owner) = _owner.keep().unwrap();
        drop(file);
        (owner, stage)
    }

    #[test]
    fn completed_private_image_is_not_a_restore_point_until_publication() {
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new();
        let keys = keys(&fixture);
        let target = root.path().join("new.obk");
        let pending = NewImage::create(&target, &keys).unwrap();
        assert!(is_pending_name(
            pending.path().file_name().unwrap().to_str().unwrap()
        ));
        let mut writer = ObkWriter::new(
            BufWriter::new(pending.writer_file().unwrap()),
            &fixture.chain,
            fixture.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        writer
            .write_cluster(
                0,
                &vec![0x37; 1 << crate::seal::obk::container_tests::cluster_bits()],
            )
            .unwrap();
        writer
            .finish(b"complete metadata")
            .unwrap()
            .into_inner()
            .unwrap()
            .sync_all()
            .unwrap();
        assert!(!target.exists());
        assert!(crate::seal::obk::read_listing(pending.path()).is_err());
        assert!(
            crate::chains::list_job_chains_strict(root.path(), "job-under-test")
                .unwrap()
                .is_empty()
        );
        pending.publish().unwrap();
        let mut image = ObkImage::open_path(&target, &fixture.chain).unwrap();
        assert_eq!(&image.sealed_metadata().unwrap()[..], b"complete metadata");
        assert_eq!(
            crate::chains::list_job_chains_strict(root.path(), "job-under-test")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn next_filename_and_reopened_keys_reclaim_only_this_jobs_dead_proved_output() {
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new();
        let keys_a = keys(&fixture);
        let first = NewImage::create(&root.path().join("first.obk"), &keys_a).unwrap();
        first
            .writer_file()
            .unwrap()
            .write_all(b"interrupted ciphertext fixture")
            .unwrap();
        let (owner, stage) = abandoned(first);
        let foreign = root
            .path()
            .join(".own-backup-obk-11111111111111111111111111111111.tmp.obk");
        std::fs::write(&foreign, b"unregistered foreign content").unwrap();
        let other = Fixture::new();
        let live_other = NewImage::create(&root.path().join("other.obk"), &keys(&other)).unwrap();
        assert!(owner.exists() && stage.exists());
        let reopened = keys(&fixture);
        let next = NewImage::create(&root.path().join("next.obk"), &reopened).unwrap();
        assert!(!owner.exists() && !stage.exists());
        assert!(foreign.exists() && live_other.path().exists());
        drop(next);
    }

    #[test]
    fn a_live_writer_and_a_replaced_stage_are_never_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new();
        let keys = keys(&fixture);
        let live = NewImage::create(&root.path().join("live.obk"), &keys).unwrap();
        let second = NewImage::create(&root.path().join("second.obk"), &keys).unwrap();
        assert!(live.path().exists());
        let (owner, stage) = abandoned(second);
        // Keep the original object alive under a different name so a new inode
        // cannot accidentally reuse its identity in this test.
        std::fs::rename(&stage, stage.with_extension("original")).unwrap();
        std::fs::write(&stage, b"replacement owned by someone else").unwrap();
        let _third = NewImage::create(&root.path().join("third.obk"), &keys).unwrap();
        assert!(owner.exists() && live.path().exists());
        assert_eq!(
            std::fs::read(stage).unwrap(),
            b"replacement owned by someone else"
        );
    }

    #[test]
    fn publication_never_overwrites_a_file_that_appeared_during_the_backup() {
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new();
        let target = root.path().join("new.obk");
        let pending = NewImage::create(&target, &keys(&fixture)).unwrap();
        pending
            .writer_file()
            .unwrap()
            .write_all(b"completed candidate")
            .unwrap();
        std::fs::write(&target, b"foreign file").unwrap();
        assert!(pending.publish().is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"foreign file");
    }
}
