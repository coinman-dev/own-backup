use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::disk::DiskInfo;
use crate::preflight::PreflightReport;
use crate::types::{BackupMode, BackupType, BootMode, CompressionAlgo, PartitionTable};

pub const OBK_MAGIC: &[u8; 8] = b"OWNBKUP\0";
pub const OBK_VERSION: u32 = 1;
pub const DEFAULT_BLOCK_SIZE: u32 = 1_048_576; // 1 MiB

/// Fixed-size header at the start of .obk file (48 bytes).
/// Written as placeholder first, then rewritten at finish with actual offsets.
#[derive(Debug, Clone, Copy)]
pub struct ImageHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub flags: u32,
    pub manifest_offset: u64,
    pub manifest_size: u64,
    pub block_index_offset: u64,
    pub block_index_size: u64,
}

impl ImageHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn placeholder() -> Self {
        Self {
            magic: *OBK_MAGIC,
            version: OBK_VERSION,
            flags: 0,
            manifest_offset: 0,
            manifest_size: 0,
            block_index_offset: 0,
            block_index_size: 0,
        }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.magic);
        buf[8..12].copy_from_slice(&self.version.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf[16..24].copy_from_slice(&self.manifest_offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.manifest_size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.block_index_offset.to_le_bytes());
        buf[40..48].copy_from_slice(&self.block_index_size.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            magic: buf[0..8].try_into().unwrap(),
            version: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            manifest_offset: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            manifest_size: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            block_index_offset: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            block_index_size: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
        }
    }

    /// Maximum allowed manifest size (100 MB) to prevent OOM from malformed files.
    pub const MAX_MANIFEST_SIZE: u64 = 100 * 1024 * 1024;
    /// Maximum allowed block count to prevent OOM.
    pub const MAX_BLOCK_COUNT: u64 = 1_000_000_000; // 1 billion blocks

    pub fn validate(&self) -> crate::error::Result<()> {
        let magic = self.magic;
        if &magic != OBK_MAGIC {
            return Err(crate::error::OwnBackupError::ImageFormat(
                "Invalid magic bytes".into(),
            ));
        }
        let version = self.version;
        if version != OBK_VERSION {
            return Err(crate::error::OwnBackupError::ImageFormat(format!(
                "Unsupported version: {}, expected {}",
                version, OBK_VERSION
            )));
        }
        if self.manifest_size > Self::MAX_MANIFEST_SIZE {
            return Err(crate::error::OwnBackupError::ImageFormat(format!(
                "Manifest size {} exceeds maximum {}",
                self.manifest_size,
                Self::MAX_MANIFEST_SIZE
            )));
        }
        if self.manifest_offset < Self::SIZE as u64 {
            return Err(crate::error::OwnBackupError::ImageFormat(
                "Manifest offset is within header area".into(),
            ));
        }
        if self.block_index_offset < Self::SIZE as u64 {
            return Err(crate::error::OwnBackupError::ImageFormat(
                "Block index offset is within header area".into(),
            ));
        }
        // Check for overflow
        if self.manifest_offset.checked_add(self.manifest_size).is_none() {
            return Err(crate::error::OwnBackupError::ImageFormat(
                "Manifest offset + size overflows".into(),
            ));
        }
        if self.block_index_offset.checked_add(self.block_index_size).is_none() {
            return Err(crate::error::OwnBackupError::ImageFormat(
                "Block index offset + size overflows".into(),
            ));
        }
        Ok(())
    }
}

/// State of the volume's own encryption (BitLocker / LUKS).
/// Distinct from archive encryption (our AES-256-GCM on the .obk file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeEncryptionKind {
    /// Windows BitLocker
    BitLocker,
    /// Linux LUKS1
    Luks1,
    /// Linux LUKS2
    Luks2,
    /// Other (name stored in `note`)
    Other,
}

/// How the encrypted volume was captured in the backup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeCaptureMode {
    /// Blocks read through the OS/filesystem — backup contains plaintext data.
    /// After restore, volume encryption must be re-enabled manually.
    Plaintext,
    /// Raw encrypted sectors copied verbatim — backup preserves encryption.
    /// Original credentials needed to access data after restore.
    SectorBySector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeEncryptionState {
    pub kind: VolumeEncryptionKind,
    pub capture_mode: VolumeCaptureMode,
    /// LUKS header backup (base64-encoded). Critical: without this a damaged
    /// LUKS header makes the entire volume permanently inaccessible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub luks_header_backup_b64: Option<String>,
    /// Human-readable note for the restore wizard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    pub backup_id: Uuid,
    pub created_at: String,
    pub backup_type: BackupType,
    pub parent_backup_id: Option<Uuid>,
    pub source_os: String,
    pub source_disk: DiskInfo,
    pub boot_mode: BootMode,
    pub partition_table: PartitionTable,
    pub compression_algo: CompressionAlgo,
    pub compression_level: i32,
    /// Algorithm name for archive-level encryption (e.g., "AES-256-GCM").
    pub encryption_algo: Option<String>,
    /// Per-file random salt used for key derivation. Unique per backup.
    pub encryption_salt: Option<Vec<u8>>,
    pub block_size: u32,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub backup_mode: BackupMode,
    pub checksum_algorithm: String,
    pub preflight_report: Option<PreflightReport>,
    /// State of the source volume's own encryption (BitLocker / LUKS).
    /// `None` if the source volume was not encrypted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_encryption: Option<VolumeEncryptionState>,
}

/// One entry per block in the block index section.
/// Serialized as fixed-size binary records for fast seeking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BlockIndexEntry {
    pub block_number: u64,
    pub offset_in_file: u64,
    pub original_size: u32,
    pub compressed_size: u32,
    pub is_compressed: bool,
    pub checksum: u64,
}

impl BlockIndexEntry {
    /// Binary size of one entry when serialized (33 bytes).
    pub const BINARY_SIZE: usize = 8 + 8 + 4 + 4 + 1 + 8;

    pub fn to_bytes(&self) -> [u8; Self::BINARY_SIZE] {
        let mut buf = [0u8; Self::BINARY_SIZE];
        buf[0..8].copy_from_slice(&self.block_number.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset_in_file.to_le_bytes());
        buf[16..20].copy_from_slice(&self.original_size.to_le_bytes());
        buf[20..24].copy_from_slice(&self.compressed_size.to_le_bytes());
        buf[24] = self.is_compressed as u8;
        buf[25..33].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::BINARY_SIZE]) -> Self {
        Self {
            block_number: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            offset_in_file: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            original_size: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            compressed_size: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
            is_compressed: buf[24] != 0,
            checksum: u64::from_le_bytes(buf[25..33].try_into().unwrap()),
        }
    }
}

/// Footer at the end of the .obk file for quick access to offsets.
#[derive(Debug, Clone, Copy)]
pub struct ImageFooter {
    pub magic: [u8; 8],
    pub manifest_offset: u64,
    pub block_index_offset: u64,
    pub block_count: u64,
}

impl ImageFooter {
    pub const SIZE: usize = 8 + 8 + 8 + 8;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.magic);
        buf[8..16].copy_from_slice(&self.manifest_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.block_index_offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.block_count.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            magic: buf[0..8].try_into().unwrap(),
            manifest_offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            block_index_offset: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            block_count: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
        }
    }
}

// Flag constants for ImageHeader.flags
pub const FLAG_ENCRYPTED: u32 = 1 << 0;
pub const FLAG_COMPRESSED: u32 = 1 << 1;
