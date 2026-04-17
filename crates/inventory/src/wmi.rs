// WMI-based disk inventory -- Windows only.
//
// This module is compiled exclusively on Windows where we have access to
// the COM/WMI subsystem via the `windows` crate.
//
// Current implementation: pragmatic approach using wmic.exe and PowerShell
// commands via std::process::Command. A full COM-based implementation will
// replace this once the COM initialisation plumbing is complete.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use own_backup_common::disk::{DiskInfo, PartitionInfo};
use own_backup_common::types::{FileSystem, PartitionTable};
use own_backup_common::{OwnBackupError, Result};
use std::process::Command;
use tracing::{debug, info, warn};

// Windows crate imports for WMI COM access (will be used in future full implementation).
#[allow(unused_imports)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
#[allow(unused_imports)]
use windows::Win32::System::Wmi::{
    IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY,
};

/// WMI-backed disk inventory for Windows.
///
/// Current implementation uses `wmic` and PowerShell commands as a pragmatic
/// approach. A future version will use direct COM/WMI calls via the `windows`
/// crate for better performance and reliability.
///
/// Queries:
/// - `wmic diskdrive` for physical disk metadata
/// - `wmic partition` for basic partition info
/// - PowerShell `Get-Partition` for detailed partition data (GPT GUIDs, offsets)
/// - PowerShell `Get-Volume` for volume letters and filesystem info
pub struct WmiInventory {
    _private: (),
}

impl WmiInventory {
    /// Create a new `WmiInventory`.
    ///
    /// In this pragmatic implementation, no COM initialisation is needed since
    /// we shell out to wmic/PowerShell.
    pub fn new() -> Result<Self> {
        Ok(WmiInventory { _private: () })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers for parsing wmic / PowerShell output
// ---------------------------------------------------------------------------

/// Parse CSV output from wmic (first line is header, subsequent lines are data).
/// wmic /format:csv produces: Node,Field1,Field2,...
/// The first column is always the node name which we skip.
fn parse_wmic_csv(output: &str) -> Vec<std::collections::HashMap<String, String>> {
    let lines: Vec<&str> = output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.len() < 2 {
        return Vec::new();
    }

    let headers: Vec<&str> = lines[0].split(',').collect();
    let mut results = Vec::new();

    for line in &lines[1..] {
        let values: Vec<&str> = line.split(',').collect();
        if values.len() != headers.len() {
            continue;
        }
        let mut row = std::collections::HashMap::new();
        for (i, header) in headers.iter().enumerate() {
            row.insert(header.to_string(), values[i].to_string());
        }
        results.push(row);
    }

    results
}

/// Extract the disk index number from a DeviceID like `\\.\PHYSICALDRIVE0` -> 0
fn extract_disk_index(device_id: &str) -> Option<u32> {
    let upper = device_id.to_uppercase();
    if let Some(idx_str) = upper.strip_prefix(r"\\.\PHYSICALDRIVE") {
        idx_str.parse::<u32>().ok()
    } else {
        // Try to find trailing digits
        let digits: String = device_id.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
        let digits: String = digits.chars().rev().collect();
        digits.parse::<u32>().ok()
    }
}

/// Parse a filesystem string to our FileSystem enum.
fn parse_filesystem(fs: &str) -> FileSystem {
    match fs.trim().to_uppercase().as_str() {
        "NTFS" => FileSystem::Ntfs,
        "FAT32" => FileSystem::Fat32,
        "EXFAT" | "EXFAT" => FileSystem::ExFat,
        "EXT4" => FileSystem::Ext4,
        "XFS" => FileSystem::Xfs,
        "BTRFS" => FileSystem::Btrfs,
        "REFS" => FileSystem::ReFs,
        _ => FileSystem::Unknown,
    }
}

/// Determine partition table type from PowerShell Get-Disk or wmic output.
fn parse_partition_table(style: &str) -> PartitionTable {
    match style.trim().to_uppercase().as_str() {
        "GPT" => PartitionTable::Gpt,
        "MBR" => PartitionTable::Mbr,
        _ => PartitionTable::Gpt, // Default to GPT for modern systems
    }
}

/// Known EFI System Partition GPT type GUID.
const GPT_TYPE_EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// Check if a GPT type GUID indicates a system (EFI) partition.
fn is_efi_system_partition(gpt_type: &str) -> bool {
    gpt_type.to_uppercase().contains("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
}

/// Run a command and return its stdout as a String, or an error.
fn run_command(program: &str, args: &[&str]) -> Result<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let output = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| OwnBackupError::Wmi(format!("Failed to run {}: {}", program, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("{} failed (exit {:?}): {}", program, output.status.code(), stderr);
        // Return empty string on failure rather than hard-erroring, so we can
        // degrade gracefully (e.g. powershell not available).
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Represents parsed PowerShell Get-Partition output.
#[derive(Debug, Default)]
struct PsPartition {
    partition_number: u32,
    offset: u64,
    size: u64,
    gpt_type: String,
    drive_letter: Option<char>,
}

/// Represents parsed PowerShell Get-Volume output.
#[derive(Debug, Default)]
struct PsVolume {
    drive_letter: Option<char>,
    filesystem_type: String,
    size: u64,
    size_remaining: u64,
    label: String,
}

/// Parse PowerShell JSON output from Get-Partition.
fn parse_ps_partitions(json_str: &str) -> Vec<PsPartition> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // PowerShell may return a single object or an array
    let parsed: std::result::Result<Vec<serde_json::Value>, _> = serde_json::from_str(trimmed);
    let items = match parsed {
        Ok(arr) => arr,
        Err(_) => {
            // Try parsing as a single object
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(obj) => vec![obj],
                Err(e) => {
                    warn!("Failed to parse Get-Partition JSON: {}", e);
                    return Vec::new();
                }
            }
        }
    };

    items
        .into_iter()
        .map(|v| {
            let drive_letter = v
                .get("DriveLetter")
                .and_then(|d| d.as_str())
                .and_then(|s| s.chars().next())
                .filter(|c| c.is_ascii_alphabetic());

            PsPartition {
                partition_number: v
                    .get("PartitionNumber")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32,
                offset: v
                    .get("Offset")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0),
                size: v.get("Size").and_then(|n| n.as_u64()).unwrap_or(0),
                gpt_type: v
                    .get("GptType")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .trim_matches('{')
                    .trim_matches('}')
                    .to_uppercase(),
                drive_letter,
            }
        })
        .collect()
}

/// Parse PowerShell JSON output from Get-Volume.
fn parse_ps_volumes(json_str: &str) -> Vec<PsVolume> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let parsed: std::result::Result<Vec<serde_json::Value>, _> = serde_json::from_str(trimmed);
    let items = match parsed {
        Ok(arr) => arr,
        Err(_) => {
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(obj) => vec![obj],
                Err(e) => {
                    warn!("Failed to parse Get-Volume JSON: {}", e);
                    return Vec::new();
                }
            }
        }
    };

    items
        .into_iter()
        .map(|v| {
            let drive_letter = v
                .get("DriveLetter")
                .and_then(|d| d.as_str())
                .and_then(|s| s.chars().next())
                .filter(|c| c.is_ascii_alphabetic());

            PsVolume {
                drive_letter,
                filesystem_type: v
                    .get("FileSystemType")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                size: v.get("Size").and_then(|n| n.as_u64()).unwrap_or(0),
                size_remaining: v
                    .get("SizeRemaining")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0),
                label: v
                    .get("FileSystemLabel")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            }
        })
        .collect()
}

/// Query partition table style for a disk via PowerShell Get-Disk.
fn get_partition_table_style(disk_index: u32) -> PartitionTable {
    let cmd = format!(
        "(Get-Disk -Number {}).PartitionStyle",
        disk_index
    );
    match run_command("powershell", &["-NoProfile", "-Command", &cmd]) {
        Ok(output) => parse_partition_table(output.trim()),
        Err(_) => PartitionTable::Gpt,
    }
}

/// Query disk GUID via PowerShell Get-Disk.
fn get_disk_guid(disk_index: u32) -> String {
    let cmd = format!(
        "(Get-Disk -Number {}).Guid",
        disk_index
    );
    match run_command("powershell", &["-NoProfile", "-Command", &cmd]) {
        Ok(output) => {
            let guid = output.trim().trim_matches('{').trim_matches('}').to_string();
            if guid.is_empty() {
                format!("disk-{}", disk_index)
            } else {
                guid
            }
        }
        Err(_) => format!("disk-{}", disk_index),
    }
}

// ---------------------------------------------------------------------------
// DiskInventory implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl super::traits::DiskInventory for WmiInventory {
    async fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        info!("Enumerating disks via wmic/PowerShell");

        // Step 1: Get all physical disks via wmic
        let wmic_output = run_command(
            "wmic",
            &[
                "diskdrive",
                "get",
                "DeviceID,Model,SerialNumber,Size,BytesPerSector,SectorsPerTrack,MediaType",
                "/format:csv",
            ],
        )?;

        let disk_rows = parse_wmic_csv(&wmic_output);
        if disk_rows.is_empty() {
            warn!("wmic returned no disk drives");
            return Ok(Vec::new());
        }

        // Step 4 (done once): Get all volumes upfront
        let volumes_output = run_command(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Get-Volume | Select-Object DriveLetter,FileSystemType,Size,SizeRemaining,FileSystemLabel | ConvertTo-Json",
            ],
        )?;
        let all_volumes = parse_ps_volumes(&volumes_output);

        let mut disks = Vec::new();

        for row in &disk_rows {
            let device_id = row.get("DeviceID").cloned().unwrap_or_default();
            if device_id.is_empty() {
                continue;
            }

            let model = row.get("Model").cloned().unwrap_or_default();
            let serial = row.get("SerialNumber").cloned().unwrap_or_default().trim().to_string();
            let size_bytes: u64 = row
                .get("Size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let sector_size: u32 = row
                .get("BytesPerSector")
                .and_then(|s| s.parse().ok())
                .unwrap_or(512);
            let media_type = row.get("MediaType").cloned().unwrap_or_else(|| "Unknown".into());

            let disk_index = match extract_disk_index(&device_id) {
                Some(idx) => idx,
                None => {
                    warn!("Could not extract disk index from {}", device_id);
                    continue;
                }
            };

            debug!(disk_index, model = %model, "Processing disk");

            // Step 2: Get partitions for this disk via wmic
            let partition_filter = format!("DiskIndex={}", disk_index);
            let _wmic_part_output = run_command(
                "wmic",
                &[
                    "partition",
                    "where",
                    &partition_filter,
                    "get",
                    "DeviceID,BlockSize,Bootable,Size,Type,Index",
                    "/format:csv",
                ],
            )?;

            // Step 3: Get detailed partition info via PowerShell
            let ps_cmd = format!(
                "Get-Partition -DiskNumber {} | Select-Object PartitionNumber,Offset,Size,Type,DriveLetter,GptType | ConvertTo-Json",
                disk_index
            );
            let ps_part_output =
                run_command("powershell", &["-NoProfile", "-Command", &ps_cmd])?;
            let ps_partitions = parse_ps_partitions(&ps_part_output);

            // Determine partition table type and disk GUID
            let partition_table = get_partition_table_style(disk_index);
            let disk_guid = get_disk_guid(disk_index);

            // Step 5: Build PartitionInfo for each partition
            let mut partitions = Vec::new();

            for ps_part in &ps_partitions {
                // Match volume to partition by drive letter
                let matched_volume = ps_part
                    .drive_letter
                    .and_then(|letter| {
                        all_volumes
                            .iter()
                            .find(|v| v.drive_letter == Some(letter))
                    });

                let filesystem = matched_volume
                    .map(|v| parse_filesystem(&v.filesystem_type))
                    .unwrap_or(FileSystem::Unknown);

                let volume_label = matched_volume
                    .map(|v| v.label.clone())
                    .unwrap_or_default();

                let is_boot = is_efi_system_partition(&ps_part.gpt_type);

                // A partition is the "system" partition if it contains the OS
                // (typically C: with NTFS on Windows).
                let is_system = ps_part.drive_letter == Some('C')
                    && filesystem == FileSystem::Ntfs;

                let partition_info = PartitionInfo {
                    index: ps_part.partition_number,
                    offset_bytes: ps_part.offset,
                    size_bytes: ps_part.size,
                    gpt_type_guid: ps_part.gpt_type.clone(),
                    gpt_partition_guid: String::new(), // Not available via Get-Partition
                    filesystem,
                    volume_letter: ps_part.drive_letter,
                    volume_label,
                    fs_uuid: String::new(), // Not easily available via PowerShell
                    is_system,
                    is_boot,
                };

                partitions.push(partition_info);
            }

            let disk_info = DiskInfo {
                device_id,
                model,
                serial,
                size_bytes,
                sector_size_logical: sector_size,
                sector_size_physical: sector_size, // wmic doesn't distinguish; same default
                media_type,
                partition_table,
                disk_guid,
                partitions,
            };

            disks.push(disk_info);
        }

        info!(count = disks.len(), "Disk enumeration complete");
        Ok(disks)
    }

    async fn get_disk_details(&self, device_id: &str) -> Result<DiskInfo> {
        info!(device_id, "Fetching disk details via wmic/PowerShell");

        // Enumerate all disks and find the matching one.
        // This is acceptable for the pragmatic implementation since there are
        // typically very few physical disks.
        let disks = self.enumerate_disks().await?;

        disks
            .into_iter()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| {
                OwnBackupError::Wmi(format!("Disk not found: {}", device_id))
            })
    }
}
