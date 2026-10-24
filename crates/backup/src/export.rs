//! Save one version of a backup as a VHDX that stands on its own.
//!
//! A version is rarely one file. An increment is its own blocks plus everything
//! under it, a qcow2 copy is compressed, an `.obk` one is encrypted — and what
//! Windows, Hyper-V or another program can open by itself is a plain dynamic
//! VHDX with no parent. This reads the virtual disk exactly as that version
//! shows it, through the whole chain, and writes it into such a file.
//!
//! # What is copied
//!
//! The whole virtual disk, start to end, a megabyte at a time — not the list of
//! blocks in the sidecar. That list covers the volume's data only; the partition
//! table at the front, the description of the copy in its own partition and the
//! backup table at the back are outside it, and a disk without them is not a
//! disk. A megabyte that reads as zeros is not written: the readers answer an
//! unallocated region with zeros and do no I/O for it, and the writer leaves an
//! untouched block out of the file, so empty space costs a `memset`.
//!
//! # What the result says about itself
//!
//! Copied byte for byte, the description inside would still say "increment of
//! job such-and-such, parent file so-and-so, qcow2". Every reader of ours
//! believes that description — it would look for a parent that is not there,
//! and the job's own cleanup would count the file as one of its versions. So
//! the copy is given a description of what it now is: a full copy in VHDX,
//! with no parent, of no job, with an identity of its own. The block checksums
//! stay — a child's sidecar already carries the checksums of the whole state —
//! so the result can be verified like any other copy.
//!
//! An encrypted copy has no description partition (it is sealed inside the
//! container), so there is nowhere inside the disk to put one; it goes beside
//! the file, as the older images have it. **The result of exporting an
//! encrypted copy is not encrypted** — that is what was asked for, and the
//! caller says so to the person before starting.
//!
//! # A file that is not there until it is whole
//!
//! The work goes into `<target>.partial` and is renamed at the end. The
//! extension matters: everything that lists a folder's copies looks at
//! `.vhdx`, and the strict listing — the one retention and deletion use —
//! refuses the whole folder when one candidate cannot be read. A half-written
//! `.vhdx` left by a power cut in a job's own folder would stop that job's
//! cleanup until somebody found it.

use std::path::{Path, PathBuf};

use own_backup_common::cancel::CancelToken;
use own_backup_common::types::{BackupType, CompressionAlgo};
use own_backup_common::{tr, OwnBackupError, Result};
use uuid::Uuid;

use crate::seal::context::ArchiveKeys;
use crate::sidecar::{
    detect_container_format, meta_partition_of, read_sidecar_with_keys, sidecar_path_for,
    write_sidecar_v2_full_to, ChainRole, Sidecar,
};
use crate::vhdx_format::VhdxWriter;

const MIB: u64 = 1024 * 1024;
/// Read and written at a time: one payload block of the target.
const CHUNK: u64 = MIB;

/// What an export produced.
#[derive(Debug, Clone)]
pub struct ExportSummary {
    pub target: PathBuf,
    /// Size of the virtual disk.
    pub virtual_size: u64,
    /// How much of it holds data — what was actually written.
    pub bytes_stored: u64,
}

/// A version's virtual disk, whatever it is stored in.
enum Source {
    Vhdx(Box<crate::vhdx_bat::VhdxGuestReader>),
    Qcow2(crate::qcow2::Qcow2RandomReader),
    Obk(Box<crate::seal::obk::ObkRandomReader>),
}

impl Source {
    /// The reader and the size of the disk it reads.
    fn open(image: &Path, container: &str, keys: Option<&ArchiveKeys>) -> Result<(Self, u64)> {
        match container {
            "vhdx" => {
                // The reader follows parents by itself but does not say how
                // big the disk is; the allocation table of the file does.
                let size = crate::vhdx_bat::allocated_payload_blocks(image)?.virtual_size;
                let reader = crate::vhdx_bat::VhdxGuestReader::open(image)?;
                Ok((Source::Vhdx(Box::new(reader)), size))
            }
            "qcow2" => {
                let reader = crate::qcow2::Qcow2RandomReader::open(image)?;
                let size = reader.virtual_size();
                Ok((Source::Qcow2(reader), size))
            }
            "obk" => {
                let keys = keys.ok_or_else(|| {
                    OwnBackupError::Encryption(tr!("export.needs_key").into_owned())
                })?;
                let chain = keys.open_chain_of(image)?;
                let reader = crate::seal::obk::ObkRandomReader::open(image, &chain.key)?;
                let size = reader.virtual_size();
                Ok((Source::Obk(Box::new(reader)), size))
            }
            other => Err(OwnBackupError::ImageFormat(format!(
                "unsupported container format: {other}"
            ))),
        }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Source::Vhdx(r) => r.read_at(offset, buf),
            Source::Qcow2(r) => r.read_at(offset, buf),
            Source::Obk(r) => r.read_at(offset, buf),
        }
    }
}

/// `<target>.partial` — see the module comment for why not `.vhdx`.
fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

/// The sector size the image was laid out with. It is in the description, not
/// in the container: qcow2 and `.obk` have no notion of one.
fn sector_size_of(sidecar: &Sidecar) -> u32 {
    let recorded = sidecar
        .disk_manifest
        .as_ref()
        .map(|d| d.layout.sector_size)
        .unwrap_or(sidecar.manifest.source_disk.sector_size_logical);
    if recorded == 4096 {
        4096
    } else {
        512
    }
}

/// An empty parentless dynamic VHDX at `path`, open for writing.
///
/// On Windows the file is made by Windows itself and then written by us, as a
/// backup's is — that pair is the one that has been attached, merged and
/// restored from. Elsewhere there is only our own writer.
fn new_target(path: &Path, virtual_size: u64, sector_size: u32) -> Result<VhdxWriter> {
    #[cfg(target_os = "windows")]
    {
        crate::vhdx::create_dynamic_vhdx(
            path,
            &crate::vhdx::VhdxCreateOptions {
                max_size: virtual_size,
                logical_sector_size: sector_size,
                physical_sector_size: sector_size.max(4096),
            },
        )?;
        VhdxWriter::open_existing(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        VhdxWriter::create(path, virtual_size, MIB, sector_size)
    }
}

/// The description of the copy as the file it has become: a full copy in VHDX,
/// of no job, with no parent and an identity of its own.
fn standalone(mut sidecar: Sidecar, target: &Path) -> Sidecar {
    sidecar.info.image_file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    sidecar.info.container = "vhdx".into();
    sidecar.info.chain_role = ChainRole::Full;
    sidecar.info.parent_image_file_name = None;
    // Of no job: a job counts, cleans up and builds increments on the images
    // that name it, and this one is nobody's version.
    sidecar.info.job_id = None;
    sidecar.info.consolidated_increments = 0;
    sidecar.info.chain_started_at = None;
    sidecar.manifest.backup_id = Uuid::new_v4();
    sidecar.manifest.backup_type = BackupType::Full;
    sidecar.manifest.parent_backup_id = None;
    // A VHDX is neither compressed nor encrypted, whatever it was copied from.
    sidecar.manifest.compression_algo = CompressionAlgo::None;
    sidecar.manifest.compression_level = 0;
    sidecar.manifest.encryption_algo = None;
    sidecar.manifest.encryption_salt = None;
    sidecar
}

fn sidecar_blob(sidecar: &Sidecar) -> Result<Vec<u8>> {
    let mut blob = Vec::new();
    write_sidecar_v2_full_to(
        &mut blob,
        &sidecar.info,
        &sidecar.manifest,
        &sidecar.checksums,
        sidecar.disk_manifest.as_ref(),
    )?;
    Ok(blob)
}

/// Write the version `image` shows — all of it, through its chain — into a new
/// VHDX at `target`.
///
/// `keys` are needed for an encrypted copy and ignored otherwise. `progress`
/// is called with bytes of the virtual disk read so far and its size; an error
/// from it stops the export, as does `cancel`. Nothing is left at `target`
/// unless the export finished; an existing `target` is refused, never replaced.
pub fn export_to_vhdx(
    image: &Path,
    keys: Option<&ArchiveKeys>,
    target: &Path,
    cancel: &CancelToken,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<ExportSummary> {
    if target.exists() {
        return Err(OwnBackupError::Config(
            tr!("export.target_exists", path = target.display()).into_owned(),
        ));
    }
    let container = detect_container_format(image)?;
    let sidecar = read_sidecar_with_keys(image, keys)?;
    let (mut source, virtual_size) = Source::open(image, container, keys)?;
    let sector_size = sector_size_of(&sidecar);
    let described = standalone(sidecar, target);
    let blob = sidecar_blob(&described)?;
    // Where the description lives inside the disk — an encrypted copy has no
    // such place, and gets the description as a file beside it instead.
    let embedded_at = if container == "obk" {
        None
    } else {
        let (offset, size) = meta_partition_of(image, container)?;
        if blob.len() as u64 > size {
            return Err(OwnBackupError::ImageFormat(format!(
                "sidecar ({} bytes) does not fit the meta partition ({size} bytes)",
                blob.len()
            )));
        }
        Some(offset)
    };

    let partial = partial_path(target);
    // A leftover of an earlier attempt is ours to replace; nothing else is
    // called that.
    let _ = std::fs::remove_file(&partial);

    let written = (|| -> Result<u64> {
        let mut writer = new_target(&partial, virtual_size, sector_size)?;
        let mut buf = vec![0u8; CHUNK as usize];
        let mut offset = 0u64;
        let mut stored = 0u64;
        while offset < virtual_size {
            if cancel.is_cancelled() {
                return Err(OwnBackupError::Cancelled);
            }
            let len = CHUNK.min(virtual_size - offset) as usize;
            source.read_at(offset, &mut buf[..len])?;
            if buf[..len].iter().any(|&b| b != 0) {
                writer.write_at(offset, &buf[..len])?;
                stored += len as u64;
            }
            offset += len as u64;
            progress(offset, virtual_size)?;
        }
        // Over the copied description, through the writer that is still open:
        // those blocks are allocated by now, so this lands in place.
        if let Some(at) = embedded_at {
            writer.write_at(at, &blob)?;
        }
        writer.finish()?;
        Ok(stored)
    })();

    let stored = match written {
        Ok(stored) => stored,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };

    if embedded_at.is_none() {
        // Before the rename: a copy that shows up must already have its
        // description, and a description alone harms nobody.
        use std::io::Write;
        let description = sidecar_path_for(target);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&description)
        {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(e.into());
            }
        };
        let saved = file.write_all(&blob).and_then(|_| file.sync_all());
        drop(file);
        if let Err(e) = saved {
            let _ = std::fs::remove_file(&partial);
            let _ = std::fs::remove_file(&description);
            return Err(e.into());
        }
    }
    // The destination can appear while a large disk is being copied. Publish
    // without replacing it; a check before a normal rename has a race.
    let staged = tempfile::TempPath::try_from_path(partial.clone())?;
    if let Err(e) = staged.persist_noclobber(target) {
        if embedded_at.is_none() {
            let _ = std::fs::remove_file(sidecar_path_for(target));
        }
        return Err(e.error.into());
    }

    Ok(ExportSummary {
        target: target.to_path_buf(),
        virtual_size,
        bytes_stored: stored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::{list_chains, list_job_chains};
    use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
    use crate::qcow2::{Qcow2RandomReader, Qcow2StagedWriter, Qcow2WriterOptions};
    use crate::sidecar::{write_embedded_sidecar, BlockChecksum, SidecarInfo};
    use crate::vhdx_format::VhdxReader;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::image::ImageManifest;
    use own_backup_common::types::*;

    const CS: u64 = 65536;
    /// 1 MiB of partition table, 1 MiB of volume, 512 KiB for the description,
    /// two megabytes of nothing at all, and the backup table in the last one.
    const VOLUME_LEN: u64 = MIB;
    const META_SIZE: u64 = 512 * 1024;
    const META_OFFSET: u64 = DEFAULT_PARTITION_OFFSET + VOLUME_LEN;
    const VSIZE: u64 = 6 * MIB;

    fn manifest(created_at: &str, kind: BackupType, parent: Option<Uuid>) -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::new_v4(),
            created_at: created_at.into(),
            backup_type: kind,
            parent_backup_id: parent,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\E:".into(),
                stable_device: String::new(),
                model: String::new(),
                serial: String::new(),
                size_bytes: VOLUME_LEN,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "Volume".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::Zstd,
            compression_level: 3,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 4096,
            total_blocks: VOLUME_LEN / 4096,
            used_blocks: 8,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    /// A qcow2 image with a real partition table, the description partition
    /// and its embedded sidecar, and `fill` written into `data_clusters`.
    fn put_image(
        dir: &Path,
        name: &str,
        backing: Option<&str>,
        parent_id: Option<Uuid>,
        data_clusters: &[(u64, u8)],
        checksums: &[(u64, u64)],
    ) -> (PathBuf, Uuid) {
        let path = dir.join(name);
        let mut writer = Qcow2StagedWriter::create(
            &path,
            Qcow2WriterOptions {
                virtual_size: VSIZE,
                backing_file: backing.map(|s| s.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        // Only the root of a chain lays the table down; children inherit it.
        if backing.is_none() {
            let ops = gpt_write_ops(&GptLayout {
                disk_size: VSIZE,
                sector_size: 512,
                partition_offset: DEFAULT_PARTITION_OFFSET,
                partition_size: VOLUME_LEN,
                disk_guid: Uuid::new_v4(),
                partition_guid: Uuid::new_v4(),
                partition_name: "OwnBackupData".into(),
                meta_partition_size: Some(META_SIZE),
                meta_partition_guid: Some(Uuid::new_v4()),
            })
            .unwrap();
            for op in ops {
                writer.write_at(op.offset, &op.bytes).unwrap();
            }
        }
        for &(index, fill) in data_clusters {
            writer
                .write_at(
                    DEFAULT_PARTITION_OFFSET + index * CS,
                    &vec![fill; CS as usize],
                )
                .unwrap();
        }
        writer
            .write_at(META_OFFSET, &vec![0xEEu8; META_SIZE as usize])
            .unwrap();
        writer.finish().unwrap();

        let (mut info, kind) = match backing {
            None => (
                SidecarInfo::new_full(name, DEFAULT_PARTITION_OFFSET),
                BackupType::Full,
            ),
            Some(parent) => (
                SidecarInfo::new_child(
                    name,
                    ChainRole::Incremental,
                    parent,
                    DEFAULT_PARTITION_OFFSET,
                ),
                BackupType::Incremental,
            ),
        };
        info.container = "qcow2".into();
        info.job_id = Some("j1".into());
        let manifest = manifest("2026-01-01T00:00:00Z", kind, parent_id);
        let backup_id = manifest.backup_id;
        let sidecar = Sidecar {
            info,
            manifest,
            checksums: checksums
                .iter()
                .map(|&(block_number, checksum)| BlockChecksum {
                    block_number,
                    checksum,
                })
                .collect(),
            disk_manifest: None,
        };
        write_embedded_sidecar(&path, "qcow2", &sidecar).unwrap();
        (path, backup_id)
    }

    /// base ← inc1 ← inc2, each overwriting part of what is under it.
    fn chain_of_three(dir: &Path) -> PathBuf {
        let (_, base_id) = put_image(
            dir,
            "j_C_full_1.qcow2",
            None,
            None,
            &[(1, 0x11), (2, 0x11)],
            &[(0, 100), (1, 101)],
        );
        let (_, inc1_id) = put_image(
            dir,
            "j_C_inc_2.qcow2",
            Some("j_C_full_1.qcow2"),
            Some(base_id),
            &[(2, 0x22), (3, 0x22)],
            &[(0, 100), (1, 201), (2, 202)],
        );
        put_image(
            dir,
            "j_C_inc_3.qcow2",
            Some("j_C_inc_2.qcow2"),
            Some(inc1_id),
            &[(3, 0x33), (4, 0x33)],
            &[(0, 100), (1, 201), (3, 303)],
        )
        .0
    }

    fn whole_disk_through_chain(image: &Path) -> Vec<u8> {
        let mut reader = Qcow2RandomReader::open(image).unwrap();
        let mut disk = vec![0u8; VSIZE as usize];
        reader.read_at(0, &mut disk).unwrap();
        disk
    }

    fn whole_vhdx(image: &Path) -> Vec<u8> {
        let mut reader = VhdxReader::open(image).unwrap();
        let mut disk = vec![0u8; VSIZE as usize];
        reader.read_at(0, &mut disk).unwrap();
        disk
    }

    fn export(image: &Path, target: &Path) -> Result<ExportSummary> {
        export_to_vhdx(image, None, target, &CancelToken::default(), |_, _| Ok(()))
    }

    fn encrypted_image(dir: &Path) -> (PathBuf, ArchiveKeys) {
        use crate::seal::context::ImagePlan;
        use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
        use crate::seal::obk::{ObkCompression, ObkWriter};
        use crate::seal::recovery::RecoveryKey;

        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let envelope = SealedPrivateKey::seal(
            "j1",
            &pair,
            "archive password",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        let keys = ArchiveKeys::from_private(envelope, pair.private).unwrap();
        let chain = keys.start_chain().unwrap();
        let image = dir.join("full.obk");
        let mut info = SidecarInfo::new_full("full.obk", DEFAULT_PARTITION_OFFSET);
        info.container = "obk".into();
        info.job_id = Some("j1".into());
        let sidecar = Sidecar {
            info,
            manifest: manifest("2026-01-01T00:00:00Z", BackupType::Full, None),
            checksums: vec![],
            disk_manifest: None,
        };
        let mut writer = ObkWriter::create(
            &image,
            &chain.key,
            keys.writer_options(
                &chain,
                ImagePlan {
                    backup_id: sidecar.manifest.backup_id,
                    role: ChainRole::Full,
                    parent: None,
                    created_at: sidecar.manifest.created_at.clone(),
                    virtual_size: VSIZE,
                    source_id: None,
                    consolidated_increments: 0,
                    chain_started_at: None,
                    compression: ObkCompression::Zstd { level: 3 },
                },
            ),
        )
        .unwrap();
        writer
            .write_cluster(DEFAULT_PARTITION_OFFSET / CS, &vec![0x5A; CS as usize])
            .unwrap();
        writer.finish(&sidecar_blob(&sidecar).unwrap()).unwrap();
        (image, keys)
    }

    #[test]
    fn an_encrypted_image_requires_a_key_and_exports_plain_data_with_a_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let (image, keys) = encrypted_image(dir.path());
        let target = dir.path().join("saved.vhdx");
        assert!(export(&image, &target).is_err());
        assert!(!target.exists());
        export_to_vhdx(
            &image,
            Some(&keys),
            &target,
            &CancelToken::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        let disk = whole_vhdx(&target);
        assert_eq!(
            &disk[DEFAULT_PARTITION_OFFSET as usize..(DEFAULT_PARTITION_OFFSET + CS) as usize],
            &vec![0x5A; CS as usize]
        );
        let saved = read_sidecar_with_keys(&target, None).unwrap();
        assert_eq!(saved.info.container, "vhdx");
        assert_eq!(saved.info.job_id, None);
        assert_eq!(saved.manifest.encryption_algo, None);
        assert!(sidecar_path_for(&target).exists());
    }

    #[test]
    fn an_encrypted_export_does_not_replace_an_existing_description() {
        let dir = tempfile::tempdir().unwrap();
        let (image, keys) = encrypted_image(dir.path());
        let target = dir.path().join("saved.vhdx");
        let description = sidecar_path_for(&target);
        std::fs::write(&description, b"somebody's description").unwrap();
        assert!(export_to_vhdx(
            &image,
            Some(&keys),
            &target,
            &CancelToken::default(),
            |_, _| Ok(())
        )
        .is_err());
        assert_eq!(
            std::fs::read(&description).unwrap(),
            b"somebody's description"
        );
        assert!(!target.exists());
        assert!(!partial_path(&target).exists());
    }

    /// The point of the whole thing: the one file reads what the three did.
    #[test]
    fn the_last_increment_becomes_one_file_that_reads_the_same_disk() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        let before = whole_disk_through_chain(&leaf);
        let target = dir.path().join("saved.vhdx");

        let summary = export(&leaf, &target).unwrap();
        assert_eq!(summary.virtual_size, VSIZE);
        // Four megabytes hold something; the two empty ones are not written.
        assert_eq!(summary.bytes_stored, 4 * MIB);
        assert!(std::fs::metadata(&target).unwrap().len() < VSIZE + 4 * MIB);
        assert!(!partial_path(&target).exists());

        let after = whole_vhdx(&target);
        let meta = META_OFFSET as usize..(META_OFFSET + META_SIZE) as usize;
        // Everything but the description, which is SUPPOSED to differ.
        assert!(
            before[..meta.start] == after[..meta.start],
            "table and volume"
        );
        assert!(
            before[meta.end..] == after[meta.end..],
            "tail and backup table"
        );
        // And the volume is the three layers on top of each other, not one.
        let cluster = |i: u64| after[(DEFAULT_PARTITION_OFFSET + i * CS) as usize];
        assert_eq!(
            [cluster(1), cluster(2), cluster(3), cluster(4)],
            [0x11, 0x22, 0x33, 0x33]
        );
    }

    #[test]
    fn the_file_describes_itself_as_a_full_copy_of_no_job_with_no_parent() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        let source = read_sidecar_with_keys(&leaf, None).unwrap();
        let target = dir.path().join("saved.vhdx");
        export(&leaf, &target).unwrap();

        let saved = read_sidecar_with_keys(&target, None).unwrap();
        assert_eq!(saved.info.container, "vhdx");
        assert_eq!(saved.info.image_file_name, "saved.vhdx");
        assert_eq!(saved.info.chain_role, ChainRole::Full);
        assert_eq!(saved.info.parent_image_file_name, None);
        assert_eq!(saved.info.job_id, None);
        assert_eq!(saved.manifest.backup_type, BackupType::Full);
        assert_eq!(saved.manifest.parent_backup_id, None);
        assert_ne!(saved.manifest.backup_id, source.manifest.backup_id);
        assert_eq!(saved.manifest.compression_algo, CompressionAlgo::None);
        // The checksums of the whole state travel with it.
        let pairs = |s: &Sidecar| -> Vec<(u64, u64)> {
            s.checksums
                .iter()
                .map(|c| (c.block_number, c.checksum))
                .collect()
        };
        assert_eq!(pairs(&saved), pairs(&source));
        assert_eq!(
            saved.info.volume_data_offset,
            source.info.volume_data_offset
        );
    }

    /// Saved into the job's own folder — the likeliest place — the file must
    /// not become one of the job's versions: cleanup would count it, and the
    /// next increment could be built on it.
    #[test]
    fn saved_beside_the_chain_it_is_not_one_of_the_jobs_versions() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        export(&leaf, &dir.path().join("saved.vhdx")).unwrap();

        let of_job = list_job_chains(dir.path(), "j1").unwrap();
        assert_eq!(of_job.len(), 1);
        assert_eq!(of_job[0].images.len(), 3);
        let of_nobody = list_chains(dir.path(), None).unwrap();
        assert_eq!(of_nobody.len(), 1);
        assert_eq!(of_nobody[0].images.len(), 1);
        assert!(of_nobody[0].images[0].image_path.ends_with("saved.vhdx"));
    }

    #[test]
    fn an_existing_file_is_refused_and_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        let target = dir.path().join("saved.vhdx");
        std::fs::write(&target, b"somebody's file").unwrap();
        assert!(export(&leaf, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"somebody's file");
        // The chain itself must be refused as a target too — it exists.
        assert!(export(&leaf, &leaf).is_err());
    }

    #[test]
    fn a_file_created_during_export_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        let target = dir.path().join("saved.vhdx");
        let result = export_to_vhdx(
            &leaf,
            None,
            &target,
            &CancelToken::default(),
            |done, total| {
                if done == total {
                    std::fs::write(&target, b"somebody's new file")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"somebody's new file");
        assert!(!partial_path(&target).exists());
    }

    #[test]
    fn a_cancelled_export_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = chain_of_three(dir.path());
        let target = dir.path().join("saved.vhdx");
        let cancel = CancelToken::default();
        let result = export_to_vhdx(&leaf, None, &target, &cancel, |done, _| {
            if done >= 2 * MIB {
                cancel.cancel();
            }
            Ok(())
        });
        assert!(
            matches!(result, Err(OwnBackupError::Cancelled)),
            "{result:?}"
        );
        assert!(!target.exists());
        assert!(!partial_path(&target).exists());
    }
}
