pub mod backup_jobs;
pub mod disks;
pub mod engine;
pub mod files;
pub mod keyboard;
pub mod linux_media_download;
#[cfg(target_os = "linux")]
pub mod medium_volumes;
pub mod settings;

/// A failure a screen acts on by its kind rather than its words: `code` is
/// fixed and the frontend branches on it, `message` is only shown. Most
/// commands still fail with a plain string; this is for those whose screen
/// has something different to say when, say, the service does not do a thing
/// on this platform (`unimplemented`).
#[derive(Debug, serde::Serialize)]
pub struct CodedError {
    pub code: &'static str,
    pub message: String,
}

impl From<String> for CodedError {
    /// A failure before the service answered (not reachable, no token).
    fn from(message: String) -> Self {
        Self {
            code: "other",
            message,
        }
    }
}

impl From<tonic::Status> for CodedError {
    fn from(status: tonic::Status) -> Self {
        let code = match status.code() {
            tonic::Code::Unimplemented => "unimplemented",
            tonic::Code::PermissionDenied => "permission_denied",
            tonic::Code::Unavailable => "unavailable",
            _ => "other",
        };
        Self {
            code,
            message: status.message().to_string(),
        }
    }
}
