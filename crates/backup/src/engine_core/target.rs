//! The image being written, and the rules around publishing it: what a run
//! may be asked to write, which name the finished image takes, and the last
//! question asked before it becomes a version.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::info;

use own_backup_common::progress::BackupProgress;
use own_backup_common::types::{BackupStatus, BackupType, ImageFormat};
use own_backup_common::{tr, OwnBackupError, Result};

/// Default backup block size: 1 MiB (multiple of every real sector size).
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;

/// Why there is no by-type breakdown behind the most ordinary copy of all: the
/// file walk is the exclusion walk, and with no rules to apply nobody walks.
pub(crate) fn no_patterns() -> String {
    tr!("engine_win.no_patterns").into_owned()
}

/// The result screen's sentence for a removal pass that did not happen: the
/// pass itself logged why, in detail.
pub(crate) fn entries_kept_reason() -> String {
    tr!("engine_win.entries_kept").into_owned()
}

/// Write target behind the format switch. VHDX goes through the kernel
/// (attach → raw writes to the PhysicalDrive node); qcow2 is written entirely
/// in userspace with per-cluster zstd compression — no attach, no SAN-policy
/// juggling, no flush pacing (the OS cache never holds a whole image).
pub(crate) enum ImageTarget {
    Vhdx {
        writer: crate::vhdx_format::VhdxWriter,
    },
    Qcow2 {
        writer: crate::qcow2::Qcow2StagedWriter<std::io::BufWriter<std::fs::File>>,
    },
    /// The encrypted container: the qcow2 path with our incompatible
    /// extension. Its sidecar is sealed inside instead of living in a guest
    /// meta partition, so `finalize` is where it goes in.
    Obk {
        writer: Box<crate::seal::obk::ObkStagedWriter<std::io::BufWriter<std::fs::File>>>,
    },
}

impl ImageTarget {
    /// Virtual disk size of the image being written.
    pub(crate) fn length(&self) -> u64 {
        match self {
            ImageTarget::Vhdx { writer } => writer.length(),
            ImageTarget::Qcow2 { writer } => writer.length(),
            ImageTarget::Obk { writer } => writer.length(),
        }
    }

    pub(crate) fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match self {
            ImageTarget::Vhdx { writer } => writer.write_at(offset, data),
            ImageTarget::Qcow2 { writer } => writer.write_at(offset, data),
            ImageTarget::Obk { writer } => writer.write_at(offset, data),
        }
    }

    /// Finalize the image and log per-step timings.
    ///
    /// `sealed` is the sidecar of an encrypted image, with `reserve` bytes of
    /// room so the final one — the exclusion figures and the checksums the
    /// deletion pass touched are known only later — can replace it in place.
    /// The other two containers keep their sidecar in a guest meta partition
    /// and ignore both.
    pub(crate) fn finalize(self, sealed: &[u8], reserve: usize) -> Result<()> {
        match self {
            ImageTarget::Obk { writer } => {
                let t = Instant::now();
                // Records, index, sealed sidecar, tables, header — the header
                // last, which is this container's commit point.
                let buf = writer.finish_with_reserve(sealed, reserve)?;
                let file = buf
                    .into_inner()
                    .map_err(|e| OwnBackupError::Io(e.into_error()))?;
                file.sync_all()?;
                info!(finish_ms = t.elapsed().as_millis(), "obk finalize timings");
            }
            ImageTarget::Vhdx { writer } => {
                let t = Instant::now();
                writer.finish()?;
                info!(finish_ms = t.elapsed().as_millis(), "VHDX finalize timings");
            }
            ImageTarget::Qcow2 { writer } => {
                let t = Instant::now();
                // Write L2/L1/refcounts + header, then make the file durable.
                let buf = writer.finish()?;
                let file = buf
                    .into_inner()
                    .map_err(|e| OwnBackupError::Io(e.into_error()))?;
                file.sync_all()?;
                info!(
                    finish_ms = t.elapsed().as_millis(),
                    "qcow2 finalize timings"
                );
            }
        }
        Ok(())
    }
}

/// Отправить в интерфейс разбивку по типам файлов, как только она посчитана.
///
/// Одним сообщением и один раз: гонять её в каждом отчёте о ходе незачем —
/// она не меняется до конца копирования.
pub(crate) fn report_categories(
    progress: &mut impl FnMut(&BackupProgress) -> Result<()>,
    categories: Option<&crate::exclusions::CategorySizes>,
    excluded_files: u64,
    excluded_bytes: u64,
) -> Result<()> {
    let Some(c) = categories else { return Ok(()) };
    progress(&BackupProgress {
        backup_id: "prepare".into(),
        status: BackupStatus::Running,
        categories: Some(c.as_pairs()),
        excluded: Some((excluded_files, excluded_bytes)),
        ..Default::default()
    })
}

/// What an encrypted run may be asked to write, checked before anything is
/// created. Encryption is the qcow2 path with our incompatible extension, and
/// listings and retention find those images by their `.obk` name.
pub(crate) fn check_encrypted_target(
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    format: ImageFormat,
    image_path: &Path,
) -> Result<()> {
    let Some(_) = encryption else { return Ok(()) };
    if format != ImageFormat::Qcow2 {
        return Err(OwnBackupError::Config(
            tr!("engine_win.encryption_needs_qcow2").into_owned(),
        ));
    }
    if image_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
        != Some(crate::seal::obk::OBK_EXTENSION)
    {
        return Err(OwnBackupError::Config(
            tr!(
                "engine_win.encrypted_image_extension",
                ext = crate::seal::obk::OBK_EXTENSION,
                image = image_path.display()
            )
            .into_owned(),
        ));
    }
    Ok(())
}

/// The name an image is published under once its kind is settled:
/// `full_image_path` when a child was asked for and a full was made.
pub(crate) fn settled_image_path(
    image_path: &Path,
    full_image_path: Option<&Path>,
    asked: BackupType,
    made: BackupType,
) -> PathBuf {
    match full_image_path {
        Some(full) if asked != BackupType::Full && made == BackupType::Full => full.to_path_buf(),
        _ => image_path.to_path_buf(),
    }
}

/// Neither name an image may end up under can be taken already: both are
/// this run's, and the cleanup after a failure removes both.
pub(crate) fn refuse_existing_outputs(
    image_path: &Path,
    full_image_path: Option<&Path>,
) -> Result<()> {
    for path in std::iter::once(image_path).chain(full_image_path) {
        if path.exists() {
            return Err(OwnBackupError::Config(format!(
                "output image already exists: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Asked of the caller right before an image is published. A run that was
/// cancelled, or that the job runner stopped waiting for (a destination that
/// stopped answering, see core's `engine_watch`), answers with an error, and
/// the image goes with its private name instead of becoming a version.
pub(crate) fn may_publish(
    progress: &mut impl FnMut(&BackupProgress) -> Result<()>,
    bytes_written: u64,
) -> Result<()> {
    progress(&BackupProgress {
        backup_id: "publish".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written,
        speed_bps: 0,
        categories: None,
        excluded: None,
    })
}

/// The volume-root form VSS expects; None for anything that is not a volume.
/// See [`crate::source_identity::volume_root_for_snapshot`] — it lives there
/// because it is pure string work and can be tested without Windows.
pub(crate) fn volume_root_of(device_path: &str) -> Option<String> {
    crate::source_identity::volume_root_for_snapshot(device_path)
}

/// Should we try for a shadow copy at all?
///
/// On the boot medium the answer is no, and not because VSS is missing (it is —
/// WinPE carries no Volume Shadow Copy Service) but because there is nothing for
/// it to do: the system on that disk is not running, no process holds a file
/// open, nothing is written while we read. A direct read of an offline volume is
/// already a consistent copy — that is the whole point of rescue media. Asking
/// anyway would cost a COM round trip and log a failure that means nothing.
pub(crate) fn snapshot_wanted(requested: bool) -> bool {
    if !requested {
        return false;
    }
    if own_backup_common::media_mode::is_media_mode() {
        info!("boot medium: the system is offline, reading the volume directly (cold copy)");
        return false;
    }
    true
}

/// The extension a run's images carry: `.obk` when encrypted, the job's format
/// otherwise. One place, because listings, chains and the format-change guard
/// all have to agree with the writer.
pub(crate) fn container_ext(
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
) -> String {
    match encryption {
        Some(_) => crate::seal::obk::OBK_EXTENSION.into(),
        None => format.extension().into(),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::engine_core::read::ImageReadSource;
    use crate::engine_core::testkit::test_keys;
    use crate::sidecar::ChainRole;

    const MIB: u64 = 1 << 20;
    const SIZE: u64 = 4 * MIB;

    /// Write a whole block and a GPT-header-sized piece through `target`,
    /// finalize it, and read both back with the engine's own reader.
    fn write_and_read_back(
        mut target: ImageTarget,
        path: &Path,
        container: &str,
        key: Option<&crate::seal::ChainKey>,
    ) {
        assert_eq!(target.length(), SIZE);
        target.write_at(MIB, &vec![0x5A; MIB as usize]).unwrap();
        target.write_at(512, b"EFI PART").unwrap();
        target.finalize(b"sealed sidecar", 4096).unwrap();

        let mut image = ImageReadSource::open_with(path, container, key).unwrap();
        let mut head = [0u8; 8];
        image.read_at(512, &mut head).unwrap();
        assert_eq!(&head, b"EFI PART");
        let mut block = vec![0u8; MIB as usize];
        image.read_at(MIB, &mut block).unwrap();
        assert!(block.iter().all(|&b| b == 0x5A), "{container}: block lost");
        image.read_at(2 * MIB, &mut block).unwrap();
        assert!(
            block.iter().all(|&b| b == 0),
            "{container}: unwritten not zero"
        );
    }

    #[test]
    fn every_target_is_written_in_userspace_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();

        let vhdx = dir.path().join("t.vhdx");
        let writer = crate::vhdx_format::VhdxWriter::create(&vhdx, SIZE, MIB, 512).unwrap();
        write_and_read_back(ImageTarget::Vhdx { writer }, &vhdx, "vhdx", None);

        let qcow2 = dir.path().join("t.qcow2");
        let writer = crate::qcow2::Qcow2StagedWriter::create(
            &qcow2,
            crate::qcow2::Qcow2WriterOptions {
                virtual_size: SIZE,
                ..Default::default()
            },
        )
        .unwrap();
        write_and_read_back(ImageTarget::Qcow2 { writer }, &qcow2, "qcow2", None);

        let obk = dir.path().join("t.obk");
        let keys = test_keys();
        let chain = keys.start_chain().unwrap();
        let options = keys.writer_options(
            &chain,
            crate::seal::context::ImagePlan {
                backup_id: Uuid::new_v4(),
                role: ChainRole::Full,
                parent: None,
                created_at: "2026-10-18T00:00:00Z".into(),
                virtual_size: SIZE,
                source_id: Some("volume".into()),
                consolidated_increments: 0,
                chain_started_at: None,
                compression: crate::seal::obk::ObkCompression::Zstd { level: 3 },
            },
        );
        let writer =
            Box::new(crate::seal::obk::ObkStagedWriter::create(&obk, &chain.key, options).unwrap());
        write_and_read_back(ImageTarget::Obk { writer }, &obk, "obk", Some(&chain.key));
    }

    #[test]
    fn a_full_made_where_a_child_was_asked_takes_the_full_name() {
        let image = Path::new("/backups/inc.qcow2");
        let full = Path::new("/backups/full.qcow2");
        let settled = |asked, made, full| settled_image_path(image, full, asked, made);
        assert_eq!(
            settled(BackupType::Incremental, BackupType::Full, Some(full)),
            full
        );
        assert_eq!(
            settled(BackupType::Differential, BackupType::Full, Some(full)),
            full
        );
        // What was asked for came out, or there is no other name: it stays.
        assert_eq!(
            settled(BackupType::Incremental, BackupType::Incremental, Some(full)),
            image
        );
        assert_eq!(
            settled(BackupType::Full, BackupType::Full, Some(full)),
            image
        );
        assert_eq!(
            settled(BackupType::Incremental, BackupType::Full, None),
            image
        );
    }

    #[test]
    fn an_output_name_already_taken_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("inc.qcow2");
        let full = dir.path().join("full.qcow2");
        refuse_existing_outputs(&image, Some(&full)).unwrap();

        std::fs::write(&full, b"someone else's").unwrap();
        assert!(matches!(
            refuse_existing_outputs(&image, Some(&full)),
            Err(OwnBackupError::Config(_))
        ));
        refuse_existing_outputs(&image, None).unwrap();
        std::fs::write(&image, b"someone else's").unwrap();
        assert!(matches!(
            refuse_existing_outputs(&image, None),
            Err(OwnBackupError::Config(_))
        ));
    }

    #[test]
    fn an_encrypted_run_writes_qcow2_under_an_obk_name() {
        let keys = test_keys();
        check_encrypted_target(Some(&keys), ImageFormat::Qcow2, Path::new("copy.OBK")).unwrap();
        assert!(matches!(
            check_encrypted_target(Some(&keys), ImageFormat::Vhdx, Path::new("copy.obk")),
            Err(OwnBackupError::Config(_))
        ));
        assert!(matches!(
            check_encrypted_target(Some(&keys), ImageFormat::Qcow2, Path::new("copy.qcow2")),
            Err(OwnBackupError::Config(_))
        ));
        // Without encryption the name is the format's business.
        check_encrypted_target(None, ImageFormat::Vhdx, Path::new("copy.vhdx")).unwrap();

        assert_eq!(container_ext(ImageFormat::Vhdx, None), "vhdx");
        assert_eq!(container_ext(ImageFormat::Qcow2, None), "qcow2");
        assert_eq!(container_ext(ImageFormat::Vhdx, Some(&keys)), "obk");
    }

    #[test]
    fn the_caller_is_asked_before_publication_and_can_refuse() {
        let mut seen = Vec::new();
        may_publish(
            &mut |p: &BackupProgress| {
                seen.push((p.backup_id.clone(), p.bytes_written));
                Ok(())
            },
            42,
        )
        .unwrap();
        assert_eq!(seen, vec![("publish".to_string(), 42)]);

        let refused = may_publish(&mut |_: &BackupProgress| Err(OwnBackupError::Cancelled), 42);
        assert!(matches!(refused, Err(OwnBackupError::Cancelled)));
    }

    #[test]
    fn categories_are_reported_once_counted_and_not_before() {
        let mut seen = Vec::new();
        let mut progress = |p: &BackupProgress| {
            seen.push((p.backup_id.clone(), p.categories.is_some(), p.excluded));
            Ok(())
        };
        report_categories(&mut progress, None, 1, 2).unwrap();
        let sizes = crate::exclusions::CategorySizes::default();
        report_categories(&mut progress, Some(&sizes), 3, 4096).unwrap();
        assert_eq!(seen, vec![("prepare".to_string(), true, Some((3, 4096)))]);
    }
}
