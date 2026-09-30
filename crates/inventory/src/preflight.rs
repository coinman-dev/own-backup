use own_backup_common::preflight::{resolve_support_tier, PreflightReport};
#[cfg(not(target_os = "windows"))]
use own_backup_common::types::{BootMode, DiskType, FileSystem, PartitionTable, SupportTier};
use own_backup_common::Result;
use tracing::info;

/// Orchestrates preflight analysis for a target disk.
///
/// On Windows this delegates to [`crate::preflight_windows::collect_windows_preflight`]
/// which calls real Win32 APIs. On Linux (dev/CI) it uses synthetic mock data
/// that represents a healthy UEFI/GPT/NTFS system, so the rest of the pipeline
/// can be tested end-to-end.
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

    /// On non-Windows platforms, return mock data suitable for dev/CI testing.
    /// This simulates a healthy Windows system with UEFI, GPT, NTFS, and
    /// working VSS -- the most common configuration we aim to support.
    #[cfg(not(target_os = "windows"))]
    fn collect_raw_data(&self, device_id: &str) -> Result<PreflightReport> {
        info!(
            device_id,
            "Using mock preflight data (non-Windows platform)"
        );

        Ok(PreflightReport {
            boot_mode: BootMode::Uefi,
            secure_boot: true,
            partition_table: PartitionTable::Gpt,
            disk_type: DiskType::Basic,
            sector_size_logical: 512,
            sector_size_physical: 4096,
            filesystem: FileSystem::Ntfs,
            vss_service_running: true,
            vss_writers_healthy: true,
            free_space_on_target: 500_000_000_000,  // 500 GB
            estimated_backup_size: 120_000_000_000, // 120 GB
            efi_partition_present: true,
            support_tier: SupportTier::A, // will be recalculated by resolve_support_tier
            warnings: vec![],
            blockers: vec![],
        })
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
    use own_backup_common::types::SupportTier;

    #[test]
    fn mock_preflight_returns_tier_a() {
        let analyzer = PreflightAnalyzer::new();
        let report = analyzer.analyze(r"\\.\PhysicalDrive0").unwrap();

        assert_eq!(report.support_tier, SupportTier::A);
        assert!(report.blockers.is_empty());
        assert!(report.warnings.is_empty());
        assert_eq!(report.boot_mode, BootMode::Uefi);
        assert_eq!(report.partition_table, PartitionTable::Gpt);
        assert_eq!(report.filesystem, FileSystem::Ntfs);
    }
}
