//! A partition's used-map and its file exclusions, before anything is copied.

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use super::target::no_patterns;
use crate::exclusions::ByteRanges;
use crate::used_sectors::UsedSectorMap;

/// A prepared partition used-map plus its file-exclusion outcome (plan D2).
pub(crate) struct PreparedMap {
    /// Used-sector map (`None` → sector-by-sector). For NTFS/FAT with exclusions
    /// the excluded clusters are already dropped from this map.
    pub(crate) map: Option<Box<dyn UsedSectorMap>>,
    /// Detected filesystem name for the manifest.
    pub(crate) filesystem: String,
    /// Excluded byte ranges (VOLUME-relative, finalized): zeroed in the image
    /// copy so excluded file DATA never leaves the machine.
    pub(crate) zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): deleted
    /// directory entries + freed allocation-table chains, so excluded FAT files
    /// truly disappear instead of remaining zero-filled ghosts.
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2):
    /// the FS driver deletes them inside the finished image so the catalog
    /// entry disappears too (not just a zero-filled ghost). Empty for FAT
    /// (patched directly) and Linux filesystems.
    pub(crate) removal_paths: Vec<crate::exclusions::ExcludedPath>,
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of the KEPT files on this partition (powers
    /// the dashboard's data breakdown); `None` when no exclusion walk ran.
    pub(crate) categories: Option<crate::exclusions::CategorySizes>,
    /// Why no walk ran over this partition, for the result screen. `None` when
    /// one did.
    pub(crate) skipped: Option<String>,
    /// Matched content the walk left in the copy, and directories it could
    /// not walk (`exclusions::key_store_left_behind`).
    pub(crate) left: Vec<crate::exclusions::LeftInCopy>,
    /// What the result screen says about this partition's exclusions besides
    /// the figures — files its `$MFT` scan could not reach, for one.
    pub(crate) notes: Vec<String>,
}

/// Карта и исключения для раздела, который Windows не монтирует: NTFS внутри
/// BitLocker, прочитанная через расшифрованное окно.
///
/// Исключения решаются ДО копии, по $MFT (план exclusions-before-copy, B3):
/// перечислить файлы обходом дерева тут нельзя — тома для Windows не
/// существует, — а по MFT можно, и тогда исключённые кластеры просто не
/// попадают в карту.
pub(crate) fn prepare_encrypted_partition_map(
    mut src: Box<dyn crate::fat_map::RawVolume + Send>,
    block_size: u32,
    patterns: &[String],
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> Result<PreparedMap> {
    let mut out = PreparedMap {
        map: None,
        filesystem: "NTFS".into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
        skipped: None,
        left: Vec::new(),
        notes: Vec::new(),
    };

    let scan = if patterns.is_empty() {
        out.skipped = Some(no_patterns());
        None
    } else {
        let cur = crate::ntfs_scan::RawVolumeCursor::new(src.as_mut());
        let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
        match crate::ntfs_scan::scan_mft_with_cancel(&mut buffered, patterns, &mut tick) {
            Ok(scan) => {
                info!(
                    files = scan.files_excluded,
                    bytes = scan.bytes_excluded,
                    kept = scan.kept.len(),
                    paths = scan.paths.len(),
                    records = scan.records_scanned,
                    "exclusions resolved from the MFT BEFORE the copy"
                );
                Some(scan)
            }
            Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
            Err(e) => {
                // Грязный том, не NTFS внутри, битая запись — копия делается
                // целиком, и об этом сказано вслух.
                warn!(error = %e, "MFT scan failed — this partition keeps every file");
                out.skipped = Some(tr!("engine_win.skip_mft_failed", error = e).into_owned());
                None
            }
        }
    };

    let excluded = scan.as_ref().map(|s| &s.ranges).filter(|r| !r.is_empty());
    match crate::ntfs_map::build_ntfs_used_map_excl(src.as_mut(), block_size, excluded) {
        Ok(map) => out.map = Some(Box::new(map)),
        Err(e) => {
            warn!(error = %e, "no NTFS map over the decrypted partition — copying every block");
            out.skipped.get_or_insert_with(|| {
                tr!("engine_win.skip_ntfs_map_failed", error = e).into_owned()
            });
            return Ok(out);
        }
    }
    if let Some(scan) = scan {
        out.excluded_files = scan.files_excluded;
        out.excluded_bytes = scan.bytes_excluded;
        out.categories = Some(scan.categories);
        out.left = super::volume_exclusions::ntfs_left(&scan.kept);
        // No walk of a partition Windows does not mount: what the scan could
        // not reach is said.
        if scan.records_unreachable > 0 {
            out.notes.push(
                tr!(
                    "engine.mft_records_unreachable",
                    count = scan.records_unreachable
                )
                .into_owned(),
            );
        }

        out.removal_paths = scan
            .paths
            .iter()
            .map(|p| crate::exclusions::ExcludedPath {
                rel: p.rel.clone(),
                is_dir: p.is_dir,
                is_reparse: p.is_reparse,
                whole_records: p.whole_records.clone(),
            })
            .collect();
        out.zero_ranges = scan.ranges;
    }
    Ok(out)
}

/// The ext/btrfs/xfs part of `engine_win`'s `prepare_partition_map`: the
/// used-map of a Linux filesystem and its file exclusions under the Linux
/// masks. `Some` when `src` holds ext, btrfs or xfs; `None` when it holds
/// none of them, and the caller goes on to its own answer.
///
/// `src` is the volume the map is built from; `open` opens the same volume
/// again — through the same alignment and decryption — for each scanner.
/// `read_path` only names the partition in the log.
pub(crate) fn linux_fs_exclusions(
    mut src: Box<dyn crate::fat_map::RawVolume + Send>,
    open: impl Fn() -> Result<Box<dyn crate::fat_map::RawVolume + Send>>,
    read_path: &str,
    block_size: u32,
    patterns_linux: &[String],
    // The partition was captured with its filesystem frozen (never on
    // Windows): an XFS log dirty only from the freeze is read past, and its
    // exclusions stop at Stage 1 unless the log is clean
    // (`xfs_exclusions::scan_xfs_exclusions_quiesced`).
    quiesced: bool,
    tick: &mut impl FnMut(u64, u64) -> Result<()>,
) -> Result<Option<PreparedMap>> {
    // The caller's starting point: a map, nothing excluded yet.
    let none = |fs: &str, map: Option<Box<dyn UsedSectorMap>>| PreparedMap {
        map,
        filesystem: fs.into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
        skipped: None,
        left: Vec::new(),
        notes: Vec::new(),
    };
    if let Ok(mut m) = crate::ext_map::build_ext_used_map(src.as_mut(), block_size) {
        // ext2/3/4: full file-level exclusions (Stage 1 drop+zero, Stage 2
        // metadata true-removal) — ext data blocks are exclusively owned, so
        // this is safe (unlike btrfs). Scanned over the DECRYPTED view for LUKS.
        drop(src);
        let mut out = none("ext4", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                match crate::ext_exclusions::scan_ext_exclusions(
                    src2.as_mut(),
                    patterns_linux,
                    |n| tick(n, 0),
                ) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        out.left = scan.left;
                        if !scan.ranges.is_empty() {
                            // Only what Stage 2 actually freed leaves the image.
                            // A zero-filled ghost keeps its blocks: the
                            // filesystem still lists the file, so those sectors
                            // must be restored as the zeros they became.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "ext exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "ext").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return Ok(Some(out));
    }
    if let Ok(mut m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), block_size) {
        // btrfs: CoW-safe file exclusions. Only extents PROVEN exclusively
        // owned by matched files are zeroed + dropped; anything shared with a
        // snapshot or a reflink is kept. Files whose extents are all private
        // are removed from the FS tree outright (Stage 2).
        //
        // Stage 2 destroyed a copy once (2026-07-28): excluding dense subtrees
        // (/proc, /sys, /dev, /run, /var/cache) emptied whole FS-tree leaves,
        // which stayed in the tree with no items while their parents still
        // pointed at them. The restored system aborted its first mount with
        // `tree first key mismatch … has=(0,0,0)`; later, stale csum entries
        // over freed extents corrupted the tree at first write. Three guards
        // now stand in the way (FS-, extent- and csum-tree leaves: files whose
        // removal would empty or overflow a leaf are spared to Stage 1), freed
        // extents LOSE their csum entries (`btrfs_del_csums` contract), and if
        // a leaf still breaks, the whole Stage 2 is discarded. A failed scan
        // keeps every file — a bigger image is always better than a corrupt
        // one.
        drop(src);
        let mut out = none("btrfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                // `true` = Stage 2: excluded files are REALLY removed from the
                // catalog (product requirement 2026-07-28, CLAUDE.md). The
                // scanner spares anything it cannot remove safely (shared
                // extents, hardlinks, leaf-breaking edits) down to Stage 1,
                // and any Stage-2 failure keeps the whole volume on Stage 1 —
                // see `scan_btrfs_exclusions`.
                match crate::btrfs_exclusions::scan_btrfs_exclusions(
                    src2.as_mut(),
                    patterns_linux,
                    true,
                    |n| tick(n, 0),
                ) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        out.left = scan.left;
                        if !scan.ranges.is_empty() {
                            // Stage-2 frees may be dropped; Stage-1 ghosts keep
                            // their blocks — btrfs rewrote their checksums to
                            // the checksum of zeros, so a restore that leaves
                            // the target's old bytes there fails every read
                            // (EIO) instead of returning an empty file.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "btrfs exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "btrfs").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return Ok(Some(out));
    }
    let xfs_map = if quiesced {
        crate::xfs_map::build_xfs_used_map_frozen(src.as_mut(), block_size)
    } else {
        crate::xfs_map::build_xfs_used_map(src.as_mut(), block_size)
    };
    if let Ok(mut m) = xfs_map {
        // xfs: reflink-aware Stage-1 zeroing (data is not checksummed on xfs, so
        // no metadata edits) — only extents proven NOT shared (refcount==1) are
        // zeroed + dropped. Catalog true-removal (Stage 2) is a follow-up.
        drop(src);
        let mut out = none("xfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                // `true` — Stage 2: записи каталогов, иноды и блоки. План
                // собирается целиком и применяется только если все три шага
                // согласились; иначе файл остаётся нулевым призраком.
                let scanned = if quiesced {
                    crate::xfs_exclusions::scan_xfs_exclusions_quiesced(
                        src2.as_mut(),
                        patterns_linux,
                        |n| tick(n, 0),
                    )
                } else {
                    crate::xfs_exclusions::scan_xfs_exclusions(
                        src2.as_mut(),
                        patterns_linux,
                        true,
                        |n| tick(n, 0),
                    )
                };
                match scanned {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        out.left = scan.left;
                        if !scan.ranges.is_empty() {
                            // Из образа уходит только то, что Stage 2 успел
                            // вернуть в свободное место. Призрак держит свои
                            // блоки: ФС всё ещё их читает.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(OwnBackupError::Cancelled) => return Err(OwnBackupError::Cancelled),
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "xfs exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "xfs").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return Ok(Some(out));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fat_map::RawVolume;

    /// The map granularity: one xfs/btrfs/ext block, so a freed extent shows.
    const BLOCK: u32 = 4096;

    struct FileVolume {
        file: std::fs::File,
        len: u64,
    }

    impl RawVolume for FileVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(buf)?;
            Ok(())
        }
        fn len(&self) -> u64 {
            self.len
        }
    }

    fn volume(path: &Path) -> Result<Box<dyn RawVolume + Send>> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Box::new(FileVolume { file, len }))
    }

    /// A fixture from tests/data, decompressed; `None` (and the test skips,
    /// like the scanners' own tests) when it is not there.
    fn fixture(name: &str) -> Option<(tempfile::TempDir, PathBuf)> {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        if !zst.exists() {
            eprintln!("{name} missing — skipping");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        let raw = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        std::fs::write(&img, raw).unwrap();
        Some((dir, img))
    }

    fn patterns(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    fn prepare(img: &Path, masks: &[&str]) -> Result<Option<PreparedMap>> {
        linux_fs_exclusions(
            volume(img).unwrap(),
            || volume(img),
            "fixture",
            BLOCK,
            &patterns(masks),
            false,
            &mut |_, _| Ok(()),
        )
    }

    fn used(out: &PreparedMap) -> u64 {
        out.map.as_ref().expect("a used-map").used_block_count()
    }

    #[test]
    fn xfs_drops_what_it_freed_and_keeps_what_is_shared() {
        let Some((_dir, img)) = fixture("xfs_reflink.img.zst") else {
            return;
        };
        let whole = prepare(&img, &[]).unwrap().expect("an xfs volume");
        assert_eq!(whole.filesystem, "xfs");
        assert_eq!(
            whole.skipped,
            Some(no_patterns()),
            "nobody walked, and it says so"
        );
        assert_eq!(whole.excluded_files, 0);

        // cache/big.bin is private; secret.log shares its extent with a reflink.
        let out = prepare(&img, &["cache", "secret.log"])
            .unwrap()
            .expect("an xfs volume");
        assert_eq!(out.filesystem, "xfs");
        assert!(out.skipped.is_none(), "{:?}", out.skipped);
        assert!(out.excluded_files >= 1);
        assert!(out.categories.is_some());
        assert!(
            !out.zero_ranges.is_empty(),
            "the private file's data is zeroed"
        );
        assert!(
            used(&out) < used(&whole),
            "what Stage 2 freed leaves the map ({} vs {})",
            used(&out),
            used(&whole)
        );
    }

    #[test]
    fn btrfs_never_zeroes_a_shared_extent() {
        let Some((_dir, img)) = fixture("btrfs_shared.img.zst") else {
            return;
        };
        // orig.bin's data is shared with a reflinked clone and a snapshot.
        let out = prepare(&img, &["orig.bin"])
            .unwrap()
            .expect("a btrfs volume");
        assert_eq!(out.filesystem, "btrfs");
        assert!(out.skipped.is_none(), "{:?}", out.skipped);
        assert!(out.excluded_files >= 1);
        assert!(
            out.zero_ranges.is_empty(),
            "shared extents were zeroed — CoW safety violated"
        );
        assert!(out.map.is_some());
    }

    #[test]
    fn a_cancelled_scan_cancels_the_preparation() {
        let Some((_dir, img)) = fixture("btrfs_shared.img.zst") else {
            return;
        };
        let outcome = linux_fs_exclusions(
            volume(&img).unwrap(),
            || volume(&img),
            "fixture",
            BLOCK,
            &patterns(&["orig.bin"]),
            false,
            &mut |_, _| Err(OwnBackupError::Cancelled),
        );
        assert!(matches!(outcome, Err(OwnBackupError::Cancelled)));
    }

    #[test]
    fn ext_exclusions_run_under_the_linux_masks() {
        let tools = std::process::Command::new("mkfs.ext4").arg("-V").output();
        if tools.is_err() {
            eprintln!("mkfs.ext4 unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("cache")).unwrap();
        std::fs::write(tree.join("keep.txt"), vec![0xCD; 64 * 1024]).unwrap();
        std::fs::write(tree.join("cache/junk.bin"), vec![0xAB; 256 * 1024]).unwrap();
        let img = dir.path().join("fs.img");
        let made = std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-b", "4096", "-d"])
            .arg(&tree)
            .arg(&img)
            .arg("16M")
            .status()
            .unwrap();
        assert!(made.success());

        let whole = prepare(&img, &[]).unwrap().expect("an ext volume");
        assert_eq!(whole.filesystem, "ext4");
        let out = prepare(&img, &["cache"]).unwrap().expect("an ext volume");
        assert!(out.skipped.is_none(), "{:?}", out.skipped);
        assert!(out.excluded_files >= 1);
        assert!(!out.zero_ranges.is_empty(), "the excluded data is zeroed");
        assert!(
            used(&out) < used(&whole),
            "what Stage 2 freed leaves the map"
        );
    }

    #[test]
    fn a_volume_of_no_linux_filesystem_is_left_to_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("zeros.img");
        std::fs::write(&img, vec![0u8; 4 << 20]).unwrap();
        let opened = std::cell::Cell::new(0);
        let out = linux_fs_exclusions(
            volume(&img).unwrap(),
            || {
                opened.set(opened.get() + 1);
                volume(&img)
            },
            "zeros",
            BLOCK,
            &patterns(&["cache"]),
            false,
            &mut |_, _| Ok(()),
        )
        .unwrap();
        assert!(out.is_none());
        assert_eq!(opened.get(), 0, "nothing to scan, nothing reopened");
    }

    /// A decrypted window that holds no filesystem at all.
    struct Zeros(u64);

    impl RawVolume for Zeros {
        fn read_at(&mut self, _offset: u64, buf: &mut [u8]) -> Result<()> {
            buf.fill(0);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn an_unreadable_encrypted_partition_says_why_it_keeps_every_file() {
        // No rules: nobody walks, and that is the reason given.
        let out =
            prepare_encrypted_partition_map(Box::new(Zeros(4 << 20)), BLOCK, &[], |_, _| Ok(()))
                .unwrap();
        assert!(out.map.is_none(), "no NTFS map: every block is copied");
        assert_eq!(out.skipped, Some(no_patterns()));

        // Rules, but no MFT to read them from: the scan's own reason stands,
        // not overwritten by the map's.
        let out = prepare_encrypted_partition_map(
            Box::new(Zeros(4 << 20)),
            BLOCK,
            &patterns(&["cache"]),
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(out.map.is_none());
        let why = out.skipped.expect("a reason for the result screen");
        assert_ne!(why, no_patterns());
        assert_eq!(out.excluded_files, 0);
        assert!(out.removal_paths.is_empty() && out.zero_ranges.is_empty());
    }
}
