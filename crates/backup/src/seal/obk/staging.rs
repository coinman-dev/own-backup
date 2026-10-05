//! Authenticated ownership of crash leftovers, separate from publication.
//! Each immutable random name binds one owner to one sibling. Sync the owner
//! before the image-sized copy; keep it locked throughout the transaction.
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use own_backup_common::Result;
use tempfile::{NamedTempFile, TempPath};
use uuid::Uuid;

use crate::seal::{aead_open, aead_seal, ChainKey};

const PREFIX: &str = ".own-backup-obk-";
const MAGIC: &[u8; 8] = b"OBKSTG01";
const LIMIT: u64 = 64 * 1024;

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Identity {
    volume: u64,
    id: [u8; 16],
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    target: PathBuf,
    owner_identity: Option<Identity>,
    stage_identity: Option<Identity>,
}

#[cfg(unix)]
fn identity(file: &File) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    let mut id = [0; 16];
    id[..8].copy_from_slice(&meta.ino().to_le_bytes());
    Some(Identity {
        volume: meta.dev(),
        id,
    })
}
#[cfg(windows)]
fn identity(file: &File) -> Option<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the live file handle and correctly sized output remain valid.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    }
    .ok()?;
    if info.FileId.Identifier == [0; 16] {
        return None;
    }
    Some(Identity {
        volume: info.VolumeSerialNumber,
        id: info.FileId.Identifier,
    })
}
#[cfg(not(any(unix, windows)))]
fn identity(_file: &File) -> Option<Identity> {
    None
}

fn open_stage(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Foundation::GENERIC_READ;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        opts.access_mode(GENERIC_READ.0 | DELETE.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    opts.open(path)
}
#[cfg(windows)]
fn remove_stage(file: File, _path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{BOOLEAN, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: BOOLEAN(1),
    };
    // Delete the object whose handle/identity was checked, not a reopened path.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    }
    .map_err(|e| super::bad(format!("cannot reclaim staged file: {e}")))?;
    drop(file);
    Ok(())
}
#[cfg(not(windows))]
fn remove_stage(_file: File, path: &Path) -> Result<()> {
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
#[path = "staging_faults.rs"]
mod faults;

fn key(chain: &ChainKey, id: &str) -> zeroize::Zeroizing<[u8; 32]> {
    chain.derive(&format!("obk-stage-owner/{id}"))
}
fn owner_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix(PREFIX)?.strip_suffix(".owner")?;
    (id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    .then_some(id)
}
fn regular(metadata: &Metadata) -> bool {
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return false;
        }
    }
    true
}
fn open_owner(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        opts.access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0);
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    opts.open(path)
}
pub(super) fn create(target: &Path, chain: &ChainKey) -> Result<(NamedTempFile, TempPath)> {
    let parent = target
        .parent()
        .ok_or_else(|| super::bad("stage target has no parent"))?;
    let id = Uuid::new_v4().simple().to_string();
    let prefix = format!("{PREFIX}{id}");
    let mut owner = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".owner")
        .rand_bytes(0)
        .tempfile_in(parent)?;
    owner
        .as_file()
        .try_lock()
        .map_err(|e| super::bad(format!("cannot lock stage owner: {e}")))?;
    // A zero-length sibling exists before registration, but no large copy is
    // allowed until its target and file identity have been durably recorded.
    let stage = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .rand_bytes(0)
        .tempfile_in(parent)?;
    // Domain-separated key per fresh UUID; this zero nonce is used once.
    let plain = serde_json::to_vec(&Record {
        target: target.to_owned(),
        owner_identity: identity(owner.as_file()),
        stage_identity: identity(stage.as_file()),
    })
    .map_err(|e| super::bad(format!("stage target encoding: {e}")))?;
    if plain.len() as u64 + MAGIC.len() as u64 + 16 > LIMIT {
        return Err(super::bad("stage target too long"));
    }
    let sealed = aead_seal(&key(chain, &id), &[0; 12], MAGIC, &plain)?;
    owner.write_all(MAGIC)?;
    owner.write_all(&sealed)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_NORMAL};
        let wide: Vec<u16> = owner
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_NORMAL) }
            .map_err(|e| super::bad(format!("cannot prepare stage owner: {e}")))?;
    }
    owner.as_file().sync_all()?;
    super::transaction::sync_parent(parent)?;
    Ok((owner, stage.into_temp_path()))
}

/// Reclaim only a proved, unlocked sibling for this canonical target/key.
/// Unknown old files, malformed/foreign owners and active leases stay intact.
/// A confirmed deletion error aborts the new copy and keeps ownership proof.
pub(super) fn cleanup(target: &Path, chain: &ChainKey) -> Result<usize> {
    let parent = target
        .parent()
        .ok_or_else(|| super::bad("stage target has no parent"))?;
    let mut removed = 0;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(owner_id) else {
            continue;
        };
        if !matches!(entry.file_type(), Ok(kind) if kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(mut file) = open_owner(&path) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !regular(&metadata) || metadata.len() > LIMIT {
            continue;
        }
        if file.try_lock().is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if (&mut file).take(LIMIT + 1).read_to_end(&mut bytes).is_err()
            || bytes.len() as u64 > LIMIT
            || !bytes.starts_with(MAGIC)
        {
            continue;
        }
        let Ok(plain) = aead_open(&key(chain, id), &[0; 12], MAGIC, &bytes[MAGIC.len()..]) else {
            continue;
        };
        let Ok(recorded) = serde_json::from_slice::<Record>(&plain) else {
            continue;
        };
        if recorded.target != target
            || recorded.owner_identity.is_none()
            || identity(&file) != recorded.owner_identity
        {
            continue;
        }
        // The strictly validated basename determines the sibling, never input
        // from an owner payload. No recursive deletion or stage promotion.
        let stage = parent.join(format!("{PREFIX}{id}.tmp"));
        match std::fs::symlink_metadata(&stage) {
            Ok(meta) => {
                if !regular(&meta) {
                    continue;
                }
                let Ok(stage_file) = open_stage(&stage) else {
                    continue;
                };
                let Ok(meta) = stage_file.metadata() else {
                    continue;
                };
                if !regular(&meta)
                    || recorded.stage_identity.is_none()
                    || identity(&stage_file) != recorded.stage_identity
                {
                    continue;
                }
                remove_stage(stage_file, &stage)?;
                #[cfg(test)]
                super::publication_faults::hit(
                    super::publication_faults::Point::OrphanStageRemoved,
                    &path,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => continue,
        }
        // Retain the lock until the owner is unlinked, after the large sibling.
        #[cfg(windows)]
        {
            remove_stage(file, &path)?;
            removed += 1;
        }
        #[cfg(not(windows))]
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if removed > 0 {
        super::transaction::sync_parent(parent)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, b"published original").unwrap();
        std::fs::canonicalize(path).unwrap()
    }
    pub(super) fn abandon(target: &Path, key: &ChainKey) -> (PathBuf, PathBuf) {
        let (owner, stage) = create(target, key).unwrap();
        std::fs::copy(target, &stage).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&stage)
            .unwrap()
            .sync_all()
            .unwrap();
        let stage = stage.keep().unwrap();
        let (file, owner) = owner.keep().unwrap();
        drop(file); // model an owner whose process no longer holds the lock
        (owner, stage)
    }
    #[test]
    fn only_the_requested_target_and_key_are_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let a = target(root.path(), "a.obk");
        let b = target(root.path(), "b.obk");
        let key = ChainKey::generate().unwrap();
        let (a_owner, a_stage) = abandon(&a, &key);
        let (b_owner, b_stage) = abandon(&b, &key);
        assert_eq!(cleanup(&a, &ChainKey::generate().unwrap()).unwrap(), 0);
        assert!(a_stage.exists() && b_stage.exists());
        assert_eq!(cleanup(&a, &key).unwrap(), 1);
        assert!(!a_owner.exists() && !a_stage.exists());
        assert!(b_owner.exists() && b_stage.exists());
        assert_eq!(std::fs::read(&a).unwrap(), b"published original");
        assert_eq!(std::fs::read(&b).unwrap(), b"published original");
    }
    #[test]
    fn active_owner_is_kept_until_the_lock_is_released() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = create(&path, &key).unwrap();
        std::fs::write(&stage, b"active candidate").unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert_eq!(std::fs::read(&stage).unwrap(), b"active candidate");
        let stage = stage.keep().unwrap();
        let (file, owner) = owner.keep().unwrap();
        drop(file);
        assert_eq!(cleanup(&path, &key).unwrap(), 1);
        assert!(!stage.exists() && !owner.exists());
    }
    #[test]
    fn tampered_copied_oversized_and_legacy_files_are_not_deletion_proof() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        let mut bytes = std::fs::read(&owner).unwrap();
        let another = format!("{PREFIX}{}", Uuid::new_v4().simple());
        let copied_owner = root.path().join(format!("{another}.owner"));
        let copied_stage = root.path().join(format!("{another}.tmp"));
        std::fs::write(&copied_owner, &bytes).unwrap();
        std::fs::write(&copied_stage, b"copied-name control").unwrap();
        bytes[8] ^= 1;
        std::fs::write(&owner, bytes).unwrap();
        let oversized = root
            .path()
            .join(format!("{PREFIX}{}.owner", Uuid::new_v4().simple()));
        std::fs::write(&oversized, vec![0u8; LIMIT as usize + 1]).unwrap();
        let legacy = root.path().join(".own-backup-obk-TZ6W6D.tmp");
        std::fs::write(&legacy, b"unregistered legacy control").unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        for p in [owner, stage, copied_owner, copied_stage, oversized, legacy] {
            assert!(p.exists());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"published original");
    }
    #[test]
    fn an_owner_without_a_stage_is_reclaimed_after_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        std::fs::remove_file(stage).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 1);
        assert!(!owner.exists());
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
    }
    #[test]
    fn a_replaced_sibling_is_not_ours_even_with_the_original_name() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        let outside = target(root.path(), "unrelated.bin");
        std::fs::remove_file(&stage).unwrap();
        std::fs::rename(outside, &stage).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert!(owner.exists());
        assert_eq!(std::fs::read(stage).unwrap(), b"published original");
    }
    #[test]
    fn a_copied_owner_cannot_claim_the_original_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        let replacement = root.path().join("replacement-owner");
        std::fs::write(&replacement, std::fs::read(&owner).unwrap()).unwrap();
        std::fs::rename(&owner, root.path().join("displaced-owner")).unwrap();
        std::fs::rename(&replacement, &owner).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert!(stage.exists() && owner.exists());
        assert_eq!(std::fs::read(&stage).unwrap(), b"published original");
    }
    #[cfg(windows)]
    #[test]
    fn checked_names_cannot_be_swapped_until_the_delete_handle_closes() {
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        for (name, open) in [
            (&owner, open_owner as fn(&Path) -> std::io::Result<File>),
            (&stage, open_stage),
        ] {
            let handle = open(name).unwrap();
            let moved = root.path().join("swapped-object");
            assert!(std::fs::rename(name, &moved).is_err());
            assert!(name.exists());
            drop(handle);
            std::fs::rename(name, &moved).unwrap();
            std::fs::rename(&moved, name).unwrap();
        }
        assert_eq!(cleanup(&path, &key).unwrap(), 1);
        assert!(!owner.exists() && !stage.exists());
    }
    #[cfg(unix)]
    #[test]
    fn links_and_directories_are_kept_without_following_them() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let path = target(root.path(), "base.obk");
        let control = target(root.path(), "control.txt");
        let key = ChainKey::generate().unwrap();
        let (owner, stage) = abandon(&path, &key);
        std::fs::remove_file(&stage).unwrap();
        symlink(&control, &stage).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert!(owner.exists());
        assert_eq!(std::fs::read(&control).unwrap(), b"published original");
        std::fs::remove_file(&stage).unwrap();
        std::fs::create_dir(&stage).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert!(owner.exists() && stage.is_dir());
        std::fs::remove_dir(&stage).unwrap();
        std::fs::remove_file(&owner).unwrap();
        symlink(&control, &owner).unwrap();
        assert_eq!(cleanup(&path, &key).unwrap(), 0);
        assert_eq!(std::fs::read(control).unwrap(), b"published original");
    }
    #[test]
    fn a_live_owner_in_another_process_cannot_be_reclaimed() {
        const CHILD: &str = "OWN_BACKUP_OBK_LIVE_OWNER_TEST";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::fs::canonicalize(path).unwrap();
            let key = super::super::read_open_header(&path)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(cleanup(&path, &key).unwrap(), 0);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let f = super::super::container_tests::Fixture::new();
        let path = root.path().join("base.obk");
        let writer = super::super::ObkWriter::create(
            &path,
            &f.chain,
            f.options(Uuid::new_v4(), crate::sidecar::ChainRole::Full, None),
        )
        .unwrap();
        writer.finish(b"metadata").unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let (owner, stage) = create(&path, &f.chain).unwrap();
        std::fs::write(&stage, b"live process candidate").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "seal::obk::staging::tests::a_live_owner_in_another_process_cannot_be_reclaimed",
            ])
            .env(CHILD, &path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(owner.path().exists());
        assert_eq!(std::fs::read(stage).unwrap(), b"live process candidate");
    }
}
