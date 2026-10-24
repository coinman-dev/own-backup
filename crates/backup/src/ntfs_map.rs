//! Used-cluster map for NTFS, parsed directly from the raw volume.
//!
//! On Windows the engine gets this bitmap for free from
//! `FSCTL_GET_VOLUME_BITMAP` (see [`crate::used_sectors_win`]). On Linux there
//! is no NTFS driver we can trust for the allocation bitmap, so — exactly as
//! [`crate::ext_map`]/[`crate::fat_map`] do for their filesystems — we read the
//! on-disk structures ourselves: the boot sector (BPB), MFT record #6
//! (`$Bitmap`) and its non-resident `$DATA` cluster-allocation bitmap. Only
//! allocated clusters (plus everything the bitmap does not cover) are copied.
//!
//! This is strictly read-only and reads no file data — only the boot sector,
//! one MFT record and the bitmap stream it points at.
//!
//! Failure policy mirrors the other maps: ANY doubt — bad signature, geometry
//! that does not fit the device, a failed update-sequence fixup, an attribute
//! layout we don't handle, a bitmap that leaves the MFT or the boot sector
//! marked free — is an error, and the engine falls back to sector-by-sector.
//! Missing data in an image is far worse than a bigger image. NTFS on-disk
//! integers are little-endian.

use tracing::info;

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::used_sectors::UsedSectorMap;

/// `$Bitmap` is fixed MFT record number 6 in every NTFS volume.
const BITMAP_RECORD: u64 = 6;
/// `$Volume` — record number 3 — carries the volume label.
const VOLUME_RECORD: u64 = 3;
/// Attribute type code for `$DATA`.
const ATTR_DATA: u64 = 0x80;
/// Attribute type code for `$VOLUME_NAME`.
const ATTR_VOLUME_NAME: u64 = 0x60;
/// Attribute-list end marker.
const ATTR_END: u64 = 0xFFFF_FFFF;
/// Attribute flag: the stream is compressed (we cannot follow its runs).
const ATTR_FLAG_COMPRESSED: u64 = 0x0001;

/// Largest MFT record we will read (real NTFS uses 1 KiB, occasionally 4 KiB).
const MAX_MFT_RECORD_SIZE: u64 = 64 * 1024;
/// Largest cluster-allocation bitmap we will assemble (512 MiB ≈ 4 billion
/// clusters ≈ a 16 TiB volume at 4 KiB clusters — larger volumes use bigger
/// clusters and stay well under this).
const MAX_BITMAP_BYTES: u64 = 512 * 1024 * 1024;
/// Largest NTFS cluster we accept (Windows tops out at 2 MiB).
const MAX_CLUSTER_SIZE: u64 = 2 * 1024 * 1024;

/// Per-backup-block used map for an NTFS volume.
pub struct NtfsUsedMap {
    block_used: Vec<bool>,
    used_count: u64,
    /// NTFS cluster size in bytes.
    pub cluster_size: u32,
}

impl UsedSectorMap for NtfsUsedMap {
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

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("NTFS map: {msg}"))
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

/// Mark the byte range `[start, end)` as used in the per-backup-block map.
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

/// Test one cluster's bit in the allocation bitmap.
fn cluster_allocated(bitmap: &[u8], cluster: u64) -> bool {
    bitmap
        .get((cluster / 8) as usize)
        .is_some_and(|b| b & (1 << (cluster % 8)) != 0)
}

/// A non-resident `$DATA` run: `count` clusters starting at LCN `lcn`. `hole`
/// runs (sparse) contribute zero bytes to the stream and have no LCN.
struct Run {
    lcn: u64,
    count: u64,
    hole: bool,
}

/// The volume geometry the boot sector declares — everything the rest of the
/// parsing needs, and nothing that costs a read to get.
struct Geometry {
    bytes_per_sector: u64,
    cluster_size: u64,
    total_clusters: u64,
    /// `total_clusters * cluster_size` — the clustered area; the device may be
    /// a little longer (the backup boot sector lives past it).
    clusters_bytes: u64,
    mft_lcn: u64,
    mft_record_size: u64,
}

/// Read and check the NTFS boot sector. Errors on anything that is not NTFS or
/// does not fit the device — the callers all treat that as "not our
/// filesystem", never as a reason to guess.
fn read_geometry(vol: &mut dyn RawVolume) -> Result<Geometry> {
    // --- Boot sector / BPB ---
    let mut boot = [0u8; 512];
    vol.read_at(0, &mut boot)?;
    if &boot[3..11] != b"NTFS    " {
        return Err(bad("no 'NTFS' OEM id in the boot sector"));
    }
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(bad("no boot sector signature (55AA)"));
    }

    let bytes_per_sector = u16at(&boot, 0x0B);
    if !bytes_per_sector.is_power_of_two() || !(256..=4096).contains(&bytes_per_sector) {
        return Err(bad(format!("implausible bytes/sector {bytes_per_sector}")));
    }
    // Sectors/cluster: literal 1..=128, or (>0x80) the log form 2^(0x100-value).
    let spc_raw = boot[0x0D] as u64;
    let spc = if spc_raw <= 0x80 {
        spc_raw
    } else if (0xF4..=0xFF).contains(&spc_raw) {
        1u64 << (0x100 - spc_raw)
    } else {
        return Err(bad(format!(
            "implausible sectors/cluster byte {spc_raw:#x}"
        )));
    };
    let cluster_size = bytes_per_sector
        .checked_mul(spc)
        .filter(|c| c.is_power_of_two() && (512..=MAX_CLUSTER_SIZE).contains(c))
        .ok_or_else(|| {
            bad(format!(
                "implausible cluster size ({bytes_per_sector}*{spc})"
            ))
        })?;

    let total_sectors = u64at(&boot, 0x28);
    let mft_lcn = u64at(&boot, 0x30);
    let total_clusters = total_sectors / spc;
    if total_sectors == 0 || total_clusters == 0 {
        return Err(bad("zero total sectors/clusters"));
    }
    // The clustered area must fit the device (the last sector — the backup boot
    // sector — lives past it and is copied via the tail below).
    let clusters_bytes = total_clusters
        .checked_mul(cluster_size)
        .ok_or_else(|| bad("total_clusters * cluster_size overflows"))?;
    if clusters_bytes > vol.len() {
        return Err(bad(format!(
            "filesystem claims {clusters_bytes} clustered bytes but the device is {}",
            vol.len()
        )));
    }
    if mft_lcn >= total_clusters {
        return Err(bad(format!("$MFT LCN {mft_lcn} outside the volume")));
    }

    // MFT record size: positive → clusters/record, negative → 2^|value| bytes.
    let cpm = boot[0x40] as i8;
    let mft_record_size = if cpm >= 0 {
        (cpm as u64) * cluster_size
    } else {
        1u64 << ((-cpm) as u32)
    };
    if !mft_record_size.is_power_of_two() || !(256..=MAX_MFT_RECORD_SIZE).contains(&mft_record_size)
    {
        return Err(bad(format!(
            "implausible MFT record size {mft_record_size}"
        )));
    }

    Ok(Geometry {
        bytes_per_sector,
        cluster_size,
        total_clusters,
        clusters_bytes,
        mft_lcn,
        mft_record_size,
    })
}

/// Read one of the first sixteen MFT records, fixups applied.
///
/// Those sixteen are guaranteed contiguous from the `$MFT`'s first cluster, so
/// their offsets are arithmetic — no need to walk the `$MFT`'s own runs.
fn read_fixed_mft_record(vol: &mut dyn RawVolume, g: &Geometry, number: u64) -> Result<Vec<u8>> {
    let record_off = g.mft_lcn * g.cluster_size + number * g.mft_record_size;
    if record_off + g.mft_record_size > vol.len() {
        return Err(bad(format!("MFT record #{number} runs past the device")));
    }
    let mut rec = vec![0u8; g.mft_record_size as usize];
    vol.read_at(record_off, &mut rec)?;
    apply_fixups(&mut rec, g.bytes_per_sector)?;
    Ok(rec)
}

/// Read the volume's cluster-allocation bitmap (`$Bitmap`'s `$DATA`), checked.
fn read_cluster_bitmap(vol: &mut dyn RawVolume, g: &Geometry) -> Result<Vec<u8>> {
    let rec = read_fixed_mft_record(vol, g, BITMAP_RECORD)?;

    // --- Locate the unnamed non-resident $DATA ($Bitmap's cluster bitmap) ---
    let needed = g.total_clusters.div_ceil(8);
    if needed > MAX_BITMAP_BYTES {
        return Err(bad(format!(
            "bitmap would need {needed} bytes (volume too large)"
        )));
    }
    let bitmap = read_bitmap_stream(vol, &rec, g.cluster_size, needed)?;

    // Strong guards: a correctly parsed bitmap always marks the boot sector
    // (cluster 0) and the $MFT itself as allocated. If either reads free we
    // misaligned somewhere — refuse and let the engine copy every sector.
    if !cluster_allocated(&bitmap, 0) {
        return Err(bad(
            "bitmap leaves cluster 0 (boot) free — parse misaligned",
        ));
    }
    if !cluster_allocated(&bitmap, g.mft_lcn) {
        return Err(bad(
            "bitmap leaves the $MFT cluster free — parse misaligned",
        ));
    }
    Ok(bitmap)
}

/// How many bytes an NTFS volume has in use, straight from `$Bitmap`.
///
/// Windows answers this with `GetDiskFreeSpaceEx` for a volume it has mounted.
/// A volume inside a container we have just unlocked ourselves is not mounted
/// and cannot be — so the same number is counted here, from the same bitmap the
/// used-block map is built from. Cheap next to the copy that follows: one MFT
/// record and the bitmap stream (≈11 MiB for a 350 GiB volume), then a popcount.
pub fn ntfs_used_bytes(vol: &mut dyn RawVolume) -> Result<u64> {
    let g = read_geometry(vol)?;
    let bitmap = read_cluster_bitmap(vol, &g)?;
    let needed = g.total_clusters.div_ceil(8) as usize;
    // The last byte covers clusters past the end of the volume; those bits are
    // not always zero, so mask them off rather than counting phantom clusters.
    let tail_bits = (g.total_clusters % 8) as u32;
    let mut used = 0u64;
    for (i, &byte) in bitmap.iter().take(needed).enumerate() {
        let byte = if i + 1 == needed && tail_bits != 0 {
            byte & ((1u16 << tail_bits) - 1) as u8
        } else {
            byte
        };
        used += byte.count_ones() as u64;
    }
    Ok(used * g.cluster_size)
}

/// Which clusters an NTFS volume currently has free, read off `$Bitmap`.
///
/// The true-removal pass needs this after the FS driver has deleted the
/// excluded entries: the clusters those files held are now free, and the pass
/// zeroes them so the data does not linger in the image's free space. Asking
/// the bitmap — rather than trusting the extent list collected before the
/// deletions — is what makes the zeroing safe: a cluster the filesystem reused
/// for its own metadata while deleting reads as ALLOCATED here and is left
/// alone. Overwriting it would corrupt the very filesystem we just repaired.
pub struct NtfsFreeMap {
    bitmap: Vec<u8>,
    pub cluster_size: u64,
    pub total_clusters: u64,
}

impl NtfsFreeMap {
    /// Is this cluster free? Clusters past the end of the volume are not.
    pub fn is_free(&self, cluster: u64) -> bool {
        cluster < self.total_clusters && !cluster_allocated(&self.bitmap, cluster)
    }

    /// Narrow `ranges` (volume-relative byte ranges) down to the clusters that
    /// are free right now. Returns merged byte ranges, so a run of adjacent
    /// free clusters comes back as one write.
    ///
    /// WHOLE clusters, never a slice of one: allocation is cluster-granular, so
    /// a free cluster that a range touches at all belonged entirely to what was
    /// deleted — and a raw disk refuses any write that is not a whole number of
    /// sectors anyway. Ranges are clipped to the volume's own length.
    pub fn free_parts_of(&self, ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
        let cs = self.cluster_size;
        let mut out: Vec<(u64, u64)> = Vec::new();
        for &(start, end) in ranges {
            if end <= start {
                continue;
            }
            let first = start / cs;
            let last = ((end - 1) / cs).min(self.total_clusters.saturating_sub(1));
            for cluster in first..=last {
                if !self.is_free(cluster) {
                    continue;
                }
                let (s, e) = (cluster * cs, cluster * cs + cs);
                match out.last_mut() {
                    Some(prev) if prev.1 == s => prev.1 = e,
                    _ => out.push((s, e)),
                }
            }
        }
        out
    }
}

/// The volume's cluster size, from its boot sector alone (one 512-byte read).
///
/// Errors when the volume is not NTFS — which is also the answer to "can the
/// in-image exclusion pass work here", since everything it does downstream
/// reads `$Bitmap`.
pub fn ntfs_cluster_size(vol: &mut dyn RawVolume) -> Result<u32> {
    Ok(read_geometry(vol)?.cluster_size as u32)
}

/// Read `$Bitmap` off an NTFS volume and answer which clusters are free.
pub fn ntfs_free_map(vol: &mut dyn RawVolume) -> Result<NtfsFreeMap> {
    let g = read_geometry(vol)?;
    let bitmap = read_cluster_bitmap(vol, &g)?;
    Ok(NtfsFreeMap {
        bitmap,
        cluster_size: g.cluster_size,
        total_clusters: g.total_clusters,
    })
}

/// The volume label of an NTFS volume, or `None` when it has none.
///
/// It lives in MFT record #3 (`$Volume`), attribute `$VOLUME_NAME` — not in the
/// boot sector, which is why [`own_backup_inventory::fs_probe`] cannot report
/// it and every unlocked NTFS volume would otherwise show up unnamed.
pub fn ntfs_volume_label(vol: &mut dyn RawVolume) -> Option<String> {
    let g = read_geometry(vol).ok()?;
    let rec = read_fixed_mft_record(vol, &g, VOLUME_RECORD).ok()?;
    let value = resident_attr(&rec, ATTR_VOLUME_NAME)?;
    // UTF-16LE, no terminator.
    let utf16: Vec<u16> = value
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&utf16).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The value of a resident attribute of type `atype`, if the record has one.
fn resident_attr(rec: &[u8], atype: u64) -> Option<&[u8]> {
    let first_attr = u16at(rec, 0x14) as usize;
    let used = u32at(rec, 0x18) as usize;
    if first_attr < 42 || first_attr > rec.len() || used > rec.len() || used < first_attr {
        return None;
    }
    let mut off = first_attr;
    while off + 24 <= used {
        let t = u32at(rec, off);
        if t == ATTR_END {
            return None;
        }
        let alen = u32at(rec, off + 4) as usize;
        if alen < 24 || off + alen > rec.len() {
            return None;
        }
        if t == atype && rec[off + 8] == 0 {
            let val_len = u32at(rec, off + 0x10) as usize;
            let val_off = u16at(rec, off + 0x14) as usize;
            let end = off.checked_add(val_off)?.checked_add(val_len)?;
            if end > rec.len() || end > off + alen {
                return None;
            }
            return Some(&rec[off + val_off..end]);
        }
        off += alen;
    }
    None
}

/// Detect NTFS on `vol` and build the used map. Errors (→ caller falls back to
/// sector-by-sector) on anything unrecognized or inconsistent.
pub fn build_ntfs_used_map(vol: &mut dyn RawVolume, block_size: u32) -> Result<NtfsUsedMap> {
    build_ntfs_used_map_excl(vol, block_size, None)
}

/// Та же карта, но кластеры из `excluded` в неё не попадают.
///
/// Так исключения работают ДО копирования (план
/// `.ai/exclusions-before-copy-plan_2026-08-19.md`): блок, все занятые кластеры
/// которого исключены, не читается и не пишется вовсе — как это делает Acronis.
/// Блок, где исключённое соседствует с нужным, остаётся в карте: выбросить его
/// значило бы потерять соседей, и лишнее в нём затирает `ZeroingBlockReader`
/// уже в памяти, при копировании.
///
/// Тот же приём, что у FAT (`build_fat_used_map_excl`): карта строится заново,
/// а не правится задним числом, потому что «все ли занятые кластеры блока
/// исключены» видно только на кластерах, а в готовой карте их уже нет.
pub fn build_ntfs_used_map_excl(
    vol: &mut dyn RawVolume,
    block_size: u32,
    excluded: Option<&crate::exclusions::ByteRanges>,
) -> Result<NtfsUsedMap> {
    let g = read_geometry(vol)?;
    let (cluster_size, total_clusters, clusters_bytes) =
        (g.cluster_size, g.total_clusters, g.clusters_bytes);
    let needed = total_clusters.div_ceil(8);
    let bitmap = read_cluster_bitmap(vol, &g)?;

    // --- Fold allocated clusters into the per-backup-block map ---
    let total_blocks = vol.len().div_ceil(block_size as u64);
    let mut map = vec![false; total_blocks as usize];

    // Bytes past the last cluster (backup boot sector etc.) aren't in the
    // bitmap — always copy them.
    mark_bytes(&mut map, block_size, clusters_bytes, vol.len());

    let mut used_clusters = 0u64;
    let mut excluded_clusters = 0u64;
    for (byte_idx, &byte) in bitmap.iter().take(needed as usize).enumerate() {
        if byte == 0 {
            continue;
        }
        for bit in 0..8u64 {
            if byte & (1 << bit) == 0 {
                continue;
            }
            let cluster = byte_idx as u64 * 8 + bit;
            if cluster >= total_clusters {
                break;
            }
            let start = cluster * cluster_size;
            // Исключённый кластер в карту не попадает: копия его не увидит.
            if excluded.is_some_and(|e| e.covers(start, start + cluster_size)) {
                excluded_clusters += 1;
                continue;
            }
            used_clusters += 1;
            mark_bytes(&mut map, block_size, start, start + cluster_size);
        }
    }

    let used_count = map.iter().filter(|&&u| u).count() as u64;
    info!(
        fs = "ntfs",
        cluster_size,
        total_clusters,
        used_clusters,
        excluded_clusters,
        used_blocks = used_count,
        total_blocks,
        "NTFS used-cluster map ready"
    );
    Ok(NtfsUsedMap {
        block_used: map,
        used_count,
        cluster_size: cluster_size as u32,
    })
}

/// Apply the NTFS update-sequence (fixup) array to an MFT record in place. The
/// last two bytes of every sector-sized stride hold a copy of the update
/// sequence number and must be replaced by the real bytes stored in the array.
fn apply_fixups(rec: &mut [u8], bytes_per_sector: u64) -> Result<()> {
    if &rec[0..4] != b"FILE" {
        return Err(bad("MFT record lacks the 'FILE' signature"));
    }
    let usa_off = u16at(rec, 4) as usize;
    let usa_count = u16at(rec, 6) as usize; // update sequence number + one entry per stride
    if usa_count == 0 {
        return Err(bad("zero update-sequence count"));
    }
    let fixups = usa_count - 1;
    if fixups == 0 {
        return Ok(());
    }
    if usa_off < 42 || usa_off + usa_count * 2 > rec.len() {
        return Err(bad("update-sequence array outside the record"));
    }
    // Fixups sit at the end of each sector-sized stride; the record is a whole
    // number of strides (or one sub-sector record → a single fixup).
    let stride = if bytes_per_sector as usize <= rec.len() {
        bytes_per_sector as usize
    } else {
        rec.len()
    };
    if stride == 0 || rec.len() % stride != 0 || rec.len() / stride != fixups {
        return Err(bad("record size does not match the fixup count"));
    }

    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 0..fixups {
        let pos = stride * (i + 1) - 2;
        if rec[pos] != usn[0] || rec[pos + 1] != usn[1] {
            return Err(bad("update-sequence fixup mismatch — corrupt MFT record"));
        }
        let src = usa_off + 2 * (i + 1);
        rec[pos] = rec[src];
        rec[pos + 1] = rec[src + 1];
    }
    Ok(())
}

/// Walk the record's attributes, find the unnamed `$DATA`, and return its
/// cluster-allocation bitmap (`needed` bytes). Handles both the (usual)
/// non-resident stream and the resident case for tiny volumes.
fn read_bitmap_stream(
    vol: &mut dyn RawVolume,
    rec: &[u8],
    cluster_size: u64,
    needed: u64,
) -> Result<Vec<u8>> {
    let first_attr = u16at(rec, 0x14) as usize;
    let used = u32at(rec, 0x18) as usize;
    if first_attr < 42 || first_attr > rec.len() || used > rec.len() || used < first_attr {
        return Err(bad("MFT record header offsets out of range"));
    }

    let mut off = first_attr;
    while off + 8 <= used {
        let atype = u32at(rec, off);
        if atype == ATTR_END {
            break;
        }
        let alen = u32at(rec, off + 4) as usize;
        if alen < 24 || off + alen > rec.len() {
            return Err(bad("attribute length out of range"));
        }

        if atype == ATTR_DATA && rec[off + 9] == 0 {
            // Unnamed $DATA — the cluster bitmap.
            let non_resident = rec[off + 8];
            let attr_flags = u16at(rec, off + 0x0C);
            if non_resident == 0 {
                // Resident: tiny volume, bitmap sits inside the record.
                let val_len = u32at(rec, off + 0x10) as usize;
                let val_off = u16at(rec, off + 0x14) as usize;
                if off + val_off + val_len > rec.len() || (val_len as u64) < needed {
                    return Err(bad("resident $Bitmap too small or out of range"));
                }
                return Ok(rec[off + val_off..off + val_off + needed as usize].to_vec());
            }
            if attr_flags & ATTR_FLAG_COMPRESSED != 0 {
                return Err(bad("$Bitmap $DATA is compressed"));
            }
            let start_vcn = u64at(rec, off + 0x10);
            if start_vcn != 0 {
                return Err(bad(
                    "$Bitmap $DATA does not start at VCN 0 (attribute list?)",
                ));
            }
            let real_size = u64at(rec, off + 0x30);
            if real_size < needed {
                return Err(bad(format!(
                    "$Bitmap real size {real_size} < needed {needed}"
                )));
            }
            let runs_off = u16at(rec, off + 0x20) as usize;
            if runs_off < 24 || off + alen > rec.len() || runs_off >= alen {
                return Err(bad("bad mapping-pairs offset"));
            }
            let runs = parse_data_runs(&rec[off + runs_off..off + alen])?;
            return assemble_bitmap(vol, &runs, cluster_size, needed);
        }
        off += alen;
    }
    Err(bad("no unnamed $DATA attribute in the $Bitmap record"))
}

/// Parse an NTFS mapping-pairs (data-runs) list into absolute-LCN runs.
fn parse_data_runs(runs: &[u8]) -> Result<Vec<Run>> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    let mut i = 0usize;
    while i < runs.len() {
        let header = runs[i];
        if header == 0 {
            break;
        }
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        i += 1;
        if len_bytes == 0
            || len_bytes > 8
            || off_bytes > 8
            || i + len_bytes + off_bytes > runs.len()
        {
            return Err(bad("truncated or malformed data run"));
        }

        let mut count: u64 = 0;
        for b in 0..len_bytes {
            count |= (runs[i + b] as u64) << (8 * b);
        }
        i += len_bytes;
        if count == 0 {
            return Err(bad("zero-length data run"));
        }

        if off_bytes == 0 {
            out.push(Run {
                lcn: 0,
                count,
                hole: true,
            });
        } else {
            let mut delta: i64 = 0;
            for b in 0..off_bytes {
                delta |= (runs[i + b] as i64) << (8 * b);
            }
            // Sign-extend the signed LCN delta.
            let shift = 64 - 8 * off_bytes as u32;
            delta = (delta << shift) >> shift;
            i += off_bytes;
            lcn = lcn
                .checked_add(delta)
                .ok_or_else(|| bad("data run LCN overflow"))?;
            if lcn < 0 {
                return Err(bad("negative LCN in data run"));
            }
            out.push(Run {
                lcn: lcn as u64,
                count,
                hole: false,
            });
        }
    }
    if out.is_empty() {
        return Err(bad("empty data-run list"));
    }
    Ok(out)
}

/// Read `needed` bytes of the bitmap stream from its runs (holes read as zero).
fn assemble_bitmap(
    vol: &mut dyn RawVolume,
    runs: &[Run],
    cluster_size: u64,
    needed: u64,
) -> Result<Vec<u8>> {
    let mut bitmap = vec![0u8; needed as usize];
    let mut filled: u64 = 0;
    for run in runs {
        if filled >= needed {
            break;
        }
        let run_bytes = run
            .count
            .checked_mul(cluster_size)
            .ok_or_else(|| bad("data run byte size overflows"))?;
        let take = run_bytes.min(needed - filled);
        if !run.hole {
            let disk_off = run.lcn * cluster_size;
            if disk_off + take > vol.len() {
                return Err(bad("data run points past the device"));
            }
            vol.read_at(
                disk_off,
                &mut bitmap[filled as usize..(filled + take) as usize],
            )?;
        }
        filled += take;
    }
    if filled < needed {
        return Err(bad("data runs cover less than the $Bitmap size"));
    }
    Ok(bitmap)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            if e > self.0.len() {
                return Err(bad("read past end"));
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn put_u16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A tiny NTFS volume: 512-byte sectors, 1 sector/cluster (512-byte
    /// clusters), 64 clusters. $MFT at cluster 4, 1 KiB MFT records, so record
    /// #6 sits at byte 8192 (cluster 16). Its $Bitmap $DATA is a single-run
    /// non-resident stream living in cluster 20. Allocated clusters (per the
    /// bitmap): 0, 4, 16, 17, 20, 21.
    fn tiny_ntfs() -> MemVolume {
        let bps = 512u64;
        let spc = 1u64;
        let cluster = bps * spc; // 512
        let total_sectors = 64u64;
        let mut img = vec![0u8; (total_sectors * bps) as usize];

        // Boot sector.
        img[3..11].copy_from_slice(b"NTFS    ");
        put_u16(&mut img, 0x0B, bps as u16);
        img[0x0D] = spc as u8;
        put_u64(&mut img, 0x28, total_sectors);
        put_u64(&mut img, 0x30, 4); // $MFT LCN
        put_u64(&mut img, 0x38, 32); // $MFTMirr LCN
        img[0x40] = (-10i8) as u8; // clusters_per_MFT_record → 2^10 = 1024 bytes
        img[510] = 0x55;
        img[511] = 0xAA;

        // MFT record #6 at byte 8192 (1024 bytes, two 512-byte fixup strides).
        let rec = 8192usize;
        img[rec..rec + 4].copy_from_slice(b"FILE");
        put_u16(&mut img, rec + 4, 0x30); // update-sequence array offset
        put_u16(&mut img, rec + 6, 3); // USN + two fixups
        put_u16(&mut img, rec + 0x14, 0x38); // first attribute offset
        put_u32(&mut img, rec + 0x18, 0x38 + 0x48 + 4); // used size (attr + end marker)
                                                        // Update sequence array at 0x30: USN then two saved words (all zero here).
        put_u16(&mut img, rec + 0x30, 0x0001); // USN
        put_u16(&mut img, rec + 0x32, 0x0000); // saved word for stride 0
        put_u16(&mut img, rec + 0x34, 0x0000); // saved word for stride 1
                                               // Fixup slots (last 2 bytes of each 512-byte stride) carry the USN.
        put_u16(&mut img, rec + 510, 0x0001);
        put_u16(&mut img, rec + 1022, 0x0001);

        // $DATA attribute at record+0x38 (non-resident, unnamed).
        let a = rec + 0x38;
        put_u32(&mut img, a + 0x00, 0x80); // type = $DATA
        put_u32(&mut img, a + 0x04, 0x48); // attribute length
        img[a + 0x08] = 1; // non-resident
        img[a + 0x09] = 0; // name length
        put_u64(&mut img, a + 0x10, 0); // starting VCN
        put_u64(&mut img, a + 0x18, 0); // last VCN (1 cluster)
        put_u16(&mut img, a + 0x20, 0x40); // mapping-pairs offset
        put_u64(&mut img, a + 0x28, cluster); // allocated size
        put_u64(&mut img, a + 0x30, 8); // real size = 8 bytes (64 clusters)
        put_u64(&mut img, a + 0x38, 8); // initialized size
                                        // Data runs at a+0x40: one run of 1 cluster at LCN 20.
        img[a + 0x40] = 0x11; // 1 length byte, 1 offset byte
        img[a + 0x41] = 0x01; // length = 1 cluster
        img[a + 0x42] = 20; // LCN delta = 20
        img[a + 0x43] = 0x00; // end of runs
                              // Attribute-list end marker just after the attribute.
        put_u32(&mut img, a + 0x48, 0xFFFF_FFFF);

        // MFT record #3 ($Volume) at byte 5120, with a resident $VOLUME_NAME.
        let vrec = 4 * cluster as usize + 3 * 1024;
        img[vrec..vrec + 4].copy_from_slice(b"FILE");
        put_u16(&mut img, vrec + 4, 0x30); // update-sequence array offset
        put_u16(&mut img, vrec + 6, 3); // USN + two fixups
        put_u16(&mut img, vrec + 0x14, 0x38); // first attribute offset
        put_u16(&mut img, vrec + 0x30, 0x0001); // USN
        put_u16(&mut img, vrec + 510, 0x0001);
        put_u16(&mut img, vrec + 1022, 0x0001);
        // $VOLUME_NAME at vrec+0x38: resident, value (UTF-16LE) at +0x18.
        let name: Vec<u16> = "Тест".encode_utf16().collect();
        let val_len = name.len() * 2;
        let alen = 0x18 + val_len.next_multiple_of(8);
        let v = vrec + 0x38;
        put_u32(&mut img, v + 0x00, 0x60); // type = $VOLUME_NAME
        put_u32(&mut img, v + 0x04, alen as u32);
        img[v + 0x08] = 0; // resident
        img[v + 0x09] = 0; // name length
        put_u32(&mut img, v + 0x10, val_len as u32);
        put_u16(&mut img, v + 0x14, 0x18); // value offset
        for (i, unit) in name.iter().enumerate() {
            put_u16(&mut img, v + 0x18 + i * 2, *unit);
        }
        put_u32(&mut img, v + alen, 0xFFFF_FFFF); // attribute-list end
        put_u32(&mut img, vrec + 0x18, (0x38 + alen + 4) as u32); // used size

        // Allocation bitmap in cluster 20 (byte 10240): clusters 0,4,16,17,20,21.
        let bm = 20 * cluster as usize;
        img[bm] = 0b0001_0001; // bits 0 and 4  → clusters 0, 4
        img[bm + 2] = 0b0011_0011; // bits 0,1,4,5 → clusters 16,17,20,21
        MemVolume(img)
    }

    /// The bytes of [`tiny_ntfs`], for tests that read it from a file (the
    /// Linux platform's).
    #[cfg(target_os = "linux")]
    pub(crate) fn tiny_ntfs_image() -> Vec<u8> {
        tiny_ntfs().0
    }

    #[test]
    fn marks_allocated_clusters() {
        let mut vol = tiny_ntfs();
        // Backup block size == cluster size → block N maps to cluster N.
        let map = build_ntfs_used_map(&mut vol, 512).unwrap();
        assert_eq!(map.cluster_size, 512);

        for c in [0u64, 4, 16, 17, 20, 21] {
            assert!(map.is_block_used(c), "cluster {c} should be used");
        }
        for c in [1u64, 2, 3, 5, 15, 18, 22, 63] {
            assert!(!map.is_block_used(c), "cluster {c} should be free");
        }
        assert_eq!(map.used_block_count(), 6);
    }

    #[test]
    fn folds_clusters_into_larger_backup_blocks() {
        let mut vol = tiny_ntfs();
        // 4 KiB backup blocks over 512-byte clusters: 8 clusters per block.
        let map = build_ntfs_used_map(&mut vol, 4096).unwrap();
        // Clusters 0 & 4 → block 0; 16,17 → block 2; 20,21 → block 2.
        assert!(map.is_block_used(0));
        assert!(!map.is_block_used(1));
        assert!(map.is_block_used(2));
        for b in [3u64, 4, 5, 6, 7] {
            assert!(!map.is_block_used(b), "block {b} should be free");
        }
        assert_eq!(map.used_block_count(), 2);
    }

    #[test]
    fn rejects_non_ntfs() {
        assert!(build_ntfs_used_map(&mut MemVolume(vec![0u8; 64 * 512]), 512).is_err());
        assert!(ntfs_used_bytes(&mut MemVolume(vec![0u8; 64 * 512])).is_err());
    }

    #[test]
    fn counts_used_bytes_from_the_bitmap() {
        // Six allocated clusters of 512 bytes — the number the wizard shows for
        // a volume Windows cannot be asked about.
        assert_eq!(ntfs_used_bytes(&mut tiny_ntfs()).unwrap(), 6 * 512);
    }

    #[test]
    fn ignores_bitmap_bits_past_the_last_cluster() {
        // 60 clusters: the last bitmap byte covers four that do not exist. NTFS
        // leaves those bits set, and counting them would overstate "занято".
        let mut img = tiny_ntfs().0;
        put_u64(&mut img, 0x28, 60);
        img[20 * 512 + 7] = 0xF0; // bits 4..7 → clusters 60..63
        assert_eq!(ntfs_used_bytes(&mut MemVolume(img)).unwrap(), 6 * 512);
    }

    #[test]
    fn free_map_answers_only_for_clusters_that_are_really_free() {
        // tiny_ntfs allocates 0, 4, 16, 17, 20, 21 — the rest are free.
        let free = ntfs_free_map(&mut tiny_ntfs()).unwrap();
        assert_eq!(free.cluster_size, 512);
        for c in [0u64, 4, 16, 17, 20, 21] {
            assert!(!free.is_free(c), "cluster {c} is in use");
        }
        for c in [1u64, 2, 3, 5, 15, 18, 22, 63] {
            assert!(free.is_free(c), "cluster {c} is free");
        }
        // Past the end of the volume is not "free space" — it is not space.
        assert!(!free.is_free(64));
        assert!(!free.is_free(1_000));
    }

    #[test]
    fn zeroing_skips_clusters_the_filesystem_took_back() {
        // The heart of the post-deletion zero pass: an excluded file used to
        // hold clusters 16..22; by the time we look, the filesystem has taken
        // 16, 17, 20 and 21 back for itself. Only 18 and 19 may be zeroed —
        // overwriting the other four would corrupt live metadata.
        let free = ntfs_free_map(&mut tiny_ntfs()).unwrap();
        let parts = free.free_parts_of(&[(16 * 512, 22 * 512)]);
        assert_eq!(
            parts,
            vec![(18 * 512, 20 * 512)],
            "adjacent free clusters merge into one write"
        );

        // A range entirely inside free space comes back whole.
        assert_eq!(free.free_parts_of(&[(512, 4 * 512)]), vec![(512, 4 * 512)]);
        // Half a cluster in: the whole cluster is zeroed. Allocation is
        // cluster-granular, so the rest of it was that file's too — and a raw
        // disk would refuse a sub-sector write in any case.
        assert_eq!(
            free.free_parts_of(&[(512 + 100, 512 + 200)]),
            vec![(512, 2 * 512)]
        );
        // A range running past the end of the volume stops at the last cluster.
        assert_eq!(
            free.free_parts_of(&[(62 * 512, 500 * 512)]),
            vec![(62 * 512, 64 * 512)]
        );
        // A range entirely in use produces nothing at all.
        assert!(free.free_parts_of(&[(16 * 512, 18 * 512)]).is_empty());
        // Two runs separated by a used cluster stay separate writes.
        assert_eq!(
            free.free_parts_of(&[(3 * 512, 6 * 512)]),
            vec![(3 * 512, 4 * 512), (5 * 512, 6 * 512)]
        );
    }

    #[test]
    fn reads_the_volume_label() {
        assert_eq!(ntfs_volume_label(&mut tiny_ntfs()).as_deref(), Some("Тест"));
    }

    #[test]
    fn no_label_when_the_volume_record_has_none() {
        // Blank the $VOLUME_NAME type code: the attribute walk finds nothing and
        // the caller keeps whatever name it already had.
        let mut img = tiny_ntfs().0;
        put_u32(&mut img, 4 * 512 + 3 * 1024 + 0x38, 0xFFFF_FFFF);
        assert_eq!(ntfs_volume_label(&mut MemVolume(img)), None);
    }

    #[test]
    fn rejects_bad_fixup() {
        // Corrupt a fixup slot so the update-sequence check fails.
        let mut img = tiny_ntfs().0;
        img[8192 + 510] = 0xAB;
        assert!(build_ntfs_used_map(&mut MemVolume(img), 512).is_err());
    }

    #[test]
    fn rejects_bitmap_with_free_mft() {
        // Clear the $MFT's own bit (cluster 4) — the safety net must reject it.
        let mut img = tiny_ntfs().0;
        img[20 * 512] = 0b0000_0001; // keep cluster 0, drop cluster 4
        assert!(build_ntfs_used_map(&mut MemVolume(img), 512).is_err());
    }

    #[test]
    fn rejects_geometry_larger_than_device() {
        let mut img = tiny_ntfs().0;
        put_u64(&mut img, 0x28, 1_000_000); // total sectors far past the device
        assert!(build_ntfs_used_map(&mut MemVolume(img), 512).is_err());
    }
}
