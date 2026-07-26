use thiserror::Error;

#[derive(Debug, Error)]
pub enum OwnBackupError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Image format error: {0}")]
    ImageFormat(String),

    #[error("Compression error: {0}")]
    Compression(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Checksum mismatch: block {block_index}, expected {expected:016x}, got {actual:016x}")]
    ChecksumMismatch {
        block_index: u64,
        expected: u64,
        actual: u64,
    },

    #[error("VSS error: {0}")]
    Vss(String),

    #[error("WMI error: {0}")]
    Wmi(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Preflight blocker: {0}")]
    PreflightBlocker(String),

    #[error("No snapshot provider available for device: {0}")]
    NoSnapshotProvider(String),

    #[error("Backup chain broken: {0}")]
    ChainBroken(String),

    #[error("Restore incompatible: {0}")]
    RestoreIncompatible(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Device I/O error: {0}")]
    Device(String),

    #[error("Virtual disk (VHDX) error: {0}")]
    Vhdx(String),

    #[error("Cancelled by user")]
    Cancelled,

    /// A capability that is planned but not built yet. Distinct from an error:
    /// the request was valid, the code simply does not exist. Returning success
    /// here is how a partition silently ends up empty after a "successful"
    /// restore.
    #[error("Not implemented: {0}")]
    NotImplemented(String),
}

pub type Result<T> = std::result::Result<T, OwnBackupError>;

impl From<serde_json::Error> for OwnBackupError {
    fn from(e: serde_json::Error) -> Self {
        OwnBackupError::Serialization(e.to_string())
    }
}
