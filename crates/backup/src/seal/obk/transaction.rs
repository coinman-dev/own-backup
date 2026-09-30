//! Stage edits to an existing image beside it and publish them with one rename.
//!
//! A crash during a write to the stage leaves the original image intact. The
//! stage is on the same filesystem, and its bytes are synced before the rename.
//! Readers of existing `.obk` files need no format change.

use std::fs::OpenOptions;
use std::path::Path;

#[cfg(unix)]
use std::fs::File;

use own_backup_common::Result;

use super::bad;
use super::reader::ObkImage;
use crate::seal::ChainKey;

pub(super) fn replace<T>(
    path: &Path,
    chain: &ChainKey,
    edit: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    // Resolve a symlink once, so publication replaces the image it points to
    // instead of replacing the symlink itself.
    let target = std::fs::canonicalize(path)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::Builder::new()
        .prefix(".own-backup-obk-")
        .suffix(".tmp")
        .tempfile_in(parent)?
        .into_temp_path();

    // `copy` keeps the source file's permissions. It also lets the OS use a
    // filesystem copy primitive when one is available; the stage may still
    // need as much free space as the complete image.
    std::fs::copy(&target, &stage)?;
    let value = edit(&stage)?;
    ObkImage::open_path(&stage, chain)?;
    // Windows FlushFileBuffers requires a handle opened for writing.
    OpenOptions::new().write(true).open(&stage)?.sync_all()?;
    stage
        .persist(&target)
        .map_err(|e| bad(format!("cannot publish {}: {}", target.display(), e.error)))?;
    sync_parent(parent)?;
    Ok(value)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    use crate::seal::obk::container_tests::{cluster_bits, Fixture};
    use crate::seal::obk::{ObkChain, ObkWriter};
    use crate::sidecar::ChainRole;
    use uuid::Uuid;

    #[test]
    fn a_torn_staged_header_cannot_replace_a_previous_restore_point() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("full.obk");
        let base_id = Uuid::new_v4();
        let mut writer =
            ObkWriter::create(&base, &f.chain, f.options(base_id, ChainRole::Full, None)).unwrap();
        writer
            .write_cluster(0, &vec![0x42; 1 << cluster_bits()])
            .unwrap();
        writer.finish(b"original point").unwrap();

        let leaf = dir.path().join("inc.obk");
        let mut writer = ObkWriter::create(
            &leaf,
            &f.chain,
            f.options(
                Uuid::new_v4(),
                ChainRole::Incremental,
                Some((base_id, "full.obk")),
            ),
        )
        .unwrap();
        writer
            .write_cluster(1, &vec![0x57; 1 << cluster_bits()])
            .unwrap();
        writer.finish(b"child point").unwrap();

        let original = std::fs::read(&base).unwrap();
        for written in [1usize, 16, 128, 512, 2048, (1 << cluster_bits()) - 1] {
            // Simulate an interrupted header write on the stage. Even if the
            // edit reports success, validation must refuse to publish it.
            assert!(replace(&base, &f.chain, |stage| {
                let mut file = std::fs::OpenOptions::new().write(true).open(stage)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&vec![0; written])?;
                Ok(())
            })
            .is_err());
            assert_eq!(std::fs::read(&base).unwrap(), original);
            let mut chain = ObkChain::open(&leaf, &f.chain).unwrap();
            assert_eq!(
                &chain.read_cluster(0).unwrap()[..],
                &vec![0x42; 1 << cluster_bits()]
            );
            assert_eq!(
                &chain.read_cluster(1).unwrap()[..],
                &vec![0x57; 1 << cluster_bits()]
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn process_exit_during_staged_write_leaves_the_original_image() {
        const CHILD_PATH: &str = "OWN_BACKUP_OBK_STAGE_EXIT_TEST";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = Path::new(&path);
            let unused_key = ChainKey::generate().unwrap();
            let _: Result<()> = replace(path, &unused_key, |stage| {
                let mut file = std::fs::OpenOptions::new().write(true).open(stage)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&[0])?;
                file.sync_all()?;
                std::process::exit(23);
            });
            unreachable!();
        }

        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.obk");
        let mut writer = ObkWriter::create(
            &path,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        writer
            .write_cluster(0, &vec![0x62; 1 << cluster_bits()])
            .unwrap();
        writer.finish(b"previous point").unwrap();
        let original = std::fs::read(&path).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("seal::obk::transaction::tests::process_exit_during_staged_write_leaves_the_original_image")
            .env(CHILD_PATH, &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(23));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut chain = ObkChain::open(&path, &f.chain).unwrap();
        assert_eq!(
            &chain.read_cluster(0).unwrap()[..],
            &vec![0x62; 1 << cluster_bits()]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vm_power_cut_during_staged_write_recovers_the_chain() {
        use crate::seal::obk::read_open_header;

        let Ok(root) = std::env::var("OWN_BACKUP_H03_POWER_CUT_DIR") else {
            return;
        };
        let root = Path::new(&root);
        let base = root.join("full.obk");
        let leaf = root.join("inc.obk");
        match std::env::var("OWN_BACKUP_H03_POWER_CUT_PHASE").as_deref() {
            Ok("stage") => {
                std::fs::create_dir_all(root).unwrap();
                let f = Fixture::new();
                let base_id = Uuid::new_v4();
                let mut writer =
                    ObkWriter::create(&base, &f.chain, f.options(base_id, ChainRole::Full, None))
                        .unwrap();
                writer
                    .write_cluster(0, &vec![0x11; 1 << cluster_bits()])
                    .unwrap();
                writer
                    .finish(b"previous base")
                    .unwrap()
                    .into_inner()
                    .unwrap()
                    .sync_all()
                    .unwrap();

                let mut writer = ObkWriter::create(
                    &leaf,
                    &f.chain,
                    f.options(
                        Uuid::new_v4(),
                        ChainRole::Incremental,
                        Some((base_id, "full.obk")),
                    ),
                )
                .unwrap();
                writer
                    .write_cluster(1, &vec![0x22; 1 << cluster_bits()])
                    .unwrap();
                writer
                    .finish(b"previous child")
                    .unwrap()
                    .into_inner()
                    .unwrap()
                    .sync_all()
                    .unwrap();

                let _: Result<()> = replace(&base, &f.chain, |stage| {
                    let mut file = OpenOptions::new().write(true).open(stage)?;
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&[0; 512])?;
                    file.sync_all()?;
                    let mut marker = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(root.join("stage-ready"))?;
                    marker.write_all(b"partial header synced")?;
                    marker.sync_all()?;
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                });
                unreachable!();
            }
            Ok("verify") => {
                let open = read_open_header(&leaf).unwrap();
                let key = open.chain_key_with_password("пароль архива").unwrap();
                let mut chain = ObkChain::open(&leaf, &key).unwrap();
                assert_eq!(chain.chain_len(), 2);
                assert_eq!(
                    &chain.read_cluster(0).unwrap()[..],
                    &vec![0x11; 1 << cluster_bits()]
                );
                assert_eq!(
                    &chain.read_cluster(1).unwrap()[..],
                    &vec![0x22; 1 << cluster_bits()]
                );
            }
            other => panic!("unknown H03 power-cut phase: {other:?}"),
        }
    }
}
