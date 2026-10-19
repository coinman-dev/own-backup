//! Whether an earlier image can be the parent of the one about to be written.

use std::path::Path;

use own_backup_common::types::ImageFormat;
use own_backup_common::{OwnBackupError, Result};

use super::read::load_sidecar_with_keys;
use super::target::container_ext;
use crate::sidecar::{read_embedded_sidecar, Sidecar};

/// Check that `parent_image` can serve as the parent of a new child image for
/// the volume being backed up.
pub(crate) fn validate_parent(
    parent_image: &Path,
    block_size: u32,
    volume_len: u64,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent image not found: {}",
            parent_image.display()
        )));
    }
    let sidecar = load_sidecar_with_keys(parent_image, keys)?;
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    if sidecar.manifest.source_disk.size_bytes != volume_len {
        return Err(OwnBackupError::Config(format!(
            "volume size changed since the parent backup ({} -> {volume_len} bytes)",
            sidecar.manifest.source_disk.size_bytes
        )));
    }
    Ok(sidecar)
}

/// Why a child must not be written over a parent that is otherwise fine, or
/// `None` when it may. The answer is a reason for the log: the engine then
/// writes a full copy, as for any other unusable chain.
///
/// - A qcow2 or `.obk` child writes whole blocks into its clusters, and a
///   cluster it writes only part of is filled with zeros, not with the
///   parent's bytes (`qcow2/io.rs`): its blocks must cover whole clusters.
/// - A VHDX child only where the platform makes children Windows accepts
///   ([`super::platform::EnginePlatform::VHDX_CHAINS`]), and never over a
///   parent another system wrote (decision R2): a cross-system VHDX chain has
///   not been checked on both sides yet.
pub(crate) fn child_refusal(
    format: ImageFormat,
    encrypted: bool,
    block_size: u32,
    parent: &Sidecar,
    source_os: &str,
    vhdx_chains: bool,
) -> Option<String> {
    let cluster = 1u32 << crate::qcow2::DEFAULT_CLUSTER_BITS;
    match (format, encrypted) {
        (ImageFormat::Qcow2, _) | (_, true) if !block_size.is_multiple_of(cluster) => {
            Some(format!(
                "block size {block_size} does not cover whole {cluster}-byte clusters — \
             a child would zero-fill the parent's bytes"
            ))
        }
        (ImageFormat::Vhdx, false) if !vhdx_chains => {
            Some("VHDX chains are not written on this system yet (CHECKS 12)".into())
        }
        (ImageFormat::Vhdx, false) if parent.manifest.source_os != source_os => Some(format!(
            "the parent VHDX was written on '{}', this copy on '{source_os}' — \
             a VHDX chain does not cross systems (decision R2)",
            parent.manifest.source_os
        )),
        _ => None,
    }
}

/// Validate that `parent_image` can serve as the parent of a new whole-disk
/// child: it exists, is itself a whole-disk image (carries an embedded disk
/// manifest), uses the same block size and container format. The image-size
/// (geometry) match is checked by the caller once the child's plan is known.
/// Returns the parent's full sidecar (its checksums seed the incremental skip).
pub(crate) fn validate_disk_parent(
    parent_image: &Path,
    block_size: u32,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent whole-disk image not found: {}",
            parent_image.display()
        )));
    }
    // An encrypted parent keeps its sidecar sealed inside the container, so
    // continuing that chain needs the job's key — the same key the new image
    // will be written with.
    let sidecar = match encryption {
        Some(keys) => {
            let chain = keys.open_chain_of(parent_image)?;
            crate::sidecar::read_sealed_sidecar(parent_image, &chain.key)?
        }
        None => {
            let ext = parent_image
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            read_embedded_sidecar(parent_image, &ext)?
        }
    };
    if sidecar.disk_manifest.is_none() {
        return Err(OwnBackupError::Config(format!(
            "parent {} is not a whole-disk image (no embedded disk manifest)",
            parent_image.display()
        )));
    }
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    let want = container_ext(format, encryption);
    if sidecar.info.container != want {
        return Err(OwnBackupError::Config(format!(
            "parent image is '{}' but the requested format is '{want}' — a backup \
             chain cannot change container format",
            sidecar.info.container,
        )));
    }
    Ok(sidecar)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use own_backup_common::gpt::{DiskBackupManifest, DiskLayout};

    use super::*;
    use crate::engine_core::testkit::{manifest, test_keys, write_adjacent_sidecar};
    use crate::seal::context::ArchiveKeys;
    use crate::sidecar::{ChainRole, SidecarInfo};

    const BLOCK: u64 = 64 * 1024;
    const VOLUME: u64 = 16 * BLOCK;

    /// A per-volume qcow2 copy of a `VOLUME`-byte source, sidecar beside it.
    fn volume_copy(dir: &Path) -> PathBuf {
        let image = dir.join("full.qcow2");
        let file = std::fs::File::create(&image).unwrap();
        crate::qcow2::Qcow2Writer::new(
            std::io::BufWriter::new(file),
            crate::qcow2::Qcow2WriterOptions {
                virtual_size: VOLUME,
                ..Default::default()
            },
        )
        .unwrap()
        .finish()
        .unwrap();
        let info = SidecarInfo {
            container: "qcow2".into(),
            ..SidecarInfo::new_full("full.qcow2", 0)
        };
        write_adjacent_sidecar(&image, &info, &vec![0u8; VOLUME as usize], BLOCK);
        image
    }

    /// An encrypted copy whose sealed sidecar says `container` and carries a
    /// disk manifest when `whole_disk`.
    fn sealed_copy(
        dir: &Path,
        name: &str,
        keys: &ArchiveKeys,
        container: &str,
        whole_disk: bool,
    ) -> PathBuf {
        let image = dir.join(name);
        let chain = keys.start_chain().unwrap();
        let options = keys.writer_options(
            &chain,
            crate::seal::context::ImagePlan {
                backup_id: Uuid::new_v4(),
                role: ChainRole::Full,
                parent: None,
                created_at: "2026-10-18T00:00:00Z".into(),
                virtual_size: VOLUME,
                source_id: Some("disk3".into()),
                consolidated_increments: 0,
                chain_started_at: None,
                compression: crate::seal::obk::ObkCompression::Zstd { level: 3 },
            },
        );
        let writer =
            crate::seal::obk::ObkStagedWriter::create(&image, &chain.key, options).unwrap();
        let info = SidecarInfo {
            container: container.into(),
            ..SidecarInfo::new_full(name, 0)
        };
        let disk_manifest = whole_disk.then(|| DiskBackupManifest {
            format_version: DiskBackupManifest::FORMAT_VERSION,
            created_at: "2026-10-18T00:00:00Z".into(),
            job_id: None,
            disk_number: 3,
            disk_model: "test".into(),
            layout: DiskLayout {
                disk_guid: Uuid::nil().to_string(),
                sector_size: 512,
                size_bytes: VOLUME,
                partitions: vec![],
            },
            images: vec![],
            source_disk: None,
        });
        let mut sealed = Vec::new();
        crate::sidecar::write_sidecar_v2_full_to(
            &mut sealed,
            &info,
            &manifest(VOLUME, BLOCK),
            &[],
            disk_manifest.as_ref(),
        )
        .unwrap();
        writer.finish(&sealed).unwrap();
        image
    }

    fn is_config<T>(r: Result<T>) -> bool {
        matches!(r, Err(OwnBackupError::Config(_)))
    }

    #[test]
    fn a_volume_parent_must_exist_and_match_the_block_and_volume_size() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("full.qcow2");
        assert!(is_config(validate_parent(
            &missing,
            BLOCK as u32,
            VOLUME,
            None
        )));

        let parent = volume_copy(dir.path());
        let sidecar = validate_parent(&parent, BLOCK as u32, VOLUME, None).unwrap();
        assert_eq!(sidecar.manifest.block_size, BLOCK as u32);
        assert!(is_config(validate_parent(
            &parent,
            2 * BLOCK as u32,
            VOLUME,
            None
        )));
        assert!(
            is_config(validate_parent(&parent, BLOCK as u32, VOLUME + BLOCK, None)),
            "a resized volume starts a new chain"
        );
    }

    #[test]
    fn a_whole_disk_parent_must_carry_a_disk_manifest_and_keep_the_container() {
        let dir = tempfile::tempdir().unwrap();
        let keys = test_keys();
        let bs = BLOCK as u32;
        let missing = dir.path().join("none.obk");
        assert!(is_config(validate_disk_parent(
            &missing,
            bs,
            ImageFormat::Qcow2,
            Some(&keys)
        )));

        let disk = sealed_copy(dir.path(), "disk.obk", &keys, "obk", true);
        let sidecar = validate_disk_parent(&disk, bs, ImageFormat::Qcow2, Some(&keys)).unwrap();
        assert!(sidecar.disk_manifest.is_some());
        assert!(is_config(validate_disk_parent(
            &disk,
            2 * bs,
            ImageFormat::Qcow2,
            Some(&keys)
        )));
        // A sealed sidecar cannot be read without the job's key.
        assert!(validate_disk_parent(&disk, bs, ImageFormat::Qcow2, None).is_err());

        let volume = sealed_copy(dir.path(), "volume.obk", &keys, "obk", false);
        assert!(
            is_config(validate_disk_parent(
                &volume,
                bs,
                ImageFormat::Qcow2,
                Some(&keys)
            )),
            "a per-volume image is no parent for a whole-disk child"
        );

        let other = sealed_copy(dir.path(), "other.obk", &keys, "qcow2", true);
        assert!(
            is_config(validate_disk_parent(
                &other,
                bs,
                ImageFormat::Qcow2,
                Some(&keys)
            )),
            "a chain cannot change container format"
        );
    }

    #[test]
    fn a_child_is_refused_where_it_would_not_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut parent = load_sidecar_with_keys(&volume_copy(dir.path()), None).unwrap();
        parent.manifest.source_os = "linux".into();
        let refuse = |format, encrypted, block, chains| {
            child_refusal(format, encrypted, block, &parent, "linux", chains)
        };
        let mib = 1024 * 1024;
        assert_eq!(refuse(ImageFormat::Qcow2, false, mib, false), None);
        assert_eq!(refuse(ImageFormat::Qcow2, true, mib, false), None);
        assert_eq!(refuse(ImageFormat::Qcow2, false, 64 * 1024, false), None);
        // Part of a cluster: the rest would be zeros, not the parent's bytes.
        assert!(refuse(ImageFormat::Qcow2, false, 4096, true).is_some());
        assert!(
            refuse(ImageFormat::Vhdx, true, 4096, true).is_some(),
            ".obk is qcow2-based"
        );
        // VHDX: only where the platform makes children, never across systems.
        assert_eq!(refuse(ImageFormat::Vhdx, false, 4096, true), None);
        assert!(refuse(ImageFormat::Vhdx, false, mib, false).is_some());
        assert!(child_refusal(ImageFormat::Vhdx, false, mib, &parent, "windows", true).is_some());
        assert_eq!(
            child_refusal(ImageFormat::Qcow2, false, mib, &parent, "windows", true),
            None
        );
    }
}
