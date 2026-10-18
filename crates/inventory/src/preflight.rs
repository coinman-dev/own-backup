use own_backup_common::preflight::{resolve_support_tier, PreflightReport};
use own_backup_common::Result;
use tracing::info;

/// Orchestrates preflight analysis for a target disk.
///
/// On Windows this delegates to [`crate::preflight_windows::collect_windows_preflight`]
/// which calls real Win32 APIs. Elsewhere there is no analysis yet, and it says
/// so: it used to return a made-up healthy UEFI/NTFS/VSS report.
pub struct PreflightAnalyzer {
    _private: (),
}

impl PreflightAnalyzer {
    pub fn new() -> Self {
        PreflightAnalyzer { _private: () }
    }

    /// Run preflight analysis for the given device and return a fully
    /// populated [`PreflightReport`] with support tier, warnings, and
    /// blockers resolved.
    pub fn analyze(&self, device_id: &str) -> Result<PreflightReport> {
        info!(device_id, "Starting preflight analysis");

        let mut report = self.collect_raw_data(device_id)?;
        resolve_support_tier(&mut report);

        info!(
            device_id,
            tier = %report.support_tier,
            warnings = report.warnings.len(),
            blockers = report.blockers.len(),
            "Preflight analysis complete"
        );

        Ok(report)
    }

    /// Collect raw preflight data from the platform.
    #[cfg(target_os = "windows")]
    fn collect_raw_data(&self, device_id: &str) -> Result<PreflightReport> {
        crate::preflight_windows::collect_windows_preflight(device_id)
    }

    /// No analysis off Windows yet — an error, not an invented healthy report.
    #[cfg(not(target_os = "windows"))]
    fn collect_raw_data(&self, device_id: &str) -> Result<PreflightReport> {
        Err(own_backup_common::OwnBackupError::NotImplemented(format!(
            "preflight analysis of {device_id} is only available on Windows"
        )))
    }
}

impl Default for PreflightAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn no_invented_report_off_windows() {
        let err = PreflightAnalyzer::new().analyze("/dev/sda").unwrap_err();
        assert!(matches!(
            err,
            own_backup_common::OwnBackupError::NotImplemented(_)
        ));
    }
}
