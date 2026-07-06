//! Used-cluster maps for FAT32 and exFAT, parsed directly from the raw volume.
//!
//! Unlike NTFS (where `FSCTL_GET_VOLUME_BITMAP` has clean LCN semantics), the
//! defrag FSCTLs on FAT volumes are inconsistently supported and their cluster
//! numbering is offset by the FS metadata area — a silent mismatch there would
//! corrupt backups. Both formats are simple and openly specified, so we parse
//! the on-disk structures ourselves: the FAT table for FAT32 (entry != 0 ⇒
//! cluster allocated), the Allocation Bitmap file for exFAT. Everything before
//! the data area (boot sectors, FSInfo, FATs, exFAT root metadata) and any
//! tail past the cluster heap is always copied.
//!
//! Failure policy: ANY doubt — bad signature, geometry that does not match the
//! device, truncated structures — is an error, and the engine falls back to
//! sector-by-sector. A wrong map must never guess: missing data in an image is
//! worse than a bigger image.

use tracing::{debug, info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::exclusions::ByteRanges;
use crate::used_sectors::UsedSectorMap;

/// Raw random-access volume reads. Implemented by `WinRawDevice` on Windows
/// and by in-memory buffers in tests. All offsets this module uses are
/// sector-aligned (device requirement).
pub trait RawVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn len(&self) -> u64;
}

#[cfg(target_os = "windows")]
impl RawVolume for crate::win_device::WinRawDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        crate::win_device::WinRawDevice::read_at(self, offset, buf)
    }
    fn len(&self) -> u64 {
        self.length()
    }
}

/// Per-backup-block used map for a FAT32/exFAT volume.
pub struct FatUsedMap {
    block_used: Vec<bool>,
    used_count: u64,
    /// "FAT32" or "exFAT" — for logs and diagnostics.
    pub filesystem: &'static str,
    /// FS cluster size in bytes.
    pub cluster_size: u32,
}

impl UsedSectorMap for FatUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.block_used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.used_count
    }
}

/// Sanity cap for FAT/bitmap structure reads (an 8 TB exFAT bitmap is 32 MB;
/// a FAT32 FAT tops out near 1 GB but real volumes stay far below this).
const MAX_STRUCT_BYTES: u64 = 256 * 1024 * 1024;

/// Detect FAT32/exFAT on `vol` and build the used map. Errors if the volume is
/// neither, or if any structure fails validation — callers fall back to
/// sector-by-sector.
pub fn build_fat_used_map(vol: &mut dyn RawVolume, block_size: u32) -> Result<FatUsedMap> {
    build_fat_used_map_excl(vol, block_size, None)
}

/// Like [`build_fat_used_map`], but every cluster fully covered by `excluded`
/// (excluded file extents) is treated as free — so exclusions drop blocks and
/// shrink the image cluster-by-cluster, exactly like the NTFS path.
pub fn build_fat_used_map_excl(
    vol: &mut dyn RawVolume,
    block_size: u32,
    excluded: Option<&ByteRanges>,
) -> Result<FatUsedMap> {
    let mut boot = [0u8; 512];
    vol.read_at(0, &mut boot)?;
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(bad("no boot sector signature (55AA)"));
    }

    if &boot[3..11] == b"EXFAT   " {
        build_exfat(vol, &boot, block_size, excluded)
    } else {
        build_fat32(vol, &boot, block_size, excluded)
    }
}

/// FAT32 on-disk geometry, shared by the used-map builder and the exclusion
/// walker ([`crate::fat_exclusions`]).
pub struct Fat32Geometry {
    pub bytes_per_sector: u64,
    pub cluster_size: u32,
    /// Number of data clusters (cluster numbers run 2..cluster_count+2).
    pub cluster_count: u64,
    /// Byte offset of cluster 2 (start of the data area).
    pub data_start: u64,
    /// Byte offset of the first FAT.
    pub fat_offset: u64,
    pub fat_bytes: u64,
    /// First cluster of the root directory (`BPB_RootClus`).
    pub root_cluster: u64,
}

/// Parse + validate FAT32 geometry from the boot sector. `Err` (→ sector-by-
/// sector fallback) on FAT12/16, bad signatures or geometry that overflows the
/// device — never a silent guess.
pub fn parse_fat32_geometry(boot: &[u8], vol_len: u64) -> Result<Fat32Geometry> {
    let bps = u16at(boot, 11);
    let spc = boot[13] as u64;
    let reserved = u16at(boot, 14);
    let nfats = boot[16] as u64;
    let root_ent_cnt = u16at(boot, 17);
    let tot_sec16 = u16at(boot, 19);
    let fatsz16 = u16at(boot, 22);
    let tot_sec32 = u32at(boot, 32);
    let fatsz32 = u32at(boot, 36);
    let root_cluster = u32at(boot, 44);

    // FAT32 discriminators (FAT12/16 have fatsz16 != 0 and a fixed root dir).
    if fatsz16 != 0 || fatsz32 == 0 || root_ent_cnt != 0 {
        return Err(bad("not a FAT32 volume (FAT12/16 or unknown)"));
    }
    if !bps.is_power_of_two() || !(512..=4096).contains(&bps) {
        return Err(bad(format!("implausible bytes/sector {bps}")));
    }
    if !spc.is_power_of_two() || spc > 128 || spc == 0 {
        return Err(bad(format!("implausible sectors/cluster {spc}")));
    }
    if reserved == 0 || nfats == 0 || nfats > 4 {
        return Err(bad(format!("implausible reserved={reserved} nfats={nfats}")));
    }
    let tot_sec = if tot_sec32 != 0 { tot_sec32 } else { tot_sec16 };
    let data_start_sec = reserved + nfats * fatsz32;
    if tot_sec <= data_start_sec {
        return Err(bad("total sectors do not cover the data area"));
    }
    if tot_sec * bps > vol_len {
        return Err(bad(format!(
            "filesystem claims {} bytes but the device is {}",
            tot_sec * bps,
            vol_len
        )));
    }
    let cluster_size = (bps * spc) as u32;
    let cluster_count = (tot_sec - data_start_sec) / spc;
    let fat_bytes = fatsz32 * bps;
    if fat_bytes / 4 < cluster_count + 2 {
        return Err(bad("FAT too small for the cluster count"));
    }
    if fat_bytes > MAX_STRUCT_BYTES {
        return Err(bad(format!("FAT is implausibly large ({fat_bytes} bytes)")));
    }
    if root_cluster < 2 || root_cluster >= cluster_count + 2 {
        return Err(bad(format!("root cluster {root_cluster} out of range")));
    }
    Ok(Fat32Geometry {
        bytes_per_sector: bps,
        cluster_size,
        cluster_count,
        data_start: data_start_sec * bps,
        fat_offset: reserved * bps,
        fat_bytes,
        root_cluster,
    })
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("FAT map: {msg}"))
}

fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Mark the byte range `[start, end)` as used in the per-block map.
fn mark_bytes(map: &mut [bool], block_size: u32, start: u64, end: u64) {
    if end <= start {
        return;
    }
    let first = (start / block_size as u64) as usize;
    let last = ((end - 1) / block_size as u64) as usize;
    for b in map.iter_mut().take(last + 1).skip(first) {
        *b = true;
    }
}

// ---------------------------------------------------------------------------
// FAT32
// ---------------------------------------------------------------------------

fn build_fat32(
    vol: &mut dyn RawVolume,
    boot: &[u8],
    block_size: u32,
    excluded: Option<&ByteRanges>,
) -> Result<FatUsedMap> {
    let g = parse_fat32_geometry(boot, vol.len())?;
    let cluster_size = g.cluster_size;
    let data_start = g.data_start;

    // Read FAT #1 and mark allocated clusters (entry & 0x0FFFFFFF != 0 —
    // includes bad-marked clusters: safer to copy them than to guess).
    let mut fat = vec![0u8; g.fat_bytes as usize];
    vol.read_at(g.fat_offset, &mut fat)?;

    let total_blocks = vol.len().div_ceil(block_size as u64);
    let mut map = vec![false; total_blocks as usize];

    // Metadata area (boot + FSInfo + backup boot + FATs) is always copied.
    mark_bytes(&mut map, block_size, 0, data_start);

    let mut used_clusters = 0u64;
    let mut excluded_clusters = 0u64;
    for n in 0..g.cluster_count {
        let e = u32at(&fat, ((n + 2) * 4) as usize) & 0x0FFF_FFFF;
        if e != 0 {
            let start = data_start + n * cluster_size as u64;
            let end = start + cluster_size as u64;
            // An excluded cluster is treated as free: its block may drop out, and
            // any block still copied gets these bytes zeroed by ZeroingBlockReader.
            if excluded.is_some_and(|ex| ex.covers(start, end)) {
                excluded_clusters += 1;
                continue;
            }
            used_clusters += 1;
            mark_bytes(&mut map, block_size, start, end);
        }
    }

    // Anything past the cluster heap (slack to the device end) is copied too.
    let heap_end = data_start + g.cluster_count * cluster_size as u64;
    mark_bytes(&mut map, block_size, heap_end, vol.len());

    let used_count = map.iter().filter(|&&u| u).count() as u64;
    info!(
        fs = "FAT32",
        cluster_size,
        cluster_count = g.cluster_count,
        used_clusters,
        excluded_clusters,
        used_blocks = used_count,
        total_blocks,
        "FAT32 used-cluster map ready"
    );
    Ok(FatUsedMap {
        block_used: map,
        used_count,
        filesystem: "FAT32",
        cluster_size,
    })
}

// ---------------------------------------------------------------------------
// exFAT
// ---------------------------------------------------------------------------

/// exFAT on-disk geometry, shared by the used-map builder and the exclusion
/// walker ([`crate::fat_exclusions`]).
pub struct ExfatGeometry {
    pub bytes_per_sector: u64,
    pub cluster_size: u32,
    pub cluster_count: u64,
    /// Byte offset of cluster 2 (start of the cluster heap).
    pub heap_start: u64,
    /// Byte offset of the (first) FAT.
    pub fat_offset: u64,
    pub fat_bytes: u64,
    /// First cluster of the root directory.
    pub root_cluster: u64,
    pub volume_bytes: u64,
}

/// Parse + validate exFAT geometry from the boot sector.
pub fn parse_exfat_geometry(boot: &[u8], vol_len: u64) -> Result<ExfatGeometry> {
    let volume_length = u64at(boot, 72); // sectors
    let fat_offset = u32at(boot, 80); // sectors
    let fat_length = u32at(boot, 84); // sectors
    let heap_offset = u32at(boot, 88); // sectors
    let cluster_count = u32at(boot, 92);
    let root_first = u32at(boot, 96);
    let bps_shift = boot[108];
    let spc_shift = boot[109];

    if !(9..=12).contains(&bps_shift) || spc_shift > 16 {
        return Err(bad(format!(
            "implausible exFAT shifts bps=2^{bps_shift} spc=2^{spc_shift}"
        )));
    }
    let bps = 1u64 << bps_shift;
    let cluster_size = (bps << spc_shift) as u32;
    if fat_offset == 0 || fat_length == 0 || heap_offset == 0 || cluster_count == 0 {
        return Err(bad("zero exFAT geometry fields"));
    }
    if volume_length * bps > vol_len {
        return Err(bad(format!(
            "filesystem claims {} bytes but the device is {}",
            volume_length * bps,
            vol_len
        )));
    }
    let heap_start = heap_offset * bps;
    let heap_end = heap_start + cluster_count * cluster_size as u64;
    if heap_end > volume_length * bps {
        return Err(bad("cluster heap extends past the volume"));
    }
    if !(2..cluster_count + 2).contains(&root_first) {
        return Err(bad("root directory cluster out of range"));
    }
    let fat_bytes = fat_length * bps;
    if fat_bytes > MAX_STRUCT_BYTES {
        return Err(bad("exFAT FAT is implausibly large"));
    }
    Ok(ExfatGeometry {
        bytes_per_sector: bps,
        cluster_size,
        cluster_count,
        heap_start,
        fat_offset: fat_offset * bps,
        fat_bytes,
        root_cluster: root_first,
        volume_bytes: volume_length * bps,
    })
}

fn build_exfat(
    vol: &mut dyn RawVolume,
    boot: &[u8],
    block_size: u32,
    excluded: Option<&ByteRanges>,
) -> Result<FatUsedMap> {
    let g = parse_exfat_geometry(boot, vol.len())?;
    let cluster_size = g.cluster_size;
    let cluster_count = g.cluster_count;
    let heap_start = g.heap_start;
    let heap_end = heap_start + cluster_count * cluster_size as u64;
    let root_first = g.root_cluster;

    // The FAT (for following cluster chains of the root dir / bitmap file).
    let mut fat = vec![0u8; g.fat_bytes as usize];
    vol.read_at(g.fat_offset, &mut fat)?;
    let fat_entry = |n: u64| -> u64 {
        let idx = (n * 4) as usize;
        if idx + 4 <= fat.len() { u32at(&fat, idx) } else { 0 }
    };
    let cluster_offset = |n: u64| heap_start + (n - 2) * cluster_size as u64;

    // Walk the root directory chain looking for the Allocation Bitmap entry
    // (type 0x81, BitmapFlags bit0 == 0 → the first/active bitmap).
    let mut bitmap_first = 0u64;
    let mut bitmap_len = 0u64;
    let mut dir_cluster = root_first;
    let mut buf = vec![0u8; cluster_size as usize];
    'walk: for _ in 0..4096 {
        vol.read_at(cluster_offset(dir_cluster), &mut buf)?;
        for entry in buf.chunks_exact(32) {
            match entry[0] {
                0x00 => break 'walk, // end of directory
                0x81 if entry[1] & 0x1 == 0 => {
                    bitmap_first = u32at(entry, 20);
                    bitmap_len = u64at(entry, 24);
                    break 'walk;
                }
                _ => {}
            }
        }
        match fat_entry(dir_cluster) {
            0xFFFF_FFFF => break, // end of chain
            next if (2..cluster_count + 2).contains(&next) => dir_cluster = next,
            _ => break, // broken chain — stop searching
        }
    }
    if bitmap_first == 0 {
        return Err(bad("allocation bitmap entry not found in the root directory"));
    }
    let needed = cluster_count.div_ceil(8);
    if bitmap_len < needed {
        return Err(bad(format!(
            "allocation bitmap too small: {bitmap_len} bytes for {cluster_count} clusters"
        )));
    }
    if !(2..cluster_count + 2).contains(&bitmap_first) {
        return Err(bad("allocation bitmap cluster out of range"));
    }

    // Read the bitmap file, following its FAT chain; if the chain is invalid
    // (bitmaps are almost always contiguous, some formatters do not chain
    // them), continue contiguously with a warning.
    let mut bitmap = Vec::with_capacity(needed as usize);
    let mut cluster = bitmap_first;
    while (bitmap.len() as u64) < needed {
        vol.read_at(cluster_offset(cluster), &mut buf)?;
        let take = (needed as usize - bitmap.len()).min(cluster_size as usize);
        bitmap.extend_from_slice(&buf[..take]);
        if (bitmap.len() as u64) >= needed {
            break;
        }
        cluster = match fat_entry(cluster) {
            next if (2..cluster_count + 2).contains(&next) => next,
            other => {
                if other != 0xFFFF_FFFF {
                    debug!(entry = other, "exFAT bitmap chain invalid — assuming contiguous");
                } else {
                    warn!("exFAT bitmap chain ended early — assuming contiguous");
                }
                cluster + 1
            }
        };
        if cluster >= cluster_count + 2 {
            return Err(bad("allocation bitmap runs past the cluster heap"));
        }
    }

    let total_blocks = vol.len().div_ceil(block_size as u64);
    let mut map = vec![false; total_blocks as usize];

    // Boot regions + FAT(s) + everything before the heap: always copied.
    mark_bytes(&mut map, block_size, 0, heap_start);

    let mut used_clusters = 0u64;
    for n in 0..cluster_count {
        if bitmap[(n / 8) as usize] & (1 << (n % 8)) != 0 {
            let start = heap_start + n * cluster_size as u64;
            let end = start + cluster_size as u64;
            if excluded.is_some_and(|ex| ex.covers(start, end)) {
                continue;
            }
            used_clusters += 1;
            mark_bytes(&mut map, block_size, start, end);
        }
    }

    // Tail past the heap (backup VBR slack etc.).
    mark_bytes(&mut map, block_size, heap_end, vol.len());

    let used_count = map.iter().filter(|&&u| u).count() as u64;
    info!(
        fs = "exFAT",
        cluster_size,
        cluster_count,
        used_clusters,
        used_blocks = used_count,
        total_blocks,
        "exFAT used-cluster map ready"
    );
    Ok(FatUsedMap {
        block_used: map,
        used_count,
        filesystem: "exFAT",
        cluster_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(bad("read past end"));
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Tiny FAT32: 512 B sectors, 1 sector/cluster, 4 reserved, 1 FAT of
    /// 2 sectors, 32 data clusters. Clusters 2, 3, 5 allocated.
    fn fat32_volume() -> MemVolume {
        let bps = 512u64;
        let reserved = 4u64;
        let fatsz = 2u64;
        let clusters = 32u64;
        let tot_sec = reserved + fatsz + clusters;
        let mut img = vec![0u8; (tot_sec * bps) as usize];

        img[11..13].copy_from_slice(&(bps as u16).to_le_bytes());
        img[13] = 1; // sectors per cluster
        img[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        img[16] = 1; // number of FATs
        img[19..21].copy_from_slice(&0u16.to_le_bytes()); // tot_sec16
        img[22..24].copy_from_slice(&0u16.to_le_bytes()); // fatsz16 (0 => FAT32)
        img[32..36].copy_from_slice(&(tot_sec as u32).to_le_bytes());
        img[36..40].copy_from_slice(&(fatsz as u32).to_le_bytes());
        img[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
        img[510] = 0x55;
        img[511] = 0xAA;

        let fat_off = (reserved * bps) as usize;
        let mut entry = |n: usize, v: u32| {
            img[fat_off + n * 4..fat_off + n * 4 + 4].copy_from_slice(&v.to_le_bytes());
        };
        entry(0, 0x0FFF_FFF8); // media
        entry(1, 0x0FFF_FFFF);
        entry(2, 0x0FFF_FFFF); // used
        entry(3, 0x0FFF_FFFF); // used
        entry(5, 0x0FFF_FFFF); // used
        MemVolume(img)
    }

    #[test]
    fn fat32_map_marks_metadata_and_used_clusters() {
        let mut vol = fat32_volume();
        let map = build_fat_used_map(&mut vol, 1024).unwrap();
        assert_eq!(map.filesystem, "FAT32");
        assert_eq!(map.cluster_size, 512);

        // Metadata: bytes [0, 3072) -> blocks 0..=2.
        for b in 0..=2 {
            assert!(map.is_block_used(b), "metadata block {b}");
        }
        // Cluster 2 @3072, 3 @3584 -> block 3; cluster 5 @4608 -> block 4.
        assert!(map.is_block_used(3));
        assert!(map.is_block_used(4));
        // Free clusters after that -> unused blocks.
        assert!(!map.is_block_used(5));
        assert_eq!(map.used_block_count(), 5);
    }

    #[test]
    fn fat32_rejects_bad_geometry() {
        // Signature missing.
        let mut img = fat32_volume().0;
        img[510] = 0;
        assert!(build_fat_used_map(&mut MemVolume(img), 1024).is_err());

        // Claims more sectors than the device has.
        let mut img = fat32_volume().0;
        img[32..36].copy_from_slice(&10_000u32.to_le_bytes());
        assert!(build_fat_used_map(&mut MemVolume(img), 1024).is_err());

        // FAT12/16-style (fatsz16 set) is not supported -> error (fallback).
        let mut img = fat32_volume().0;
        img[22..24].copy_from_slice(&2u16.to_le_bytes());
        assert!(build_fat_used_map(&mut MemVolume(img), 1024).is_err());

        // Garbage.
        assert!(build_fat_used_map(&mut MemVolume(vec![0u8; 4096]), 1024).is_err());
    }

    /// Tiny exFAT: 512 B sectors, 1 sector/cluster, FAT at sector 4 (1 sector),
    /// heap at sector 8, 16 clusters, root dir in cluster 4, bitmap file in
    /// cluster 2. Allocated: clusters 2, 3, 4, 7.
    fn exfat_volume() -> MemVolume {
        let bps = 512u64;
        let heap_off = 8u64;
        let clusters = 16u64;
        let vol_len = heap_off + clusters; // sectors
        let mut img = vec![0u8; (vol_len * bps) as usize];

        img[3..11].copy_from_slice(b"EXFAT   ");
        img[72..80].copy_from_slice(&vol_len.to_le_bytes());
        img[80..84].copy_from_slice(&4u32.to_le_bytes()); // fat offset
        img[84..88].copy_from_slice(&1u32.to_le_bytes()); // fat length
        img[88..92].copy_from_slice(&(heap_off as u32).to_le_bytes());
        img[92..96].copy_from_slice(&(clusters as u32).to_le_bytes());
        img[96..100].copy_from_slice(&4u32.to_le_bytes()); // root dir cluster
        img[108] = 9; // 512 B sectors
        img[109] = 0; // 1 sector/cluster
        img[510] = 0x55;
        img[511] = 0xAA;

        // FAT: bitmap (cluster 2) and root (cluster 4) are single-cluster chains.
        let fat_off = (4 * bps) as usize;
        img[fat_off + 2 * 4..fat_off + 2 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        img[fat_off + 4 * 4..fat_off + 4 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        // Allocation bitmap in cluster 2: bits for clusters 2,3,4,7.
        let c = |n: u64| ((heap_off + (n - 2)) * bps) as usize;
        img[c(2)] = 0b0010_0111;

        // Root dir in cluster 4: bitmap entry then end-of-directory.
        let root = c(4);
        img[root] = 0x81;
        img[root + 20..root + 24].copy_from_slice(&2u32.to_le_bytes()); // first cluster
        img[root + 24..root + 32].copy_from_slice(&2u64.to_le_bytes()); // 2 bytes
        MemVolume(img)
    }

    #[test]
    fn exfat_map_marks_metadata_and_used_clusters() {
        let mut vol = exfat_volume();
        let map = build_fat_used_map(&mut vol, 1024).unwrap();
        assert_eq!(map.filesystem, "exFAT");

        // Metadata [0, 4096) -> blocks 0..=3.
        for b in 0..=3 {
            assert!(map.is_block_used(b), "metadata block {b}");
        }
        // Clusters 2,3 @4096..5120 -> block 4; cluster 4 @5120 -> block 5;
        // cluster 7 @6656 -> block 6.
        assert!(map.is_block_used(4));
        assert!(map.is_block_used(5));
        assert!(map.is_block_used(6));
        // Clusters 8.. are free.
        assert!(!map.is_block_used(7));
        assert_eq!(map.used_block_count(), 7);
    }

    #[test]
    fn exfat_missing_bitmap_entry_is_an_error() {
        let mut img = exfat_volume().0;
        // Wipe the root directory -> no 0x81 entry.
        let root = ((8 + 2) * 512) as usize;
        img[root..root + 512].fill(0);
        assert!(build_fat_used_map(&mut MemVolume(img), 1024).is_err());
    }
}
