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
    copy_security(&target, &stage)?;
    let value = edit(&stage)?;
    ObkImage::open_path(&stage, chain)?;
    // Windows FlushFileBuffers requires a handle opened for writing.
    OpenOptions::new().write(true).open(&stage)?.sync_all()?;
    publish(stage, &target)?;
    sync_parent(parent)?;
    Ok(value)
}

#[cfg(not(target_os = "windows"))]
fn copy_security(_source: &Path, _stage: &Path) -> Result<()> {
    Ok(()) // std::fs::copy preserves Unix mode bits
}

#[cfg(target_os = "windows")]
fn copy_security(source: &Path, stage: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorGroup, GetSecurityDescriptorOwner, DACL_SECURITY_INFORMATION,
        GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let stage: Vec<u16> = stage.as_os_str().encode_wide().chain(Some(0)).collect();
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0;
    // Request the self-relative descriptor size. DWORD storage below keeps the
    // descriptor aligned; source/archive access failures abort publication.
    unsafe {
        GetFileSecurityW(
            PCWSTR(source.as_ptr()),
            info.0,
            PSECURITY_DESCRIPTOR(std::ptr::null_mut()),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(bad(format!(
            "cannot read image security: {}",
            windows::core::Error::from_win32()
        )));
    }
    let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
    let descriptor = PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast());
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: the aligned buffer is large enough, names are terminated, and
    // the descriptor remains alive through synchronous query/set operations.
    unsafe {
        GetFileSecurityW(
            PCWSTR(source.as_ptr()),
            info.0,
            descriptor,
            needed,
            &mut needed,
        )
        .ok()
        .map_err(|e| bad(format!("cannot read image security: {e}")))?;
        GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)
            .map_err(|e| bad(format!("cannot read image DACL control: {e}")))?;
        let protection = if control & SE_DACL_PROTECTED.0 != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        let mut owner = PSID::default();
        let mut group = PSID::default();
        let mut defaulted = BOOL::default();
        let mut present = BOOL::default();
        let mut dacl = std::ptr::null_mut();
        GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted)
            .map_err(|e| bad(format!("cannot read image owner: {e}")))?;
        GetSecurityDescriptorGroup(descriptor, &mut group, &mut defaulted)
            .map_err(|e| bad(format!("cannot read image group: {e}")))?;
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            .map_err(|e| bad(format!("cannot read image DACL: {e}")))?;
        let result = SetNamedSecurityInfoW(
            PCWSTR(stage.as_ptr()),
            SE_FILE_OBJECT,
            info | protection,
            owner,
            group,
            Some(dacl),
            None,
        );
        if result.0 != 0 {
            return Err(bad(format!(
                "cannot preserve image security: Windows error {}",
                result.0
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn publish(stage: tempfile::TempPath, target: &Path) -> Result<()> {
    stage
        .persist(target)
        .map_err(|e| bad(format!("cannot publish {}: {}", target.display(), e.error)))
}

#[cfg(target_os = "windows")]
fn publish(stage: tempfile::TempPath, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, SetFileAttributesW, FILE_ATTRIBUTE_NORMAL, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = stage.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        // Match tempfile's persist preparation: temporary file attributes do
        // not provide the same consistency guarantee as a normal file.
        SetFileAttributesW(PCWSTR(source.as_ptr()), FILE_ATTRIBUTE_NORMAL)
            .map_err(|e| bad(format!("cannot prepare {}: {e}", stage.display())))?;
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| bad(format!("cannot publish {}: {e}", target.display())))?;
    }
    // The source path was moved; TempPath's cleanup of that name is harmless.
    Ok(())
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
                file.seek(SeekFrom::Start(super::super::SECOND_HEADER_OFFSET))?;
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

    #[test]
    fn measure_staged_metadata_update_on_a_large_image() {
        let Ok(root) = std::env::var("OWN_BACKUP_H03_LARGE_DIR") else {
            return;
        };
        let root = Path::new(&root);
        std::fs::create_dir_all(root).unwrap();
        let path = root.join("large.obk");
        let mib: usize = std::env::var("OWN_BACKUP_H03_LARGE_MIB")
            .unwrap_or_else(|_| "512".into())
            .parse()
            .unwrap();
        assert!((64..=1024).contains(&mib) && mib.is_multiple_of(64));
        let f = Fixture::new();
        let mut opts = f.options(Uuid::new_v4(), ChainRole::Full, None);
        opts.cluster_bits = 16;
        opts.virtual_size = (mib as u64) * 1024 * 1024;
        let mut writer = ObkWriter::create(&path, &f.chain, opts).unwrap();
        let mut state = 0x8293_1045_aa67_34d1u64;
        let clusters = mib * 16;
        for index in 0..clusters {
            let mut data = vec![0u8; 1 << 16];
            for word in data.chunks_exact_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                word.copy_from_slice(&state.to_le_bytes());
            }
            writer.write_cluster(index as u64, &data).unwrap();
        }
        writer
            .finish_with_reserve(b"before", 4096)
            .unwrap()
            .into_inner()
            .unwrap()
            .sync_all()
            .unwrap();

        let bytes = std::fs::metadata(&path).unwrap().len();
        let started = std::time::Instant::now();
        super::super::metadata::update_sealed_metadata(&path, &f.chain, b"after").unwrap();
        let elapsed_ms = started.elapsed().as_millis();
        let growth = std::fs::metadata(&path).unwrap().len() - bytes;
        let mut reader = crate::seal::obk::ObkRandomReader::open(&path, &f.chain).unwrap();
        assert_eq!(&reader.sealed_metadata().unwrap()[..], b"after");
        println!(
            "H03-LARGE image_bytes={bytes} metadata_update_ms={elapsed_ms} growth_bytes={growth}"
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

    #[cfg(target_os = "windows")]
    #[test]
    fn vm_power_cut_during_second_header_write_recovers_old_point() {
        use crate::seal::obk::read_open_header;

        let Ok(root) = std::env::var("OWN_BACKUP_H03_V2_POWER_CUT_DIR") else {
            return;
        };
        let root = Path::new(&root);
        let base = root.join("full.obk");
        let leaf = root.join("inc.obk");
        match std::env::var("OWN_BACKUP_H03_V2_POWER_CUT_PHASE").as_deref() {
            Ok("tear") => {
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

                let mut file = OpenOptions::new().write(true).open(&base).unwrap();
                file.seek(SeekFrom::Start(super::super::SECOND_HEADER_OFFSET))
                    .unwrap();
                file.write_all(&[0; 512]).unwrap();
                file.sync_all().unwrap();
                let mut marker = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(root.join("torn-ready"))
                    .unwrap();
                marker.write_all(b"second header torn").unwrap();
                marker.sync_all().unwrap();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
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
            other => panic!("unknown H03 v2 tear phase: {other:?}"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn vm_power_cut_after_publication_keeps_the_new_chain() {
        use crate::seal::obk::read_open_header;
        use crate::seal::obk::rebuild::{apply_changed_with, PreviousContents, RebuildOptions};

        let Ok(root) = std::env::var("OWN_BACKUP_H03_PUBLISHED_DIR") else {
            return;
        };
        let root = Path::new(&root);
        let base = root.join("full.obk");
        let leaf = root.join("inc.obk");
        match std::env::var("OWN_BACKUP_H03_PUBLISHED_PHASE").as_deref() {
            Ok("publish") => {
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

                apply_changed_with(
                    &base,
                    &f.chain,
                    &[0],
                    |_| Ok(vec![0x33; 1 << cluster_bits()]),
                    RebuildOptions {
                        previous: PreviousContents::Keep,
                        ..Default::default()
                    },
                )
                .unwrap();
                let mut chain = ObkChain::open(&leaf, &f.chain).unwrap();
                assert_eq!(
                    &chain.read_cluster(0).unwrap()[..],
                    &vec![0x33; 1 << cluster_bits()]
                );
                let mut marker = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(root.join("published-ready"))
                    .unwrap();
                marker.write_all(b"new image published").unwrap();
                marker.sync_all().unwrap();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            Ok("verify") => {
                let open = read_open_header(&leaf).unwrap();
                let key = open.chain_key_with_password("пароль архива").unwrap();
                let mut chain = ObkChain::open(&leaf, &key).unwrap();
                assert_eq!(chain.chain_len(), 2);
                assert_eq!(
                    &chain.read_cluster(0).unwrap()[..],
                    &vec![0x33; 1 << cluster_bits()]
                );
                assert_eq!(
                    &chain.read_cluster(1).unwrap()[..],
                    &vec![0x22; 1 << cluster_bits()]
                );
            }
            other => panic!("unknown H03 publication phase: {other:?}"),
        }
    }
}
