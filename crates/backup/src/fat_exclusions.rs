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

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes};
use crate::fat_map::{parse_fat32_geometry, Fat32Geometry, RawVolume};

/// Result of a FAT32 exclusion walk.
pub struct FatExclusionScan {
    pub ranges: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    pub bytes_excluded: u64,
    pub categories: CategorySizes,
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
    let mut scan = FatExclusionScan {
        ranges: ByteRanges::new(),
        files_excluded: 0,
        entries_scanned: 0,
        bytes_excluded: 0,
        categories: CategorySizes::default(),
    };
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

    // (directory first cluster, lowercased path components, subtree excluded)
    let mut stack: Vec<(u64, Vec<String>, bool)> = vec![(geom.root_cluster, Vec::new(), false)];
    let mut visited: HashSet<u64> = HashSet::new();
    let mut cluster_buf = vec![0u8; geom.cluster_size as usize];

    while let Some((dir_cluster, rel, subtree_excluded)) = stack.pop() {
        if !visited.insert(dir_cluster) {
            continue; // already walked (or a chain loop) — skip
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
            &mut stack,
            &mut tick,
        )?;
    }

    scan.ranges.finalize();
    info!(
        files = scan.files_excluded,
        entries = scan.entries_scanned,
        excluded_mb = scan.bytes_excluded / (1024 * 1024),
        "FAT32 exclusion scan complete"
    );
    Ok(scan)
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
    stack: &mut Vec<(u64, Vec<String>, bool)>,
    tick: &mut impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut lfn: Vec<(u8, Vec<u16>)> = Vec::new();
    let mut cluster = dir_cluster;
    let mut guard = 0u64;

    'dir: while (2..geom.cluster_count + 2).contains(&cluster) {
        vol.read_at(cluster_offset(geom, cluster), buf)?;
        for rec in buf.chunks_exact(32) {
            match rec[0] {
                0x00 => break 'dir,          // end of directory
                0xE5 => {
                    lfn.clear();
                    continue;
                }
                _ => {}
            }
            let attr = rec[11];
            if attr == ATTR_LFN {
                // Long-name fragment: sequence in bits 0..4, chars are UTF-16.
                lfn.push((rec[0] & 0x1F, lfn_chars(rec)));
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 || rec[0] == b'.' {
                lfn.clear(); // volume label or "." / ".." — not a real entry
                continue;
            }

            let name = if lfn.is_empty() {
                short_name(rec)
            } else {
                assemble_lfn(&mut lfn)
            };
            lfn.clear();

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
            } else {
                let size = u32le(rec, 28);
                if matched {
                    let bytes = file_chain_ranges(geom, fat, first_cluster, &mut scan.ranges);
                    scan.files_excluded += 1;
                    scan.bytes_excluded += bytes;
                } else {
                    scan.categories.add(&name_lower, size);
                }
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

/// Add every cluster of a file's FAT chain (from `first`) to `ranges`; returns
/// the allocated byte count. Bounded by the cluster count so a corrupt/looping
/// chain can never spin forever.
fn file_chain_ranges(
    geom: &Fat32Geometry,
    fat: &[u8],
    first: u64,
    ranges: &mut ByteRanges,
) -> u64 {
    let mut c = first;
    let mut bytes = 0u64;
    let mut guard = 0u64;
    while (2..geom.cluster_count + 2).contains(&c) {
        let start = cluster_offset(geom, c);
        ranges.add(start, start + geom.cluster_size as u64);
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
}

/// Walk an exFAT volume, collecting the cluster byte-ranges of matched files.
pub fn scan_exfat_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<FatExclusionScan> {
    use crate::fat_map::parse_exfat_geometry;

    let mut scan = FatExclusionScan {
        ranges: ByteRanges::new(),
        files_excluded: 0,
        entries_scanned: 0,
        bytes_excluded: 0,
        categories: CategorySizes::default(),
    };
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
            vol.read_at(exfat_cluster_offset(&g, cluster), &mut buf)?;
            for rec in buf.chunks_exact(32) {
                let etype = rec[0];
                if etype == EXFAT_END {
                    break 'dir;
                }
                if etype & 0x80 == 0 {
                    pending = None; // not-in-use entry: drop any partial set
                    continue;
                }
                match etype {
                    EXFAT_FILE => {
                        let attrs = u16le(rec, 4);
                        pending = Some(Pending {
                            is_dir: attrs & EXFAT_ATTR_DIRECTORY != 0,
                            name: Vec::new(),
                            name_len: 0,
                            first_cluster: 0,
                            data_length: 0,
                            no_fat_chain: false,
                            remaining: rec[1], // SecondaryCount
                        });
                    }
                    EXFAT_STREAM => {
                        if let Some(p) = pending.as_mut() {
                            p.no_fat_chain = rec[1] & EXFAT_FLAG_NO_FAT_CHAIN != 0;
                            p.name_len = rec[3] as usize;
                            p.data_length = u64le(rec, 24);
                            p.first_cluster = u32le(rec, 20);
                            p.remaining = p.remaining.saturating_sub(1);
                        }
                    }
                    EXFAT_NAME => {
                        if let Some(p) = pending.as_mut() {
                            for i in 0..15 {
                                if p.name.len() >= p.name_len {
                                    break;
                                }
                                p.name.push(u16::from_le_bytes([rec[2 + i * 2], rec[3 + i * 2]]));
                            }
                            p.remaining = p.remaining.saturating_sub(1);
                        }
                    }
                    _ => {
                        pending = None; // bitmap / upcase / volume label
                        continue;
                    }
                }

                // Entry set complete?
                if pending.as_ref().is_some_and(|p| p.remaining == 0) {
                    let p = pending.take().unwrap();
                    finalize_exfat_entry(&g, &fat, p, &rel, subtree_excluded, &parsed, &mut scan, &mut stack);
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
    }

    scan.ranges.finalize();
    info!(
        files = scan.files_excluded,
        entries = scan.entries_scanned,
        excluded_mb = scan.bytes_excluded / (1024 * 1024),
        "exFAT exclusion scan complete"
    );
    Ok(scan)
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
            stack.push((p.first_cluster, p.no_fat_chain, max_clusters, rel_child, matched));
        }
    } else if matched {
        let bytes = exfat_file_ranges(g, fat, &p, &mut scan.ranges);
        scan.files_excluded += 1;
        scan.bytes_excluded += bytes;
    } else {
        scan.categories.add(&name_lower, p.data_length);
    }
}

/// Byte offset of an exFAT cluster.
fn exfat_cluster_offset(g: &crate::fat_map::ExfatGeometry, cluster: u64) -> u64 {
    g.heap_start + (cluster - 2) * g.cluster_size as u64
}

/// Add a matched exFAT file's clusters to `ranges`. Contiguous (NoFatChain)
/// files span `data_length` bytes from `first_cluster`; otherwise follow the FAT.
fn exfat_file_ranges(
    g: &crate::fat_map::ExfatGeometry,
    fat: &[u8],
    p: &Pending,
    ranges: &mut ByteRanges,
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
            bytes += cs;
        }
    } else {
        let mut c = p.first_cluster;
        let mut guard = 0u64;
        while (2..g.cluster_count + 2).contains(&c) {
            let start = exfat_cluster_offset(g, c);
            ranges.add(start, start + cs);
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
                    let u = chars.get(ci).copied().unwrap_or(if ci == chars.len() { 0 } else { 0xFFFF });
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
}
