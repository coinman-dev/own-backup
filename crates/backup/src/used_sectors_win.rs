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

use crate::exclusions::ByteRanges;
use crate::used_sectors::UsedSectorMap;
use crate::win_device::WinRawDevice;

/// Chunk size for volume bitmap queries (bitmap bytes per DeviceIoControl).
const BITMAP_CHUNK: usize = 1024 * 1024;

/// NTFS used-sector map backed by `FSCTL_GET_VOLUME_BITMAP`.
pub struct NtfsUsedSectorMap {
    block_used: Vec<bool>,
    used_count: u64,
    cluster_size: u32,
    /// Raw cluster allocation bitmap, retained so that file exclusions can
    /// clear cluster bits after the fact. Freed via [`Self::release_bitmap`]
    /// once exclusions are applied (or immediately when there are none).
    bitmap: Option<Vec<u8>>,
    total_clusters: u64,
    volume_len: u64,
    block_size: u32,
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

        // Cluster size + count. NTFS exposes them via FSCTL_GET_NTFS_VOLUME_DATA.
        // ReFS does NOT, but it DOES support FSCTL_GET_VOLUME_BITMAP — so on ReFS
        // read the cluster count from the bitmap header and derive the cluster
        // size from the volume length (validated). Any doubt → error → the engine
        // falls back to sector-by-sector.
        let (cluster_size, total_clusters) = match ntfs_volume_data(&device) {
            Some((cs, tc)) if cs != 0 && tc != 0 => (cs, tc),
            _ => {
                let tc = probe_bitmap_cluster_count(&device).map_err(|e| {
                    OwnBackupError::Device(format!(
                        "{volume_path}: not NTFS and the volume-bitmap probe failed too: {e}"
                    ))
                })?;
                let cs = derive_cluster_size(volume_len, tc).ok_or_else(|| {
                    OwnBackupError::Device(format!(
                        "{volume_path}: cannot derive a cluster size (len {volume_len}, {tc} clusters)"
                    ))
                })?;
                info!(
                    volume = volume_path,
                    cluster_size = cs,
                    total_clusters = tc,
                    "volume-bitmap map for a non-NTFS filesystem (e.g. ReFS)"
                );
                (cs, tc)
            }
        };

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
            "volume bitmap loaded"
        );

        let (block_used, used_count) =
            fold_bitmap(&bitmap, total_clusters, cluster_size, volume_len, block_size);
        info!(
            volume = volume_path,
            total_blocks = block_used.len(),
            used_blocks = used_count,
            "used-sector map ready (volume bitmap)"
        );

        Ok(Self {
            block_used,
            used_count,
            cluster_size,
            bitmap: Some(bitmap),
            total_clusters,
            volume_len,
            block_size,
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

    /// Clear the allocation bits of every cluster fully covered by `ranges`
    /// (byte offsets on the volume; extents from `FSCTL_GET_RETRIEVAL_POINTERS`
    /// are cluster-aligned by construction) and re-fold the per-block map.
    /// Blocks that only carried excluded clusters drop out of the backup
    /// entirely; partially-affected blocks stay and get their excluded bytes
    /// zeroed by [`crate::exclusions::ZeroingBlockReader`].
    pub fn apply_excluded_ranges(&mut self, ranges: &ByteRanges) {
        let Some(bitmap) = self.bitmap.as_mut() else {
            debug!("apply_excluded_ranges called after release_bitmap — ignored");
            return;
        };
        let cs = self.cluster_size as u64;
        for (start, end) in ranges.iter() {
            // Defensive: only whole clusters inside the range.
            let first = start.div_ceil(cs);
            let last = end / cs; // exclusive
            for cluster in first..last.min(self.total_clusters) {
                let byte = (cluster / 8) as usize;
                if let Some(b) = bitmap.get_mut(byte) {
                    *b &= !(1u8 << (cluster % 8));
                }
            }
        }
        let (block_used, used_count) = fold_bitmap(
            bitmap,
            self.total_clusters,
            self.cluster_size,
            self.volume_len,
            self.block_size,
        );
        self.block_used = block_used;
        self.used_count = used_count;
    }

    /// Free the retained cluster bitmap (tens of MB per TB of volume). Call
    /// once exclusions have been applied — the folded block map stays valid.
    pub fn release_bitmap(&mut self) {
        self.bitmap = None;
    }
}

/// NTFS cluster size + count via `FSCTL_GET_NTFS_VOLUME_DATA`. `None` on any
/// non-NTFS filesystem (ReFS, FAT, …), where the FSCTL is not supported.
fn ntfs_volume_data(device: &WinRawDevice) -> Option<(u32, u64)> {
    let mut vd = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    // SAFETY: out pointer valid for the size passed.
    let ok = unsafe {
        DeviceIoControl(
            device.handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&mut vd as *mut _ as *mut _),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .is_ok();
    ok.then_some((vd.BytesPerCluster, vd.TotalClusters as u64))
}

/// Read just the cluster count from `FSCTL_GET_VOLUME_BITMAP` (the `BitmapSize`
/// field of the reply header at LCN 0). Works on any filesystem that supports
/// the volume bitmap — notably ReFS. The tiny output buffer means the call
/// returns `ERROR_MORE_DATA`, but the fixed header (StartingLcn + BitmapSize) is
/// always written first, which is all we need here.
fn probe_bitmap_cluster_count(device: &WinRawDevice) -> Result<u64> {
    let input = STARTING_LCN_INPUT_BUFFER { StartingLcn: 0 };
    let header = 2 * std::mem::size_of::<i64>();
    let mut out = vec![0u8; header + 16];
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
    match call {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {}
        Err(e) => {
            return Err(OwnBackupError::Device(format!(
                "FSCTL_GET_VOLUME_BITMAP probe failed: {e}"
            )))
        }
    }
    // BitmapSize (number of clusters) is the second i64 in the reply header.
    let total = i64::from_le_bytes(out[8..16].try_into().unwrap());
    if total <= 0 {
        return Err(OwnBackupError::Device(
            "volume bitmap reported zero clusters".into(),
        ));
    }
    Ok(total as u64)
}

/// Infer the cluster size (bytes) from the volume length and cluster count when
/// the filesystem doesn't report it (ReFS). Cluster sizes are always powers of
/// two; the correct one is the unique candidate whose `count × size` covers the
/// whole volume without exceeding it (bar a sub-two-cluster tail). Returns `None`
/// if nothing fits — the caller then falls back to sector-by-sector, so a wrong
/// guess can never silently corrupt a backup.
fn derive_cluster_size(volume_len: u64, total_clusters: u64) -> Option<u32> {
    if total_clusters == 0 {
        return None;
    }
    for &cs in &[512u32, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let covered = cs as u64 * total_clusters;
        if covered <= volume_len && volume_len - covered < cs as u64 * 2 {
            return Some(cs);
        }
    }
    None
}

/// Fold a cluster allocation bitmap into a per-backup-block used map.
fn fold_bitmap(
    bitmap: &[u8],
    total_clusters: u64,
    cluster_size: u32,
    volume_len: u64,
    block_size: u32,
) -> (Vec<bool>, u64) {
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
    (block_used, used_count)
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
