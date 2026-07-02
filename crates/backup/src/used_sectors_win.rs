//! Windows used-sector bitmap via `FSCTL_GET_VOLUME_BITMAP`.
//!
//! Queries the NTFS allocation bitmap to determine which clusters are in use,
//! allowing smart-sector backups to skip unallocated regions. The volume
//! bitmap is folded into a per-backup-block "used" map at open time, so
//! lookups during the backup are O(1).
//!
//! Only meaningful for NTFS volumes; for other filesystems (or whole-disk
//! non-volume regions) fall back to [`crate::used_sectors::AllUsedMap`].

#![cfg(target_os = "windows")]

use tracing::{debug, info};
use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_VOLUME_BITMAP, NTFS_VOLUME_DATA_BUFFER,
    STARTING_LCN_INPUT_BUFFER,
};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::{OwnBackupError, Result};

use crate::used_sectors::UsedSectorMap;
use crate::win_device::WinRawDevice;

/// Chunk size for volume bitmap queries (bitmap bytes per DeviceIoControl).
const BITMAP_CHUNK: usize = 1024 * 1024;

/// NTFS used-sector map backed by `FSCTL_GET_VOLUME_BITMAP`.
pub struct NtfsUsedSectorMap {
    block_used: Vec<bool>,
    used_count: u64,
    cluster_size: u32,
}

impl NtfsUsedSectorMap {
    /// Query the NTFS volume bitmap and build the per-block used map.
    ///
    /// `volume_path` is e.g. `\\.\C:`, `C:` or a VSS snapshot device path.
    /// `block_size` is the backup block size in bytes (must be a multiple of
    /// the volume sector size; typically 1 MiB).
    pub fn open(volume_path: &str, block_size: u32) -> Result<Self> {
        let device = WinRawDevice::open_read(volume_path)?;
        let volume_len = device.length();

        // --- NTFS volume data: cluster size and cluster count ---
        let mut volume_data = NTFS_VOLUME_DATA_BUFFER::default();
        let mut returned = 0u32;
        // SAFETY: out pointer valid for the size passed.
        unsafe {
            DeviceIoControl(
                device.handle(),
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&mut volume_data as *mut _ as *mut _),
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "FSCTL_GET_NTFS_VOLUME_DATA({volume_path}) failed (not an NTFS volume?): {e}"
            ))
        })?;

        let cluster_size = volume_data.BytesPerCluster;
        let total_clusters = volume_data.TotalClusters as u64;
        if cluster_size == 0 || total_clusters == 0 {
            return Err(OwnBackupError::Device(format!(
                "NTFS volume data for {volume_path} reports zero clusters"
            )));
        }

        // --- Read the allocation bitmap in chunks ---
        let mut bitmap: Vec<u8> = Vec::with_capacity((total_clusters as usize).div_ceil(8));
        let mut next_lcn: u64 = 0;

        while next_lcn < total_clusters {
            let input = STARTING_LCN_INPUT_BUFFER {
                StartingLcn: next_lcn as i64,
            };
            // Output layout: VOLUME_BITMAP_BUFFER { StartingLcn: i64, BitmapSize: i64, Buffer: [u8] }
            let header = 2 * std::mem::size_of::<i64>();
            let mut out = vec![0u8; header + BITMAP_CHUNK];
            let mut returned = 0u32;

            // SAFETY: in/out pointers valid for the sizes passed.
            let call = unsafe {
                DeviceIoControl(
                    device.handle(),
                    FSCTL_GET_VOLUME_BITMAP,
                    Some(&input as *const _ as *const _),
                    std::mem::size_of::<STARTING_LCN_INPUT_BUFFER>() as u32,
                    Some(out.as_mut_ptr() as *mut _),
                    out.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };
            let more_data = match call {
                Ok(()) => false,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
                Err(e) => {
                    return Err(OwnBackupError::Device(format!(
                        "FSCTL_GET_VOLUME_BITMAP({volume_path}) at LCN {next_lcn} failed: {e}"
                    )));
                }
            };

            if (returned as usize) <= header {
                return Err(OwnBackupError::Device(format!(
                    "FSCTL_GET_VOLUME_BITMAP({volume_path}) returned no data at LCN {next_lcn}"
                )));
            }

            let starting_lcn = i64::from_le_bytes(out[0..8].try_into().unwrap()) as u64;
            let bits_remaining = i64::from_le_bytes(out[8..16].try_into().unwrap()) as u64;
            let data_bytes = returned as usize - header;
            // The kernel returns whole bytes; the last byte of the final chunk
            // may contain padding bits past the end of the volume.
            let bits_in_chunk =
                std::cmp::min((data_bytes as u64) * 8, bits_remaining);

            if starting_lcn != next_lcn {
                return Err(OwnBackupError::Device(format!(
                    "FSCTL_GET_VOLUME_BITMAP returned LCN {starting_lcn}, expected {next_lcn}"
                )));
            }

            bitmap.extend_from_slice(&out[header..header + data_bytes]);
            next_lcn += bits_in_chunk;

            if !more_data {
                break;
            }
            // Continue from a whole-byte boundary so appended bytes stay bit-aligned.
            next_lcn = (next_lcn / 8) * 8;
            bitmap.truncate((next_lcn / 8) as usize);
        }

        debug!(
            volume = volume_path,
            total_clusters,
            cluster_size,
            bitmap_bytes = bitmap.len(),
            "NTFS volume bitmap loaded"
        );

        // --- Fold clusters into backup blocks ---
        let total_blocks = volume_len.div_ceil(block_size as u64);
        let mut block_used = vec![false; total_blocks as usize];

        // Bytes past the last cluster (e.g. the NTFS backup boot sector at the
        // end of the volume) are not covered by the bitmap — always copy them.
        let clusters_end = total_clusters * cluster_size as u64;
        if clusters_end < volume_len {
            let first_tail_block = clusters_end / block_size as u64;
            for b in block_used.iter_mut().skip(first_tail_block as usize) {
                *b = true;
            }
        }

        for (byte_idx, &byte) in bitmap.iter().enumerate() {
            if byte == 0 {
                continue;
            }
            for bit in 0..8u32 {
                if byte & (1 << bit) == 0 {
                    continue;
                }
                let cluster = byte_idx as u64 * 8 + bit as u64;
                if cluster >= total_clusters {
                    break;
                }
                let start = cluster * cluster_size as u64;
                let end = start + cluster_size as u64 - 1;
                let first_block = (start / block_size as u64) as usize;
                let last_block = (end / block_size as u64) as usize;
                for b in block_used
                    .iter_mut()
                    .take(last_block + 1)
                    .skip(first_block)
                {
                    *b = true;
                }
            }
        }

        let used_count = block_used.iter().filter(|&&u| u).count() as u64;
        info!(
            volume = volume_path,
            total_blocks,
            used_blocks = used_count,
            "NTFS used-sector map ready"
        );

        Ok(Self {
            block_used,
            used_count,
            cluster_size,
        })
    }

    /// NTFS cluster size of the source volume in bytes.
    pub fn cluster_size(&self) -> u32 {
        self.cluster_size
    }

    /// Total number of blocks covered by this map.
    pub fn total_blocks(&self) -> u64 {
        self.block_used.len() as u64
    }
}

impl UsedSectorMap for NtfsUsedSectorMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.block_used
            .get(block_number as usize)
            .copied()
            .unwrap_or(false)
    }

    fn used_block_count(&self) -> u64 {
        self.used_count
    }
}
