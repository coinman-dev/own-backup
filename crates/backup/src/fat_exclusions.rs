//! FAT32 file exclusions.
//!
//! FAT has no `FSCTL_GET_RETRIEVAL_POINTERS` equivalent (and no ACLs), so we walk
//! the directory tree straight from the raw clusters — reusing the same
//! [`RawVolume`](crate::fat_map::RawVolume) abstraction as the used-map — match
//! every file against the job's glob patterns, and follow each matched file's FAT
//! cluster chain to its byte ranges. Those ranges are then passed to
//! [`build_fat_used_map_excl`](crate::fat_map::build_fat_used_map_excl), which
//! drops the excluded clusters cluster-by-cluster (like the NTFS path), and to
//! the `ZeroingBlockReader` for any block that is still copied.
//!
//! Failure policy (as elsewhere): any parse ambiguity keeps the file in the
//! backup — excluding less is safe, corrupting the image is not.

use std::collections::HashSet;

use tracing::info;

use own_backup_common::Result;

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes, LeftInCopy, LeftReason};
use crate::fat_map::{parse_fat32_geometry, Fat32Geometry, RawVolume};

/// Result of a FAT32 exclusion walk.
pub struct FatExclusionScan {
    pub ranges: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    pub bytes_excluded: u64,
    pub categories: CategorySizes,
    /// Byte-level metadata edits that make each excluded file truly *disappear*
    /// (rather than remain a zero-filled ghost): the directory entry is marked
    /// deleted and its allocation-table entries are freed. Applied to the copied
    /// image via [`MultiPatchReader`](crate::block_reader::MultiPatchReader).
    /// Offsets are volume-relative; patches never overlap.
    pub patches: Vec<(u64, Vec<u8>)>,
    /// Matched entries whose content is still in the copy, and directories
    /// the walk could not enter (`exclusions::key_store_left_behind`).
    pub left: Vec<LeftInCopy>,
}

impl FatExclusionScan {
    fn empty() -> Self {
        FatExclusionScan {
            ranges: ByteRanges::new(),
            files_excluded: 0,
            entries_scanned: 0,
            bytes_excluded: 0,
            categories: CategorySizes::default(),
            patches: Vec::new(),
            left: Vec::new(),
        }
    }
}

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LFN: u8 = 0x0F;

/// Walk a FAT32 volume and collect the cluster byte-ranges of every file that
/// matches `patterns`. `tick(entries_scanned)` runs periodically so the caller
/// can report progress and cancel (return `Err`).
pub fn scan_fat32_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<FatExclusionScan> {
    let mut scan = FatExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    let mut boot = [0u8; 512];
    vol.read_at(0, &mut boot)?;
    let geom = parse_fat32_geometry(&boot, vol.len())?;
    let mut fat = vec![0u8; geom.fat_bytes as usize];
    vol.read_at(geom.fat_offset, &mut fat)?;

    // Cluster numbers freed by the matched files' chains — turned into FAT-entry
    // zeroing patches (in every FAT copy) once the whole tree is walked.
    let mut freed_clusters: Vec<u64> = Vec::new();

    // (directory first cluster, lowercased path components, subtree excluded)
    let mut stack: Vec<(u64, Vec<String>, bool)> = vec![(geom.root_cluster, Vec::new(), false)];
    let mut visited: HashSet<u64> = HashSet::new();
    let mut cluster_buf = vec![0u8; geom.cluster_size as usize];

    while let Some((dir_cluster, rel, subtree_excluded)) = stack.pop() {
        if !visited.insert(dir_cluster) {
            // Already walked (or a chain loop) — skip. Reached again inside
            // an excluded subtree, it was walked under another name that may
            // not have been: what it holds may still be in the copy.
            if subtree_excluded {
                scan.left
                    .push(LeftInCopy::new(&rel, LeftReason::Unreadable));
            }
            continue;
        }
        walk_directory(
            vol,
            &geom,
            &fat,
            &mut cluster_buf,
            dir_cluster,
            &rel,
            subtree_excluded,
            &parsed,
            &mut scan,
            &mut freed_clusters,
            &mut stack,
            &mut tick,
        )?;
    }

    // Free every excluded cluster's FAT entry in all FAT copies.
    append_fat32_free_patches(&geom, &mut freed_clusters, &mut scan.patches);
    // …and count them free in FSInfo, or the filesystem says it has less
    // space than it has (fsck.fat: «Free cluster summary wrong»).
    append_fsinfo_patches(
        vol,
        &boot,
        &geom,
        freed_clusters.len() as u64,
        &mut scan.patches,
    )?;

    scan.ranges.finalize();
    info!(
        files = scan.files_excluded,
        entries = scan.entries_scanned,
        excluded_mb = scan.bytes_excluded / (1024 * 1024),
        patches = scan.patches.len(),
        "FAT32 exclusion scan complete"
    );
    Ok(scan)
}

/// Coalesce the sorted-unique freed cluster numbers into runs and emit one
/// zero-fill patch per run per FAT copy (FAT entry for cluster N is 4 bytes at
/// `fat_offset + k*fat_bytes + N*4`). Consumes/sorts `freed` in place.
fn append_fat32_free_patches(
    geom: &Fat32Geometry,
    freed: &mut Vec<u64>,
    patches: &mut Vec<(u64, Vec<u8>)>,
) {
    if freed.is_empty() {
        return;
    }
    freed.sort_unstable();
    freed.dedup();
    let mut i = 0;
    while i < freed.len() {
        let start = freed[i];
        let mut j = i;
        while j + 1 < freed.len() && freed[j + 1] == freed[j] + 1 {
            j += 1;
        }
        let run_len = (j - i + 1) as u64;
        let zeros = vec![0u8; (run_len * 4) as usize];
        for k in 0..geom.num_fats {
            let off = geom.fat_offset + k * geom.fat_bytes + start * 4;
            patches.push((off, zeros.clone()));
        }
        i = j + 1;
    }
}

/// Add `freed` clusters to the free count of the FSInfo sector and of its
/// backup copy (FAT32 §6). Only a sector that carries all three FSInfo
/// signatures and a known count (not `0xFFFFFFFF`) is touched; the next-free
/// hint stays as it is — it is only a hint.
fn append_fsinfo_patches(
    vol: &mut dyn RawVolume,
    boot: &[u8; 512],
    geom: &Fat32Geometry,
    freed: u64,
    patches: &mut Vec<(u64, Vec<u8>)>,
) -> Result<()> {
    if freed == 0 {
        return Ok(());
    }
    let fsinfo = u16::from_le_bytes([boot[48], boot[49]]) as u64;
    let backup_boot = u16::from_le_bytes([boot[50], boot[51]]) as u64;
    if fsinfo == 0 || fsinfo == 0xFFFF {
        return Ok(());
    }
    let mut sectors = vec![fsinfo];
    if backup_boot != 0 && backup_boot != 0xFFFF {
        sectors.push(backup_boot + fsinfo);
    }
    let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let mut sector = [0u8; 512];
    for n in sectors {
        let at = n * geom.bytes_per_sector;
        if at + 512 > vol.len() {
            continue;
        }
        vol.read_at(at, &mut sector)?;
        let signed = le32(&sector, 0) == 0x4161_5252
            && le32(&sector, 484) == 0x6141_7272
            && le32(&sector, 508) == 0xAA55_0000;
        let count = le32(&sector, 488);
        if !signed || count == 0xFFFF_FFFF {
            continue;
        }
        let new = (count as u64 + freed).min(geom.cluster_count) as u32;
        patches.push((at + 488, new.to_le_bytes().to_vec()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_directory(
    vol: &mut dyn RawVolume,
    geom: &Fat32Geometry,
    fat: &[u8],
    buf: &mut [u8],
    dir_cluster: u64,
    rel: &[String],
    subtree_excluded: bool,
    parsed: &[crate::exclusions::ExclusionPattern],
    scan: &mut FatExclusionScan,
    freed_clusters: &mut Vec<u64>,
    stack: &mut Vec<(u64, Vec<String>, bool)>,
    tick: &mut impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    // Running long-name fragments and their on-disk byte offsets (kept across
    // cluster reads so an entry set spanning a cluster boundary still works).
    let mut lfn: Vec<(u8, Vec<u16>)> = Vec::new();
    let mut lfn_offsets: Vec<u64> = Vec::new();
    let mut cluster = dir_cluster;
    let mut guard = 0u64;

    'dir: while (2..geom.cluster_count + 2).contains(&cluster) {
        let base = cluster_offset(geom, cluster);
        vol.read_at(base, buf)?;
        for (i, rec) in buf.chunks_exact(32).enumerate() {
            let rec_offset = base + (i as u64) * 32;
            match rec[0] {
                0x00 => break 'dir, // end of directory
                0xE5 => {
                    lfn.clear();
                    lfn_offsets.clear();
                    continue;
                }
                _ => {}
            }
            let attr = rec[11];
            if attr == ATTR_LFN {
                // Long-name fragment: sequence in bits 0..4, chars are UTF-16.
                lfn.push((rec[0] & 0x1F, lfn_chars(rec)));
                lfn_offsets.push(rec_offset);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 || rec[0] == b'.' {
                lfn.clear(); // volume label or "." / ".." — not a real entry
                lfn_offsets.clear();
                continue;
            }

            let name = if lfn.is_empty() {
                short_name(rec)
            } else {
                assemble_lfn(&mut lfn)
            };
            lfn.clear();
            // The LFN entries belonging to THIS 8.3 record (in disk order).
            let entry_lfn_offsets = std::mem::take(&mut lfn_offsets);

            scan.entries_scanned += 1;
            if scan.entries_scanned % 512 == 0 {
                tick(scan.entries_scanned)?;
            }

            let name_lower = name.to_lowercase();
            let mut rel_child = rel.to_vec();
            rel_child.push(name_lower.clone());
            let matched =
                subtree_excluded || parsed.iter().any(|p| p.matches(&name_lower, &rel_child));

            let first_cluster = u16le(rec, 20) << 16 | u16le(rec, 26);
            if attr & ATTR_DIRECTORY != 0 {
                if (2..geom.cluster_count + 2).contains(&first_cluster) {
                    stack.push((first_cluster, rel_child, matched));
                }
            } else if matched {
                // True removal: mark the 8.3 record (and its long-name entries)
                // deleted, and free the file's cluster chain. The data clusters
                // are separately dropped from the used map + zeroed.
                scan.patches.push((rec_offset, vec![0xE5]));
                for off in entry_lfn_offsets {
                    scan.patches.push((off, vec![0xE5]));
                }
                let bytes =
                    file_chain_ranges(geom, fat, first_cluster, &mut scan.ranges, freed_clusters);
                scan.files_excluded += 1;
                scan.bytes_excluded += bytes;
            } else {
                scan.categories.add(&rel_child, u32le(rec, 28));
            }
        }

        cluster = fat_next(fat, cluster);
        guard += 1;
        if guard > geom.cluster_count {
            break; // pathological chain — stop
        }
    }
    Ok(())
}

/// Add every cluster of a file's FAT chain (from `first`) to `ranges` and its
/// cluster number to `freed`; returns the allocated byte count. Bounded by the
/// cluster count so a corrupt/looping chain can never spin forever.
fn file_chain_ranges(
    geom: &Fat32Geometry,
    fat: &[u8],
    first: u64,
    ranges: &mut ByteRanges,
    freed: &mut Vec<u64>,
) -> u64 {
    let mut c = first;
    let mut bytes = 0u64;
    let mut guard = 0u64;
    while (2..geom.cluster_count + 2).contains(&c) {
        let start = cluster_offset(geom, c);
        ranges.add(start, start + geom.cluster_size as u64);
        freed.push(c);
        bytes += geom.cluster_size as u64;
        c = fat_next(fat, c);
        guard += 1;
        if guard > geom.cluster_count {
            break;
        }
    }
    bytes
}

fn cluster_offset(geom: &Fat32Geometry, cluster: u64) -> u64 {
    geom.data_start + (cluster - 2) * geom.cluster_size as u64
}

/// Next cluster in a FAT32 chain (28-bit entry). Out-of-range / EOC values fall
/// outside `2..cluster_count+2`, which stops the caller's loop.
fn fat_next(fat: &[u8], cluster: u64) -> u64 {
    let idx = (cluster * 4) as usize;
    if idx + 4 <= fat.len() {
        u32::from_le_bytes(fat[idx..idx + 4].try_into().unwrap()) as u64 & 0x0FFF_FFFF
    } else {
        0x0FFF_FFFF
    }
}

fn u16le(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32le(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// The 13 UTF-16 code units carried by one LFN directory entry.
fn lfn_chars(rec: &[u8]) -> Vec<u16> {
    let mut v = Vec::with_capacity(13);
    for &(off, count) in &[(1usize, 5usize), (14, 6), (28, 2)] {
        for i in 0..count {
            let p = off + i * 2;
            v.push(u16::from_le_bytes([rec[p], rec[p + 1]]));
        }
    }
    v
}

/// Reassemble the collected LFN fragments (they appear in reverse order) into a
/// String, stopping at the NUL terminator and ignoring 0xFFFF padding.
fn assemble_lfn(parts: &mut [(u8, Vec<u16>)]) -> String {
    parts.sort_by_key(|(seq, _)| *seq);
    let mut units: Vec<u16> = Vec::new();
    for (_, chars) in parts.iter() {
        for &u in chars {
            if u == 0x0000 {
                return String::from_utf16_lossy(&units);
            }
            if u != 0xFFFF {
                units.push(u);
            }
        }
    }
    String::from_utf16_lossy(&units)
}

/// The 8.3 short name from a directory entry, as `NAME.EXT` (no padding).
fn short_name(rec: &[u8]) -> String {
    let base = String::from_utf8_lossy(&rec[0..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&rec[8..11]).trim_end().to_string();
    let mut base = base;
    if rec[0] == 0x05 {
        // 0x05 stands in for a leading 0xE5 (a valid Kanji lead byte).
        if let Some(rest) = base.strip_prefix('\u{5}') {
            base = format!("\u{e5}{rest}");
        }
    }
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

// ---------------------------------------------------------------------------
// exFAT
// ---------------------------------------------------------------------------

/// Detect FAT32 vs exFAT and run the matching exclusion walk.
pub fn scan_fat_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    tick: impl FnMut(u64) -> Result<()>,
) -> Result<FatExclusionScan> {
    let mut boot = [0u8; 512];
    vol.read_at(0, &mut boot)?;
    if &boot[3..11] == b"EXFAT   " {
        scan_exfat_exclusions(vol, patterns, tick)
    } else {
        scan_fat32_exclusions(vol, patterns, tick)
    }
}

// exFAT directory entry types (with the in-use bit 0x80 set).
const EXFAT_END: u8 = 0x00;
const EXFAT_FILE: u8 = 0x85;
const EXFAT_STREAM: u8 = 0xC0;
const EXFAT_NAME: u8 = 0xC1;
const EXFAT_ATTR_DIRECTORY: u64 = 0x10;
const EXFAT_FLAG_NO_FAT_CHAIN: u8 = 0x02;

/// One in-progress exFAT directory entry set (a File entry + its secondaries).
struct Pending {
    is_dir: bool,
    name: Vec<u16>,
    name_len: usize,
    first_cluster: u64,
    data_length: u64,
    no_fat_chain: bool,
    remaining: u8,
    /// (volume offset, first byte) of every entry in the set — for clearing the
    /// in-use bit (0x80) when the file is removed.
    entries: Vec<(u64, u8)>,
}

/// A file entry set dropped before it was complete: its name was never
/// matched and, for a directory, nothing under it was walked. Said in
/// `scan.left`, so a copy that must prove the key directory gone does not
/// take silence for it.
fn abandon_exfat_set(
    pending: Option<Pending>,
    rel: &[String],
    subtree_excluded: bool,
    scan: &mut FatExclusionScan,
) {
    let Some(p) = pending else { return };
    let mut path = rel.to_vec();
    path.push(String::from_utf16_lossy(&p.name).to_lowercase());
    if p.is_dir {
        scan.left
            .push(LeftInCopy::new(&path, LeftReason::SubtreeNotWalked));
    } else if subtree_excluded {
        scan.left
            .push(LeftInCopy::new(&path, LeftReason::Unreadable));
    }
}

/// Walk an exFAT volume, collecting the cluster byte-ranges of matched files.
pub fn scan_exfat_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<FatExclusionScan> {
    use crate::fat_map::parse_exfat_geometry;

    let mut scan = FatExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    let mut boot = [0u8; 512];
    vol.read_at(0, &mut boot)?;
    let g = parse_exfat_geometry(&boot, vol.len())?;
    let mut fat = vec![0u8; g.fat_bytes as usize];
    vol.read_at(g.fat_offset, &mut fat)?;

    // Clusters freed by removed files: every one → an allocation-bitmap bit to
    // clear; fragmented (FAT-chained) files additionally need their FAT entries
    // zeroed (contiguous NoFatChain files are not represented in the FAT).
    let mut freed_clusters: Vec<u64> = Vec::new();
    let mut fat_frees: Vec<u64> = Vec::new();

    // (first cluster, contiguous?, max clusters, rel path, subtree excluded).
    // The root is walked via the FAT chain (bounded by the cluster count).
    let mut stack: Vec<(u64, bool, u64, Vec<String>, bool)> =
        vec![(g.root_cluster, false, g.cluster_count, Vec::new(), false)];
    let mut visited: HashSet<u64> = HashSet::new();
    let mut buf = vec![0u8; g.cluster_size as usize];

    while let Some((first, contiguous, max_clusters, rel, subtree_excluded)) = stack.pop() {
        if !visited.insert(first) {
            continue;
        }
        let mut pending: Option<Pending> = None;
        let mut cluster = first;
        let mut walked = 0u64;

        'dir: while (2..g.cluster_count + 2).contains(&cluster) && walked < max_clusters {
            let base = exfat_cluster_offset(&g, cluster);
            vol.read_at(base, &mut buf)?;
            for (i, rec) in buf.chunks_exact(32).enumerate() {
                let rec_offset = base + (i as u64) * 32;
                let etype = rec[0];
                if etype == EXFAT_END {
                    break 'dir;
                }
                if etype & 0x80 == 0 {
                    // not-in-use entry: drop any partial set
                    abandon_exfat_set(pending.take(), &rel, subtree_excluded, &mut scan);
                    continue;
                }
                match etype {
                    EXFAT_FILE => {
                        let attrs = u16le(rec, 4);
                        abandon_exfat_set(pending.take(), &rel, subtree_excluded, &mut scan);
                        pending = Some(Pending {
                            is_dir: attrs & EXFAT_ATTR_DIRECTORY != 0,
                            name: Vec::new(),
                            name_len: 0,
                            first_cluster: 0,
                            data_length: 0,
                            no_fat_chain: false,
                            remaining: rec[1], // SecondaryCount
                            entries: vec![(rec_offset, EXFAT_FILE)],
                        });
                    }
                    EXFAT_STREAM => {
                        if let Some(p) = pending.as_mut() {
                            p.no_fat_chain = rec[1] & EXFAT_FLAG_NO_FAT_CHAIN != 0;
                            p.name_len = rec[3] as usize;
                            p.data_length = u64le(rec, 24);
                            p.first_cluster = u32le(rec, 20);
                            p.remaining = p.remaining.saturating_sub(1);
                            p.entries.push((rec_offset, EXFAT_STREAM));
                        }
                    }
                    EXFAT_NAME => {
                        if let Some(p) = pending.as_mut() {
                            for i in 0..15 {
                                if p.name.len() >= p.name_len {
                                    break;
                                }
                                p.name
                                    .push(u16::from_le_bytes([rec[2 + i * 2], rec[3 + i * 2]]));
                            }
                            p.remaining = p.remaining.saturating_sub(1);
                            p.entries.push((rec_offset, EXFAT_NAME));
                        }
                    }
                    _ => {
                        // Bitmap / upcase / volume label — or a secondary
                        // entry of a set this walk does not follow (vendor
                        // extension or allocation), which ends that set
                        // unevaluated.
                        abandon_exfat_set(pending.take(), &rel, subtree_excluded, &mut scan);
                        continue;
                    }
                }

                // Entry set complete?
                if pending.as_ref().is_some_and(|p| p.remaining == 0) {
                    let p = pending.take().unwrap();
                    finalize_exfat_entry(
                        &g,
                        &fat,
                        p,
                        &rel,
                        subtree_excluded,
                        &parsed,
                        &mut scan,
                        &mut freed_clusters,
                        &mut fat_frees,
                        &mut stack,
                    );
                    scan.entries_scanned += 1;
                    if scan.entries_scanned % 512 == 0 {
                        tick(scan.entries_scanned)?;
                    }
                }
            }

            // Next directory cluster.
            cluster = if contiguous {
                cluster + 1
            } else {
                fat_next(&fat, cluster)
            };
            walked += 1;
        }
        abandon_exfat_set(pending.take(), &rel, subtree_excluded, &mut scan);
    }

    // --- Metadata patches for true removal ---
    // Clearing the entry set alone would orphan the file's clusters in the
    // bitmap. So both must happen together, and only if we can safely locate the
    // bitmap; on any doubt keep the entries and fall back to zeroing (the data
    // clusters are dropped either way).
    if !freed_clusters.is_empty() {
        match crate::fat_map::read_exfat_bitmap(vol, &g, &fat) {
            Ok(bmp) => append_exfat_removal_patches(
                &g,
                bmp,
                &mut freed_clusters,
                &mut fat_frees,
                &mut scan.patches,
            ),
            Err(e) => {
                info!(error = %e, "exFAT: allocation bitmap unavailable — keeping directory entries (zeroing only)");
                scan.patches.clear();
            }
        }
    }

    scan.ranges.finalize();
    info!(
        files = scan.files_excluded,
        entries = scan.entries_scanned,
        excluded_mb = scan.bytes_excluded / (1024 * 1024),
        patches = scan.patches.len(),
        "exFAT exclusion scan complete"
    );
    Ok(scan)
}

/// Build the exFAT true-removal patches: clear the freed clusters' bits in the
/// allocation bitmap (read-modify, bit-level — neighbouring files' bits are
/// preserved) and zero the FAT entries of any fragmented removed files.
fn append_exfat_removal_patches(
    g: &crate::fat_map::ExfatGeometry,
    bmp: crate::fat_map::ExfatBitmap,
    freed: &mut [u64],
    fat_frees: &mut Vec<u64>,
    patches: &mut Vec<(u64, Vec<u8>)>,
) {
    let mut bytes = bmp.bytes;
    let mut modified: Vec<u64> = Vec::new();
    for &c in freed.iter() {
        if c < 2 {
            continue;
        }
        let bit = c - 2;
        let bidx = (bit / 8) as usize;
        if bidx < bytes.len() {
            bytes[bidx] &= !(1u8 << (bit % 8));
            modified.push(bidx as u64);
        }
    }
    modified.sort_unstable();
    modified.dedup();

    // Map a bitmap byte index to its volume offset via the bitmap's cluster
    // list (the bitmap may be fragmented, so bytes are not one contiguous run).
    let cs = g.cluster_size as u64;
    let mut voff_val: Vec<(u64, u8)> = Vec::with_capacity(modified.len());
    for &bidx in &modified {
        let k = (bidx / cs) as usize;
        if let Some(&cl) = bmp.clusters.get(k) {
            let voff = g.heap_start + (cl - 2) * cs + (bidx % cs);
            voff_val.push((voff, bytes[bidx as usize]));
        }
    }
    // Coalesce consecutive volume offsets into single patches.
    voff_val.sort_by_key(|(o, _)| *o);
    let mut i = 0;
    while i < voff_val.len() {
        let start = voff_val[i].0;
        let mut run = vec![voff_val[i].1];
        let mut j = i;
        while j + 1 < voff_val.len() && voff_val[j + 1].0 == voff_val[j].0 + 1 {
            run.push(voff_val[j + 1].1);
            j += 1;
        }
        patches.push((start, run));
        i = j + 1;
    }

    // Fragmented files: zero their FAT chain entries in every FAT copy.
    fat_frees.sort_unstable();
    fat_frees.dedup();
    let mut i = 0;
    while i < fat_frees.len() {
        let start = fat_frees[i];
        let mut j = i;
        while j + 1 < fat_frees.len() && fat_frees[j + 1] == fat_frees[j] + 1 {
            j += 1;
        }
        let zeros = vec![0u8; ((j - i + 1) * 4) as usize];
        for k in 0..g.num_fats {
            patches.push((g.fat_offset + k * g.fat_bytes + start * 4, zeros.clone()));
        }
        i = j + 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_exfat_entry(
    g: &crate::fat_map::ExfatGeometry,
    fat: &[u8],
    p: Pending,
    rel: &[String],
    subtree_excluded: bool,
    parsed: &[crate::exclusions::ExclusionPattern],
    scan: &mut FatExclusionScan,
    freed: &mut Vec<u64>,
    fat_frees: &mut Vec<u64>,
    stack: &mut Vec<(u64, bool, u64, Vec<String>, bool)>,
) {
    let name = String::from_utf16_lossy(&p.name);
    let name_lower = name.to_lowercase();
    let mut rel_child = rel.to_vec();
    rel_child.push(name_lower.clone());
    let matched = subtree_excluded || parsed.iter().any(|q| q.matches(&name_lower, &rel_child));

    let max_clusters = p.data_length.div_ceil(g.cluster_size as u64).max(1);
    if p.is_dir {
        if (2..g.cluster_count + 2).contains(&p.first_cluster) {
            stack.push((
                p.first_cluster,
                p.no_fat_chain,
                max_clusters,
                rel_child,
                matched,
            ));
        }
    } else if matched {
        // Clear the in-use bit (0x80) on every entry of the set: 0x85→0x05,
        // 0xC0→0x40, 0xC1→0x41. The clusters are freed via the bitmap (below).
        for (off, first_byte) in &p.entries {
            scan.patches.push((*off, vec![first_byte & 0x7F]));
        }
        let bytes = exfat_file_ranges(g, fat, &p, &mut scan.ranges, freed, fat_frees);
        scan.files_excluded += 1;
        scan.bytes_excluded += bytes;
    } else {
        scan.categories.add(&rel_child, p.data_length);
    }
}

/// Byte offset of an exFAT cluster.
fn exfat_cluster_offset(g: &crate::fat_map::ExfatGeometry, cluster: u64) -> u64 {
    g.heap_start + (cluster - 2) * g.cluster_size as u64
}

/// Add a matched exFAT file's clusters to `ranges` (for dropping + zeroing) and
/// to `freed` (allocation-bitmap bits to clear). Contiguous (NoFatChain) files
/// span `data_length` bytes from `first_cluster`; fragmented files follow the
/// FAT and additionally record their clusters in `fat_frees` (FAT entries to
/// zero). Returns the byte count.
fn exfat_file_ranges(
    g: &crate::fat_map::ExfatGeometry,
    fat: &[u8],
    p: &Pending,
    ranges: &mut ByteRanges,
    freed: &mut Vec<u64>,
    fat_frees: &mut Vec<u64>,
) -> u64 {
    if !(2..g.cluster_count + 2).contains(&p.first_cluster) || p.data_length == 0 {
        return 0;
    }
    let cs = g.cluster_size as u64;
    let mut bytes = 0u64;
    if p.no_fat_chain {
        let need = p.data_length.div_ceil(cs);
        for i in 0..need {
            let c = p.first_cluster + i;
            if !(2..g.cluster_count + 2).contains(&c) {
                break;
            }
            let start = exfat_cluster_offset(g, c);
            ranges.add(start, start + cs);
            freed.push(c);
            bytes += cs;
        }
    } else {
        let mut c = p.first_cluster;
        let mut guard = 0u64;
        while (2..g.cluster_count + 2).contains(&c) {
            let start = exfat_cluster_offset(g, c);
            ranges.add(start, start + cs);
            freed.push(c);
            fat_frees.push(c);
            bytes += cs;
            c = fat_next(fat, c);
            guard += 1;
            if guard > g.cluster_count {
                break;
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::OwnBackupError;

    /// An exFAT entry set dropped unevaluated (a vendor secondary entry the
    /// walk does not follow) is said: a directory as not walked, a file
    /// inside an excluded subtree as content left behind.
    #[test]
    fn an_abandoned_exfat_set_is_reported() {
        let pending = |is_dir: bool| Pending {
            is_dir,
            name: "Keys".encode_utf16().collect(),
            name_len: 4,
            first_cluster: 0,
            data_length: 0,
            no_fat_chain: false,
            remaining: 1,
            entries: Vec::new(),
        };
        let mut scan = FatExclusionScan::empty();
        let rel = vec!["a".to_string()];
        abandon_exfat_set(None, &rel, true, &mut scan);
        abandon_exfat_set(Some(pending(false)), &rel, false, &mut scan);
        assert!(scan.left.is_empty(), "a file outside the rules is not ours");
        abandon_exfat_set(Some(pending(true)), &rel, false, &mut scan);
        abandon_exfat_set(Some(pending(false)), &rel, true, &mut scan);
        assert_eq!(
            scan.left,
            vec![
                LeftInCopy::new(&["a".into(), "keys".into()], LeftReason::SubtreeNotWalked),
                LeftInCopy::new(&["a".into(), "keys".into()], LeftReason::Unreadable),
            ]
        );
    }

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(OwnBackupError::Device("read past end".into()));
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Build a tiny FAT32: 512-B sectors, 1 sector/cluster, 4 reserved, one
    /// 4-sector FAT, 64 data clusters (root=2). We place:
    ///   root dir (cl 2) → file "keep.bin" (cl 5), file "junk.tmp" (cl 6),
    ///                     subdir "Temp" (cl 3)
    ///   Temp dir (cl 3) → file "inside.log" (cl 7)
    fn fat32_tree() -> MemVolume {
        let bps = 512usize;
        let reserved = 4usize;
        let fatsz = 4usize;
        let clusters = 64usize;
        let tot_sec = reserved + fatsz + clusters;
        let mut img = vec![0u8; tot_sec * bps];

        img[11..13].copy_from_slice(&(bps as u16).to_le_bytes());
        img[13] = 1;
        img[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        img[16] = 1;
        img[32..36].copy_from_slice(&(tot_sec as u32).to_le_bytes());
        img[36..40].copy_from_slice(&(fatsz as u32).to_le_bytes());
        img[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
        img[510] = 0x55;
        img[511] = 0xAA;

        let fat_off = reserved * bps;
        let mut fat = |n: usize, v: u32| {
            img[fat_off + n * 4..fat_off + n * 4 + 4].copy_from_slice(&v.to_le_bytes());
        };
        fat(0, 0x0FFF_FFF8);
        fat(1, 0x0FFF_FFFF);
        for c in [2usize, 3, 5, 6, 7] {
            fat(c, 0x0FFF_FFFF); // each is a single-cluster chain (EOC)
        }

        let data = (reserved + fatsz) * bps;
        let cl = |n: usize| data + (n - 2) * bps;

        // 8.3 directory entry helper.
        let ent = |img: &mut [u8], at: usize, name: &[u8; 11], attr: u8, first: u32, size: u32| {
            img[at..at + 11].copy_from_slice(name);
            img[at + 11] = attr;
            img[at + 20..at + 22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
            img[at + 26..at + 28].copy_from_slice(&(first as u16).to_le_bytes());
            img[at + 28..at + 32].copy_from_slice(&size.to_le_bytes());
        };

        let root = cl(2);
        ent(&mut img, root, b"KEEP    BIN", 0x20, 5, 512);
        ent(&mut img, root + 32, b"JUNK    TMP", 0x20, 6, 512);
        ent(&mut img, root + 64, b"TEMP       ", 0x10, 3, 0);

        let temp = cl(3);
        ent(&mut img, temp, b"INSIDE  LOG", 0x20, 7, 512);

        MemVolume(img)
    }

    fn ranges_of(scan: &FatExclusionScan) -> Vec<(u64, u64)> {
        scan.ranges.iter().collect()
    }

    fn apply_patches(img: &mut [u8], patches: &[(u64, Vec<u8>)]) {
        for (off, bytes) in patches {
            let o = *off as usize;
            img[o..o + bytes.len()].copy_from_slice(bytes);
        }
    }

    #[test]
    fn excludes_by_extension() {
        let mut vol = fat32_tree();
        let scan = scan_fat32_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);
        // junk.tmp is cluster 6; data starts at sector 8 (512B) → cluster 6 @ 8*512 + 4*512.
        let data = 8 * 512u64;
        let junk = data + (6 - 2) * 512;
        assert_eq!(ranges_of(&scan), vec![(junk, junk + 512)]);
        // keep.bin stays and is categorized.
        assert!(scan.categories.total() > 0);
    }

    #[test]
    fn fat32_removal_deletes_entry_and_frees_chain() {
        let mut vol = fat32_tree();
        let scan = scan_fat32_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);
        assert!(
            !scan.patches.is_empty(),
            "removal must produce metadata patches"
        );

        // Apply the patches to a copy of the image (the source is never touched).
        let mut img = vol.0.clone();
        apply_patches(&mut img, &scan.patches);

        // junk.tmp is the 2nd root entry (root cluster 2) → marked deleted.
        let data = 8 * 512usize; // (reserved 4 + fatsz 4) * 512, single FAT
        let junk_entry = data + 32;
        assert_eq!(img[junk_entry], 0xE5, "directory entry should be deleted");

        // Its FAT entry (cluster 6) is freed; keep.bin (cluster 5) is untouched.
        let fat_off = 4 * 512usize;
        let fat_ent = |img: &[u8], n: usize| {
            u32::from_le_bytes(
                img[fat_off + n * 4..fat_off + n * 4 + 4]
                    .try_into()
                    .unwrap(),
            ) & 0x0FFF_FFFF
        };
        assert_eq!(fat_ent(&img, 6), 0, "freed cluster's FAT entry should be 0");
        assert_eq!(fat_ent(&img, 5), 0x0FFF_FFFF, "keep.bin's chain intact");

        // Re-scanning the patched image no longer finds the file.
        let mut vol2 = MemVolume(img.clone());
        let rescan = scan_fat32_exclusions(&mut vol2, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(rescan.files_excluded, 0, "file must be gone after removal");

        // The used-cluster map now shows the freed cluster's block as unused.
        use crate::used_sectors::UsedSectorMap;
        let map = crate::fat_map::build_fat_used_map(&mut MemVolume(img), 512).unwrap();
        let c6_block = (data as u64 + (6 - 2) * 512) / 512;
        assert!(
            !map.is_block_used(c6_block),
            "freed cluster's block must be unused"
        );
    }

    #[test]
    fn free_patches_cover_all_fat_copies_and_coalesce_runs() {
        let geom = Fat32Geometry {
            bytes_per_sector: 512,
            cluster_size: 512,
            cluster_count: 100,
            data_start: 100 * 512,
            fat_offset: 4 * 512,
            fat_bytes: 8 * 512,
            num_fats: 2,
            root_cluster: 2,
        };
        // A contiguous run 10..=12 plus a singleton 20 (order/dup intentional).
        let mut freed = vec![12u64, 10, 11, 20, 10];
        let mut patches = Vec::new();
        append_fat32_free_patches(&geom, &mut freed, &mut patches);
        // 2 runs × 2 FAT copies.
        assert_eq!(patches.len(), 4);
        let run0 = geom.fat_offset + 10 * 4;
        let run1 = geom.fat_offset + geom.fat_bytes + 10 * 4;
        assert!(patches
            .iter()
            .any(|(o, b)| *o == run0 && b.len() == 12 && b.iter().all(|&x| x == 0)));
        assert!(patches.iter().any(|(o, b)| *o == run1 && b.len() == 12));
        let one0 = geom.fat_offset + 20 * 4;
        let one1 = geom.fat_offset + geom.fat_bytes + 20 * 4;
        assert!(patches.iter().any(|(o, b)| *o == one0 && b.len() == 4));
        assert!(patches.iter().any(|(o, b)| *o == one1 && b.len() == 4));
    }

    #[test]
    fn excludes_a_whole_subdirectory() {
        let mut vol = fat32_tree();
        // Directory-name pattern excludes the subtree (inside.log under Temp).
        let scan = scan_fat32_exclusions(&mut vol, &["Temp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);
        let data = 8 * 512u64;
        let inside = data + (7 - 2) * 512;
        assert_eq!(ranges_of(&scan), vec![(inside, inside + 512)]);
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        let mut vol = fat32_tree();
        let scan = scan_fat32_exclusions(&mut vol, &[], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
    }

    /// End-to-end on a REAL FAT32 image built with dosfstools + mtools. This is
    /// the gold-standard check the plan calls for: `fsck.fat` validates BOTH FAT
    /// copies agree (catches zeroing only one) and that no clusters were left
    /// lost (catches deleting the entry without freeing the chain); `mdir`
    /// confirms the excluded file is gone and the kept file remains. Ignored by
    /// default (needs the tools); run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs dosfstools + mtools; run with --ignored"]
    fn fat32_removal_end_to_end_with_real_tools() {
        use std::process::Command;

        fn have(tool: &str) -> bool {
            Command::new(tool).arg("--help").output().is_ok()
        }
        for t in ["mkfs.fat", "fsck.fat", "mcopy", "mdir", "mmd"] {
            if !have(t) {
                eprintln!("SKIP fat32 e2e: {t} not installed");
                return;
            }
        }

        let run = |cmd: &mut Command| -> std::process::Output {
            let out = cmd.output().expect("spawn tool");
            assert!(
                out.status.success(),
                "command failed {:?}:\nstdout: {}\nstderr: {}",
                cmd,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };

        let dir = std::env::temp_dir().join(format!("obk-fat32-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("f.img");
        let imgs = img.to_str().unwrap();

        // 64 MiB FAT32 (mkfs.fat -F32 makes 2 FAT copies by default → the patch
        // must zero both, which fsck.fat cross-checks).
        run(Command::new("mkfs.fat").args(["-F", "32", "-C", imgs, "65536"]));

        // Populate: keep.bin at root, junk.tmp inside a temp subdirectory.
        let keep = dir.join("keep.bin");
        let junk = dir.join("junk.tmp");
        std::fs::write(&keep, vec![0xABu8; 200 * 1024]).unwrap();
        std::fs::write(&junk, vec![0xCDu8; 300 * 1024]).unwrap();
        run(Command::new("mmd").args(["-i", imgs, "::/temp"]));
        run(Command::new("mcopy").args(["-i", imgs, keep.to_str().unwrap(), "::/keep.bin"]));
        run(Command::new("mcopy").args(["-i", imgs, junk.to_str().unwrap(), "::/temp/junk.tmp"]));

        // --- Our removal: scan, then apply zeroing + metadata patches to a copy.
        let mut bytes = std::fs::read(&img).unwrap();
        let scan = {
            let mut vol = MemVolume(bytes.clone());
            scan_fat_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(())).unwrap()
        };
        assert_eq!(scan.files_excluded, 1, "junk.tmp should have been excluded");
        // Excluded data clusters are dropped from the image (sparse VHDX zero) —
        // mirror that by zeroing them here.
        for (s, e) in scan.ranges.iter() {
            bytes[s as usize..e as usize].fill(0);
        }
        apply_patches(&mut bytes, &scan.patches);
        let out = dir.join("f.out.img");
        let outs = out.to_str().unwrap();
        std::fs::write(&out, &bytes).unwrap();

        // --- Validate: fsck.fat clean (both FATs agree, no lost clusters).
        let fsck = Command::new("fsck.fat")
            .args(["-n", outs])
            .output()
            .unwrap();
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&fsck.stdout),
            String::from_utf8_lossy(&fsck.stderr)
        )
        .to_lowercase();
        for bad in [
            "lost cluster",
            "fats differ",
            "orphan",
            "truncat",
            "bad cluster",
        ] {
            assert!(
                !report.contains(bad),
                "fsck.fat reported '{bad}':\n{report}"
            );
        }

        // --- mdir: junk.tmp gone from ::/temp, keep.bin still at root.
        let temp_ls = String::from_utf8_lossy(
            &run(Command::new("mdir").args(["-i", outs, "::/temp"])).stdout,
        )
        .to_uppercase();
        assert!(
            !temp_ls.contains("JUNK"),
            "junk.tmp still listed:\n{temp_ls}"
        );
        let root_ls =
            String::from_utf8_lossy(&run(Command::new("mdir").args(["-i", outs, "::/"])).stdout)
                .to_uppercase();
        assert!(
            root_ls.contains("KEEP"),
            "keep.bin should still be present:\n{root_ls}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lfn_name_is_reassembled_and_matched() {
        let mut vol = fat32_tree();
        // Add an LFN pair (one fragment + its 8.3 stub) for "longcachefile.dat"
        // in an unused root slot, cluster 8. Exclude via "*cache*".
        let data = 8 * 512usize;
        let root = data;
        let slot = root + 96; // 4th entry
        let name: Vec<u16> = "longcachefile.dat".encode_utf16().collect();
        // Single LFN entry (<=13 chars would need one; this name is 17 chars, so
        // two entries) — build two fragments in reverse (seq 2 then seq 1).
        let put_lfn = |img: &mut [u8], at: usize, seq: u8, chars: &[u16]| {
            img[at] = seq;
            img[at + 11] = 0x0F;
            let slots = [(1usize, 5usize), (14, 6), (28, 2)];
            let mut ci = 0;
            for (off, count) in slots {
                for i in 0..count {
                    let p = at + off + i * 2;
                    let u = chars.get(ci).copied().unwrap_or(if ci == chars.len() {
                        0
                    } else {
                        0xFFFF
                    });
                    img[p..p + 2].copy_from_slice(&u.to_le_bytes());
                    ci += 1;
                }
            }
        };
        // seq 2 (last) holds chars 13.., seq 1 holds chars 0..13. On disk the LAST
        // fragment comes first.
        put_lfn(&mut vol.0, slot, 0x42, &name[13..]);
        put_lfn(&mut vol.0, slot + 32, 0x01, &name[..13]);
        // 8.3 stub after the LFN entries, pointing at cluster 8.
        let at = slot + 64;
        vol.0[at..at + 11].copy_from_slice(b"LONGCA~1DAT");
        vol.0[at + 11] = 0x20;
        vol.0[at + 26..at + 28].copy_from_slice(&8u16.to_le_bytes());
        vol.0[at + 28..at + 32].copy_from_slice(&512u32.to_le_bytes());
        let fat_off = 4 * 512;
        vol.0[fat_off + 8 * 4..fat_off + 8 * 4 + 4].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());

        let scan = scan_fat32_exclusions(&mut vol, &["*cache*".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1, "LFN name should match *cache*");
        let c8 = 8 * 512u64 + (8 - 2) * 512;
        assert!(ranges_of(&scan).contains(&(c8, c8 + 512)));
    }

    /// exFAT: 512-B sectors, 1 sector/cluster, FAT at sector 4, heap at sector 8,
    /// 16 clusters, root dir in cluster 2 with a "junk.tmp" entry set (File +
    /// Stream + Name), a contiguous file at cluster 5.
    fn exfat_tree() -> MemVolume {
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
        img[96..100].copy_from_slice(&2u32.to_le_bytes()); // root dir cluster
        img[108] = 9; // 512-B sectors
        img[109] = 0; // 1 sector/cluster
        img[110] = 1; // number of FATs
        img[510] = 0x55;
        img[511] = 0xAA;

        // FAT: root (cluster 2) is a single-cluster chain.
        let fat_off = (4 * bps) as usize;
        img[fat_off + 2 * 4..fat_off + 2 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        let cl = |n: u64| ((heap_off + (n - 2)) * bps) as usize;
        let root = cl(2);
        // File entry (0x85): SecondaryCount=2, attrs=archive.
        img[root] = 0x85;
        img[root + 1] = 2;
        img[root + 4..root + 6].copy_from_slice(&0x20u16.to_le_bytes());
        // Stream extension (0xC0): NoFatChain, name 8 chars, first cluster 5, 100 bytes.
        let s = root + 32;
        img[s] = 0xC0;
        img[s + 1] = 0x02; // NoFatChain
        img[s + 3] = 8; // NameLength
        img[s + 20..s + 24].copy_from_slice(&5u32.to_le_bytes());
        img[s + 24..s + 32].copy_from_slice(&100u64.to_le_bytes());
        // File name (0xC1): "junk.tmp".
        let nm = root + 64;
        img[nm] = 0xC1;
        for (i, u) in "junk.tmp".encode_utf16().enumerate() {
            img[nm + 2 + i * 2..nm + 2 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        // End of directory.
        img[root + 96] = 0x00;
        MemVolume(img)
    }

    #[test]
    fn exfat_excludes_matched_file() {
        let mut vol = exfat_tree();
        let scan = scan_fat_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);
        // cluster 5 @ heap(8*512) + (5-2)*512.
        let c5 = 8 * 512u64 + (5 - 2) * 512;
        assert_eq!(ranges_of(&scan), vec![(c5, c5 + 512)]);
    }

    #[test]
    fn exfat_keeps_unmatched_file() {
        let mut vol = exfat_tree();
        let scan = scan_fat_exclusions(&mut vol, &["*.log".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
        assert!(scan.categories.total() > 0); // junk.tmp categorized instead
    }

    /// exFAT with a real Allocation Bitmap (needed for removal patches): 512-B
    /// sectors, 32 clusters, root in cluster 2 holding the 0x81 bitmap entry, a
    /// "junk.tmp" set (contiguous, cluster 5) and a "keep.log" set (contiguous,
    /// cluster 6). Bitmap file lives in cluster 3, marking clusters 2,3,5,6 used.
    fn exfat_tree_with_bitmap() -> MemVolume {
        let bps = 512u64;
        let heap_off = 8u64;
        let clusters = 32u64;
        let vol_len = heap_off + clusters; // sectors
        let mut img = vec![0u8; (vol_len * bps) as usize];

        img[3..11].copy_from_slice(b"EXFAT   ");
        img[72..80].copy_from_slice(&vol_len.to_le_bytes());
        img[80..84].copy_from_slice(&4u32.to_le_bytes()); // fat offset (sector)
        img[84..88].copy_from_slice(&1u32.to_le_bytes()); // fat length (sector)
        img[88..92].copy_from_slice(&(heap_off as u32).to_le_bytes());
        img[92..96].copy_from_slice(&(clusters as u32).to_le_bytes());
        img[96..100].copy_from_slice(&2u32.to_le_bytes()); // root dir cluster
        img[108] = 9; // 512-B sectors
        img[109] = 0; // 1 sector/cluster
        img[110] = 1; // number of FATs
        img[510] = 0x55;
        img[511] = 0xAA;

        // FAT: root (cluster 2) and the bitmap file (cluster 3) are EOC chains.
        let fat_off = (4 * bps) as usize;
        img[fat_off + 2 * 4..fat_off + 2 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        img[fat_off + 3 * 4..fat_off + 3 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        let cl = |n: u64| ((heap_off + (n - 2)) * bps) as usize;

        // Allocation bitmap in cluster 3: bits for clusters 2,3,5,6 (indices
        // 0,1,3,4) → 0b0001_1011 = 0x1B.
        img[cl(3)] = 0b0001_1011;

        // A file entry set writer: File (0x85) + Stream (0xC0) + Name (0xC1).
        let put_set = |img: &mut [u8], at: usize, name: &str, first: u32, len: u64| {
            img[at] = 0x85;
            img[at + 1] = 2; // SecondaryCount
            img[at + 4..at + 6].copy_from_slice(&0x20u16.to_le_bytes()); // archive
            let s = at + 32;
            img[s] = 0xC0;
            img[s + 1] = 0x02; // NoFatChain (contiguous)
            img[s + 3] = name.encode_utf16().count() as u8;
            img[s + 20..s + 24].copy_from_slice(&first.to_le_bytes());
            img[s + 24..s + 32].copy_from_slice(&len.to_le_bytes());
            let nm = at + 64;
            img[nm] = 0xC1;
            for (i, u) in name.encode_utf16().enumerate() {
                img[nm + 2 + i * 2..nm + 2 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
        };

        let root = cl(2);
        // Entry 0: allocation bitmap (0x81) → first cluster 3, 4 bytes.
        img[root] = 0x81;
        img[root + 20..root + 24].copy_from_slice(&3u32.to_le_bytes());
        img[root + 24..root + 32].copy_from_slice(&4u64.to_le_bytes());
        // Entries 1..4: junk.tmp (cluster 5). Entries 4..7: keep.log (cluster 6).
        put_set(&mut img, root + 32, "junk.tmp", 5, 100);
        put_set(&mut img, root + 128, "keep.log", 6, 100);
        img[root + 224] = 0x00; // end of directory
        MemVolume(img)
    }

    #[test]
    fn exfat_removal_clears_entry_and_bitmap() {
        use crate::used_sectors::UsedSectorMap;

        let mut vol = exfat_tree_with_bitmap();
        let scan = scan_fat_exclusions(&mut vol, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);
        assert!(!scan.patches.is_empty(), "removal must produce patches");

        let mut img = vol.0.clone();
        apply_patches(&mut img, &scan.patches);

        let heap = 8 * 512u64;
        let cl = |n: u64| (heap + (n - 2) * 512) as usize;
        let root = cl(2);
        // junk.tmp's set (entries 1..4 in root) has the in-use bit cleared.
        assert_eq!(img[root + 32], 0x05, "File entry (0x85→0x05)");
        assert_eq!(img[root + 64], 0x40, "Stream entry (0xC0→0x40)");
        assert_eq!(img[root + 96], 0x41, "Name entry (0xC1→0x41)");
        // keep.log's set (entries 4..7) is untouched.
        assert_eq!(img[root + 128], 0x85, "keep.log File entry intact");

        // Bitmap: cluster 5's bit (index 3) cleared; cluster 6's (index 4) kept.
        // 0x1B & ~(1<<3) = 0x13.
        assert_eq!(img[cl(3)], 0b0001_0011, "bitmap bit for cluster 5 cleared");

        // Re-scan: junk.tmp gone; keep.log still findable.
        let rescan_tmp =
            scan_fat_exclusions(&mut MemVolume(img.clone()), &["*.tmp".into()], |_| Ok(()))
                .unwrap();
        assert_eq!(rescan_tmp.files_excluded, 0, "junk.tmp must be gone");
        let rescan_log =
            scan_fat_exclusions(&mut MemVolume(img.clone()), &["*.log".into()], |_| Ok(()))
                .unwrap();
        assert_eq!(
            rescan_log.files_excluded, 1,
            "keep.log must still be present"
        );

        // Used map: cluster 5 freed, cluster 6 still used.
        let map = crate::fat_map::build_fat_used_map(&mut MemVolume(img), 512).unwrap();
        assert!(!map.is_block_used((cl(5) / 512) as u64), "cluster 5 freed");
        assert!(
            map.is_block_used((cl(6) / 512) as u64),
            "cluster 6 still used"
        );
    }

    #[test]
    fn exfat_removal_frees_fragmented_fat_chain() {
        // A fragmented (FAT-chained) file must also get its FAT entries zeroed.
        let mut base = exfat_tree_with_bitmap();
        let bps = 512usize;
        let heap_off = 8usize;
        let cl = |n: usize| (heap_off + (n - 2)) * bps;
        // Turn junk.tmp into a 2-cluster FAT-chained file: clusters 5 → 7.
        let root = cl(2);
        let s = root + 32 + 32; // its Stream entry
        base.0[s + 1] = 0x00; // clear NoFatChain → follow the FAT
        base.0[s + 24..s + 32].copy_from_slice(&600u64.to_le_bytes()); // 2 clusters
                                                                       // FAT chain 5 → 7 → EOC.
        let fat_off = 4 * bps;
        base.0[fat_off + 5 * 4..fat_off + 5 * 4 + 4].copy_from_slice(&7u32.to_le_bytes());
        base.0[fat_off + 7 * 4..fat_off + 7 * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // Mark cluster 7 (index 5) used in the bitmap too: 0x1B | (1<<5) = 0x3B.
        base.0[cl(3)] = 0b0011_1011;

        let scan = scan_fat_exclusions(&mut base, &["*.tmp".into()], |_| Ok(())).unwrap();
        assert_eq!(scan.files_excluded, 1);

        let mut img = base.0.clone();
        apply_patches(&mut img, &scan.patches);
        // FAT entries for clusters 5 and 7 must be zeroed.
        let e = |img: &[u8], n: usize| {
            u32::from_le_bytes(
                img[fat_off + n * 4..fat_off + n * 4 + 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(e(&img, 5), 0, "FAT entry for cluster 5 zeroed");
        assert_eq!(e(&img, 7), 0, "FAT entry for cluster 7 zeroed");
        // Bitmap bits for clusters 5 (idx 3) and 7 (idx 5) cleared: 0x3B & ~0x28 = 0x13.
        assert_eq!(
            img[cl(3)],
            0b0001_0011,
            "both fragmented clusters freed in bitmap"
        );
    }
}
