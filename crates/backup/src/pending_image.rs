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
//! A plain image has no key to prove whose it is, so the proof is kept apart:
//! the installed service records each private name in a ledger inside its
//! SYSTEM-only directory before the image exists, and holds a lock on that
//! record for the whole run. A record nobody holds belongs to a killed run;
//! the next run deletes the image it names, with the scratch files that start
//! with the same random name, and then the record. Nothing without a record is
//! deleted: a foreground or boot-medium run, which has no such directory, and
//! images killed before the ledger existed are left as they were.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use own_backup_common::{OwnBackupError, Result};
use tempfile::TempPath;
use tracing::{info, warn};

const PREFIX: &str = ".own-backup-new-";
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

static LEDGER: OnceLock<PathBuf> = OnceLock::new();

/// Called once by the installed service with a directory only it can write.
/// Images written after this are reclaimed if their run is killed.
pub fn use_ledger(dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    LEDGER
        .set(dir)
        .map_err(|_| OwnBackupError::Config("pending image ledger set twice".into()))
}

/// What [`reclaim_abandoned`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    pub files: usize,
    pub bytes: u64,
    /// Records kept because their folder could not be read or a file there
    /// could not be deleted (a destination that is not connected right now).
    pub kept: usize,
}

/// Delete what killed runs left, as far as the ledger proves it ours.
pub fn reclaim_abandoned() -> Reclaimed {
    match LEDGER.get() {
        Some(ledger) => reclaim_in(ledger),
        None => Reclaimed::default(),
    }
}

/// One ledger record, locked while its run lives and removed when it ends.
struct Record {
    path: PathBuf,
    file: Option<File>,
}

impl Record {
    fn create(ledger: &Path, id: &str, image: &Path) -> Result<Self> {
        let path = ledger.join(format!("{id}.json"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        // From here on a failure removes the record again.
        let mut record = Self {
            path,
            file: Some(file),
        };
        let file = record.file.as_mut().expect("just set");
        file.try_lock().map_err(|e| {
            OwnBackupError::Config(format!("cannot lock pending image record: {e}"))
        })?;
        let body = serde_json::to_vec(&serde_json::json!({ "image": image }))
            .map_err(|e| OwnBackupError::Config(format!("pending image record: {e}")))?;
        file.write_all(&body)?;
        // On disk before the image is: a kill in between leaves a record
        // naming nothing, which the next run simply drops.
        file.sync_all()?;
        Ok(record)
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        // The image is gone or published by now; release, then forget it.
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

fn record_id(name: &str) -> Option<&str> {
    let id = name.strip_suffix(".json")?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

fn regular(meta: &std::fs::Metadata) -> bool {
    if !meta.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn reclaim_in(ledger: &Path) -> Reclaimed {
    let mut done = Reclaimed::default();
    let Ok(entries) = std::fs::read_dir(ledger) else {
        return done;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(record_id) else {
            continue;
        };
        let record = entry.path();
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&record) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue; // its run is still going
        }
        let mut body = Vec::new();
        if (&mut file).take(64 * 1024).read_to_end(&mut body).is_err() {
            continue;
        }
        let image = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("image")?.as_str().map(PathBuf::from));
        match image.map(|image| remove_image(&image, id)) {
            Some(Ok((files, bytes))) => {
                done.files += files;
                done.bytes += bytes;
            }
            Some(Err(e)) => {
                warn!(record = %record.display(), error = %e, "unfinished image of a killed run kept for now");
                done.kept += 1;
                continue;
            }
            // Torn while being written: its image did not exist yet.
            None => {}
        }
        drop(file);
        let _ = std::fs::remove_file(&record);
    }
    if done.files > 0 {
        info!(
            files = done.files,
            bytes = done.bytes,
            "unfinished images of killed runs deleted"
        );
    }
    done
}

/// The image a record names and every file beside it that starts with the
/// same private name. The name must carry the record's own id, so a record
/// can only ever reach the files its run created.
fn remove_image(image: &Path, id: &str) -> std::io::Result<(usize, u64)> {
    let invalid =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "not this record's image");
    let name = image
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let own = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_prefix(id))
        .is_some_and(|rest| rest.starts_with(".tmp."));
    if !own || !is_pending_name(name) {
        return Err(invalid());
    }
    let folder = image.parent().ok_or_else(invalid)?;
    let beside = format!("{name}.");
    let (mut files, mut bytes) = (0, 0);
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let Some(found) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if found != name && !found.starts_with(&beside) {
            continue;
        }
        // Another sweep may have been first (the service's own at start).
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Err(e) if gone(&e) => continue,
            meta => meta?,
        };
        if !regular(&meta) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Err(e) if gone(&e) => continue,
            done => done?,
        }
        files += 1;
        bytes += meta.len();
    }
    Ok((files, bytes))
}

/// The private name of one new image, deleted with this value unless published.
pub(crate) struct PendingImage {
    path: TempPath,
    // After `path`: fields drop in order, so the image goes before its record.
    _record: Option<Record>,
}

impl PendingImage {
    /// A name beside `target` that does not exist yet: the writers create the
    /// file themselves, and `CreateVirtualDisk` refuses one that is there.
    pub(crate) fn new(target: &Path) -> Result<Self> {
        Self::new_in(target, LEDGER.get().map(PathBuf::as_path))
    }

    fn new_in(target: &Path, ledger: Option<&Path>) -> Result<Self> {
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
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = target.with_file_name(format!("{PREFIX}{id}.tmp.{ext}"));
        let record = match ledger {
            Some(ledger) => {
                reclaim_in(ledger);
                Some(Record::create(ledger, &id, &path)?)
            }
            None => None,
        };
        Ok(Self {
            path: TempPath::try_from_path(path)?,
            _record: record,
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
        let pending =
            PendingImage::new(&root.path().join("job_C_inc_20261016-120000.avhdx")).unwrap();
        let name = pending
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
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

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// What a killed service leaves: the image, its scratch files and a record
    /// whose lock went with the process.
    fn killed(ledger: &Path, target: &Path) -> PathBuf {
        let pending = PendingImage::new_in(target, Some(ledger)).unwrap();
        let image = pending.path().to_path_buf();
        std::fs::write(&image, b"half written").unwrap();
        let mut scratch = image.clone().into_os_string();
        scratch.push(".excl.scratch");
        std::fs::write(scratch, b"scratch").unwrap();
        let PendingImage { path, _record } = pending;
        path.keep().unwrap();
        let mut record = _record.unwrap();
        drop(record.file.take());
        std::mem::forget(record);
        image
    }

    #[test]
    fn a_killed_runs_image_is_reclaimed_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let ledger = root.path().join("ledger");
        let dest = root.path().join("dest");
        std::fs::create_dir_all(&ledger).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let target = dest.join("job_C_full_20261017-120000.vhdx");
        std::fs::write(dest.join("job_C_full_20261016-120000.vhdx"), b"a version").unwrap();
        // Someone else's private-looking file without a record stays.
        let stranger = ".own-backup-new-0123456789abcdef0123456789abcdef.tmp.vhdx";
        std::fs::write(dest.join(stranger), b"not ours").unwrap();

        killed(&ledger, &target);
        assert_eq!(names(&dest).len(), 4);
        assert_eq!(names(&ledger).len(), 1);

        let done = reclaim_in(&ledger);
        assert_eq!(
            done,
            Reclaimed {
                files: 2,
                bytes: 19,
                kept: 0
            }
        );
        assert_eq!(
            names(&dest),
            vec![
                stranger.to_string(),
                "job_C_full_20261016-120000.vhdx".into()
            ]
        );
        assert!(names(&ledger).is_empty());
    }

    #[test]
    fn a_live_run_is_not_touched_and_its_record_goes_with_it() {
        let root = tempfile::tempdir().unwrap();
        let ledger = root.path().join("ledger");
        std::fs::create_dir_all(&ledger).unwrap();
        let target = root.path().join("job_C_full_20261017-120000.qcow2");
        let live = PendingImage::new_in(&target, Some(&ledger)).unwrap();
        std::fs::write(live.path(), b"being written").unwrap();
        assert_eq!(reclaim_in(&ledger), Reclaimed::default());
        assert!(live.path().exists());
        live.publish_as(&target).unwrap();
        assert!(names(&ledger).is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), b"being written");

        let failed =
            PendingImage::new_in(&target.with_file_name("x.qcow2"), Some(&ledger)).unwrap();
        std::fs::write(failed.path(), b"failed").unwrap();
        drop(failed);
        assert!(names(&ledger).is_empty());
        assert_eq!(
            names(root.path()),
            vec![
                "job_C_full_20261017-120000.qcow2".to_string(),
                "ledger".into()
            ]
        );
    }

    #[test]
    fn a_record_reaches_only_its_own_name_and_waits_for_its_folder() {
        let root = tempfile::tempdir().unwrap();
        let ledger = root.path().join("ledger");
        std::fs::create_dir_all(&ledger).unwrap();
        // A record naming a file that is not its own image is kept, not obeyed.
        let victim = root.path().join("job_C_full_20261016-120000.vhdx");
        std::fs::write(&victim, b"a version").unwrap();
        std::fs::write(
            ledger.join("0123456789abcdef0123456789abcdef.json"),
            serde_json::to_vec(&serde_json::json!({ "image": victim })).unwrap(),
        )
        .unwrap();
        // A destination that is not connected: the record stays for later.
        let offline = root.path().join("unplugged");
        std::fs::create_dir_all(&offline).unwrap();
        let image = killed(&ledger, &offline.join("job_D_full_20261017-120000.vhdx"));
        let moved = root.path().join("plugged-later");
        std::fs::rename(&offline, &moved).unwrap();

        assert_eq!(
            reclaim_in(&ledger),
            Reclaimed {
                files: 0,
                bytes: 0,
                kept: 2
            }
        );
        assert!(victim.exists());
        assert_eq!(names(&ledger).len(), 2);

        std::fs::rename(&moved, &offline).unwrap();
        assert_eq!(
            reclaim_in(&ledger),
            Reclaimed {
                files: 2,
                bytes: 19,
                kept: 1
            }
        );
        assert!(!image.exists());
        assert!(victim.exists());
    }

    #[test]
    fn a_record_torn_before_its_image_existed_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let ledger = root.path().join("ledger");
        std::fs::create_dir_all(&ledger).unwrap();
        std::fs::write(
            ledger.join("0123456789abcdef0123456789abcdef.json"),
            b"{\"ima",
        )
        .unwrap();
        let image = killed(&ledger, &root.path().join("j.vhdx"));
        std::fs::remove_file(&image).unwrap();
        let mut scratch = image.into_os_string();
        scratch.push(".excl.scratch");
        std::fs::remove_file(scratch).unwrap();
        assert_eq!(reclaim_in(&ledger), Reclaimed::default());
        assert!(names(&ledger).is_empty());
    }
}
