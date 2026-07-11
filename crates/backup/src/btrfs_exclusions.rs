//! btrfs file exclusions — the CoW-aware counterpart of
//! [`crate::ext_exclusions`] (ext4) / [`crate::fat_exclusions`] (FAT).
//!
//! btrfs is copy-on-write: a data extent can be **shared** by several files
//! (reflink copies, dedup) and by snapshots (`@`, `@home`, timeshift). Zeroing a
//! shared extent to exclude ONE file would corrupt every other file referencing
//! it — silent data loss on the restored volume. So this module never guesses:
//!
//! * **Stage 1 (content):** a matched file's data extent is zeroed and dropped
//!   from the used-map **only when the extent tree proves exclusive ownership**
//!   — `refs` fully accounted for by `EXTENT_DATA_REF` back-references whose
//!   `(root, inode)` all belong to matched files. Any `SHARED_DATA_REF`, any
//!   foreign back-reference, any accounting mismatch → the extent is kept.
//!   Zeroed ranges also get their **checksum-tree entries rewritten** to the
//!   crc32c of a zero sector — unlike ext4/NTFS, btrfs verifies data checksums
//!   on read, so a zero-filled ghost would otherwise return EIO.
//! * **Stage 2 (metadata true removal, follow-up):** B-tree item removal so the
//!   file vanishes from the catalog; built on the same [`EditSet`] machinery.
//!
//! Matching is per subvolume: every subvolume/snapshot tree is walked and the
//! patterns are applied relative to *its* root, so `var/cache/**` matches inside
//! `@` and inside a snapshot of `@` alike. Subvolume boundaries are never
//! crossed by a directory-subtree exclusion, and a subvolume itself is never
//! removed.
//!
//! Failure policy (as everywhere): any parse ambiguity keeps the file — a
//! bigger image is always better than a corrupted one.

use std::collections::{BTreeMap, HashMap, HashSet};

use tracing::{info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::btrfs_map::{
    for_each_item, parse_chunk, read_node, walk_tree, ChunkMap, CHUNK_ITEM, HEADER, MAGIC,
    ROOT_ITEM, SUPER_OFFSET, SUPER_SIZE,
};
use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes, ExclusionPattern};
use crate::fat_map::RawVolume;

// --- Additional key types (btrfs_map exports the tree-level ones) ---
const INODE_ITEM: u8 = 1;
#[allow(dead_code)] // Stage 2 (catalog removal)
const INODE_REF: u8 = 12;
#[allow(dead_code)] // Stage 2 (catalog removal)
const DIR_ITEM: u8 = 84;
const DIR_INDEX: u8 = 96;
const EXTENT_DATA: u8 = 108;
const EXTENT_CSUM: u8 = 128;
const EXTENT_DATA_REF: u8 = 178;
const SHARED_DATA_REF: u8 = 184;
const EXTENT_ITEM_TYPE: u8 = 168;

const FS_TREE_OBJECTID: u64 = 5;
const CSUM_TREE_OBJECTID: u64 = 7;
const FIRST_FREE_OBJECTID: u64 = 256;
const LAST_FREE_OBJECTID: u64 = u64::MAX - 256;
/// Objectid of every EXTENT_CSUM item ((u64)-10).
const EXTENT_CSUM_OBJECTID: u64 = u64::MAX - 9;

/// `EXTENT_ITEM.flags` bit: a data extent (vs tree block).
const EXTENT_FLAG_DATA: u64 = 0x1;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("btrfs exclusions: {msg}"))
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

// --- CRC-32C (Castagnoli), raw running form (same primitive as ext4's) ---
fn crc32c_raw(seed: u32, data: &[u8]) -> u32 {
    let mut crc = seed;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    crc
}

/// btrfs crc32c flavor, calibrated against the superblock's own checksum at
/// bootstrap (`seed` and whether the result is complemented differ between
/// implementations — trusting a constant here would be a silent corruption
/// hazard, so we derive it from the filesystem itself).
#[derive(Clone, Copy)]
struct CsumFlavor {
    seed: u32,
    complement: bool,
}

impl CsumFlavor {
    fn compute(&self, data: &[u8]) -> u32 {
        let c = crc32c_raw(self.seed, data);
        if self.complement {
            !c
        } else {
            c
        }
    }

    /// Try all conventional (seed, complement) combinations against a known
    /// (data, stored) pair; `None` if nothing matches (foreign csum type).
    fn calibrate(data: &[u8], stored: u32) -> Option<Self> {
        for seed in [0xFFFF_FFFFu32, 0] {
            for complement in [false, true] {
                let f = CsumFlavor { seed, complement };
                if f.compute(data) == stored {
                    return Some(f);
                }
            }
        }
        None
    }
}

/// Result of a btrfs exclusion walk (mirrors the ext4/FAT scan structs).
pub struct BtrfsExclusionScan {
    /// PHYSICAL volume byte ranges of exclusively-owned excluded extents —
    /// zeroed in the image and dropped from the used-map.
    pub ranges: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    /// Bytes of excluded file data actually removed (exclusive extents only).
    pub bytes_excluded: u64,
    /// Files matched but kept whole because their data is shared (snapshot /
    /// reflink) — the catalog entry survives as-is in Stage 1.
    pub files_kept_shared: u64,
    pub categories: CategorySizes,
    /// Metadata patches (PHYSICAL offset → bytes): checksum-tree rewrites for
    /// the zeroed ranges (Stage 1) — and, once Stage 2 lands, the full catalog
    /// removal. Non-overlapping; every touched tree block carries a recomputed
    /// node checksum, DUP metadata patched in both copies.
    pub patches: Vec<(u64, Vec<u8>)>,
}

impl BtrfsExclusionScan {
    fn empty() -> Self {
        Self {
            ranges: ByteRanges::new(),
            files_excluded: 0,
            entries_scanned: 0,
            bytes_excluded: 0,
            files_kept_shared: 0,
            categories: CategorySizes::default(),
            patches: Vec::new(),
        }
    }
}

/// Bootstrapped filesystem context.
struct Fs {
    nodesize: u64,
    sectorsize: u64,
    chunks: ChunkMap,
    flavor: CsumFlavor,
    /// Tree roots from the root tree: objectid → root node logical.
    tree_roots: HashMap<u64, u64>,
}

impl Fs {
    /// Bootstrap: superblock → chunk map → root tree. Refuses multi-device,
    /// RAID, unknown incompat features and non-crc32c checksums.
    fn bootstrap(vol: &mut dyn RawVolume) -> Result<Fs> {
        if SUPER_OFFSET + SUPER_SIZE > vol.len() {
            return Err(bad("device too small"));
        }
        let mut sb = vec![0u8; SUPER_SIZE as usize];
        vol.read_at(SUPER_OFFSET, &mut sb)?;
        if &sb[0x40..0x48] != MAGIC {
            return Err(bad("no btrfs magic"));
        }
        let root = u64at(&sb, 0x50);
        let chunk_root = u64at(&sb, 0x58);
        let num_devices = u64at(&sb, 0x88);
        let sectorsize = u32at(&sb, 0x90);
        let nodesize = u32at(&sb, 0x94);
        let sys_array_size = u32at(&sb, 0xA0);
        let csum_type = u16at(&sb, 0xC4);
        let our_devid = u64at(&sb, 0xC9);

        if num_devices != 1 {
            return Err(bad("multi-device filesystem"));
        }
        if csum_type != 0 {
            return Err(bad(format!("csum type {csum_type} (not crc32c) — exclusions unsupported")));
        }
        // Same incompat allowance as the used-map builder.
        const INCOMPAT_SUPPORTED: u64 =
            0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400;
        let unknown = u64at(&sb, 0xBC) & !INCOMPAT_SUPPORTED;
        if unknown != 0 {
            return Err(bad(format!("unsupported incompat features {unknown:#x}")));
        }
        if !(512..=65536).contains(&sectorsize)
            || !sectorsize.is_power_of_two()
            || !(512..=262_144).contains(&nodesize)
            || !nodesize.is_power_of_two()
        {
            return Err(bad("implausible sector/node size"));
        }
        if sys_array_size == 0 || 0x32B + sys_array_size as usize > sb.len() {
            return Err(bad("implausible sys_chunk_array_size"));
        }

        // Calibrate the crc32c flavor from the superblock's own checksum
        // (crc over [0x20, 0x1000), stored little-endian at 0).
        let stored = u32at(&sb, 0) as u32;
        let flavor = CsumFlavor::calibrate(&sb[0x20..SUPER_SIZE as usize], stored)
            .ok_or_else(|| bad("superblock checksum matches no known crc32c flavor"))?;

        // Bootstrap chunk map from sys_chunk_array, then the chunk tree.
        let mut boot = ChunkMap::new();
        let sys = &sb[0x32B..0x32B + sys_array_size as usize];
        let mut pos = 0usize;
        while pos < sys.len() {
            if pos + 17 > sys.len() {
                return Err(bad("truncated sys_chunk_array key"));
            }
            if sys[pos + 8] != CHUNK_ITEM {
                return Err(bad("unexpected key type in sys_chunk_array"));
            }
            let logical = u64at(sys, pos + 9);
            let cd = &sys[pos + 17..];
            if cd.len() < 48 {
                return Err(bad("truncated sys chunk"));
            }
            let n = u16at(cd, 44) as usize;
            let clen = 48 + 32 * n;
            if pos + 17 + clen > sys.len() {
                return Err(bad("sys chunk exceeds array"));
            }
            boot.push(parse_chunk(logical, &cd[..clen], our_devid)?);
            pos += 17 + clen;
        }
        let nodesize = nodesize as u64;
        let mut chunks = ChunkMap::new();
        walk_tree(vol, &boot, chunk_root, nodesize, |node, nritems| {
            for_each_item(node, nritems, |_oid, ktype, koffset, data| {
                if ktype == CHUNK_ITEM {
                    chunks.push(parse_chunk(koffset, data, our_devid)?);
                }
                Ok(())
            })
        })?;
        if chunks.chunks.is_empty() {
            return Err(bad("empty chunk tree"));
        }

        // Root tree → every tree root we may need (fs/subvol/extent/csum).
        let mut tree_roots: HashMap<u64, u64> = HashMap::new();
        walk_tree(vol, &chunks, root, nodesize, |node, nritems| {
            for_each_item(node, nritems, |objectid, ktype, _koffset, data| {
                if ktype == ROOT_ITEM {
                    if data.len() < 176 + 8 {
                        return Err(bad("truncated root_item"));
                    }
                    tree_roots.insert(objectid, u64at(data, 176));
                }
                Ok(())
            })
        })?;

        Ok(Fs { nodesize, sectorsize: sectorsize as u64, chunks, flavor, tree_roots })
    }

    /// Subvolume tree roots to scan: the default FS tree and every subvolume /
    /// snapshot (a whole-disk backup must exclude in all of them).
    fn subvol_roots(&self) -> Vec<(u64, u64)> {
        self.tree_roots
            .iter()
            .filter(|(&id, _)| id == FS_TREE_OBJECTID || (FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&id))
            .map(|(&id, &root)| (id, root))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Transactional tree-block editor
// ---------------------------------------------------------------------------

/// In-memory edit buffer over tree blocks: nodes are loaded once by LOGICAL
/// address, mutated, and emitted as physical patches with recomputed node
/// checksums — into **every** stripe copy (DUP metadata). Used by the Stage-1
/// checksum rewrites and by the Stage-2 catalog removal alike.
struct EditSet {
    nodesize: u64,
    nodes: BTreeMap<u64, Vec<u8>>,
}

impl EditSet {
    fn new(nodesize: u64) -> Self {
        Self { nodesize, nodes: BTreeMap::new() }
    }

    /// Borrow a mutable copy of the tree block at `logical`, loading it on
    /// first touch.
    fn node_mut(&mut self, vol: &mut dyn RawVolume, fs: &Fs, logical: u64) -> Result<&mut Vec<u8>> {
        if !self.nodes.contains_key(&logical) {
            let n = read_node(vol, &fs.chunks, logical, self.nodesize)?;
            self.nodes.insert(logical, n);
        }
        Ok(self.nodes.get_mut(&logical).unwrap())
    }

    /// Recompute every touched node's checksum and emit `(physical, bytes)`
    /// patches for all stripe copies.
    fn finalize(mut self, fs: &Fs) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut out = Vec::new();
        for (logical, mut node) in std::mem::take(&mut self.nodes) {
            // Node checksum: crc over [0x20, nodesize), stored LE at 0.
            let c = fs.flavor.compute(&node[0x20..]);
            node[0..4].copy_from_slice(&c.to_le_bytes());
            node[4..0x20].fill(0);
            for phys in fs.chunks.physical(logical, self.nodesize)? {
                out.push((phys, node.clone()));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// FS-tree collection
// ---------------------------------------------------------------------------

/// One directory entry (from DIR_INDEX, the canonical enumeration).
struct DirEnt {
    parent: u64,
    name: String,
    /// `Ok(ino)` for a file/dir in this tree; `Err(())` for a subvolume link
    /// (never removed, never descended into from here).
    child: std::result::Result<u64, ()>,
}

/// One regular (non-inline) data extent reference of a file.
#[derive(Clone, Copy)]
struct DataRef {
    disk_bytenr: u64,
    disk_num_bytes: u64,
}

/// Everything collected from one subvolume tree in a single walk.
#[derive(Default)]
struct SubvolData {
    /// ino → (size, nlink, is_dir)
    inodes: HashMap<u64, (u64, u32, bool)>,
    dirents: Vec<DirEnt>,
    /// ino → its regular data extents.
    extents: HashMap<u64, Vec<DataRef>>,
    /// ino → true when any EXTENT_DATA is inline (data inside the leaf).
    has_inline: HashSet<u64>,
}

const S_IFMT: u32 = 0xF000;
const S_IFDIR: u32 = 0x4000;
#[allow(dead_code)] // Stage 2 (catalog removal)
const S_IFREG: u32 = 0x8000;

fn collect_subvol(vol: &mut dyn RawVolume, fs: &Fs, root: u64) -> Result<SubvolData> {
    let mut d = SubvolData::default();
    walk_tree(vol, &fs.chunks, root, fs.nodesize, |node, nritems| {
        for_each_item(node, nritems, |objectid, ktype, koffset, data| {
            match ktype {
                INODE_ITEM => {
                    if data.len() >= 160 {
                        let size = u64at(data, 16);
                        let nlink = u32at(data, 40) as u32;
                        let mode = u32at(data, 52) as u32;
                        d.inodes.insert(objectid, (size, nlink, mode & S_IFMT == S_IFDIR));
                    }
                }
                DIR_INDEX => {
                    // btrfs_dir_item: location key(17) + transid(8) + data_len(2)
                    // + name_len(2) + type(1) + name. DIR_INDEX holds exactly one.
                    if data.len() >= 30 {
                        let loc_objectid = u64at(data, 0);
                        let loc_type = data[8];
                        let name_len = u16at(data, 27) as usize;
                        if 30 + name_len <= data.len() {
                            let name = String::from_utf8_lossy(&data[30..30 + name_len]).to_string();
                            let child = if loc_type == ROOT_ITEM {
                                Err(()) // subvolume link
                            } else {
                                Ok(loc_objectid)
                            };
                            d.dirents.push(DirEnt { parent: objectid, name, child });
                        }
                    }
                    let _ = koffset;
                }
                EXTENT_DATA => {
                    // btrfs_file_extent_item; type at 20 (0 inline, 1 regular,
                    // 2 prealloc); regular: disk_bytenr@21, disk_num_bytes@29.
                    if data.len() >= 21 {
                        if data[20] == 0 {
                            d.has_inline.insert(objectid);
                        } else if data.len() >= 53 {
                            let disk_bytenr = u64at(data, 21);
                            let disk_num_bytes = u64at(data, 29);
                            if disk_bytenr != 0 && disk_num_bytes != 0 {
                                d.extents
                                    .entry(objectid)
                                    .or_default()
                                    .push(DataRef { disk_bytenr, disk_num_bytes });
                            }
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })
    })?;
    Ok(d)
}

/// Match the collected tree against `patterns`; returns the excluded inode set
/// (files + directory subtree closure, never crossing subvolume links).
fn match_subvol(
    d: &SubvolData,
    parsed: &[ExclusionPattern],
    scan: &mut BtrfsExclusionScan,
) -> HashSet<u64> {
    // Children by parent for the BFS.
    let mut children: HashMap<u64, Vec<&DirEnt>> = HashMap::new();
    for e in &d.dirents {
        children.entry(e.parent).or_default().push(e);
    }
    let mut matched: HashSet<u64> = HashSet::new();
    // (dir ino, rel components); root dir of a subvolume is inode 256.
    let mut stack: Vec<(u64, Vec<String>, bool)> = vec![(FIRST_FREE_OBJECTID, Vec::new(), false)];
    let mut visited: HashSet<u64> = HashSet::new();
    while let Some((dir, rel, sub_excluded)) = stack.pop() {
        if !visited.insert(dir) {
            continue;
        }
        let Some(entries) = children.get(&dir) else { continue };
        for e in entries {
            scan.entries_scanned += 1;
            let name_lower = e.name.to_lowercase();
            let mut crel = rel.clone();
            crel.push(name_lower.clone());
            let hit = sub_excluded || parsed.iter().any(|p| p.matches(&name_lower, &crel));
            match e.child {
                Ok(ino) => {
                    let is_dir = d.inodes.get(&ino).map(|&(_, _, dir)| dir).unwrap_or(false);
                    if hit {
                        matched.insert(ino);
                        if is_dir {
                            stack.push((ino, crel, true));
                        }
                    } else if is_dir {
                        stack.push((ino, crel, false));
                    } else if let Some(&(size, _, _)) = d.inodes.get(&ino) {
                        scan.categories.add(&name_lower, size);
                    }
                }
                Err(()) => {
                    if hit {
                        warn!(name = %e.name, "btrfs exclusions: pattern matches a SUBVOLUME — kept (subvolumes are never removed)");
                    }
                    // Never descend: the subvolume is walked as its own tree.
                }
            }
        }
    }
    matched
}

// ---------------------------------------------------------------------------
// Extent-tree ownership analysis
// ---------------------------------------------------------------------------

/// Ownership summary of one candidate data extent.
struct ExtentOwn {
    /// `EXTENT_ITEM.refs`.
    refs: u64,
    /// Sum of `EXTENT_DATA_REF.count` whose (root, objectid) is in the matched
    /// set (per-subvolume).
    matched_count: u64,
    /// Any SHARED_DATA_REF / foreign backref / non-data flag seen.
    foreign: bool,
}

/// Walk the extent tree once and resolve ownership of every candidate extent.
/// `matched`: (subvol root objectid, inode) pairs of all matched files.
fn analyze_extents(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    candidates: &HashMap<u64, u64>, // disk_bytenr → disk_num_bytes
    matched: &HashSet<(u64, u64)>,
) -> Result<HashMap<u64, ExtentOwn>> {
    let extent_root = *fs
        .tree_roots
        .get(&crate::btrfs_map::EXTENT_TREE_OBJECTID)
        .ok_or_else(|| bad("no extent tree root"))?;
    let mut own: HashMap<u64, ExtentOwn> = HashMap::new();

    walk_tree(vol, &fs.chunks, extent_root, fs.nodesize, |node, nritems| {
        for_each_item(node, nritems, |objectid, ktype, koffset, data| {
            match ktype {
                EXTENT_ITEM_TYPE => {
                    let Some(&want_len) = candidates.get(&objectid) else { return Ok(()) };
                    if data.len() < 24 {
                        return Err(bad("truncated extent item"));
                    }
                    let refs = u64at(data, 0);
                    let flags = u64at(data, 16);
                    let mut e = ExtentOwn { refs, matched_count: 0, foreign: false };
                    if koffset != want_len || flags & EXTENT_FLAG_DATA == 0 {
                        e.foreign = true; // length mismatch / not a data extent
                    }
                    // Inline back-references follow at offset 24.
                    let mut p = 24usize;
                    while p < data.len() {
                        let rt = data[p];
                        match rt {
                            EXTENT_DATA_REF => {
                                if p + 29 > data.len() {
                                    e.foreign = true;
                                    break;
                                }
                                let root = u64at(data, p + 1);
                                let ino = u64at(data, p + 9);
                                let count = u32at(data, p + 25);
                                if matched.contains(&(root, ino)) {
                                    e.matched_count += count;
                                } else {
                                    e.foreign = true;
                                }
                                p += 29;
                            }
                            SHARED_DATA_REF => {
                                // A CoW-shared leaf reference (snapshots): the
                                // owner cannot be attributed — keep the data.
                                e.foreign = true;
                                p += 13;
                            }
                            _ => {
                                e.foreign = true; // unknown backref kind
                                break;
                            }
                        }
                    }
                    own.insert(objectid, e);
                }
                EXTENT_DATA_REF => {
                    // Keyed (non-inline) data backref for some extent.
                    if let Some(e) = own.get_mut(&objectid) {
                        if data.len() >= 28 {
                            let root = u64at(data, 0);
                            let ino = u64at(data, 8);
                            let count = u32at(data, 24);
                            if matched.contains(&(root, ino)) {
                                e.matched_count += count;
                            } else {
                                e.foreign = true;
                            }
                        } else {
                            e.foreign = true;
                        }
                    }
                }
                SHARED_DATA_REF => {
                    if let Some(e) = own.get_mut(&objectid) {
                        e.foreign = true;
                    }
                }
                _ => {}
            }
            Ok(())
        })
    })?;
    Ok(own)
}

// ---------------------------------------------------------------------------
// Checksum-tree rewrite for zeroed ranges
// ---------------------------------------------------------------------------

/// Rewrite the EXTENT_CSUM entries covering `zeroed` (LOGICAL ranges, finalized)
/// to the checksum of an all-zero sector, so the zero-filled data reads back
/// cleanly. Edits whole leaves through `edits` (checksums recomputed there).
fn rewrite_csums_for_zeroed(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    zeroed_logical: &ByteRanges,
    edits: &mut EditSet,
) -> Result<()> {
    let Some(&csum_root) = fs.tree_roots.get(&CSUM_TREE_OBJECTID) else {
        return Ok(()); // no csum tree (nodatasum mount) — nothing to fix
    };
    let zero_sector = vec![0u8; fs.sectorsize as usize];
    let zero_csum = fs.flavor.compute(&zero_sector).to_le_bytes();

    // Collect the leaves + item positions to edit first (walk borrows vol).
    struct CsumEdit {
        leaf_logical: u64,
        data_off: usize,
        entries: Vec<usize>, // entry indices within the item to overwrite
    }
    let mut planned: Vec<CsumEdit> = Vec::new();
    walk_tree(vol, &fs.chunks, csum_root, fs.nodesize, |node, nritems| {
        let leaf_logical = u64at(node, 0x30); // header.bytenr
        for i in 0..nritems as usize {
            let it = HEADER + i * 25;
            let objectid = u64at(node, it);
            let ktype = node[it + 8];
            let start = u64at(node, it + 9); // logical byte the item covers from
            if ktype != EXTENT_CSUM || objectid != EXTENT_CSUM_OBJECTID {
                continue;
            }
            let data_off = HEADER + u32at(node, it + 17) as usize;
            let data_len = u32at(node, it + 21) as usize;
            let sectors = data_len / 4;
            let mut entries = Vec::new();
            for s in 0..sectors {
                let a = start + s as u64 * fs.sectorsize;
                if zeroed_logical.covers(a, a + fs.sectorsize) {
                    entries.push(s);
                }
            }
            if !entries.is_empty() {
                planned.push(CsumEdit { leaf_logical, data_off, entries });
            }
        }
        Ok(())
    })?;

    for e in planned {
        let node = edits.node_mut(vol, fs, e.leaf_logical)?;
        for s in e.entries {
            let o = e.data_off + s * 4;
            if o + 4 <= node.len() {
                node[o..o + 4].copy_from_slice(&zero_csum);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Walk every subvolume of the btrfs volume, match `patterns`, and produce the
/// Stage-1 exclusion: physical zero-ranges for exclusively-owned extents plus
/// checksum-tree patches keeping the zeroed data readable. Shared data is kept.
pub fn scan_btrfs_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<BtrfsExclusionScan> {
    let mut scan = BtrfsExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }
    let fs = Fs::bootstrap(vol)?;

    // 1. Walk every subvolume, match, and gather candidates.
    let mut matched_pairs: HashSet<(u64, u64)> = HashSet::new(); // (root, ino)
    let mut candidates: HashMap<u64, u64> = HashMap::new(); // bytenr → len
    // (root, ino) → its extents / metadata, for the accounting pass.
    let mut file_extents: HashMap<(u64, u64), Vec<DataRef>> = HashMap::new();
    let mut file_sizes: HashMap<(u64, u64), u64> = HashMap::new();
    let mut inline_files: HashSet<(u64, u64)> = HashSet::new();

    for (subvol_id, root) in fs.subvol_roots() {
        let d = collect_subvol(vol, &fs, root)?;
        tick(scan.entries_scanned)?;
        let matched = match_subvol(&d, &parsed, &mut scan);
        tick(scan.entries_scanned)?;
        for ino in matched {
            // Hard-linked files (nlink > 1) are kept whole, like on ext4/NTFS.
            if let Some(&(size, nlink, is_dir)) = d.inodes.get(&ino) {
                if !is_dir && nlink > 1 {
                    continue;
                }
                matched_pairs.insert((subvol_id, ino));
                file_sizes.insert((subvol_id, ino), if is_dir { 0 } else { size });
                if d.has_inline.contains(&ino) {
                    inline_files.insert((subvol_id, ino));
                }
                if let Some(exts) = d.extents.get(&ino) {
                    for e in exts {
                        candidates
                            .entry(e.disk_bytenr)
                            .and_modify(|l| *l = (*l).max(e.disk_num_bytes))
                            .or_insert(e.disk_num_bytes);
                    }
                    file_extents.insert((subvol_id, ino), exts.clone());
                }
            }
        }
    }
    if matched_pairs.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    // 2. Prove exclusive ownership through the extent tree.
    let own = analyze_extents(vol, &fs, &candidates, &matched_pairs)?;
    let exclusive = |bytenr: u64| -> bool {
        own.get(&bytenr)
            .map(|e| !e.foreign && e.refs > 0 && e.refs == e.matched_count)
            .unwrap_or(false)
    };

    // 3. Zero-ranges (physical) + logical ranges for the csum rewrite.
    let mut zeroed_logical = ByteRanges::new();
    for (&(root, ino), exts) in &file_extents {
        let all_exclusive = exts.iter().all(|e| exclusive(e.disk_bytenr));
        if !all_exclusive {
            scan.files_kept_shared += 1;
            info!(
                root, ino,
                "btrfs exclusions: file data is shared (snapshot/reflink) — data kept"
            );
            continue;
        }
        for e in exts {
            zeroed_logical.add(e.disk_bytenr, e.disk_bytenr + e.disk_num_bytes);
        }
        scan.bytes_excluded += file_sizes.get(&(root, ino)).copied().unwrap_or(0);
    }
    zeroed_logical.finalize();
    for (start, end) in zeroed_logical.iter() {
        for phys in fs.chunks.physical(start, end - start)? {
            scan.ranges.add(phys, phys + (end - start));
        }
    }
    scan.ranges.finalize();
    scan.files_excluded = matched_pairs.len() as u64;

    // Inline files (data inside the leaf) can only disappear with Stage 2.
    if !inline_files.is_empty() {
        info!(
            files = inline_files.len(),
            "btrfs exclusions: inline-data files await catalog removal (Stage 2)"
        );
    }

    // 4. Checksum rewrite for the zeroed ranges (EIO-free ghosts).
    let mut edits = EditSet::new(fs.nodesize);
    rewrite_csums_for_zeroed(vol, &fs, &zeroed_logical, &mut edits)?;
    scan.patches = edits.finalize(&fs)?;

    info!(
        files = scan.files_excluded,
        kept_shared = scan.files_kept_shared,
        bytes = scan.bytes_excluded,
        zero_ranges = scan.ranges.iter().count(),
        patches = scan.patches.len(),
        "btrfs exclusions: Stage-1 scan complete"
    );
    Ok(scan)
}

// ---------------------------------------------------------------------------
// Integration tests (need mkfs.btrfs / btrfs; Linux only)
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "linux"))]
mod btrfs_integration {
    use super::*;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    struct FileVol {
        f: std::fs::File,
        len: u64,
    }
    impl RawVolume for FileVol {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.f.read_exact_at(buf, offset).map_err(|e| bad(format!("read: {e}")))
        }
        fn len(&self) -> u64 {
            self.len
        }
    }

    fn have(cmd: &str) -> bool {
        std::process::Command::new(cmd).arg("--version").output().is_ok()
    }

    /// Populated btrfs image via `mkfs.btrfs --rootdir` (no mount needed).
    /// keep.txt = 0xCD, secret.log + cache/big.bin = 0xAB.
    fn make_btrfs() -> Option<(PathBuf, tempfile::TempDir)> {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).ok()?;
        std::fs::File::create(src.join("keep.txt")).ok()?.write_all(&[0xCD; 65536]).ok()?;
        std::fs::File::create(src.join("secret.log")).ok()?.write_all(&[0xAB; 98304]).ok()?;
        std::fs::File::create(src.join("cache/big.bin")).ok()?.write_all(&[0xAB; 196608]).ok()?;
        let img = dir.path().join("fs.img");
        // 128 MiB — above mkfs.btrfs's minimum device size.
        let f = std::fs::File::create(&img).ok()?;
        f.set_len(128 * 1024 * 1024).ok()?;
        drop(f);
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some((img, dir))
    }

    fn run_scan(img: &Path, patterns: &[&str]) -> (BtrfsExclusionScan, FileVol) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        let scan = scan_btrfs_exclusions(&mut vol, &pats, |_| Ok(())).unwrap();
        (scan, vol)
    }

    fn btrfs_check(img: &Path, data_csums: bool) -> (bool, String) {
        let mut c = std::process::Command::new("btrfs");
        c.arg("check");
        if data_csums {
            c.arg("--check-data-csum");
        }
        let out = c.arg(img).output().unwrap();
        let s = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), s)
    }

    /// Apply Stage-1 output to a copy: zero the ranges, apply the csum patches.
    fn apply_stage1(img: &Path, scan: &BtrfsExclusionScan, out_path: &Path) {
        let mut data = std::fs::read(img).unwrap();
        for (start, end) in scan.ranges.iter() {
            data[start as usize..end as usize].fill(0);
        }
        for (off, bytes) in &scan.patches {
            let o = *off as usize;
            assert!(o + bytes.len() <= data.len(), "patch out of range");
            data[o..o + bytes.len()].copy_from_slice(bytes);
        }
        std::fs::write(out_path, &data).unwrap();
    }

    #[test]
    fn stage1_zeroes_exclusive_extents_and_stays_check_clean() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let (scan, mut vol) = run_scan(&img, &["secret.log", "cache/**"]);
        // secret.log + cache dir + big.bin (dirs count as matched entries).
        assert!(scan.files_excluded >= 2, "excluded {}", scan.files_excluded);
        assert!(!scan.ranges.is_empty(), "no zero ranges");
        assert_eq!(scan.files_kept_shared, 0);
        assert_eq!(scan.bytes_excluded, 98304 + 196608);

        // Safety: no zero-range byte may hold the kept file's 0xCD data, and
        // together they must cover all 0xAB bytes of both excluded files.
        let mut buf = vec![0u8; 65536];
        let mut ab = 0u64;
        for (start, end) in scan.ranges.iter() {
            let mut off = start;
            while off < end {
                let n = ((end - off) as usize).min(buf.len());
                vol.read_at(off, &mut buf[..n]).unwrap();
                assert!(!buf[..n].contains(&0xCD), "zero range touches kept data");
                ab += buf[..n].iter().filter(|&&b| b == 0xAB).count() as u64;
                off += n as u64;
            }
        }
        assert_eq!(ab, 98304 + 196608, "0xAB coverage");

        // Apply zeroing + csum patches → btrfs check (incl. data csums) clean.
        assert!(!scan.patches.is_empty(), "expected csum-tree patches");
        let patched = keep.path().join("patched.img");
        apply_stage1(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check failed:\n{log}");

        // The kept file must still be extractable, byte-identical.
        let outdir = keep.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let out = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(out.status.success(), "btrfs restore failed");
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD), "keep.txt damaged");
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        let Some((img, _keep)) = make_btrfs() else { return };
        let (scan, _) = run_scan(&img, &[]);
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
    }

    /// Shared-extent fixture (reflink + snapshot), pre-generated with root and
    /// stored compressed in tests/data (see gen_btrfs_shared.sh). The excluded
    /// original's data is shared with a kept clone and a snapshot — nothing may
    /// be zeroed.
    #[test]
    fn shared_extents_are_never_zeroed() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/btrfs_shared.img.zst");
        if !zst.exists() {
            eprintln!("btrfs_shared.img.zst missing — skipping shared-extent test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shared.img");
        let raw = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        std::fs::write(&img, &raw).unwrap();

        // orig.bin is excluded; clone.bin (reflink of orig) and snap/ are kept.
        let (scan, _) = run_scan(&img, &["orig.bin"]);
        assert!(scan.files_excluded >= 1);
        assert!(
            scan.ranges.is_empty(),
            "shared extents were zeroed — CoW safety violated"
        );
        assert!(scan.files_kept_shared >= 1, "expected shared-kept accounting");
    }
}
