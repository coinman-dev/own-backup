use thiserror::Error;

/// Variants that carry a complete sentence for a person (`Config`, `Device`,
/// `PreflightBlocker`, `ChainBroken`, `RestoreIncompatible`, `NotImplemented`,
/// `KeyProtection`) display just that sentence: their text is in the user's
/// language and reaches the window as it is, where an English label in front
/// of a Russian sentence ("Device I/O error: запись 0002 …") said nothing a
/// person could use. Technical wrappers keep their label for the log.
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

    #[error("Disk inventory error: {0}")]
    Inventory(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    PreflightBlocker(String),

    #[error("No snapshot provider available for device: {0}")]
    NoSnapshotProvider(String),

    #[error("{0}")]
    ChainBroken(String),

    #[error("{0}")]
    RestoreIncompatible(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("{0}")]
    Device(String),

    #[error("Virtual disk (VHDX) error: {0}")]
    Vhdx(String),

    #[error("Cancelled by user")]
    Cancelled,

    /// A capability that is planned but not built yet. Distinct from an error:
    /// the request was valid, the code simply does not exist. Returning success
    /// here is how a partition silently ends up empty after a "successful"
    /// restore.
    #[error("{0}")]
    NotImplemented(String),

    /// What protects the service's keys on this machine failed — the TPM or
    /// `systemd-creds` on Linux. Not a lost authorization: authorizing the
    /// job again needs the same protection and fails the same way, so the
    /// remedy is the machine's, not the job's.
    #[error("{0}")]
    KeyProtection(String),
}

pub type Result<T> = std::result::Result<T, OwnBackupError>;

impl From<serde_json::Error> for OwnBackupError {
    fn from(e: serde_json::Error) -> Self {
        OwnBackupError::Serialization(e.to_string())
    }
}
