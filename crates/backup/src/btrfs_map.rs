//! Used-block map for btrfs (single-device), parsed read-only from the volume.
//!
//! Windows has no native btrfs driver, so — as [`crate::fat_map`]/[`crate::ext_map`]
//! do for their filesystems — we read the on-disk structures ourselves to learn
//! which blocks are allocated and copy only those. Strictly read-only: we walk
//! B-trees and never touch file contents.
//!
//! btrfs has no allocation bitmap; allocated space lives in the **extent tree**.
//! Reaching it means resolving btrfs's logical address space to physical device
//! offsets, so the walk is:
//!
//! 1. superblock (at 64 KiB) → bootstrap chunk map from `sys_chunk_array`;
//! 2. read the **chunk tree** through that bootstrap map → the full
//!    logical→physical chunk map;
//! 3. read the **root tree** → find the extent tree's root;
//! 4. walk the **extent tree**, marking every allocated extent (translated to
//!    physical, both copies for DUP metadata).
//!
//! Scope: single-device filesystems with the SINGLE or DUP profile only. Any
//! multi-device / RAID / unknown-incompat filesystem — or any structural doubt —
//! is an error, and the engine falls back to sector-by-sector. A wrong map must
//! never guess: missing data in an image is worse than a bigger image.

use tracing::info;

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::used_sectors::UsedSectorMap;

/// Primary superblock location and size.
pub(crate) const SUPER_OFFSET: u64 = 0x1_0000; // 64 KiB
pub(crate) const SUPER_SIZE: u64 = 0x1000; // 4 KiB
/// btrfs magic `_BHRfS_M`, at superblock offset 0x40.
pub(crate) const MAGIC: &[u8; 8] = b"_BHRfS_M";
/// Physical offsets of the superblock mirror copies (written only if the device
/// is large enough): 64 KiB, 64 MiB, 256 GiB.
pub(crate) const SB_MIRRORS: [u64; 3] = [0x1_0000, 0x400_0000, 0x40_0000_0000];
/// btrfs reserves the first 1 MiB of the device (holds the primary superblock).
const RESERVED_HEAD: u64 = 0x10_0000;

// Item key types.
pub(crate) const CHUNK_ITEM: u8 = 228;
pub(crate) const ROOT_ITEM: u8 = 132;
pub(crate) const EXTENT_ITEM: u8 = 168;
pub(crate) const METADATA_ITEM: u8 = 169;
pub(crate) const EXTENT_TREE_OBJECTID: u64 = 2;

// Block-group / chunk `type` flag bits.
const BG_DUP: u64 = 0x20;
/// All RAID/DUP profile bits — a zero result means the SINGLE profile.
const BG_PROFILE_MASK: u64 = 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400;

/// btrfs B-tree header size (`struct btrfs_header`).
pub(crate) const HEADER: usize = 0x65;

// Guards against corrupt structures / cyclic trees.
const MAX_NODE: u64 = 256 * 1024;
const MAX_TREE_BLOCKS: u64 = 4_000_000;
const MAX_CHUNKS: usize = 1_000_000;

/// Per-backup-block used map for a btrfs volume.
pub struct BtrfsUsedMap {
    block_used: Vec<bool>,
    used_count: u64,
    /// btrfs sector size in bytes.
    pub sector_size: u32,
}

impl UsedSectorMap for BtrfsUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.block_used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.used_count
    }
}

impl BtrfsUsedMap {
    /// Drop every backup block that lies ENTIRELY within `ranges` (excluded,
    /// exclusively-owned file data — plan D2) so it is not copied. A block only
    /// partly excluded stays used and is zeroed by the caller instead.
    /// `backup_block_size` is the block size the map was built with.
    pub fn apply_excluded_ranges(
        &mut self,
        ranges: &crate::exclusions::ByteRanges,
        backup_block_size: u32,
    ) {
        let bs = backup_block_size as u64;
        for (i, used) in self.block_used.iter_mut().enumerate() {
            if *used {
                let start = i as u64 * bs;
                if ranges.covers(start, start + bs) {
                    *used = false;
                    self.used_count -= 1;
                }
            }
        }
    }
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("btrfs map: {msg}"))
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

/// A logical→physical chunk mapping on the (single) device. `stripes` holds one
/// physical start offset for SINGLE, two for DUP.
pub(crate) struct Chunk {
    pub(crate) logical: u64,
    pub(crate) length: u64,
    pub(crate) stripes: Vec<u64>,
}

/// Ordered set of chunks; lookups translate a logical range to physical offsets.
pub(crate) struct ChunkMap {
    pub(crate) chunks: Vec<Chunk>,
}

impl ChunkMap {
    pub(crate) fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    pub(crate) fn push(&mut self, c: Chunk) {
        self.chunks.push(c);
    }

    /// Physical start offset(s) of a `len`-byte logical range, which must fall
    /// entirely inside a single chunk. Returns one offset per mirror (SINGLE →
    /// 1, DUP → 2).
    pub(crate) fn physical(&self, logical: u64, len: u64) -> Result<Vec<u64>> {
        let end = logical
            .checked_add(len)
            .ok_or_else(|| bad("logical range overflows"))?;
        for c in &self.chunks {
            if logical >= c.logical && end <= c.logical + c.length {
                let delta = logical - c.logical;
                return Ok(c.stripes.iter().map(|s| s + delta).collect());
            }
        }
        Err(bad(format!("logical range [{logical}, {end}) not in any chunk")))
    }
}

/// Parse a `struct btrfs_chunk` at `data` (mapped to logical `logical`).
/// Rejects anything but SINGLE/DUP on our single device.
pub(crate) fn parse_chunk(logical: u64, data: &[u8], our_devid: u64) -> Result<Chunk> {
    if data.len() < 48 {
        return Err(bad("truncated chunk item"));
    }
    let length = u64at(data, 0);
    let ctype = u64at(data, 24);
    let num_stripes = u16at(data, 44);
    if length == 0 || num_stripes == 0 {
        return Err(bad("zero chunk length / stripes"));
    }
    let profile = ctype & BG_PROFILE_MASK;
    if profile != 0 && profile != BG_DUP {
        return Err(bad(format!("unsupported chunk profile {profile:#x} (RAID)")));
    }
    let expect = if profile == BG_DUP { 2 } else { 1 };
    if num_stripes != expect {
        return Err(bad(format!(
            "profile {profile:#x} with {num_stripes} stripes (expected {expect})"
        )));
    }
    if data.len() < 48 + 32 * num_stripes as usize {
        return Err(bad("truncated chunk stripes"));
    }
    let mut stripes = Vec::with_capacity(num_stripes as usize);
    for i in 0..num_stripes as usize {
        let base = 48 + i * 32;
        let devid = u64at(data, base);
        if devid != our_devid {
            return Err(bad("chunk stripe on another device (multi-device)"));
        }
        stripes.push(u64at(data, base + 8)); // stripe offset (physical)
    }
    Ok(Chunk { logical, length, stripes })
}

/// Read one B-tree block (`nodesize` bytes) at logical address `logical`.
pub(crate) fn read_node(
    vol: &mut dyn RawVolume,
    map: &ChunkMap,
    logical: u64,
    nodesize: u64,
) -> Result<Vec<u8>> {
    let phys = map.physical(logical, nodesize)?[0];
    if phys + nodesize > vol.len() {
        return Err(bad(format!("tree block at {phys} runs past the device")));
    }
    let mut node = vec![0u8; nodesize as usize];
    vol.read_at(phys, &mut node)?;
    Ok(node)
}

/// Walk a B-tree from `root` (logical), invoking `leaf` on every leaf node with
/// `(node, nritems)`. Internal nodes are followed depth-first.
pub(crate) fn walk_tree(
    vol: &mut dyn RawVolume,
    map: &ChunkMap,
    root: u64,
    nodesize: u64,
    mut leaf: impl FnMut(&[u8], u64) -> Result<()>,
) -> Result<()> {
    let mut stack = vec![root];
    let mut visited = 0u64;
    while let Some(logical) = stack.pop() {
        visited += 1;
        if visited > MAX_TREE_BLOCKS {
            return Err(bad("tree too large or cyclic"));
        }
        let node = read_node(vol, map, logical, nodesize)?;
        let nritems = u32at(&node, 0x60);
        let level = node[0x64];
        if level == 0 {
            // Leaf: HEADER + nritems × btrfs_item(25). Bounds are validated by
            // the leaf handler as it reads each item.
            if HEADER + nritems as usize * 25 > node.len() {
                return Err(bad("leaf item array exceeds node"));
            }
            leaf(&node, nritems)?;
        } else {
            // Internal: HEADER + nritems × btrfs_key_ptr(33); child at +17.
            if HEADER + nritems as usize * 33 > node.len() {
                return Err(bad("internal key-ptr array exceeds node"));
            }
            for i in 0..nritems as usize {
                let off = HEADER + i * 33;
                stack.push(u64at(&node, off + 17));
            }
        }
    }
    Ok(())
}

/// Iterate a leaf's items as `(key_objectid, key_type, key_offset, data)`.
pub(crate) fn for_each_item(node: &[u8], nritems: u64, mut f: impl FnMut(u64, u8, u64, &[u8]) -> Result<()>) -> Result<()> {
    for i in 0..nritems as usize {
        let it = HEADER + i * 25;
        let objectid = u64at(node, it);
        let ktype = node[it + 8];
        let koffset = u64at(node, it + 9);
        let data_off = HEADER + u32at(node, it + 17) as usize;
        let data_len = u32at(node, it + 21) as usize;
        let end = data_off
            .checked_add(data_len)
            .ok_or_else(|| bad("item data overflows"))?;
        if end > node.len() {
            return Err(bad("item data exceeds node"));
        }
        f(objectid, ktype, koffset, &node[data_off..end])?;
    }
    Ok(())
}

/// Detect btrfs on `vol` and build the used map. Errors (→ caller falls back to
/// sector-by-sector) on multi-device / RAID / anything unrecognized.
pub fn build_btrfs_used_map(vol: &mut dyn RawVolume, block_size: u32) -> Result<BtrfsUsedMap> {
    // --- Superblock (4 KiB at 64 KiB) ---
    if SUPER_OFFSET + SUPER_SIZE > vol.len() {
        return Err(bad("device too small for a btrfs superblock"));
    }
    let mut sb = vec![0u8; SUPER_SIZE as usize];
    vol.read_at(SUPER_OFFSET, &mut sb)?;
    if &sb[0x40..0x48] != MAGIC {
        return Err(bad("no btrfs magic at superblock offset 0x40"));
    }

    let root = u64at(&sb, 0x50); // root tree, logical
    let chunk_root = u64at(&sb, 0x58); // chunk tree, logical
    let log_root = u64at(&sb, 0x60); // fsync log tree (0 = clean shutdown)
    let total_bytes = u64at(&sb, 0x70);
    let bytes_used = u64at(&sb, 0x78);
    let num_devices = u64at(&sb, 0x88);
    let sectorsize = u32at(&sb, 0x90);
    let nodesize = u32at(&sb, 0x94);
    let sys_array_size = u32at(&sb, 0xA0);
    // dev_item.devid — this device's id (single-device: everything must match it).
    let our_devid = u64at(&sb, 0xC9);

    if num_devices != 1 {
        return Err(bad(format!("multi-device filesystem ({num_devices} devices)")));
    }
    if log_root != 0 {
        // Unreplayed fsync log: its tree blocks are NOT in the extent tree, so
        // a map built from extents would drop them from the copy and the
        // restored system's log replay would read garbage. A full-sector copy
        // carries the log along and replays fine.
        return Err(bad("log tree not empty (unclean shutdown) — falling back to a full copy"));
    }
    // Refuse any incompat feature that could change how allocated space is
    // recorded. Only features that leave the extent-tree walk correct are
    // allowed (mixed_backref, default_subvol, mixed_groups, compress lzo/zstd,
    // big_metadata, extended_iref, skinny_metadata, no_holes, metadata_uuid);
    // RAID56/RAID1C34/zoned/extent_tree_v2/raid_stripe_tree and any future flag
    // fall back to a full copy per this module's policy.
    const INCOMPAT_SUPPORTED: u64 =
        0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400;
    let unknown_incompat = u64at(&sb, 0xBC) & !INCOMPAT_SUPPORTED;
    if unknown_incompat != 0 {
        return Err(bad(format!(
            "unsupported btrfs incompat features {unknown_incompat:#x}"
        )));
    }
    if !(512..=MAX_NODE).contains(&(nodesize as u64)) || !nodesize.is_power_of_two() {
        return Err(bad(format!("implausible nodesize {nodesize}")));
    }
    if !(512..=65536).contains(&(sectorsize as u64)) || !sectorsize.is_power_of_two() {
        return Err(bad(format!("implausible sectorsize {sectorsize}")));
    }
    if total_bytes > vol.len() {
        return Err(bad(format!(
            "filesystem claims {total_bytes} bytes but the device is {}",
            vol.len()
        )));
    }
    if sys_array_size == 0 || 0x32B + sys_array_size as usize > sb.len() {
        return Err(bad(format!("implausible sys_chunk_array_size {sys_array_size}")));
    }

    let nodesize = nodesize as u64;

    // --- Bootstrap chunk map from sys_chunk_array (packed key+chunk pairs) ---
    let mut map = ChunkMap::new();
    let sys = &sb[0x32B..0x32B + sys_array_size as usize];
    let mut pos = 0usize;
    while pos < sys.len() {
        if pos + 17 > sys.len() {
            return Err(bad("truncated sys_chunk_array key"));
        }
        let ktype = sys[pos + 8];
        let logical = u64at(sys, pos + 9);
        if ktype != CHUNK_ITEM {
            return Err(bad(format!("unexpected key type {ktype} in sys_chunk_array")));
        }
        let chunk_data = &sys[pos + 17..];
        if chunk_data.len() < 48 {
            return Err(bad("truncated sys_chunk_array chunk"));
        }
        let num_stripes = u16at(chunk_data, 44) as usize;
        let chunk_len = 48 + 32 * num_stripes;
        if pos + 17 + chunk_len > sys.len() {
            return Err(bad("sys_chunk_array chunk exceeds array"));
        }
        map.push(parse_chunk(logical, &chunk_data[..chunk_len], our_devid)?);
        pos += 17 + chunk_len;
        if map.chunks.len() > MAX_CHUNKS {
            return Err(bad("too many system chunks"));
        }
    }
    if map.chunks.is_empty() {
        return Err(bad("empty sys_chunk_array"));
    }

    // --- Read the chunk tree for the complete logical→physical map ---
    let mut full = ChunkMap::new();
    walk_tree(vol, &map, chunk_root, nodesize, |node, nritems| {
        for_each_item(node, nritems, |_objectid, ktype, koffset, data| {
            if ktype == CHUNK_ITEM {
                full.push(parse_chunk(koffset, data, our_devid)?);
            }
            Ok(())
        })
    })?;
    if full.chunks.is_empty() {
        return Err(bad("chunk tree contained no chunks"));
    }
    if full.chunks.len() > MAX_CHUNKS {
        return Err(bad("too many chunks"));
    }

    // --- Root tree → extent tree root ---
    let mut extent_root = 0u64;
    walk_tree(vol, &full, root, nodesize, |node, nritems| {
        for_each_item(node, nritems, |objectid, ktype, _koffset, data| {
            if ktype == ROOT_ITEM && objectid == EXTENT_TREE_OBJECTID {
                // struct btrfs_root_item: bytenr at offset 176.
                if data.len() < 176 + 8 {
                    return Err(bad("truncated root_item"));
                }
                extent_root = u64at(data, 176);
            }
            Ok(())
        })
    })?;
    if extent_root == 0 {
        return Err(bad("extent tree root not found in the root tree"));
    }

    // --- Build the per-backup-block map ---
    let total_backup_blocks = vol.len().div_ceil(block_size as u64);
    let mut used = vec![false; total_backup_blocks as usize];

    // Always copy the reserved first megabyte and every superblock mirror that
    // lands inside this device — these live outside the extent tree.
    mark_bytes(&mut used, block_size, 0, RESERVED_HEAD.min(vol.len()));
    for &m in &SB_MIRRORS {
        if m + SUPER_SIZE <= vol.len() {
            mark_bytes(&mut used, block_size, m, m + SUPER_SIZE);
        }
    }

    // --- Walk the extent tree, marking every allocated extent ---
    walk_tree(vol, &full, extent_root, nodesize, |node, nritems| {
        for_each_item(node, nritems, |objectid, ktype, koffset, _data| {
            let len = match ktype {
                EXTENT_ITEM => koffset,      // key.offset = extent length
                METADATA_ITEM => nodesize,   // one tree block
                _ => return Ok(()),
            };
            if len == 0 {
                return Ok(());
            }
            // objectid = extent start (logical). Map to physical (both copies
            // for DUP) and mark.
            for phys in full.physical(objectid, len)? {
                mark_bytes(&mut used, block_size, phys, phys + len);
            }
            Ok(())
        })
    })?;

    let used_count = used.iter().filter(|&&u| u).count() as u64;

    // Coarse safety net: what we kept must cover at least the superblock's
    // reported allocated bytes. Folding into larger backup blocks only adds
    // coverage, so keeping less means we missed extents — refuse and fall back.
    let covered = (used_count * block_size as u64).min(vol.len());
    if covered < bytes_used {
        return Err(bad(format!(
            "kept {covered} bytes but the filesystem reports {bytes_used} used"
        )));
    }

    info!(
        fs = "btrfs",
        sectorsize,
        nodesize,
        chunks = full.chunks.len(),
        used_blocks = used_count,
        total_blocks = total_backup_blocks,
        "btrfs used-block map ready"
    );
    Ok(BtrfsUsedMap {
        block_used: used,
        used_count,
        sector_size: sectorsize as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (start, end) = (offset as usize, offset as usize + buf.len());
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

    fn put_u16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    const DEVID: u64 = 1;
    const NODESIZE: u64 = 4096;
    // One SINGLE chunk covers all logical addresses used below; logical L maps
    // to physical STRIPE + L.
    const STRIPE: u64 = 0x10_0000; // 1 MiB
    const CHUNK_LEN: u64 = 0x100_0000; // 16 MiB

    /// Write a `btrfs_chunk` (SINGLE) into `b` at `off`; returns bytes written.
    fn put_chunk(b: &mut [u8], off: usize, length: u64, ctype: u64) -> usize {
        put_u64(b, off, length);
        put_u64(b, off + 8, 0); // owner
        put_u64(b, off + 16, 65536); // stripe_len
        put_u64(b, off + 24, ctype);
        put_u32(b, off + 32, 4096); // io_align
        put_u32(b, off + 36, 4096); // io_width
        put_u32(b, off + 40, 4096); // sector_size
        put_u16(b, off + 44, 1); // num_stripes
        put_u16(b, off + 46, 0); // sub_stripes
        put_u64(b, off + 48, DEVID); // stripe.devid
        put_u64(b, off + 48 + 8, STRIPE); // stripe.offset
        48 + 32
    }

    /// Write a leaf header (level 0) with `nritems`.
    fn put_leaf_header(b: &mut [u8], off: usize, nritems: u32) {
        put_u32(b, off + 0x60, nritems);
        b[off + 0x64] = 0; // level
    }

    /// Write a `btrfs_item` (key + data ptr) at item slot `i`; `data_off` is the
    /// data offset relative to the node's header end.
    fn put_item(
        b: &mut [u8],
        node: usize,
        i: usize,
        objectid: u64,
        ktype: u8,
        koffset: u64,
        data_off: u32,
        data_len: u32,
    ) {
        let it = node + HEADER + i * 25;
        put_u64(b, it, objectid);
        b[it + 8] = ktype;
        put_u64(b, it + 9, koffset);
        put_u32(b, it + 17, data_off);
        put_u32(b, it + 21, data_len);
    }

    /// Hand-built minimal btrfs: chunk tree at logical 0, root tree at 0x4000,
    /// extent tree at 0x8000; the extent tree records two extents.
    fn minimal_btrfs() -> MemVolume {
        let dev = 0x110_0000usize; // 17 MiB (== STRIPE + CHUNK_LEN)
        let mut b = vec![0u8; dev];

        // --- Superblock at 64 KiB ---
        let sb = SUPER_OFFSET as usize;
        b[sb + 0x40..sb + 0x48].copy_from_slice(MAGIC);
        put_u64(&mut b, sb + 0x50, 0x4000); // root tree logical
        put_u64(&mut b, sb + 0x58, 0x0); // chunk tree logical
        put_u64(&mut b, sb + 0x70, dev as u64); // total_bytes
        put_u64(&mut b, sb + 0x78, 0x2000); // bytes_used (coarse check only)
        put_u64(&mut b, sb + 0x88, 1); // num_devices
        put_u32(&mut b, sb + 0x90, 4096); // sectorsize
        put_u32(&mut b, sb + 0x94, NODESIZE as u32); // nodesize
        put_u64(&mut b, sb + 0xC9, DEVID); // dev_item.devid
        // sys_chunk_array: one key (CHUNK_ITEM, offset = chunk logical 0) + chunk.
        let sys = sb + 0x32B;
        b[sys + 8] = CHUNK_ITEM;
        put_u64(&mut b, sys + 9, 0); // chunk logical
        let clen = put_chunk(&mut b, sys + 17, CHUNK_LEN, 0x2 /* SYSTEM, SINGLE */);
        put_u32(&mut b, sb + 0xA0, (17 + clen) as u32); // sys_chunk_array_size

        // Helper: physical of a logical tree-block address.
        let phys = |logical: u64| (STRIPE + logical) as usize;

        // --- Chunk tree leaf @ logical 0 → physical STRIPE ---
        let ct = phys(0x0);
        put_leaf_header(&mut b, ct, 1);
        // item 0: key (256, CHUNK_ITEM, 0), data = the chunk struct.
        let cdata = 2048u32; // data offset within node (past the item array)
        put_item(&mut b, ct, 0, 256, CHUNK_ITEM, 0, cdata, (48 + 32) as u32);
        put_chunk(&mut b, ct + HEADER + cdata as usize, CHUNK_LEN, 0x2);

        // --- Root tree leaf @ logical 0x4000 ---
        let rt = phys(0x4000);
        put_leaf_header(&mut b, rt, 1);
        // item 0: key (EXTENT_TREE_OBJECTID=2, ROOT_ITEM, 0); root_item.bytenr@176.
        let rdata = 1024u32;
        put_item(&mut b, rt, 0, EXTENT_TREE_OBJECTID, ROOT_ITEM, 0, rdata, 256);
        put_u64(&mut b, rt + HEADER + rdata as usize + 176, 0x8000); // extent root logical

        // --- Extent tree leaf @ logical 0x8000: two allocated extents ---
        let et = phys(0x8000);
        put_leaf_header(&mut b, et, 2);
        // EXTENT_ITEM at logical 0xA0_0000, length 0x1000 → physical STRIPE+0xA00000.
        put_item(&mut b, et, 0, 0xA0_0000, EXTENT_ITEM, 0x1000, 2048, 0);
        // METADATA_ITEM at logical 0xC0_0000 → one nodesize block.
        put_item(&mut b, et, 1, 0xC0_0000, METADATA_ITEM, 0, 2048, 0);

        MemVolume(b)
    }

    #[test]
    fn walks_trees_and_marks_extents() {
        let mut vol = minimal_btrfs();
        // Backup block size == sector size (4 KiB) → block N covers bytes [N*4K..).
        let map = build_btrfs_used_map(&mut vol, 4096).unwrap();
        assert_eq!(map.sector_size, 4096);

        let block = |phys: u64| phys / 4096;
        // Reserved first MiB is always kept.
        assert!(map.is_block_used(0));
        assert!(map.is_block_used(block(0xF_F000))); // last block under 1 MiB
        // Data extent: physical STRIPE + 0xA0_0000.
        assert!(map.is_block_used(block(STRIPE + 0xA0_0000)));
        // Metadata extent: physical STRIPE + 0xC0_0000.
        assert!(map.is_block_used(block(STRIPE + 0xC0_0000)));
        // A gap between the two extents is free.
        assert!(!map.is_block_used(block(STRIPE + 0xB0_0000)));
    }

    #[test]
    fn rejects_non_btrfs() {
        assert!(build_btrfs_used_map(&mut MemVolume(vec![0u8; 0x11_0000]), 4096).is_err());
    }

    #[test]
    fn rejects_multi_device() {
        let mut vol = minimal_btrfs();
        put_u64(&mut vol.0, SUPER_OFFSET as usize + 0x88, 2); // num_devices = 2
        assert!(build_btrfs_used_map(&mut vol, 4096).is_err());
    }

    #[test]
    fn parse_chunk_rejects_raid_and_foreign_device() {
        let mut buf = vec![0u8; 48 + 32 * 2];
        // RAID1 (0x10) profile with the right stripe count is still refused.
        super::put_chunk_test(&mut buf, CHUNK_LEN, 0x4 | 0x10, 2);
        assert!(parse_chunk(0, &buf, DEVID).is_err());

        // SINGLE but the stripe points at another device.
        let mut buf = vec![0u8; 48 + 32];
        super::put_chunk_test(&mut buf, CHUNK_LEN, 0x1, 1);
        put_u64(&mut buf, 48, 999); // devid
        assert!(parse_chunk(0, &buf, DEVID).is_err());
    }
}

/// Test-only chunk writer (kept out of the `tests` module so it can vary the
/// stripe count for the RAID-rejection test).
#[cfg(test)]
fn put_chunk_test(b: &mut [u8], length: u64, ctype: u64, num_stripes: u16) {
    b[0..8].copy_from_slice(&length.to_le_bytes());
    b[24..32].copy_from_slice(&ctype.to_le_bytes());
    b[44..46].copy_from_slice(&num_stripes.to_le_bytes());
    for i in 0..num_stripes as usize {
        b[48 + i * 32..48 + i * 32 + 8].copy_from_slice(&1u64.to_le_bytes()); // devid
    }
}
