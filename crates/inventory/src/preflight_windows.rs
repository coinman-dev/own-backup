// Windows-specific preflight data collection.
//
// This module is compiled exclusively on Windows and uses Win32 APIs
// that are not available on Linux.

#![cfg(target_os = "windows")]
// Stub pending real Win32 data collection (plan v2); imports document intent.
#![allow(unused_imports)]

use own_backup_common::preflight::PreflightReport;
use own_backup_common::types::{BootMode, DiskType, FileSystem, PartitionTable, SupportTier};
use own_backup_common::Result;
use tracing::info;

/// Collect raw preflight data from a Windows machine for the given device.
///
/// This gathers everything needed to build a [`PreflightReport`] using
/// Windows-specific APIs. The caller is expected to pass the result through
/// [`own_backup_common::preflight::resolve_support_tier`] afterwards to
/// compute warnings/blockers/tier.
///
/// Data sources:
///
/// - **Boot mode** -- Call `GetFirmwareEnvironmentVariableW` with a dummy
///   variable name. If it succeeds or returns `ERROR_INVALID_FUNCTION` the
///   system is Legacy BIOS; if it returns `ERROR_NOACCESS` it is UEFI.
///   (Requires SE_SYSTEM_ENVIRONMENT privilege.)
///
/// - **Secure Boot** -- Read registry key
///   `HKLM\SYSTEM\CurrentControlSet\Control\SecureBoot\State\UEFISecureBootEnabled`.
///
/// - **Partition table / disk type** -- `DeviceIoControl` with
///   `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` on `\\.\PhysicalDriveN`.
///
/// - **Sector sizes** -- `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`.
///
/// - **File system** -- `GetVolumeInformationW` on the volume mount point.
///
/// - **VSS service status** -- `OpenSCManagerW` / `OpenServiceW` /
///   `QueryServiceStatusEx` for the `VSS` service.
///
/// - **VSS writer health** -- `CreateVssBackupComponents` + `GatherWriterStatus`.
///
/// - **Free space** -- `GetDiskFreeSpaceExW` on the target path.
///
/// - **Estimated backup size** -- Walk used clusters via
///   `FSCTL_GET_VOLUME_BITMAP` (for used-sector mode) or just use total
///   partition size (for sector-by-sector mode).
///
/// - **EFI partition presence** -- Check for a partition with GPT type GUID
///   `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` in the drive layout.
pub fn collect_windows_preflight(device_id: &str) -> Result<PreflightReport> {
    info!(device_id, "Collecting Windows preflight data");

    // TODO: Implement using Win32 APIs described above.
    //
    // Rough sequence:
    //   1. Detect boot mode via GetFirmwareEnvironmentVariableW
    //   2. Read Secure Boot registry key
    //   3. Open \\.\PhysicalDriveN and call IOCTL_DISK_GET_DRIVE_LAYOUT_EX
    //      -> partition_table, disk_type, efi_partition_present
    //   4. Call IOCTL_DISK_GET_DRIVE_GEOMETRY_EX -> sector sizes
    //   5. GetVolumeInformationW -> filesystem
    //   6. Query VSS service status via SCM
    //   7. CreateVssBackupComponents + GatherWriterStatus -> vss_writers_healthy
    //   8. GetDiskFreeSpaceExW -> free_space_on_target
    //   9. FSCTL_GET_VOLUME_BITMAP -> estimated_backup_size
    //  10. Assemble PreflightReport (support_tier computed by caller)
    todo!(
        "Collect preflight data using Win32 APIs. \
         Requires running on a real Windows host with admin privileges."
    )
}
