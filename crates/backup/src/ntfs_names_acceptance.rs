//! Acceptance of the NTFS name removal on Linux, through the whole engine:
//! a real NTFS volume built by ntfs-3g from the shared corpus
//! ([`crate::exclusion_corpus`]), copied with exclusions into every container
//! and as a chain, restored, and then judged twice — statically (`ntfsfix -n`)
//! and by use: mounted read-write, every kept file read back, the free space
//! written, everything read again, checked again.
//!
//! What stands in for the kernel: [`FileExposer`] instead of the NBD device
//! (the cloud and containers have no `nbd.ko`), and ntfs-3g mounts files
//! directly. The NBD path itself is the stand scenario's
//! (`scripts/lab/linux-ntfs-names-scenario.sh`); Windows' own judgement,
//! `chkdsk`, is in its list for the Windows stand.
//!
//! Needs root, `/dev/fuse`, ntfs-3g and mkntfs; skipped without them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use own_backup_common::types::{BackupType, ImageFormat};
use own_backup_snapshot::Snapshot;

use super::tests::ntfs_volume;
use super::*;
use crate::engine_linux::{
    restore_volume, verify_image_with_keys, DiskBackupRequest, DiskPartitionBackup,
    LinuxRestoreRequest, VolumeBackupRequest, DEFAULT_BLOCK_SIZE,
};
use crate::engine_platform_linux::LinuxPlatform;
use crate::exclusion_corpus::{populate, Caps, Corpus};

const MIB: u64 = 1024 * 1024;

/// The job's Windows-family rules: a directory by name (the corpus' whole
/// `cache/` tree), the contents of a nested one, and two root files.
fn rules() -> Vec<String> {
    vec![
        "cache".into(),
        "Users\\*\\AppData\\Local\\Temp\\**".into(),
        "pagefile.sys".into(),
        "*.dmp".into(),
        "System Volume Information".into(),
    ]
}

/// What the NTFS test adds to the shared corpus: a profile tree whose `Temp`
/// must stay while its contents go, root files Windows-style, a named stream
/// on a kept file (and on an excluded one), and the folder Windows recreates.
struct Extras {
    kept: Vec<(PathBuf, Vec<u8>)>,
    gone: Vec<PathBuf>,
    stays_empty: Vec<PathBuf>,
}

fn add_extras(mnt: &Path) -> Extras {
    let temp = mnt.join("Users/u/AppData/Local/Temp");
    std::fs::create_dir_all(temp.join("a/b/c")).unwrap();
    let mut gone = Vec::new();
    for (i, p) in ["x.tmp", "a/y.tmp", "a/b/c/z.tmp"].iter().enumerate() {
        std::fs::write(temp.join(p), vec![0xAB; 20_000 + i]).unwrap();
        gone.push(temp.join(p));
    }
    gone.push(temp.join("a"));
    let mut kept = Vec::new();
    for (p, data) in [
        ("Users/u/AppData/Local/settings.ini", b"[keep]\n".to_vec()),
        (
            "Users/u/Documents/report.docx",
            crate::exclusion_corpus::pattern(42, 300_000),
        ),
        (
            "Windows/System32/config/SYSTEM",
            crate::exclusion_corpus::pattern(43, 70_000),
        ),
    ] {
        let path = mnt.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &data).unwrap();
        kept.push((path, data));
    }
    std::fs::write(mnt.join("pagefile.sys"), vec![0xAB; 3 * MIB as usize]).unwrap();
    std::fs::write(mnt.join("MEMORY.DMP"), vec![0xAB; 100_000]).unwrap();
    gone.push(mnt.join("pagefile.sys"));
    gone.push(mnt.join("MEMORY.DMP"));
    // Named streams: ntfs-3g's `user.*` attributes.
    crate::exclusion_corpus::setxattr(
        &mnt.join("Users/u/Documents/report.docx"),
        "user.Zone.Identifier",
        b"[ZoneTransfer]\r\nZoneId=3\r\n",
    );
    crate::exclusion_corpus::setxattr(&temp.join("x.tmp"), "user.stream", &[0xAB; 9000]);
    let svi = mnt.join("System Volume Information");
    std::fs::create_dir_all(&svi).unwrap();
    std::fs::write(svi.join("tracking.log"), [0xAB; 8192]).unwrap();
    gone.push(svi.join("tracking.log"));
    Extras {
        kept,
        gone,
        stays_empty: vec![temp, svi],
    }
}

/// Mount `file` with ntfs-3g at `mnt` for as long as `f` runs, and unmount
/// cleanly after.
fn with_mount<T>(file: &Path, mnt: &Path, f: impl FnOnce() -> T) -> T {
    let mut m = Ntfs3gMount::mount(&ntfs3g().unwrap(), file.to_str().unwrap(), mnt).unwrap();
    let out = f();
    m.unmount().unwrap();
    out
}

/// A populated NTFS volume in `dir`: the corpus, the extras, mounted at
/// `dir/mnt` while built (so every recorded path is under that directory).
fn ntfs_source(dir: &Path) -> Option<(PathBuf, PathBuf, Corpus, Extras)> {
    let src = dir.join("src.ntfs");
    ntfs_volume(&src, 160 * MIB)?;
    let mnt = dir.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let (corpus, extras) = with_mount(&src, &mnt, || {
        (populate(&mnt, Caps::ntfs()), add_extras(&mnt))
    });
    Some((src, mnt, corpus, extras))
}

fn snapshot(path: &Path) -> Snapshot {
    let mut s = Snapshot {
        provider_name: "linux".into(),
        source_device: path.to_str().unwrap().into(),
        snapshot_device_path: path.to_str().unwrap().into(),
        created_at: chrono::Utc::now(),
        metadata: HashMap::from([("mode".to_string(), "file".to_string())]),
    };
    own_backup_snapshot::linux::assert_file_frozen(&mut s);
    s
}

/// The Linux platform with ntfs-3g working on files instead of NBD devices.
fn platform(dir: &Path, snapshots: Vec<Snapshot>) -> LinuxPlatform {
    let work = dir.join("expose");
    std::fs::create_dir_all(&work).unwrap();
    LinuxPlatform::new(snapshots)
        .with_name_removal(Box::new(FileExposer { dir: work }), dir.join("names"))
}

fn keys() -> crate::seal::context::ArchiveKeys {
    use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use crate::seal::recovery::RecoveryKey;
    let pair = JobKeyPair::generate().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let kdf = KdfParams {
        algorithm: "argon2id".into(),
        m_kib: 64 * 1024,
        t: 3,
        p: 1,
    };
    let envelope = SealedPrivateKey::seal("job-ntfs", &pair, "пароль", &recovery, &kdf).unwrap();
    crate::seal::context::ArchiveKeys::from_private(envelope, pair.private).unwrap()
}

fn volume_request(
    src: &Path,
    image: &Path,
    format: ImageFormat,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    parent: Option<&Path>,
) -> VolumeBackupRequest {
    VolumeBackupRequest {
        source: src.to_str().unwrap().into(),
        image_path: image.to_path_buf(),
        full_image_path: None,
        block_size: DEFAULT_BLOCK_SIZE,
        all_sectors: false,
        use_vss: true,
        kind: if parent.is_some() {
            BackupType::Incremental
        } else {
            BackupType::Full
        },
        parent_image: parent.map(Path::to_path_buf),
        exclusion_patterns: rules(),
        exclusion_patterns_linux: Vec::new(),
        job_id: Some("job-ntfs".into()),
        passphrase: None,
        image_format: format,
        source_disk: None,
        encryption: keys.cloned(),
        zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
    }
}

/// A 4 KiB run of `byte` anywhere in `data`: the corpus fills excluded files
/// with 0xAB, so a run of it is excluded content that survived.
fn holds_run(data: &[u8], byte: u8) -> bool {
    data.chunks(4096)
        .any(|c| c.len() == 4096 && c.iter().all(|&b| b == byte))
}

fn ntfsfix_clean(file: &Path, tag: &str) {
    let out = Command::new("ntfsfix")
        .arg("-n")
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{tag}: ntfsfix -n: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The restored volume `file`, judged: clean, the excluded names gone, the
/// kept ones intact, writable to the brim, intact again, clean again — and
/// the kernel's ntfs3 driver, where there is one, reads it too.
fn judge(file: &Path, mnt: &Path, corpus: &Corpus, extras: &Extras, tag: &str) {
    let data = std::fs::read(file).unwrap();
    assert!(!holds_run(&data, 0xAB), "{tag}: excluded content survived");
    drop(data);
    ntfsfix_clean(file, tag);
    with_mount(file, mnt, || {
        for p in corpus.excluded.iter().chain(&extras.gone) {
            assert!(
                std::fs::symlink_metadata(p).is_err(),
                "{tag}: {} is still listed",
                p.display()
            );
        }
        assert!(
            std::fs::symlink_metadata(mnt.join("cache")).is_err(),
            "{tag}: cache/"
        );
        for dir in &extras.stays_empty {
            assert_eq!(
                std::fs::read_dir(dir).map(|d| d.count()).ok(),
                Some(0),
                "{tag}: {} must stay, empty",
                dir.display()
            );
        }
        corpus.verify_kept(tag);
        for (p, want) in &extras.kept {
            assert!(std::fs::read(p).unwrap() == *want, "{tag}: {}", p.display());
        }
        assert_eq!(
            crate::exclusion_corpus::getxattr(
                &mnt.join("Users/u/Documents/report.docx"),
                "user.Zone.Identifier"
            ),
            b"[ZoneTransfer]\r\nZoneId=3\r\n"
        );
        let written = Corpus::fill(mnt, 24);
        assert!(written > 0, "{tag}: nothing could be written");
        corpus.verify_kept(&format!("{tag} after writing"));
    });
    ntfsfix_clean(file, &format!("{tag} after writing"));
    with_mount(file, mnt, || {
        corpus.verify_kept(&format!("{tag} remounted"))
    });
    kernel_ntfs3_reads(file, corpus, mnt, tag);
}

/// The in-kernel driver as a second reader, where the kernel has it.
fn kernel_ntfs3_reads(file: &Path, corpus: &Corpus, mnt: &Path, tag: &str) {
    let has = std::fs::read_to_string("/proc/filesystems")
        .map(|f| f.split_whitespace().any(|t| t == "ntfs3"))
        .unwrap_or(false);
    if !has {
        eprintln!("{tag}: no ntfs3 in this kernel — read by ntfs-3g only");
        return;
    }
    let out = Command::new("mount")
        .args(["-t", "ntfs3", "-o", "loop,ro"])
        .arg(file)
        .arg(mnt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{tag}: ntfs3: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _m = crate::exclusion_corpus::Mounted(mnt.to_path_buf());
    corpus.verify_kept(&format!("{tag} via ntfs3"));
}

fn restore(image: &Path, size: u64, keys: Option<&crate::seal::context::ArchiveKeys>) -> PathBuf {
    let target = image.with_extension("restored");
    std::fs::File::create(&target)
        .unwrap()
        .set_len(size)
        .unwrap();
    restore_volume(
        &LinuxRestoreRequest {
            image_path: image.to_path_buf(),
            target: target.to_str().unwrap().into(),
            verify_checksums: true,
            encryption: keys.cloned(),
        },
        |_| Ok(()),
    )
    .unwrap();
    target
}

/// The whole pass per container, full and — where Linux makes children —
/// incremental: the names leave the image, the image still matches its
/// checksums, the restored volume is sound, and a child's pass leaves its
/// parent as it was.
#[test]
fn excluded_ntfs_names_leave_every_container_and_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let Some((src, mnt, mut corpus, extras)) = ntfs_source(dir) else {
        return;
    };
    let size = std::fs::metadata(&src).unwrap().len();
    let keys = keys();
    for (name, format, k) in [
        ("q", ImageFormat::Qcow2, None),
        ("v", ImageFormat::Vhdx, None),
        ("e", ImageFormat::Qcow2, Some(&keys)),
    ] {
        let ext = match (format, k) {
            (_, Some(_)) => "obk",
            (ImageFormat::Vhdx, _) => "vhdx",
            _ => "qcow2",
        };
        let full = dir.join(format!("{name}-full.{ext}"));
        let req = volume_request(&src, &full, format, k, None);
        let p = platform(dir, vec![snapshot(&src)]);
        let sum = crate::engine_core::volume::backup_volume(&p, &req, |_| Ok(())).unwrap();
        assert!(
            sum.excluded_files > 1400,
            "{name}: {:?}",
            sum.file_scan_skipped
        );
        assert_eq!(
            sum.excluded_entries_kept, None,
            "{name}: the names were all deleted"
        );
        assert!(verify_image_with_keys(&full, k, |_, _| Ok(()))
            .unwrap()
            .valid());
        let restored = restore(&full, size, k);
        judge(&restored, &mnt, &corpus, &extras, &format!("{name} full"));
        std::fs::remove_file(&restored).unwrap();
    }

    // A chain: the source moves on — a new kept file, a changed one, a new
    // excluded one — and the increments of qcow2 and OBK are judged as well.
    let added = mnt.join("keep/added_later.bin");
    let fresh = crate::exclusion_corpus::pattern(4242, 200_000);
    with_mount(&src, &mnt, || {
        std::fs::write(&added, &fresh).unwrap();
        std::fs::write(mnt.join("cache/new.tmp"), [0xAB; 50_000]).unwrap();
        std::fs::write(
            mnt.join("Users/u/AppData/Local/Temp/later.tmp"),
            [0xAB; 7000],
        )
        .unwrap();
    });
    corpus.kept.push((added, fresh));
    for (name, k) in [("q", None), ("e", Some(&keys))] {
        let ext = if k.is_some() { "obk" } else { "qcow2" };
        let full = dir.join(format!("{name}-full.{ext}"));
        let parent_bytes = std::fs::read(&full).unwrap();
        let inc = dir.join(format!("{name}-inc.{ext}"));
        let req = volume_request(&src, &inc, ImageFormat::Qcow2, k, Some(&full));
        let p = platform(dir, vec![snapshot(&src)]);
        let sum = crate::engine_core::volume::backup_volume(&p, &req, |_| Ok(())).unwrap();
        assert_eq!(sum.backup_kind, BackupType::Incremental, "{name}");
        assert_eq!(sum.excluded_entries_kept, None, "{name} inc");
        assert!(
            std::fs::read(&full).unwrap() == parent_bytes,
            "{name}: the parent changed"
        );
        assert!(verify_image_with_keys(&inc, k, |_, _| Ok(()))
            .unwrap()
            .valid());
        let restored = restore(&inc, size, k);
        judge(&restored, &mnt, &corpus, &extras, &format!("{name} inc"));
        std::fs::remove_file(&restored).unwrap();
    }
}

/// An exposer whose device never comes up: what a machine without `nbd.ko`
/// or without root sees.
struct NoDevice;

impl VolumeExposer for NoDevice {
    fn expose(
        &self,
        _window: ScratchWindow,
        _f: &mut dyn FnMut(&str) -> Result<(u64, u64)>,
    ) -> Result<(u64, u64)> {
        Err(dev("/dev/nbd0 not available"))
    }
}

/// When the names cannot be deleted — no device, or a volume Windows left
/// hibernated, which ntfs-3g refuses to write — the copy is still a good
/// one: the data is gone, the image verifies, and the result says the names
/// were kept instead of claiming them removed.
#[test]
fn names_that_cannot_be_deleted_are_reported_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let Some((src, mnt, _corpus, _extras)) = ntfs_source(dir) else {
        return;
    };
    let kept_text = own_backup_common::tr!("engine.entries_kept_linux").into_owned();

    let image = dir.join("nodev.qcow2");
    let req = volume_request(&src, &image, ImageFormat::Qcow2, None, None);
    let p =
        LinuxPlatform::new([snapshot(&src)]).with_name_removal(Box::new(NoDevice), dir.join("n"));
    let sum = crate::engine_core::volume::backup_volume(&p, &req, |_| Ok(())).unwrap();
    assert_eq!(
        sum.excluded_entries_kept.as_deref(),
        Some(kept_text.as_str())
    );
    assert!(crate::engine_linux::verify_image(&image, |_, _| Ok(()))
        .unwrap()
        .valid());
    let size = std::fs::metadata(&src).unwrap().len();
    let restored = restore(&image, size, None);
    // The files' data is gone — all but the named stream of an excluded file
    // (9000 bytes: two whole 4 KiB runs). The MFT scan subtracts unnamed
    // `$DATA` only, here and on Windows; only the deletion pass, which erases
    // what the deletions freed, takes the streams away. Pinned, so that a
    // scanner fix shows up here.
    let data = std::fs::read(&restored).unwrap();
    let runs = data
        .chunks(4096)
        .filter(|c| c.len() == 4096 && c.iter().all(|&b| b == 0xAB))
        .count();
    assert_eq!(runs, 2, "only the excluded file's named stream is left");
    drop(data);
    with_mount(&restored, &mnt, || {
        assert!(mnt.join("cache").is_dir(), "the names are still there");
        assert_eq!(
            std::fs::metadata(mnt.join("pagefile.sys")).unwrap().len(),
            3 * MIB
        );
    });

    // Hibernated: the hiberfile's signature is what ntfs-3g looks at.
    with_mount(&src, &mnt, || {
        let mut hiber = b"HIBR".to_vec();
        hiber.resize(64 * 1024, 0x11);
        std::fs::write(mnt.join("hiberfil.sys"), hiber).unwrap();
    });
    let image = dir.join("hiber.qcow2");
    let req = volume_request(&src, &image, ImageFormat::Qcow2, None, None);
    let sum = crate::engine_core::volume::backup_volume(
        &platform(dir, vec![snapshot(&src)]),
        &req,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        sum.excluded_entries_kept.as_deref(),
        Some(kept_text.as_str())
    );
    assert!(crate::engine_linux::verify_image(&image, |_, _| Ok(()))
        .unwrap()
        .valid());
}

/// A whole disk: two NTFS partitions, each with its own excluded entries,
/// deleted in one pass over the disk image; then an increment.
#[test]
fn excluded_ntfs_names_leave_a_whole_disk_image() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let Some((p1, mnt, corpus, extras)) = ntfs_source(dir) else {
        return;
    };
    let p2 = dir.join("p2.ntfs");
    ntfs_volume(&p2, 48 * MIB).unwrap();
    with_mount(&p2, &mnt, || {
        std::fs::create_dir_all(mnt.join("data/cache/inner")).unwrap();
        std::fs::write(mnt.join("data/cache/inner/a.bin"), [0xAB; 30_000]).unwrap();
        std::fs::write(mnt.join("data/keep.txt"), b"second partition").unwrap();
    });

    let disk = dir.join("disk.img");
    let at1 = MIB;
    let at2 = at1 + std::fs::metadata(&p1).unwrap().len() + MIB;
    let size = at2 + std::fs::metadata(&p2).unwrap().len() + MIB;
    let layout = own_backup_common::gpt::DiskLayout {
        disk_guid: "22222222-0000-0000-0000-000000000000".into(),
        sector_size: 512,
        size_bytes: size,
        partitions: [(1u32, &p1, at1), (2, &p2, at2)]
            .iter()
            .map(
                |(index, path, offset)| own_backup_common::gpt::GptPartitionEntry {
                    index: *index,
                    type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
                    part_guid: format!("33333333-0000-0000-0000-00000000000{index}"),
                    first_lba: offset / 512,
                    last_lba: (offset + std::fs::metadata(path).unwrap().len()) / 512 - 1,
                    attributes: 0,
                    name: format!("part{index}"),
                },
            )
            .collect(),
    };
    {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::create(&disk).unwrap();
        file.set_len(size).unwrap();
        for op in crate::gpt_writer::disk_gpt_write_ops(&layout, size).unwrap() {
            file.write_all_at(&op.bytes, op.offset).unwrap();
        }
        for (path, at) in [(&p1, at1), (&p2, at2)] {
            file.write_all_at(&std::fs::read(path).unwrap(), at)
                .unwrap();
        }
    }
    let request = |image: &Path, parent: Option<&Path>| DiskBackupRequest {
        disk_number: 0,
        disk_device: Some(disk.to_str().unwrap().into()),
        disk_model: "test disk".into(),
        partitions: [(1u32, &p1), (2, &p2)]
            .iter()
            .map(|(index, path)| DiskPartitionBackup {
                device: path.to_str().unwrap().into(),
                part_index: *index,
                passphrase: None,
            })
            .collect(),
        image_path: image.to_path_buf(),
        full_image_path: None,
        block_size: DEFAULT_BLOCK_SIZE,
        image_format: ImageFormat::Qcow2,
        job_id: Some("ntfs-disk".into()),
        encryption: None,
        exclusion_patterns_windows: rules(),
        exclusion_patterns_linux: Vec::new(),
        kind: if parent.is_some() {
            BackupType::Incremental
        } else {
            BackupType::Full
        },
        parent_image: parent.map(Path::to_path_buf),
        source_disk: None,
        bootable: false,
        zstd_level: crate::qcow2::DEFAULT_ZSTD_LEVEL,
    };
    // A partition of the image, as a volume file again.
    let part_of = |image: &Path, at: u64, len: u64, name: &str| {
        let mut r = crate::qcow2::Qcow2RandomReader::open(image).unwrap();
        let mut data = vec![0u8; len as usize];
        r.read_at(at, &mut data).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, data).unwrap();
        file
    };
    let len1 = std::fs::metadata(&p1).unwrap().len();
    let len2 = std::fs::metadata(&p2).unwrap().len();

    let full = dir.join("disk_full.qcow2");
    let snaps = vec![snapshot(&p1), snapshot(&p2)];
    let sum = crate::engine_core::disk::backup_disk(
        &platform(dir, snaps.clone()),
        &request(&full, None),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        sum.excluded_entries_kept, None,
        "{:?}",
        sum.file_scan_skipped
    );
    assert!(crate::engine_linux::verify_image(&full, |_, _| Ok(()))
        .unwrap()
        .valid());
    judge(
        &part_of(&full, at1, len1, "f1.ntfs"),
        &mnt,
        &corpus,
        &extras,
        "disk p1",
    );
    let second = part_of(&full, at2, len2, "f2.ntfs");
    ntfsfix_clean(&second, "disk p2");
    with_mount(&second, &mnt, || {
        assert!(!mnt.join("data/cache").exists());
        assert_eq!(
            std::fs::read(mnt.join("data/keep.txt")).unwrap(),
            b"second partition"
        );
    });

    // The increment: only the second partition moves.
    with_mount(&p2, &mnt, || {
        std::fs::write(mnt.join("data/cache/late.bin"), [0xAB; 9000]).unwrap();
        std::fs::write(mnt.join("data/later.txt"), b"later").unwrap();
    });
    {
        use std::os::unix::fs::FileExt;
        let file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
        file.write_all_at(&std::fs::read(&p2).unwrap(), at2)
            .unwrap();
    }
    let parent_bytes = std::fs::read(&full).unwrap();
    let inc = dir.join("disk_inc.qcow2");
    let sum = crate::engine_core::disk::backup_disk(
        &platform(dir, snaps),
        &request(&inc, Some(&full)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(sum.backup_kind, BackupType::Incremental);
    assert_eq!(sum.excluded_entries_kept, None);
    assert!(
        std::fs::read(&full).unwrap() == parent_bytes,
        "the parent changed"
    );
    assert!(crate::engine_linux::verify_image(&inc, |_, _| Ok(()))
        .unwrap()
        .valid());
    judge(
        &part_of(&inc, at1, len1, "i1.ntfs"),
        &mnt,
        &corpus,
        &extras,
        "disk inc p1",
    );
    let second = part_of(&inc, at2, len2, "i2.ntfs");
    ntfsfix_clean(&second, "disk inc p2");
    with_mount(&second, &mnt, || {
        assert!(!mnt.join("data/cache").exists());
        assert_eq!(std::fs::read(mnt.join("data/later.txt")).unwrap(), b"later");
    });
}
