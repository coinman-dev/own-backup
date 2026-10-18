//! Fixtures for the engine_core tests: a manifest, sidecars describing a known
//! volume, and job keys cheap enough to make in every test.

use std::path::Path;

use uuid::Uuid;

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::types::{BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable};

use crate::checksum::compute_block_checksum;
use crate::sidecar::{BlockChecksum, SidecarInfo};

/// The manifest of a `volume_len`-byte source copied in `block_size` blocks.
pub(crate) fn manifest(volume_len: u64, block_size: u64) -> ImageManifest {
    let blocks = volume_len.div_ceil(block_size);
    ImageManifest {
        backup_id: Uuid::new_v4(),
        created_at: "2026-10-18T00:00:00Z".into(),
        backup_type: BackupType::Full,
        parent_backup_id: None,
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: "test".into(),
            model: "test".into(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: 512,
            sector_size_physical: 4096,
            media_type: "Disk".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: Uuid::nil().to_string(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
        partition_table: PartitionTable::Gpt,
        compression_algo: CompressionAlgo::None,
        compression_level: 0,
        encryption_algo: None,
        encryption_salt: None,
        block_size: block_size as u32,
        total_blocks: blocks,
        used_blocks: blocks,
        backup_mode: BackupMode::UsedSectors,
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    }
}

/// One checksum per block of `volume` — the whole state a sidecar describes.
pub(crate) fn checksums(volume: &[u8], block_size: u64) -> Vec<BlockChecksum> {
    volume
        .chunks(block_size as usize)
        .enumerate()
        .map(|(n, data)| BlockChecksum {
            block_number: n as u64,
            checksum: compute_block_checksum(data),
        })
        .collect()
}

/// The adjacent `.obkm` of `image`, describing `volume` block by block.
pub(crate) fn write_adjacent_sidecar(
    image: &Path,
    info: &SidecarInfo,
    volume: &[u8],
    block_size: u64,
) {
    crate::sidecar::write_sidecar(
        &crate::sidecar::sidecar_path_for(image),
        info,
        &manifest(volume.len() as u64, block_size),
        &checksums(volume, block_size),
    )
    .unwrap();
}

/// A job's keys with the light test KDF — the production profile costs
/// 64 MiB and seconds per test, and nothing here is about the KDF.
pub(crate) fn test_keys() -> crate::seal::context::ArchiveKeys {
    use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
    use crate::seal::recovery::RecoveryKey;
    let pair = JobKeyPair::generate().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let envelope = SealedPrivateKey::seal(
        "job-engine-core",
        &pair,
        "пароль архива",
        &recovery,
        &KdfParams::test_profile(),
    )
    .unwrap();
    crate::seal::context::ArchiveKeys::from_private(envelope, pair.private).unwrap()
}
