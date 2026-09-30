use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootMode {
    Uefi,
    LegacyBios,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionTable {
    Gpt,
    Mbr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskType {
    Basic,
    Dynamic,
    StorageSpaces,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSystem {
    Ntfs,
    Fat32,
    ExFat,
    Ext4,
    Xfs,
    Btrfs,
    ReFs,
    Unknown,
}

impl FileSystem {
    /// Filesystems Windows can mount, format and walk file-by-file. Everything
    /// else can only be moved as raw sectors — Windows has no driver for it and
    /// we are not writing one (D4).
    pub fn is_windows_family(self) -> bool {
        matches!(
            self,
            FileSystem::Ntfs | FileSystem::Fat32 | FileSystem::ExFat | FileSystem::ReFs
        )
    }

    /// Display name as users know it. The names themselves are the same in
    /// every language; only "unknown" is words.
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            FileSystem::Ntfs => Cow::Borrowed("NTFS"),
            FileSystem::Fat32 => Cow::Borrowed("FAT32"),
            FileSystem::ExFat => Cow::Borrowed("exFAT"),
            FileSystem::Ext4 => Cow::Borrowed("ext4"),
            FileSystem::Xfs => Cow::Borrowed("XFS"),
            FileSystem::Btrfs => Cow::Borrowed("Btrfs"),
            FileSystem::ReFs => Cow::Borrowed("ReFS"),
            FileSystem::Unknown => crate::tr!("types.fs_unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupportTier {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupType {
    Full,
    Incremental,
    Differential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionAlgo {
    None,
    Zstd,
}

/// Backup image container format (decision #13, 2026-07-07).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// Dynamic VHDX — Windows-native mounting, NTFS/ReFS true removal,
    /// AVHDX chains; no compression.
    #[default]
    Vhdx,
    /// qcow2 with per-cluster zstd compression — cross-platform, mounted via
    /// the service; NTFS/ReFS exclusions are zeroing-only.
    Qcow2,
}

impl ImageFormat {
    /// Conventional image file extension (`vhdx` / `qcow2`).
    pub fn extension(&self) -> &'static str {
        match self {
            ImageFormat::Vhdx => "vhdx",
            ImageFormat::Qcow2 => "qcow2",
        }
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

impl std::str::FromStr for ImageFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "vhdx" => Ok(ImageFormat::Vhdx),
            "qcow2" => Ok(ImageFormat::Qcow2),
            other => Err(format!(
                "unknown image format '{other}' (expected vhdx or qcow2)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupMode {
    UsedSectors,
    SectorBySector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Validating,
}

impl std::fmt::Display for BootMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootMode::Uefi => write!(f, "UEFI"),
            BootMode::LegacyBios => write!(f, "Legacy BIOS"),
        }
    }
}

impl std::fmt::Display for SupportTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupportTier::A => write!(f, "A (full support)"),
            SupportTier::B => write!(f, "B (limited)"),
            SupportTier::C => write!(f, "C (unsupported)"),
        }
    }
}

impl std::fmt::Display for BackupType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupType::Full => write!(f, "Full"),
            BackupType::Incremental => write!(f, "Incremental"),
            BackupType::Differential => write!(f, "Differential"),
        }
    }
}
