use serde::{Deserialize, Serialize};

use crate::types::{BootMode, DiskType, FileSystem, PartitionTable, SupportTier};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    pub boot_mode: BootMode,
    pub secure_boot: bool,
    pub partition_table: PartitionTable,
    pub disk_type: DiskType,
    pub sector_size_logical: u32,
    pub sector_size_physical: u32,
    pub filesystem: FileSystem,
    pub vss_service_running: bool,
    pub vss_writers_healthy: bool,
    pub free_space_on_target: u64,
    pub estimated_backup_size: u64,
    pub efi_partition_present: bool,
    pub support_tier: SupportTier,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

/// Pure logic: determine support tier from collected preflight data.
/// Testable on any platform.
pub fn resolve_support_tier(report: &mut PreflightReport) {
    report.blockers.clear();
    report.warnings.clear();

    // Tier C blockers
    if report.disk_type == DiskType::Dynamic {
        report
            .blockers
            .push("Dynamic disks are not supported".into());
    }
    if report.disk_type == DiskType::StorageSpaces {
        report
            .blockers
            .push("Storage Spaces are not supported".into());
    }
    if report.filesystem == FileSystem::ReFs {
        report.blockers.push("ReFS is not supported in v1".into());
    }

    if !report.blockers.is_empty() {
        report.support_tier = SupportTier::C;
        return;
    }

    // Tier B warnings
    if report.partition_table == PartitionTable::Mbr {
        report
            .warnings
            .push("MBR partition table: limited support, GPT recommended".into());
    }
    if report.boot_mode == BootMode::LegacyBios {
        report
            .warnings
            .push("Legacy BIOS boot: limited support, UEFI recommended".into());
    }
    if !report.vss_service_running {
        report
            .warnings
            .push("VSS service is not running, will attempt to start".into());
    }
    if !report.vss_writers_healthy {
        report
            .warnings
            .push("Some VSS writers are in failed state".into());
    }
    if report.free_space_on_target < report.estimated_backup_size {
        report.warnings.push(format!(
            "Target may not have enough space: {} available, ~{} needed",
            format_bytes(report.free_space_on_target),
            format_bytes(report.estimated_backup_size),
        ));
    }

    if report.warnings.is_empty() {
        report.support_tier = SupportTier::A;
    } else {
        report.support_tier = SupportTier::B;
    }
}

fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1_073_741_824;
    const MB: u64 = 1_048_576;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report() -> PreflightReport {
        PreflightReport {
            boot_mode: BootMode::Uefi,
            secure_boot: false,
            partition_table: PartitionTable::Gpt,
            disk_type: DiskType::Basic,
            sector_size_logical: 512,
            sector_size_physical: 4096,
            filesystem: FileSystem::Ntfs,
            vss_service_running: true,
            vss_writers_healthy: true,
            free_space_on_target: 500_000_000_000,
            estimated_backup_size: 100_000_000_000,
            efi_partition_present: true,
            support_tier: SupportTier::A,
            warnings: vec![],
            blockers: vec![],
        }
    }

    #[test]
    fn tier_a_for_ideal_config() {
        let mut report = make_report();
        resolve_support_tier(&mut report);
        assert_eq!(report.support_tier, SupportTier::A);
        assert!(report.warnings.is_empty());
        assert!(report.blockers.is_empty());
    }

    #[test]
    fn tier_b_for_mbr() {
        let mut report = make_report();
        report.partition_table = PartitionTable::Mbr;
        resolve_support_tier(&mut report);
        assert_eq!(report.support_tier, SupportTier::B);
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn tier_c_for_dynamic_disk() {
        let mut report = make_report();
        report.disk_type = DiskType::Dynamic;
        resolve_support_tier(&mut report);
        assert_eq!(report.support_tier, SupportTier::C);
        assert!(!report.blockers.is_empty());
    }

    #[test]
    fn tier_c_for_refs() {
        let mut report = make_report();
        report.filesystem = FileSystem::ReFs;
        resolve_support_tier(&mut report);
        assert_eq!(report.support_tier, SupportTier::C);
    }

    #[test]
    fn tier_b_for_low_space() {
        let mut report = make_report();
        report.free_space_on_target = 50_000_000_000;
        report.estimated_backup_size = 100_000_000_000;
        resolve_support_tier(&mut report);
        assert_eq!(report.support_tier, SupportTier::B);
    }
}
