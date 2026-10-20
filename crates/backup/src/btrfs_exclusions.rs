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
//! * **Stage 2 (metadata true removal):** a file whose extents are ALL cleanly
//!   exclusively-owned is fully deleted — its FS-tree items (inode, backref,
//!   extent-data, dir-item/index) are removed, its extents freed in the extent
//!   tree, block-group `used` and superblock `bytes_used` decremented, the
//!   surviving parent's dir size fixed, every touched B-tree node's checksum
//!   recomputed (DUP → both copies) and the first-key change cascaded up the
//!   parent chain. The free-space-tree feature is dropped (the kernel rebuilds
//!   it on mount). Result: `btrfs check` (incl. `--check-data-csum`) clean and
//!   the file gone. A file with any shared/snapshot extent stays on Stage 1.
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

use own_backup_common::{tr, OwnBackupError, Result};

use crate::btrfs_map::{
    for_each_item, parse_chunk, read_node, walk_tree, ChunkMap, CHUNK_ITEM, HEADER, MAGIC,
    ROOT_ITEM, SUPER_OFFSET, SUPER_SIZE,
};
use crate::exclusions::{
    parse_patterns, ByteRanges, CategorySizes, ExclusionPattern, LeftInCopy, LeftReason,
};
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

/// Test-only: make `EditSet::finalize` refuse, so the Stage-1 fallback can be
/// exercised without hunting for a filesystem that triggers it naturally.
#[cfg(test)]
thread_local! {
    static FAIL_FINALIZE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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

// --- CRC-32C (Castagnoli), raw running form (same primitive as ext4's and xfs's) ---
pub(crate) fn crc32c_raw(seed: u32, data: &[u8]) -> u32 {
    let mut crc = seed;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
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
    /// zeroed in the image.
    pub ranges: ByteRanges,
    /// The subset of [`Self::ranges`] that may also be DROPPED from the used
    /// map: extents Stage 2 actually freed, which the filesystem no longer
    /// references.
    ///
    /// A Stage-1 ghost is not in here, and that distinction is the difference
    /// between a working restore and a broken one. Its entry survives, and on
    /// btrfs its checksums were rewritten to the checksum of zeros — so those
    /// sectors MUST read back as zeros. Drop them from the map and they are
    /// never stored, never written, and the restored filesystem reads whatever
    /// the target disk happened to hold: checksum failure, EIO, and an apt that
    /// cannot open its own cache. Kept in the map they cost nothing — an
    /// all-zero block is a few bytes in qcow2 and an unallocated one in VHDX.
    pub droppable: ByteRanges,
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
    /// Matched files whose content is still in the copy, and matched
    /// subvolumes (`exclusions::key_store_left_behind`).
    pub left: Vec<crate::exclusions::LeftInCopy>,
}

impl BtrfsExclusionScan {
    fn empty() -> Self {
        Self {
            ranges: ByteRanges::new(),
            droppable: ByteRanges::new(),
            files_excluded: 0,
            entries_scanned: 0,
            bytes_excluded: 0,
            files_kept_shared: 0,
            categories: CategorySizes::default(),
            patches: Vec::new(),
            left: Vec::new(),
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
        let log_root = u64at(&sb, 0x60);
        let num_devices = u64at(&sb, 0x88);
        let sectorsize = u32at(&sb, 0x90);
        let nodesize = u32at(&sb, 0x94);
        let sys_array_size = u32at(&sb, 0xA0);
        let csum_type = u16at(&sb, 0xC4);
        let our_devid = u64at(&sb, 0xC9);

        if num_devices != 1 {
            return Err(bad("multi-device filesystem"));
        }
        if log_root != 0 {
            // A non-empty fsync log tree means an unclean shutdown: the kernel
            // replays it on the next mount, inserting items (csums included)
            // that would collide with anything we rewrite here. Invisible to
            // every static check — the collision appears at first boot.
            return Err(bad(
                "log tree not empty (unclean shutdown) — exclusions skipped",
            ));
        }
        if csum_type != 0 {
            return Err(bad(format!(
                "csum type {csum_type} (not crc32c) — exclusions unsupported"
            )));
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

        Ok(Fs {
            nodesize,
            sectorsize: sectorsize as u64,
            chunks,
            flavor,
            tree_roots,
        })
    }

    /// Subvolume tree roots to scan: the default FS tree and every subvolume /
    /// snapshot (a whole-disk backup must exclude in all of them).
    fn subvol_roots(&self) -> Vec<(u64, u64)> {
        self.tree_roots
            .iter()
            .filter(|(&id, _)| {
                id == FS_TREE_OBJECTID || (FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&id)
            })
            .map(|(&id, &root)| (id, root))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Transactional tree-block editor
// ---------------------------------------------------------------------------

/// A btrfs item key: (objectid, type, offset).
type Key = (u64, u8, u64);

fn read_key(b: &[u8], o: usize) -> Key {
    (u64at(b, o), b[o + 8], u64at(b, o + 9))
}

/// In-memory edit buffer over tree blocks: nodes are loaded once by LOGICAL
/// address, mutated, and emitted as physical patches with recomputed node
/// checksums — into **every** stripe copy (DUP metadata). Used by the Stage-1
/// checksum rewrites and by the Stage-2 catalog removal alike.
struct EditSet {
    nodesize: u64,
    nodes: BTreeMap<u64, Vec<u8>>,
    /// Leaf items to delete, by leaf logical → set of keys.
    deletes: BTreeMap<u64, HashSet<Key>>,
    /// In-place item-data patches, by leaf logical → (key, field offset, bytes).
    patches: BTreeMap<u64, Vec<(Key, usize, Vec<u8>)>>,
    /// Items to INSERT, by leaf logical → (key, item data). Everything stays
    /// within the leaf: an insert that does not fit fails the whole edit set
    /// (callers pre-spare files so it never comes to that). Trimming/splitting
    /// an existing item is delete + insert(s) with the derived keys.
    inserts: BTreeMap<u64, Vec<(Key, Vec<u8>)>>,
    /// Leaves proven un-freeable (shared with a snapshot, or their extent
    /// items would empty an extent-tree leaf). Unlike `drop_leaves` this
    /// survives the planning rounds — a verdict does not need re-deriving.
    never_drop: HashSet<u64>,
    /// Leaves we intend to UNLINK from their parent once they end up empty,
    /// instead of sparing a file to keep them alive. Advisory: `finalize`
    /// checks reality and refuses if a leaf marked here is not actually empty,
    /// because its tree block's extent has already been freed by then.
    drop_leaves: HashSet<u64>,
    /// child logical → (parent internal-node logical, slot) — for key cascade.
    parent: HashMap<u64, (u64, usize)>,
    /// Roots whose parent linkage has been learned. A node missing from
    /// `parent` is legitimately parentless ONLY if it is one of these; anything
    /// else means we are editing a tree we never mapped, and a first-key change
    /// there would silently strand the parent's key.
    roots: HashSet<u64>,
}

impl EditSet {
    fn new(nodesize: u64) -> Self {
        Self {
            nodesize,
            nodes: BTreeMap::new(),
            deletes: BTreeMap::new(),
            patches: BTreeMap::new(),
            inserts: BTreeMap::new(),
            never_drop: HashSet::new(),
            drop_leaves: HashSet::new(),
            parent: HashMap::new(),
            roots: HashSet::new(),
        }
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

    fn delete_item(&mut self, leaf: u64, key: Key) {
        self.deletes.entry(leaf).or_default().insert(key);
    }

    fn patch_item(&mut self, leaf: u64, key: Key, field_off: usize, bytes: Vec<u8>) {
        self.patches
            .entry(leaf)
            .or_default()
            .push((key, field_off, bytes));
    }

    fn insert_item(&mut self, leaf: u64, key: Key, data: Vec<u8>) {
        self.inserts.entry(leaf).or_default().push((key, data));
    }

    /// Record the whole tree's parent linkage (child logical → parent+slot) so
    /// a leaf whose first key changes can update its ancestors.
    fn learn_parents(&mut self, vol: &mut dyn RawVolume, fs: &Fs, root: u64) -> Result<()> {
        if !self.roots.insert(root) {
            return Ok(()); // already mapped
        }
        let mut stack = vec![root];
        let mut seen = 0u64;
        while let Some(logical) = stack.pop() {
            seen += 1;
            if seen > 4_000_000 {
                return Err(bad("tree too large learning parents"));
            }
            let node = read_node(vol, &fs.chunks, logical, self.nodesize)?;
            if node[0x64] == 0 {
                continue; // leaf
            }
            let nritems = u32at(&node, 0x60) as usize;
            for i in 0..nritems {
                let off = HEADER + i * 33;
                let child = u64at(&node, off + 17);
                self.parent.insert(child, (logical, i));
                stack.push(child);
            }
        }
        Ok(())
    }

    /// Apply all deletes/patches (rebuilding affected leaves), cascade any
    /// changed first-keys up through the parent chain, recompute every touched
    /// node's checksum, and emit `(physical, bytes)` patches for all stripe
    /// copies (DUP metadata patched in both).
    fn finalize(mut self, vol: &mut dyn RawVolume, fs: &Fs) -> Result<Vec<(u64, Vec<u8>)>> {
        #[cfg(test)]
        if FAIL_FINALIZE.with(|f| f.get()) && !self.deletes.is_empty() {
            return Err(bad("тестовый отказ сборки правок"));
        }
        // 1. Rebuild each leaf that has deletes/patches/inserts; track
        //    first-key changes.
        let leaves: HashSet<u64> = self
            .deletes
            .keys()
            .chain(self.patches.keys())
            .chain(self.inserts.keys())
            .copied()
            .collect();
        for leaf in leaves {
            let mut node = read_node(vol, &fs.chunks, leaf, self.nodesize)?;
            let old_first = {
                let n = u32at(&node, 0x60) as usize;
                (n > 0).then(|| read_key(&node, HEADER))
            };
            self.rebuild_leaf(
                &mut node,
                self.deletes.get(&leaf),
                self.patches.get(&leaf),
                self.inserts.get(&leaf),
            )?;
            let new_first = {
                let n = u32at(&node, 0x60) as usize;
                (n > 0).then(|| read_key(&node, HEADER))
            };
            // An emptied leaf is not a leaf we may leave behind. Its parent
            // still points at it, with the key of items that are now gone, and
            // btrfs reads the first key of an empty leaf as (0,0,0):
            //
            //   BTRFS error: tree first key mismatch, bytenr=611139584
            //                parent_transid=675 key expected=(199862,84,…) has=(0,0,0)
            //   BTRFS: Transaction aborted (error -5)
            //
            // Removing the leaf from its parent means merging nodes and freeing
            // the tree block — surgery this pass does not do. So refuse the
            // whole of Stage 2 instead: the caller falls back to keeping the
            // files, and a bigger image is always better than a corrupt one.
            // `spare_inodes_that_would_empty_a_leaf` normally prevents this;
            // reaching here means it missed a case, and being loud about it is
            // the point.
            if new_first.is_none() {
                // An emptied leaf may not be left behind: its parent still
                // points at it, and btrfs reads the first key of an empty leaf
                // as (0,0,0) — `tree first key mismatch`, transaction aborted.
                // Either it was planned for removal, or something predicted
                // wrong and the whole of Stage 2 is refused.
                if !self.drop_leaves.contains(&leaf) {
                    return Err(bad(tr!("btrfs_exclusions.leaf_would_empty", leaf = leaf)));
                }
                self.nodes.insert(leaf, node);
                self.unlink_child(vol, fs, leaf)?;
                continue;
            }
            self.nodes.insert(leaf, node);
            if new_first != old_first {
                if let Some(k) = new_first {
                    self.cascade_key(vol, fs, leaf, k)?;
                }
            }
        }
        // A leaf whose extent we already freed but which did NOT end up empty
        // would be a live tree block sitting in free space. Nothing salvages
        // that halfway, so the whole edit set goes.
        for leaf in &self.drop_leaves {
            let empty = self
                .nodes
                .get(leaf)
                .map(|n| u32at(n, 0x60) == 0)
                .unwrap_or(false);
            if !empty {
                return Err(bad(tr!(
                    "btrfs_exclusions.leaf_drop_not_empty",
                    leaf = leaf
                )));
            }
        }

        // 2. Recompute checksums + emit physical patches for every touched node.
        let mut out = Vec::new();
        for (logical, mut node) in std::mem::take(&mut self.nodes) {
            let c = fs.flavor.compute(&node[0x20..]);
            node[0..4].copy_from_slice(&c.to_le_bytes());
            node[4..0x20].fill(0);
            for phys in fs.chunks.physical(logical, self.nodesize)? {
                out.push((phys, node.clone()));
            }
        }
        Ok(out)
    }

    /// Rebuild `node` (a leaf): drop `deletes`, apply in-place `patches`
    /// (same-size), merge `inserts` in key order, and repack item pointers +
    /// data. Everything must fit back into the same leaf — an overflow (or a
    /// duplicate key) fails the whole edit set rather than emit a corrupt node.
    fn rebuild_leaf(
        &self,
        node: &mut [u8],
        deletes: Option<&HashSet<Key>>,
        patches: Option<&Vec<(Key, usize, Vec<u8>)>>,
        inserts: Option<&Vec<(Key, Vec<u8>)>>,
    ) -> Result<()> {
        let nritems = u32at(node, 0x60) as usize;
        let mut kept: Vec<(Key, Vec<u8>)> = Vec::new();
        for i in 0..nritems {
            let it = HEADER + i * 25;
            let key = read_key(node, it);
            let off = HEADER + u32at(node, it + 17) as usize;
            let len = u32at(node, it + 21) as usize;
            if off + len > node.len() {
                return Err(bad("leaf item data exceeds node"));
            }
            if deletes.is_some_and(|d| d.contains(&key)) {
                continue;
            }
            let mut data = node[off..off + len].to_vec();
            if let Some(ps) = patches {
                for (pk, fo, bytes) in ps {
                    if *pk == key && fo + bytes.len() <= data.len() {
                        data[*fo..*fo + bytes.len()].copy_from_slice(bytes);
                    }
                }
            }
            kept.push((key, data));
        }
        if let Some(ins) = inserts {
            for (key, data) in ins {
                kept.push((*key, data.clone()));
            }
            kept.sort_by_key(|e| e.0);
            for w in kept.windows(2) {
                if w[0].0 == w[1].0 {
                    return Err(bad(format!("duplicate key after insert: {:?}", w[0].0)));
                }
            }
        }
        let need = HEADER + kept.len() * 25 + kept.iter().map(|(_, d)| d.len()).sum::<usize>();
        if need > node.len() {
            return Err(bad(tr!(
                "btrfs_exclusions.leaf_overflow",
                need = need,
                size = node.len()
            )));
        }
        // Re-emit: header stays, item array from HEADER, data from the tail.
        for b in node[HEADER..].iter_mut() {
            *b = 0;
        }
        let mut data_end = node.len();
        for (idx, (key, data)) in kept.iter().enumerate() {
            let it = HEADER + idx * 25;
            node[it..it + 8].copy_from_slice(&key.0.to_le_bytes());
            node[it + 8] = key.1;
            node[it + 9..it + 17].copy_from_slice(&key.2.to_le_bytes());
            data_end -= data.len();
            node[it + 17..it + 21].copy_from_slice(&((data_end - HEADER) as u32).to_le_bytes());
            node[it + 21..it + 25].copy_from_slice(&(data.len() as u32).to_le_bytes());
            node[data_end..data_end + data.len()].copy_from_slice(data);
        }
        node[0x60..0x64].copy_from_slice(&(kept.len() as u32).to_le_bytes());
        Ok(())
    }

    /// May this leaf be unlinked instead of costing a spared file?
    ///
    /// Only if its parent keeps at least one child — counting the siblings
    /// already promised to other drops. A parent that would itself empty is a
    /// second unlink one level up, and that is not what this pass does; there
    /// the old answer still applies, spare a file and let the leaf live.
    fn try_drop_leaf(&mut self, vol: &mut dyn RawVolume, fs: &Fs, leaf: u64) -> Result<bool> {
        if self.never_drop.contains(&leaf) {
            return Ok(false);
        }
        if self.drop_leaves.contains(&leaf) {
            return Ok(true);
        }
        let Some(&(parent, _)) = self.parent.get(&leaf) else {
            return Ok(false); // a tree root has nothing to be unlinked from
        };
        let node = read_node(vol, &fs.chunks, parent, self.nodesize)?;
        let nritems = u32at(&node, 0x60) as usize;
        let mut alive = 0usize;
        for i in 0..nritems {
            let child = u64at(&node, HEADER + i * 33 + 17);
            if child != leaf && !self.drop_leaves.contains(&child) {
                alive += 1;
            }
        }
        if alive == 0 {
            return Ok(false);
        }
        self.drop_leaves.insert(leaf);
        Ok(true)
    }

    /// Remove an empty leaf from its parent: drop the parent's key_ptr slot,
    /// shift the rest down, and cascade if the parent's own first key moved.
    ///
    /// The parent keeping at least one child is decided before any of this, in
    /// `plan_leaf_drops` — a parent that would itself empty is not a case this
    /// handles, and reaching it means the plan was wrong.
    fn unlink_child(&mut self, vol: &mut dyn RawVolume, fs: &Fs, child_logical: u64) -> Result<()> {
        let Some(&(parent, slot)) = self.parent.get(&child_logical) else {
            return Err(bad(tr!(
                "btrfs_exclusions.unlink_no_parent",
                node = child_logical
            )));
        };
        let nodesize = self.nodesize as usize;
        let node = self.node_mut(vol, fs, parent)?;
        let nritems = u32at(node, 0x60) as usize;
        if slot >= nritems {
            return Err(bad(tr!(
                "btrfs_exclusions.slot_outside_node",
                slot = slot,
                parent = parent,
                nritems = nritems
            )));
        }
        if nritems <= 1 {
            return Err(bad(tr!(
                "btrfs_exclusions.node_would_be_childless",
                parent = parent
            )));
        }
        // key_ptr entries are 33 bytes: key(17) + blockptr(8) + generation(8).
        let from = HEADER + (slot + 1) * 33;
        let to = HEADER + slot * 33;
        let end = HEADER + nritems * 33;
        node.copy_within(from..end, to);
        node[end - 33..end].fill(0);
        node[0x60..0x64].copy_from_slice(&((nritems - 1) as u32).to_le_bytes());
        debug_assert!(end <= nodesize);
        let first = read_key(node, HEADER);
        // Everything below this slot moved up one place, so the parent map is
        // stale for those children — and a later unlink would target the wrong
        // slot. Fix it here rather than let it bite two calls from now.
        let moved: Vec<(u64, usize)> = (slot..nritems - 1)
            .map(|i| (u64at(node, HEADER + i * 33 + 17), i))
            .collect();
        for (child, i) in moved {
            self.parent.insert(child, (parent, i));
        }
        self.parent.remove(&child_logical);
        if slot == 0 {
            self.cascade_key(vol, fs, parent, first)?;
        }
        Ok(())
    }

    /// The child at `child_logical` now begins with `new_key`; update its parent
    /// internal node's key at that slot, cascading if the parent's slot 0 (its
    /// own first key) changed.
    fn cascade_key(
        &mut self,
        vol: &mut dyn RawVolume,
        fs: &Fs,
        child_logical: u64,
        new_key: Key,
    ) -> Result<()> {
        let Some(&(parent, slot)) = self.parent.get(&child_logical) else {
            if self.roots.contains(&child_logical) {
                return Ok(()); // a tree root: nothing above it holds its key
            }
            // Editing a tree whose linkage was never learned. Staying quiet
            // here is what destroyed a restored system: the checksum tree's
            // leaves changed their first key, no parent was updated, and every
            // read of those leaves died with `tree first key mismatch` — which
            // surfaced as the kernel's init taking SIGBUS on its own pages.
            return Err(bad(tr!(
                "btrfs_exclusions.key_changed_tree_unlearned",
                node = child_logical
            )));
        };
        let node = self.node_mut(vol, fs, parent)?;
        let ko = HEADER + slot * 33;
        node[ko..ko + 8].copy_from_slice(&new_key.0.to_le_bytes());
        node[ko + 8] = new_key.1;
        node[ko + 9..ko + 17].copy_from_slice(&new_key.2.to_le_bytes());
        if slot == 0 {
            self.cascade_key(vol, fs, parent, new_key)?;
        }
        Ok(())
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
    /// ino → its INLINE extents: small files whose bytes btrfs keeps inside the
    /// B-tree leaf instead of allocating an extent for them.
    ///
    /// Stage 1 zeroes *extents*, so for these files it zeroes nothing at all
    /// and their contents travel into the copy intact — 513 of them on the
    /// first real volume this was measured on. That is the one promise
    /// exclusions must not break, so the bytes are overwritten in place inside
    /// the leaf (a plain same-size item patch: no key moves, no checksum-tree
    /// work — inline data is covered by the node checksum, which the edit set
    /// recomputes anyway).
    inline: HashMap<u64, Vec<InlineExtent>>,
}

/// One inline `EXTENT_DATA` item: where it lives and how much of it is data.
#[derive(Clone, Copy)]
struct InlineExtent {
    leaf: u64,
    key: Key,
    /// btrfs_file_extent_item.compression. Zero means the bytes can simply be
    /// overwritten; anything else means the item has to be re-emitted
    /// uncompressed, or the decompressor would be handed nonsense.
    compression: u8,
    /// Bytes of file content in the item, after the 21-byte header.
    data_len: usize,
    /// The item's 21-byte header, kept for the compressed case so the item can
    /// be rebuilt without reading the tree again.
    header: Option<[u8; 21]>,
}

/// Largest inline file we will re-emit uncompressed. btrfs caps inline extents
/// at the sector size and, in practice, well below it; a bound keeps a corrupt
/// `ram_bytes` from turning into a huge item that fails the whole edit set.
const MAX_INLINE_REWRITE: usize = 8192;

const S_IFMT: u32 = 0xF000;
const S_IFDIR: u32 = 0x4000;
#[allow(dead_code)] // Stage 2 (catalog removal)
const S_IFREG: u32 = 0x8000;

fn collect_subvol(vol: &mut dyn RawVolume, fs: &Fs, root: u64) -> Result<SubvolData> {
    let mut d = SubvolData::default();
    walk_tree(vol, &fs.chunks, root, fs.nodesize, |node, nritems| {
        let leaf = u64at(node, 0x30); // header.bytenr — where an item may be patched
        for_each_item(node, nritems, |objectid, ktype, koffset, data| {
            match ktype {
                INODE_ITEM => {
                    if data.len() >= 160 {
                        let size = u64at(data, 16);
                        let nlink = u32at(data, 40) as u32;
                        let mode = u32at(data, 52) as u32;
                        d.inodes
                            .insert(objectid, (size, nlink, mode & S_IFMT == S_IFDIR));
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
                            let name =
                                String::from_utf8_lossy(&data[30..30 + name_len]).to_string();
                            let child = if loc_type == ROOT_ITEM {
                                Err(()) // subvolume link
                            } else {
                                Ok(loc_objectid)
                            };
                            d.dirents.push(DirEnt {
                                parent: objectid,
                                name,
                                child,
                            });
                        }
                    }
                    let _ = koffset;
                }
                EXTENT_DATA => {
                    // btrfs_file_extent_item; type at 20 (0 inline, 1 regular,
                    // 2 prealloc); regular: disk_bytenr@21, disk_num_bytes@29.
                    if data.len() >= 21 {
                        if data[20] == 0 {
                            d.inline.entry(objectid).or_default().push(InlineExtent {
                                leaf,
                                key: (objectid, ktype, koffset),
                                compression: data[16],
                                data_len: data.len() - 21,
                                header: data[..21].try_into().ok(),
                            });
                        } else if data.len() >= 53 {
                            let disk_bytenr = u64at(data, 21);
                            let disk_num_bytes = u64at(data, 29);
                            if disk_bytenr != 0 && disk_num_bytes != 0 {
                                d.extents.entry(objectid).or_default().push(DataRef {
                                    disk_bytenr,
                                    disk_num_bytes,
                                });
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
    paths: &mut HashMap<u64, Vec<String>>,
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
        let Some(entries) = children.get(&dir) else {
            continue;
        };
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
                        // `/proc`, `/sys`, `/dev`, `/run`, `/tmp` are emptied,
                        // never removed: systemd mounts on them before it can
                        // do anything, and a root without them panics the
                        // kernel instead of booting.
                        if is_dir && crate::exclusions::is_essential_linux_dir(&crel) {
                            warn!(dir = %e.name, "exclusions: kept an essential mount point (contents still excluded)");
                        } else {
                            matched.insert(ino);
                            paths.entry(ino).or_insert_with(|| crel.clone());
                        }
                        if is_dir {
                            stack.push((ino, crel, true));
                        }
                    } else if is_dir {
                        stack.push((ino, crel, false));
                    } else if let Some(&(size, _, _)) = d.inodes.get(&ino) {
                        scan.categories.add(&crel, size);
                    }
                }
                Err(()) => {
                    if hit {
                        warn!(name = %e.name, "btrfs exclusions: pattern matches a SUBVOLUME — kept (subvolumes are never removed)");
                        // Walked as its own tree, where this name is not on
                        // the path: nothing in it matches by it.
                        scan.left
                            .push(LeftInCopy::new(&crel, LeftReason::Subvolume));
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

/// A swap area's signature: "SWAPSPACE2" at the end of the first page.
const SWAP_MAGIC: &[u8; 10] = b"SWAPSPACE2";
/// Nothing smaller can be a swap area (mkswap wants ten pages), and this keeps
/// the probe off the thousands of small files a cache exclusion matches.
const SWAP_MIN_SIZE: u64 = 64 * 1024;

/// Is this file a swap area?
///
/// It matters because a swapfile must be treated unlike any other excluded
/// file. Its contents are worth nothing — the kernel never reads a page back
/// across a boot — but the FILE has to stay, with a valid header: `/etc/fstab`
/// names it, and a missing or blank one means
/// `Failed to activate swap swap-swapfile.swap` at every boot, with
/// `swap.target` failing behind it. So: keep the first page (that is the
/// header, and it holds no swapped data), drop everything after it, and never
/// remove the entry.
fn looks_like_swap(vol: &mut dyn RawVolume, fs: &Fs, first: &DataRef) -> Result<bool> {
    let page = 4096usize;
    if first.disk_num_bytes < page as u64 {
        return Ok(false);
    }
    let Some(&phys) = fs.chunks.physical(first.disk_bytenr, page as u64)?.first() else {
        return Ok(false);
    };
    let mut head = vec![0u8; page];
    vol.read_at(phys, &mut head)?;
    Ok(&head[page - 10..] == SWAP_MAGIC)
}

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

    walk_tree(
        vol,
        &fs.chunks,
        extent_root,
        fs.nodesize,
        |node, nritems| {
            for_each_item(node, nritems, |objectid, ktype, koffset, data| {
                match ktype {
                    EXTENT_ITEM_TYPE => {
                        let Some(&want_len) = candidates.get(&objectid) else {
                            return Ok(());
                        };
                        if data.len() < 24 {
                            return Err(bad("truncated extent item"));
                        }
                        let refs = u64at(data, 0);
                        let flags = u64at(data, 16);
                        let mut e = ExtentOwn {
                            refs,
                            matched_count: 0,
                            foreign: false,
                        };
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
        },
    )?;
    Ok(own)
}

// ---------------------------------------------------------------------------
// Checksum-tree rewrite for zeroed ranges
// ---------------------------------------------------------------------------

/// Plan ALL checksum-tree edits of a scan in one pass:
///
/// * Ranges FREED by Stage 2 **lose their entries** — the kernel's
///   `btrfs_del_csums` contract. Leaving them was the last open defect: the
///   space reads as free, the restored kernel reallocates it, inserts csums
///   for the new data, and the stale and the new items overlap —
///   `corrupt leaf: csum end range goes beyond the start range of the next
///   csum item` on the FIRST write, invisible to every static check before
///   that. An item fully covered is deleted; a partially covered one is
///   re-emitted as its surviving segments (delete + inserts into the same
///   leaf — the csum-tree guard pre-spares files so the leaf always fits).
/// * Ranges ZEROED by Stage 1 keep their entries, rewritten to the zero-sector
///   csum — the data reads back as zeros and must verify.
///
/// Both range sets are LOGICAL and finalized; they never overlap (an extent is
/// either freed or kept-and-zeroed).
fn plan_csum_edits(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    freed: &ByteRanges,
    zeroed: &ByteRanges,
    edits: &mut EditSet,
) -> Result<()> {
    let Some(&csum_root) = fs.tree_roots.get(&CSUM_TREE_OBJECTID) else {
        return Ok(()); // no csum tree (nodatasum mount) — nothing to fix
    };
    if freed.is_empty() && zeroed.is_empty() {
        return Ok(());
    }
    // Deleting/trimming csum items changes leaves' FIRST KEYS, and a leaf whose
    // first key moved must have its parent's key_ptr updated. Without this map
    // the cascade had nothing to walk and quietly did nothing: the parent kept
    // pointing at the old key, every read of that leaf failed with
    // `tree first key mismatch`, and the restored system's init died taking
    // SIGBUS on pages whose checksums lived there. The old in-place rewrite
    // never moved a key, which is why Stage 1 alone was safe.
    edits.learn_parents(vol, fs, csum_root)?;
    let zero_sector = vec![0u8; fs.sectorsize as usize];
    let zero_csum = fs.flavor.compute(&zero_sector).to_le_bytes();
    let ss = fs.sectorsize;

    // Collect first (the walk borrows vol), apply after.
    enum Op {
        /// Overwrite single entries in place — the item's shape is unchanged.
        Patch {
            leaf: u64,
            key: Key,
            entries: Vec<usize>,
        },
        /// Re-emit the item as its surviving segments (start, csum bytes).
        Reshape {
            leaf: u64,
            key: Key,
            segments: Vec<(u64, Vec<u8>)>,
        },
    }
    let mut ops: Vec<Op> = Vec::new();
    walk_tree(vol, &fs.chunks, csum_root, fs.nodesize, |node, nritems| {
        let leaf = u64at(node, 0x30); // header.bytenr
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
            if data_off + data_len > node.len() {
                return Err(bad("csum item data exceeds node"));
            }
            let sectors = (data_len / 4) as u64;
            let end = start + sectors * ss;
            let key = (EXTENT_CSUM_OBJECTID, EXTENT_CSUM, start);
            let data = &node[data_off..data_off + data_len];

            if !freed.overlaps(start, end) {
                // Only Stage-1 zero-rewrites, if any.
                let mut entries = Vec::new();
                for s in 0..sectors {
                    let a = start + s * ss;
                    if zeroed.covers(a, a + ss) {
                        entries.push(s as usize);
                    }
                }
                if !entries.is_empty() {
                    ops.push(Op::Patch { leaf, key, entries });
                }
                continue;
            }
            // Freed sectors cut the item into surviving segments.
            let mut segments: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut seg_start: Option<u64> = None;
            let slice = |from: u64, to: u64| -> Vec<u8> {
                data[((from - start) / ss * 4) as usize..((to - start) / ss * 4) as usize].to_vec()
            };
            for s in 0..sectors {
                let a = start + s * ss;
                match (freed.covers(a, a + ss), seg_start) {
                    (false, None) => seg_start = Some(a),
                    (true, Some(b)) => {
                        segments.push((b, slice(b, a)));
                        seg_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(b) = seg_start {
                segments.push((b, slice(b, end)));
            }
            // Stage-1 zeroed sectors inside surviving segments verify as zeros.
            for (b, bytes) in &mut segments {
                let n = (bytes.len() / 4) as u64;
                for s in 0..n {
                    let a = *b + s * ss;
                    if zeroed.covers(a, a + ss) {
                        bytes[(s * 4) as usize..(s * 4 + 4) as usize].copy_from_slice(&zero_csum);
                    }
                }
            }
            ops.push(Op::Reshape {
                leaf,
                key,
                segments,
            });
        }
        Ok(())
    })?;

    for op in ops {
        match op {
            Op::Patch { leaf, key, entries } => {
                for s in entries {
                    edits.patch_item(leaf, key, s * 4, zero_csum.to_vec());
                }
            }
            Op::Reshape {
                leaf,
                key,
                segments,
            } => {
                edits.delete_item(leaf, key);
                for (seg_start, bytes) in segments {
                    edits.insert_item(leaf, (EXTENT_CSUM_OBJECTID, EXTENT_CSUM, seg_start), bytes);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// One matched subvolume's collected state.
struct Subvol {
    id: u64,
    root: u64,
    data: SubvolData,
    /// All matched inodes (files + directory-subtree closure) in this subvol.
    matched: HashSet<u64>,
}

/// Walk every subvolume, match `patterns`, and remove the matched files:
/// **Stage 2** (true catalog removal) for files whose extents are cleanly
/// exclusively-owned — their B-tree items are deleted, extents freed, and all
/// accounting fixed to a `btrfs check`-clean state; **Stage 1** (zero + csum
/// rewrite) for files whose data is shared (snapshot/reflink), which are kept in
/// the catalog with only their exclusive extents zeroed. Shared data is never
/// touched (CoW safety).
pub fn scan_btrfs_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    // Remove the excluded files' entries from the B-trees (Stage 2), rather
    // than only zeroing their data (Stage 1).
    //
    // **On in the product since 2026-07-28 (этап C).** Stage 2 edits btrfs
    // metadata in place, and every failure mode found in real use now has a
    // named defence:
    //
    // * emptied leaves left referenced by their parents → three pre-flight
    //   guards (FS, extent and csum trees) spare the responsible files, and
    //   `EditSet::finalize` refuses an empty or overflowing leaf outright;
    // * spared children orphaned by removed parents →
    //   `spare_parents_of_spared_children` to a fixpoint;
    // * a freed extent keeping its csum entries — the kernel later reallocates
    //   the space, inserts its own csums, and the items overlap at the FIRST
    //   write (`corrupt leaf: csum end range goes beyond the start range of
    //   the next csum item`) → `plan_csum_edits` deletes/trims/splits the
    //   entries per the `btrfs_del_csums` contract, and the test suite walks
    //   every patched image asserting no csum covers unallocated space;
    // * DIR_ITEM hash collisions → `split_dir_item` removes only its own
    //   sub-entry.
    //
    // Any Stage-2 error still discards the WHOLE edit set and falls back to
    // Stage 1 for every file. Acceptance: the stage2_* tests (static) plus
    // `stage2_survives_mount_and_write` (dynamic, root-gated).
    remove_entries: bool,
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<BtrfsExclusionScan> {
    let mut scan = BtrfsExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }
    let fs = Fs::bootstrap(vol)?;

    // 1. Collect + match every subvolume.
    let mut subvols: Vec<Subvol> = Vec::new();
    let mut matched_pairs: HashSet<(u64, u64)> = HashSet::new();
    let mut candidates: HashMap<u64, u64> = HashMap::new();
    let mut file_extents: HashMap<(u64, u64), Vec<DataRef>> = HashMap::new();
    let mut file_sizes: HashMap<(u64, u64), u64> = HashMap::new();
    // (subvolume, inode) → the path it was matched under, for `scan.left`.
    let mut matched_paths: HashMap<(u64, u64), Vec<String>> = HashMap::new();
    for (id, root) in fs.subvol_roots() {
        let data = collect_subvol(vol, &fs, root)?;
        tick(scan.entries_scanned)?;
        let mut paths = HashMap::new();
        let matched = match_subvol(&data, &parsed, &mut scan, &mut paths);
        for (ino, path) in paths {
            matched_paths.insert((id, ino), path);
        }
        for &ino in &matched {
            let path = |p: &HashMap<(u64, u64), Vec<String>>| {
                p.get(&(id, ino)).cloned().unwrap_or_default()
            };
            let Some(&(size, nlink, is_dir)) = data.inodes.get(&ino) else {
                scan.left.push(LeftInCopy::new(
                    &path(&matched_paths),
                    LeftReason::Unreadable,
                ));
                continue;
            };
            {
                // Hard-linked files kept whole (like ext4/NTFS).
                if !is_dir && nlink > 1 {
                    scan.left
                        .push(LeftInCopy::new(&path(&matched_paths), LeftReason::HardLink));
                    continue;
                }
                matched_pairs.insert((id, ino));
                file_sizes.insert((id, ino), if is_dir { 0 } else { size });
                if let Some(exts) = data.extents.get(&ino) {
                    for e in exts {
                        candidates
                            .entry(e.disk_bytenr)
                            .and_modify(|l| *l = (*l).max(e.disk_num_bytes))
                            .or_insert(e.disk_num_bytes);
                    }
                    file_extents.insert((id, ino), exts.clone());
                }
            }
        }
        subvols.push(Subvol {
            id,
            root,
            data,
            matched,
        });
    }
    if matched_pairs.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    // 2. Extent-ownership analysis.
    let own = analyze_extents(vol, &fs, &candidates, &matched_pairs)?;
    let freeable = |bytenr: u64| -> bool {
        own.get(&bytenr)
            .map(|e| !e.foreign && e.refs > 0 && e.refs == e.matched_count)
            .unwrap_or(false)
    };

    // 3. Removability. A FILE is removable iff every data extent is cleanly
    //    freeable (or it has none). A DIRECTORY is removable iff every one of
    //    its (matched) children is removable and it links no subvolume.
    let mut removable: HashSet<(u64, u64)> = HashSet::new();
    for sv in &subvols {
        // children by parent, and subvol-link parents.
        let mut kids: HashMap<u64, Vec<std::result::Result<u64, ()>>> = HashMap::new();
        for e in &sv.data.dirents {
            kids.entry(e.parent).or_default().push(e.child);
        }
        // Files first.
        for &ino in &sv.matched {
            let is_dir = sv
                .data
                .inodes
                .get(&ino)
                .map(|&(_, _, d)| d)
                .unwrap_or(false);
            if is_dir {
                continue;
            }
            if !matched_pairs.contains(&(sv.id, ino)) {
                continue; // hard-linked, skipped above
            }
            let ok = file_extents
                .get(&(sv.id, ino))
                .map(|exts| exts.iter().all(|e| freeable(e.disk_bytenr)))
                .unwrap_or(true); // no extents (empty file) → removable
            if ok {
                removable.insert((sv.id, ino));
            }
        }
        // Directories, bottom-up to a fixpoint.
        loop {
            let mut changed = false;
            for &ino in &sv.matched {
                if removable.contains(&(sv.id, ino)) {
                    continue;
                }
                let is_dir = sv
                    .data
                    .inodes
                    .get(&ino)
                    .map(|&(_, _, d)| d)
                    .unwrap_or(false);
                if !is_dir {
                    continue;
                }
                let all_kids_ok = kids
                    .get(&ino)
                    .map(|cs| {
                        cs.iter()
                            .all(|c| matches!(c, Ok(ci) if removable.contains(&(sv.id, *ci))))
                    })
                    .unwrap_or(true); // empty dir → removable
                if all_kids_ok {
                    removable.insert((sv.id, ino));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    // 3b. Swap areas: their contents go, the file stays. Decided before the
    //     guards so the removal set they work on is already final.
    let mut swap_keep: HashMap<(u64, u64), u64> = HashMap::new(); // file → first page (logical)
    for (&who, exts) in &file_extents {
        let Some(first) = exts.first() else { continue };
        if file_sizes.get(&who).copied().unwrap_or(0) < SWAP_MIN_SIZE {
            continue;
        }
        if looks_like_swap(vol, &fs, first)? {
            removable.remove(&who);
            swap_keep.insert(who, first.disk_bytenr);
        }
    }
    if !swap_keep.is_empty() {
        info!(
            files = swap_keep.len(),
            "btrfs exclusions: своп-файлы оставлены с заголовком, содержимое обнулено"
        );
    }

    // 4. Apply Stage 2 (removable) — collects freed logical ranges.
    //
    // First take out anything whose removal would empty a tree leaf: those
    // files keep their entries and fall back to Stage 1 (data zeroed, ghost
    // left). An emptied leaf is what destroyed a restored filesystem once.
    // Both trees, until nothing more has to be spared. Sparing only ever
    // shrinks the removal set, so one tree's decision cannot un-do the other's
    // — but it can make a leaf survive that was about to empty, so the pair is
    // repeated until it settles. Two rounds in practice.
    //
    // Every round ends by restoring the invariant sparing breaks: a directory
    // may only be removed together with everything inside it. Deleting a
    // directory deletes its dir-entries, which are what *name* its children —
    // spare a child without sparing its parent and the child survives with
    // nothing pointing at it. `btrfs check` counted 608 of those in one copy:
    //
    //   unresolved ref dir 199852 index 2 name downloads … no dir item, no dir index
    // Run to a FIXPOINT. The bound is a runaway guard, not a budget: sparing
    // only ever shrinks the set, so it must converge — and if it somehow has
    // not, the set still violates "a directory goes only with everything inside
    // it", which is the invariant whose breach `btrfs check` once counted 608
    // times. Stopping quietly after N rounds would ship exactly that.
    // The parent linkage of every tree we might unlink a leaf from has to be
    // known BEFORE the guards run: "may this leaf go?" is a question about its
    // parent.
    let mut edits = EditSet::new(fs.nodesize);
    if remove_entries {
        for sv in &subvols {
            edits.learn_parents(vol, &fs, sv.root)?;
        }
        if let Some(&extent_root) = fs.tree_roots.get(&crate::btrfs_map::EXTENT_TREE_OBJECTID) {
            edits.learn_parents(vol, &fs, extent_root)?;
        }
        if let Some(&csum_root) = fs.tree_roots.get(&CSUM_TREE_OBJECTID) {
            edits.learn_parents(vol, &fs, csum_root)?;
        }
    }

    const MAX_ROUNDS: u32 = 64;
    let mut spared = 0u64;
    let mut settled = false;
    for _ in 0..MAX_ROUNDS {
        // Rebuilt from scratch, because the set it is derived from keeps
        // moving: `spare_parents_of_spared_children` runs LAST and puts files
        // back, after which a leaf marked for removal may no longer empty. A
        // stale mark meant its tree block was freed for a leaf that still had
        // items — caught by `finalize`, which then refused the whole of
        // Stage 2 and let every excluded file into the copy (image 6.8 → 9.4
        // GB). Verdicts that cannot change — `never_drop` — are kept.
        edits.drop_leaves.clear();
        let a =
            spare_inodes_that_would_empty_a_leaf(vol, &fs, &subvols, &mut removable, &mut edits)?;
        let b = spare_files_whose_extents_would_empty_a_leaf(
            vol,
            &fs,
            &mut removable,
            &file_extents,
            &mut edits,
        )?;
        let c = spare_files_whose_csum_edits_break_a_leaf(
            vol,
            &fs,
            &mut removable,
            &file_extents,
            &mut edits,
        )?;
        let d = spare_parents_of_spared_children(&subvols, &mut removable);
        let e = undrop_leaves_we_cannot_free(vol, &fs, &mut edits)?;
        spared += a + b + c + d;
        if a + b + c + d + e == 0 {
            settled = true;
            break;
        }
    }
    if !settled {
        return Err(bad(tr!(
            "btrfs_exclusions.removal_set_not_settled",
            rounds = MAX_ROUNDS
        )));
    }
    if spared > 0 {
        info!(
            spared,
            "btrfs exclusions: файлы оставлены в дереве ради целостности листов"
        );
    }
    let mut freed_logical = ByteRanges::new();
    // Stage 2 removes entries; Stage 1 only zeroes data. They are separate
    // promises, and the weaker one must survive the stronger one failing —
    // including the walk's file-type breakdown, which belongs to neither and
    // was lost with the whole scan the first time this could fail.
    let mut removed_bytes = 0u64;
    let mut freed_meta = 0u64;
    let stage2 = if remove_entries {
        apply_stage2(
            vol,
            &fs,
            &subvols,
            &removable,
            &file_extents,
            &file_sizes,
            &mut edits,
            &mut freed_logical,
            &mut removed_bytes,
            &mut freed_meta,
        )
    } else {
        // Stage 1 only: nothing is removed, so nothing falls back — every
        // matched file keeps its entry and loses its data below.
        removable.clear();
        Ok(())
    };
    match stage2 {
        Ok(()) => scan.bytes_excluded += removed_bytes,
        Err(e) => {
            // Everything Stage 2 touched is thrown away — a half-applied edit
            // set is exactly how a filesystem ends up with a dangling node.
            // Every matched file then falls back to Stage 1 below.
            warn!(
                error = %e,
                "btrfs exclusions: удаление записей отменено — файлы остаются в дереве, \
                 их данные всё равно обнуляются"
            );
            edits = EditSet::new(fs.nodesize);
            freed_logical = ByteRanges::new();
            freed_meta = 0;
            removable.clear();
        }
    }

    // 5. Stage 1 for matched-but-NOT-removable files: zero their exclusively
    //    owned extents (shared extents stay), rewrite those csums.
    let mut zeroed_logical = ByteRanges::new();
    for (&(sid, ino), exts) in &file_extents {
        if removable.contains(&(sid, ino)) {
            continue; // handled by Stage 2
        }
        // Only exclusively-owned extents may be zeroed.
        let excl: Vec<&DataRef> = exts.iter().filter(|e| freeable(e.disk_bytenr)).collect();
        if excl.len() != exts.len() {
            scan.files_kept_shared += 1;
            scan.left.push(LeftInCopy::new(
                matched_paths.get(&(sid, ino)).map_or(&[][..], |p| p),
                LeftReason::Shared,
            ));
        }
        let header = swap_keep.get(&(sid, ino)).copied();
        for e in excl {
            let mut from = e.disk_bytenr;
            // The swap header stays: everything after it is dead weight, the
            // header itself is what lets the restored system swapon at all.
            if header == Some(e.disk_bytenr) {
                from += 4096;
            }
            if from < e.disk_bytenr + e.disk_num_bytes {
                zeroed_logical.add(from, e.disk_bytenr + e.disk_num_bytes);
            }
        }
    }
    // 5b. Inline data of matched-but-kept files: overwritten inside the leaf.
    //     Stage 1 above only touches extents, and a file small enough to live
    //     inside the tree has none — so without this its bytes reach the copy
    //     untouched, which is precisely what an exclusion promises will not
    //     happen. Same-size patch: nothing moves, only the node checksum
    //     changes, and that is recomputed for every touched node anyway.
    let mut inline_zeroed = 0u64;
    let mut inline_compressed = 0u64;
    for sv in &subvols {
        for &ino in &sv.matched {
            if removable.contains(&(sv.id, ino)) {
                continue; // Stage 2 deletes the item outright
            }
            let Some(items) = sv.data.inline.get(&ino) else {
                continue;
            };
            for it in items {
                if it.data_len == 0 && it.compression == 0 {
                    continue;
                }
                if it.compression == 0 {
                    edits.patch_item(it.leaf, it.key, 21, vec![0u8; it.data_len]);
                    inline_zeroed += it.data_len as u64;
                    continue;
                }
                // Compressed inline data cannot simply be overwritten with
                // zeros: what comes back out of the decompressor is then
                // garbage, i.e. EIO instead of an empty file. Re-emit the item
                // UNCOMPRESSED, all zeros, the length the file claims — the
                // decompressor is no longer involved. That makes the item grow
                // (zeros do not compress into nothing), so it only happens when
                // the leaf can hold it; otherwise the file keeps its data and
                // is counted, because a silent leak is the one outcome an
                // exclusion may not have.
                let kept = || {
                    LeftInCopy::new(
                        matched_paths.get(&(sv.id, ino)).map_or(&[][..], |p| p),
                        LeftReason::Inline,
                    )
                };
                let Some(hdr) = it.header else {
                    inline_compressed += 1;
                    scan.left.push(kept());
                    continue;
                };
                let ram = u64at(&hdr, 8) as usize;
                if ram > MAX_INLINE_REWRITE {
                    inline_compressed += 1;
                    scan.left.push(kept());
                    continue;
                }
                let mut data = hdr.to_vec();
                data[16] = 0; // compression = none
                data.resize(21 + ram, 0);
                edits.delete_item(it.leaf, it.key);
                edits.insert_item(it.leaf, it.key, data);
                inline_zeroed += ram as u64;
            }
        }
    }
    if inline_compressed > 0 {
        warn!(
            items = inline_compressed,
            "btrfs exclusions: сжатые inline-данные оставлены как есть — их нельзя обнулить, \
             не сломав распаковку"
        );
    }
    if inline_zeroed > 0 {
        scan.bytes_excluded += inline_zeroed;
        info!(
            bytes = inline_zeroed,
            "btrfs exclusions: inline-данные обнулены внутри листьев"
        );
    }

    zeroed_logical.finalize();
    freed_logical.finalize();
    // Data that never reaches the image is excluded whether its entry survived
    // or not; counting only Stage-2 removals reported "Исключено 0 B" for a
    // volume whose files had all been zeroed.
    scan.bytes_excluded += zeroed_logical.iter().map(|(s, e)| e - s).sum::<u64>();

    // 6. Checksum tree: freed (Stage 2) ranges LOSE their entries — the
    //    `btrfs_del_csums` contract; a stale csum over freed space overlaps
    //    whatever the restored kernel later allocates there and corrupts the
    //    tree on its first write. Zeroed (Stage 1) ranges keep entries
    //    rewritten to the zero-sector csum. Then both translate to physical
    //    for the used-map drop / zeroing.
    plan_csum_edits(vol, &fs, &freed_logical, &zeroed_logical, &mut edits)?;
    let mut all_dropped = ByteRanges::new();
    for rl in [&freed_logical, &zeroed_logical] {
        for (s, e) in rl.iter() {
            all_dropped.add(s, e);
        }
    }
    all_dropped.finalize();
    for (start, end) in all_dropped.iter() {
        // Merged ranges can span chunks — translate span by span.
        for (phys, len) in fs.chunks.physical_spans(start, end)? {
            scan.ranges.add(phys, phys + len);
        }
    }
    scan.ranges.finalize();
    // Only what Stage 2 truly freed may leave the image; ghosts keep their
    // (zeroed) blocks, because the filesystem still reads them.
    for (start, end) in freed_logical.iter() {
        for (phys, len) in fs.chunks.physical_spans(start, end)? {
            scan.droppable.add(phys, phys + len);
        }
    }
    scan.droppable.finalize();

    // 7. Superblock: decrement bytes_used, invalidate the free-space tree (the
    //    kernel rebuilds it on mount), fix csum on every mirror. ONLY when
    //    Stage 2 actually freed extents: Stage 1 changes no allocation, so
    //    bytes_used is right and the free-space tree is still valid — patching
    //    the superblocks then would be a gratuitous edit of foreign metadata.
    let mut freed_bytes: u64 = freed_logical.iter().map(|(s, e)| e - s).sum::<u64>() + freed_meta;
    // A Stage-2 failure discovered HERE — a leaf that was to be removed and is
    // not empty, a leaf that overflowed — used to take the whole scan down with
    // it, and the engine then kept every excluded file: the copy grew from 6.8
    // to 9.4 GB with "btrfs exclusions failed" in the log. The weaker promise
    // must survive the stronger one failing, so Stage 1 is rebuilt from
    // scratch: entries stay, data still goes.
    scan.patches = match edits.finalize(vol, &fs) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                "btrfs exclusions: правки Stage 2 отвергнуты на сборке — \
                 повторяю только обнулением данных"
            );
            let mut stage1 = EditSet::new(fs.nodesize);
            let mut only_zeroed = ByteRanges::new();
            for (&(sid, ino), exts) in &file_extents {
                let _ = sid;
                let _ = ino;
                for ex in exts.iter().filter(|e| freeable(e.disk_bytenr)) {
                    only_zeroed.add(ex.disk_bytenr, ex.disk_bytenr + ex.disk_num_bytes);
                }
            }
            only_zeroed.finalize();
            plan_csum_edits(vol, &fs, &ByteRanges::new(), &only_zeroed, &mut stage1)?;
            // The used-map may not drop anything now: every entry survives, so
            // the filesystem still reads those blocks — as zeros.
            scan.droppable = ByteRanges::new();
            scan.ranges = ByteRanges::new();
            for (start, end) in only_zeroed.iter() {
                for (phys, len) in fs.chunks.physical_spans(start, end)? {
                    scan.ranges.add(phys, phys + len);
                }
            }
            scan.ranges.finalize();
            scan.bytes_excluded = only_zeroed.iter().map(|(s, e)| e - s).sum();
            // The inline zeroing of step 5b went with the rejected edit set:
            // every matched file with inline data keeps it.
            for sv in &subvols {
                for &ino in &sv.matched {
                    let has_inline = sv.data.inline.get(&ino).is_some_and(|items| {
                        items
                            .iter()
                            .any(|it| it.data_len > 0 || it.compression != 0)
                    });
                    if has_inline {
                        scan.left.push(LeftInCopy::new(
                            matched_paths.get(&(sv.id, ino)).map_or(&[][..], |p| p),
                            LeftReason::Inline,
                        ));
                    }
                }
            }
            freed_bytes = 0;
            removable.clear();
            stage1.finalize(vol, &fs)?
        }
    };
    if freed_bytes > 0 {
        patch_superblocks(vol, &fs, freed_bytes, &mut scan.patches)?;
    }

    scan.files_excluded = matched_pairs.len() as u64;
    info!(
        files = scan.files_excluded,
        removed = removable.len(),
        kept_shared = scan.files_kept_shared,
        bytes = scan.bytes_excluded,
        freed_bytes,
        patches = scan.patches.len(),
        "btrfs exclusions: scan complete (Stage 1 + Stage 2)"
    );
    Ok(scan)
}

/// Take out of `removable` every file whose removal would leave a tree leaf
/// with no items at all.
///
/// This pass exists because of a real failure. Excluding `/proc`, `/sys`,
/// `/dev`, `/run` and `/var/cache` on a live system emptied whole leaves of the
/// FS tree — every item in them belonged to the excluded subtrees. The leaves
/// stayed in the tree, referenced by parents whose keys pointed at items that
/// were gone, and the restored system died on its first mount with
/// `tree first key mismatch … has=(0,0,0)`.
///
/// Removing a leaf properly means rewriting its parent, merging siblings and
/// freeing the tree block — a different and much larger job. What this does
/// instead is give up on those particular files: they keep their entries and
/// fall back to Stage 1, which zeroes the *data* and leaves a ghost. The data
/// still never leaves the machine, which is what exclusions are for.
///
/// One pass is enough: sparing a file only ever *reduces* the deletions, and
/// fewer deletions cannot empty a leaf that survived before.
fn spare_inodes_that_would_empty_a_leaf(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    subvols: &[Subvol],
    removable: &mut HashSet<(u64, u64)>,
    edits: &mut EditSet,
) -> Result<u64> {
    let mut spared = 0u64;
    for sv in subvols {
        let rem: HashSet<u64> = sv
            .matched
            .iter()
            .filter(|&&ino| removable.contains(&(sv.id, ino)))
            .copied()
            .collect();
        if rem.is_empty() {
            continue;
        }
        // Per leaf: how many items it holds, how many would go, and which
        // inodes are responsible — exactly the deletions `apply_stage2` makes.
        let mut total: HashMap<u64, u64> = HashMap::new();
        let mut doomed: HashMap<u64, u64> = HashMap::new();
        let mut blamed: HashMap<u64, HashSet<u64>> = HashMap::new();
        walk_tree(vol, &fs.chunks, sv.root, fs.nodesize, |node, nritems| {
            let leaf = u64at(node, 0x30);
            total.insert(leaf, nritems);
            for_each_item(node, nritems, |objectid, ktype, _koffset, data| {
                // A DIR_ITEM can hold several entries under one hash. It only
                // disappears if ALL of them point at removed inodes —
                // `split_dir_item` re-emits it otherwise, and predicting a
                // deletion that does not happen is how a leaf gets marked for
                // removal and then survives.
                let victim = if rem.contains(&objectid) {
                    Some(objectid)
                } else if ktype == DIR_ITEM || ktype == DIR_INDEX {
                    let (removed_len, kept) = split_dir_item(data, &rem);
                    (removed_len > 0 && kept.is_empty()).then(|| u64at(data, 0))
                } else {
                    None
                };
                if let Some(ino) = victim {
                    *doomed.entry(leaf).or_default() += 1;
                    blamed.entry(leaf).or_default().insert(ino);
                }
                Ok(())
            })
        })?;

        // Spare the LEAST that keeps the leaf alive: one surviving inode is
        // enough, and everything else in it still goes. Sparing every file
        // that touched the leaf is what turned 82% of the matched files into
        // ghosts on a real volume — 9453 of 11499.
        let mut doomed: Vec<_> = doomed.into_iter().collect();
        doomed.sort_unstable();
        let mut kept: HashSet<u64> = HashSet::new();
        for (leaf, n) in doomed {
            if total.get(&leaf).copied().unwrap_or(0) != n {
                continue; // something survives in this leaf — nothing to do
            }
            let Some(inodes) = blamed.get(&leaf) else {
                continue;
            };
            // Better than sparing anything: take the leaf out of the tree.
            if edits.try_drop_leaf(vol, fs, leaf)? {
                continue;
            }
            if inodes.iter().any(|i| kept.contains(i)) {
                continue; // a file we already spared holds this leaf open
            }
            let mut sorted: Vec<u64> = inodes.iter().copied().collect();
            sorted.sort_unstable();
            let Some(&ino) = sorted.first() else { continue };
            kept.insert(ino);
            if removable.remove(&(sv.id, ino)) {
                spared += 1;
            }
            warn!(
                subvol = sv.id,
                leaf,
                items = n,
                ino,
                "btrfs exclusions: удаление опустошило бы лист — один файл оставлен, \
                 чтобы лист выжил (его данные всё равно обнуляются)"
            );
        }
    }
    Ok(spared)
}

/// Restore the invariant the leaf guards break: a directory may only be removed
/// together with everything inside it.
///
/// Removing a directory removes its dir-entries, and those entries are the only
/// thing naming its children. Sparing a child while its parent goes leaves the
/// child's inode with no way to reach it — `btrfs check` calls that an
/// unresolved ref, and a filesystem full of them is not one to restore from.
///
/// Runs to a fixpoint: sparing a parent can force sparing *its* parent.
fn spare_parents_of_spared_children(
    subvols: &[Subvol],
    removable: &mut HashSet<(u64, u64)>,
) -> u64 {
    let mut spared = 0u64;
    for sv in subvols {
        loop {
            let mut changed = false;
            for d in &sv.data.dirents {
                let Ok(child) = d.child else { continue };
                // The parent goes but this child stays → the parent must stay.
                if removable.contains(&(sv.id, d.parent)) && !removable.contains(&(sv.id, child)) {
                    removable.remove(&(sv.id, d.parent));
                    spared += 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    spared
}

/// The same guard for the **extent tree**: take out of `removable` every file
/// whose extents, once freed, would leave an extent-tree leaf with no items.
///
/// Freeing an extent deletes its `EXTENT_ITEM` and every keyed backref that
/// shares its objectid. A leaf holding nothing but those — perfectly ordinary
/// when a large excluded directory's extents were allocated together — empties
/// exactly like an FS-tree leaf, with the same fatal result.
fn spare_files_whose_extents_would_empty_a_leaf(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    removable: &mut HashSet<(u64, u64)>,
    file_extents: &HashMap<(u64, u64), Vec<DataRef>>,
    edits: &mut EditSet,
) -> Result<u64> {
    // bytenr → the files that would free it.
    let mut owners: HashMap<u64, HashSet<(u64, u64)>> = HashMap::new();
    for (&who, exts) in file_extents {
        if !removable.contains(&who) {
            continue;
        }
        for e in exts {
            owners.entry(e.disk_bytenr).or_default().insert(who);
        }
    }
    if owners.is_empty() {
        return Ok(0);
    }
    let extent_root = *fs
        .tree_roots
        .get(&crate::btrfs_map::EXTENT_TREE_OBJECTID)
        .ok_or_else(|| bad("no extent tree root"))?;

    let mut total: HashMap<u64, u64> = HashMap::new();
    let mut doomed: HashMap<u64, u64> = HashMap::new();
    let mut blamed: HashMap<u64, HashSet<(u64, u64)>> = HashMap::new();
    walk_tree(
        vol,
        &fs.chunks,
        extent_root,
        fs.nodesize,
        |node, nritems| {
            let leaf = u64at(node, 0x30);
            total.insert(leaf, nritems);
            for_each_item(node, nritems, |objectid, ktype, _koffset, _data| {
                let hit = matches!(ktype, EXTENT_ITEM_TYPE | EXTENT_DATA_REF | SHARED_DATA_REF)
                    && owners.contains_key(&objectid);
                if hit {
                    *doomed.entry(leaf).or_default() += 1;
                    if let Some(who) = owners.get(&objectid) {
                        blamed.entry(leaf).or_default().extend(who.iter().copied());
                    }
                }
                Ok(())
            })
        },
    )?;

    let mut spared = 0u64;
    let mut doomed: Vec<_> = doomed.into_iter().collect();
    doomed.sort_unstable();
    let mut kept: HashSet<(u64, u64)> = HashSet::new();
    for (leaf, n) in doomed {
        if total.get(&leaf).copied().unwrap_or(0) != n {
            continue;
        }
        let Some(files) = blamed.get(&leaf) else {
            continue;
        };
        if edits.try_drop_leaf(vol, fs, leaf)? {
            continue;
        }
        if files.iter().any(|f| kept.contains(f)) {
            continue; // already held open by a file spared for another leaf
        }
        let mut sorted: Vec<(u64, u64)> = files.iter().copied().collect();
        sorted.sort_unstable();
        let Some(&who) = sorted.first() else { continue };
        kept.insert(who);
        if removable.remove(&who) {
            spared += 1;
        }
        warn!(
            leaf,
            items = n,
            "btrfs exclusions: освобождение экстентов опустошило бы лист дерева экстентов — \
             один файл оставлен"
        );
    }
    Ok(spared)
}

/// The checksum-tree counterpart of the leaf guards. Freeing extents deletes
/// or reshapes EXTENT_CSUM items (`plan_csum_edits`); a leaf that would end up
/// EMPTY, or too full to hold a split's extra item headers, takes its files
/// back to Stage 1 — same pattern as the FS- and extent-tree guards.
fn spare_files_whose_csum_edits_break_a_leaf(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    removable: &mut HashSet<(u64, u64)>,
    file_extents: &HashMap<(u64, u64), Vec<DataRef>>,
    edits: &mut EditSet,
) -> Result<u64> {
    let Some(&csum_root) = fs.tree_roots.get(&CSUM_TREE_OBJECTID) else {
        return Ok(0);
    };
    // Candidate freed ranges of currently-removable files, with their owners.
    let mut freed = ByteRanges::new();
    let mut owners: Vec<(u64, u64, (u64, u64))> = Vec::new();
    for (&who, exts) in file_extents {
        if !removable.contains(&who) {
            continue;
        }
        for e in exts {
            freed.add(e.disk_bytenr, e.disk_bytenr + e.disk_num_bytes);
            owners.push((e.disk_bytenr, e.disk_bytenr + e.disk_num_bytes, who));
        }
    }
    if owners.is_empty() {
        return Ok(0);
    }
    freed.finalize();
    let ss = fs.sectorsize;

    let mut to_spare: HashSet<(u64, u64)> = HashSet::new();
    let mut emptying: Vec<(u64, Vec<(u64, u64)>)> = Vec::new();
    walk_tree(vol, &fs.chunks, csum_root, fs.nodesize, |node, nritems| {
        let leaf = u64at(node, 0x30);
        let mut items_after = 0u64;
        let mut bytes_after = 0usize;
        let mut blamed: HashSet<(u64, u64)> = HashSet::new();
        let mut touched = false;
        for i in 0..nritems as usize {
            let it = HEADER + i * 25;
            let ktype = node[it + 8];
            let data_len = u32at(node, it + 21) as usize;
            let start = u64at(node, it + 9);
            let sectors = (data_len / 4) as u64;
            let end = start + sectors * ss;
            if ktype != EXTENT_CSUM || !freed.overlaps(start, end) {
                items_after += 1;
                bytes_after += 25 + data_len;
                continue;
            }
            touched = true;
            for &(s, e, who) in &owners {
                if s < end && e > start {
                    blamed.insert(who);
                }
            }
            // Project the item's surviving segments (mirrors plan_csum_edits).
            let mut segs = 0usize;
            let mut seg_secs = 0u64;
            let mut in_seg = false;
            for s in 0..sectors {
                let a = start + s * ss;
                if freed.covers(a, a + ss) {
                    in_seg = false;
                } else {
                    if !in_seg {
                        segs += 1;
                        in_seg = true;
                    }
                    seg_secs += 1;
                }
            }
            items_after += segs as u64;
            bytes_after += segs * 25 + (seg_secs * 4) as usize;
        }
        if touched && items_after == 0 {
            // An emptied csum leaf can be unlinked; only overflow still costs a
            // spared file. Recorded here and decided after the walk, which
            // holds `vol` for the duration.
            emptying.push((leaf, blamed.iter().copied().collect::<Vec<_>>()));
            return Ok(());
        }
        if touched && (items_after == 0 || HEADER + bytes_after > fs.nodesize as usize) {
            // One file kept back leaves a csum item in the leaf, and the leaf
            // survives; the rest of its files are still removed. This is the
            // guard that produced most of the ghosts — a whole leaf's worth of
            // files spared for the sake of one item.
            if !blamed.iter().any(|f| to_spare.contains(f)) {
                let mut sorted: Vec<(u64, u64)> = blamed.iter().copied().collect();
                sorted.sort_unstable();
                if let Some(&who) = sorted.first() {
                    warn!(
                        leaf,
                        items_after,
                        bytes_after,
                        "btrfs exclusions: правка сумм сломала бы лист дерева сумм — \
                         один файл оставлен"
                    );
                    to_spare.insert(who);
                }
            }
        }
        Ok(())
    })?;
    for (leaf, blamed) in emptying {
        if edits.try_drop_leaf(vol, fs, leaf)? {
            continue;
        }
        if blamed.iter().any(|f| to_spare.contains(f)) {
            continue;
        }
        let mut sorted = blamed;
        sorted.sort_unstable();
        if let Some(&who) = sorted.first() {
            warn!(
                leaf,
                "btrfs exclusions: лист дерева сумм опустел бы — один файл оставлен"
            );
            to_spare.insert(who);
        }
    }
    let mut spared = 0u64;
    for who in to_spare {
        if removable.remove(&who) {
            spared += 1;
        }
    }
    Ok(spared)
}

/// Take back any planned leaf removal we cannot actually pay for.
///
/// Unlinking a leaf means freeing its tree block, and that is only ours to free
/// when nothing else holds it. A btrfs snapshot shares tree blocks with the
/// subvolume it was taken from until one of them is written to, so an FS-tree
/// leaf can easily belong to two trees at once — its extent item then says
/// `refs > 1`, or carries a SHARED_BLOCK_REF. Freeing that would delete a leaf
/// out from under the snapshot.
///
/// The other refusal is the familiar one: if removing those extent items would
/// empty an extent-tree leaf, we are back where we started, so the drop is
/// cancelled and the guards spare a file next round instead.
fn undrop_leaves_we_cannot_free(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    edits: &mut EditSet,
) -> Result<u64> {
    const METADATA_ITEM: u8 = 169;
    const TREE_BLOCK_REF: u8 = 176;
    const SHARED_BLOCK_REF: u8 = 182;

    let drops: HashSet<u64> = edits.drop_leaves.clone();
    if drops.is_empty() {
        return Ok(0);
    }
    let extent_root = *fs
        .tree_roots
        .get(&crate::btrfs_map::EXTENT_TREE_OBJECTID)
        .ok_or_else(|| bad("no extent tree root"))?;

    let mut exclusive: HashMap<u64, bool> = drops.iter().map(|&b| (b, false)).collect();
    let mut total: HashMap<u64, u64> = HashMap::new();
    let mut doomed: HashMap<u64, u64> = HashMap::new();
    let mut blamed: HashMap<u64, HashSet<u64>> = HashMap::new();
    walk_tree(
        vol,
        &fs.chunks,
        extent_root,
        fs.nodesize,
        |node, nritems| {
            let eleaf = u64at(node, 0x30);
            total.insert(eleaf, nritems);
            for_each_item(node, nritems, |objectid, ktype, _koffset, data| {
                if !drops.contains(&objectid) {
                    return Ok(());
                }
                match ktype {
                    EXTENT_ITEM_TYPE | METADATA_ITEM => {
                        let refs = if data.len() >= 8 { u64at(data, 0) } else { 0 };
                        exclusive.insert(objectid, refs == 1);
                        *doomed.entry(eleaf).or_default() += 1;
                        blamed.entry(eleaf).or_default().insert(objectid);
                    }
                    TREE_BLOCK_REF => {
                        *doomed.entry(eleaf).or_default() += 1;
                        blamed.entry(eleaf).or_default().insert(objectid);
                    }
                    SHARED_BLOCK_REF => {
                        exclusive.insert(objectid, false); // lives in another tree too
                    }
                    _ => {}
                }
                Ok(())
            })
        },
    )?;

    let mut undropped = 0u64;
    for (&block, &ok) in &exclusive {
        if !ok {
            edits.never_drop.insert(block);
            if edits.drop_leaves.remove(&block) {
                undropped += 1;
            }
        }
    }
    for (eleaf, n) in doomed {
        if total.get(&eleaf).copied().unwrap_or(0) != n {
            continue;
        }
        let Some(blocks) = blamed.get(&eleaf) else {
            continue;
        };
        for b in blocks {
            edits.never_drop.insert(*b);
            if edits.drop_leaves.remove(b) {
                undropped += 1;
            }
        }
    }
    if undropped > 0 {
        info!(
            undropped,
            "btrfs exclusions: удаление листьев отменено (общий блок или лист дерева экстентов)"
        );
    }
    Ok(undropped)
}

/// Delete the removable files/dirs from the FS trees, free their extents in the
/// extent tree, and fix block-group accounting. Fills `freed_logical` with the
/// logical ranges of fully-freed extents (→ physical drop + not copied) and
/// reports the removed files' bytes.
#[allow(clippy::too_many_arguments)]
fn apply_stage2(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    subvols: &[Subvol],
    removable: &HashSet<(u64, u64)>,
    file_extents: &HashMap<(u64, u64), Vec<DataRef>>,
    file_sizes: &HashMap<(u64, u64), u64>,
    edits: &mut EditSet,
    freed_logical: &mut ByteRanges,
    // Bytes of the files actually removed. Kept out of the scan until Stage 2
    // succeeds, because a failure discards all of it.
    removed_bytes: &mut u64,
    // Bytes of tree blocks freed by unlinking emptied leaves.
    freed_meta: &mut u64,
) -> Result<()> {
    // --- FS trees: delete items + fix surviving-parent dir sizes ---
    for sv in subvols {
        let rem: HashSet<u64> = sv
            .matched
            .iter()
            .filter(|&&ino| removable.contains(&(sv.id, ino)))
            .copied()
            .collect();
        if rem.is_empty() {
            continue;
        }
        edits.learn_parents(vol, fs, sv.root)?;
        let mut isize_dec: HashMap<u64, u64> = HashMap::new();
        let mut inode_leaf: HashMap<u64, u64> = HashMap::new();
        walk_tree(vol, &fs.chunks, sv.root, fs.nodesize, |node, nritems| {
            let leaf = u64at(node, 0x30);
            for_each_item(node, nritems, |objectid, ktype, koffset, data| {
                if rem.contains(&objectid) {
                    edits.delete_item(leaf, (objectid, ktype, koffset));
                } else if ktype == DIR_ITEM || ktype == DIR_INDEX {
                    // One DIR_ITEM can hold SEVERAL entries whose names share a
                    // crc hash. Deleting the whole item because one entry
                    // matched would take an innocent name with it, and keeping
                    // it whole would leave a stale entry pointing at a removed
                    // inode. Split the sub-entries and re-emit the survivors.
                    // (btrfs dir i_size counts name_len for BOTH the DIR_ITEM
                    // and the DIR_INDEX of each entry — measured: isize ==
                    // 2·Σname_len — so both types decrement it.)
                    let (removed_len, kept_bytes) = split_dir_item(data, &rem);
                    if removed_len > 0 {
                        edits.delete_item(leaf, (objectid, ktype, koffset));
                        if !kept_bytes.is_empty() {
                            edits.insert_item(leaf, (objectid, ktype, koffset), kept_bytes);
                        }
                        *isize_dec.entry(objectid).or_default() += removed_len;
                    }
                }
                if ktype == INODE_ITEM && !rem.contains(&objectid) {
                    inode_leaf.insert(objectid, leaf);
                }
                Ok(())
            })
        })?;
        for (parent, dec) in isize_dec {
            let old = sv.data.inodes.get(&parent).map(|&(s, _, _)| s).unwrap_or(0);
            if let Some(&leaf) = inode_leaf.get(&parent) {
                edits.patch_item(
                    leaf,
                    (parent, INODE_ITEM, 0),
                    16,
                    old.saturating_sub(dec).to_le_bytes().to_vec(),
                );
            }
        }
        // Account excluded bytes for removed files.
        for &ino in &rem {
            *removed_bytes += file_sizes.get(&(sv.id, ino)).copied().unwrap_or(0);
        }
    }

    // --- Extent tree: free every removable file's extents ---
    let mut freed_extents: BTreeMap<u64, u64> = BTreeMap::new(); // bytenr → len
    for (&(sid, ino), exts) in file_extents {
        if removable.contains(&(sid, ino)) {
            for e in exts {
                freed_extents.insert(e.disk_bytenr, e.disk_num_bytes);
            }
        }
    }
    if freed_extents.is_empty() && edits.drop_leaves.is_empty() {
        return Ok(());
    }
    let extent_root = *fs
        .tree_roots
        .get(&crate::btrfs_map::EXTENT_TREE_OBJECTID)
        .ok_or_else(|| bad("no extent tree root"))?;
    edits.learn_parents(vol, fs, extent_root)?;

    // Block groups: (start,len,leaf,key,used) — to decrement `used`.
    struct Bg {
        leaf: u64,
        key: Key,
        used: u64,
        start: u64,
        len: u64,
    }
    // Tree blocks of the leaves we are unlinking: their extent items go too,
    // and their bytes come off the block group and the superblock. Proven
    // exclusively ours by `undrop_leaves_we_cannot_free` before we get here.
    let dropped: HashSet<u64> = edits.drop_leaves.clone();
    const METADATA_ITEM: u8 = 169;
    const TREE_BLOCK_REF: u8 = 176;

    let mut bgs: Vec<Bg> = Vec::new();
    walk_tree(
        vol,
        &fs.chunks,
        extent_root,
        fs.nodesize,
        |node, nritems| {
            let leaf = u64at(node, 0x30);
            for_each_item(node, nritems, |objectid, ktype, koffset, data| {
                if dropped.contains(&objectid)
                    && matches!(ktype, EXTENT_ITEM_TYPE | METADATA_ITEM | TREE_BLOCK_REF)
                {
                    edits.delete_item(leaf, (objectid, ktype, koffset));
                }
                match ktype {
                EXTENT_ITEM_TYPE => {
                    if freed_extents.contains_key(&objectid) {
                        edits.delete_item(leaf, (objectid, ktype, koffset));
                    }
                }
                EXTENT_DATA_REF | SHARED_DATA_REF => {
                    // Keyed backrefs of a freed extent are objectid==bytenr.
                    if freed_extents.contains_key(&objectid) {
                        edits.delete_item(leaf, (objectid, ktype, koffset));
                    }
                }
                192 /* BLOCK_GROUP_ITEM */ => {
                    if data.len() >= 8 {
                        bgs.push(Bg { leaf, key: (objectid, ktype, koffset), used: u64at(data, 0), start: objectid, len: koffset });
                    }
                }
                _ => {}
            }
                Ok(())
            })
        },
    )?;

    // Decrement each block group's `used` by the freed bytes inside it.
    let mut bg_dec: HashMap<usize, u64> = HashMap::new();
    for (&bytenr, &len) in &freed_extents {
        let bi = bgs
            .iter()
            .position(|b| bytenr >= b.start && bytenr + len <= b.start + b.len)
            .ok_or_else(|| bad(format!("freed extent {bytenr} in no block group")))?;
        *bg_dec.entry(bi).or_default() += len;
        // Logical range → physical drop; account bytes.
        freed_logical.add(bytenr, bytenr + len);
    }
    // The unlinked tree blocks, charged to whichever (metadata) block group
    // holds them. Their bytes are NOT added to `freed_logical`: that set is
    // about file data — what gets zeroed, dropped from the image and stripped
    // of checksums. A freed tree block simply becomes free space, exactly as
    // when btrfs frees one itself.
    for &block in &dropped {
        let bi = bgs
            .iter()
            .position(|b| block >= b.start && block + fs.nodesize <= b.start + b.len)
            .ok_or_else(|| {
                bad(tr!(
                    "btrfs_exclusions.freed_block_outside_groups",
                    block = block
                ))
            })?;
        *bg_dec.entry(bi).or_default() += fs.nodesize;
        *freed_meta += fs.nodesize;
    }
    for (bi, dec) in bg_dec {
        let b = &bgs[bi];
        edits.patch_item(
            b.leaf,
            b.key,
            0,
            b.used.saturating_sub(dec).to_le_bytes().to_vec(),
        );
    }
    Ok(())
}

/// Split a DIR_ITEM/DIR_INDEX item's sub-entries into (Σ name_len of removed
/// entries, the survivors' bytes). Entry layout (`btrfs_dir_item`): location
/// key (17) + transid (8) + data_len (2) + name_len (2) + type (1) + name +
/// data. A DIR_INDEX holds exactly one entry; a DIR_ITEM holds one per name
/// sharing the key's crc hash.
fn split_dir_item(data: &[u8], rem: &HashSet<u64>) -> (u64, Vec<u8>) {
    let mut kept = Vec::new();
    let mut removed = 0u64;
    let mut p = 0usize;
    while p + 30 <= data.len() {
        let loc = u64at(data, p);
        let loc_type = data[p + 8];
        let data_len = u16at(data, p + 25) as usize;
        let name_len = u16at(data, p + 27) as usize;
        let entry_len = 30 + name_len + data_len;
        if p + entry_len > data.len() {
            // Malformed tail — keep it verbatim, never invent deletions.
            kept.extend_from_slice(&data[p..]);
            break;
        }
        if loc_type != ROOT_ITEM && rem.contains(&loc) {
            removed += name_len as u64;
        } else {
            kept.extend_from_slice(&data[p..p + entry_len]);
        }
        p += entry_len;
    }
    (removed, kept)
}

/// Patch all superblock mirrors: `bytes_used -= freed`, clear
/// `FREE_SPACE_TREE_VALID` (compat_ro bit 1 — the kernel rebuilds the FST on
/// mount), recompute the crc32c checksum. Superblocks live at fixed physical
/// offsets (not in the chunk map), so patches are emitted directly.
fn patch_superblocks(
    vol: &mut dyn RawVolume,
    fs: &Fs,
    freed: u64,
    patches: &mut Vec<(u64, Vec<u8>)>,
) -> Result<()> {
    for &off in &crate::btrfs_map::SB_MIRRORS {
        if off + SUPER_SIZE > vol.len() {
            continue;
        }
        let mut sb = vec![0u8; SUPER_SIZE as usize];
        vol.read_at(off, &mut sb)?;
        if &sb[0x40..0x48] != MAGIC {
            continue;
        }
        let used = u64at(&sb, 0x78).saturating_sub(freed);
        sb[0x78..0x80].copy_from_slice(&used.to_le_bytes());
        // Drop the whole free-space-tree feature (bit 0 FREE_SPACE_TREE + bit 1
        // VALID). btrfs check then validates the v1 space cache instead of the
        // now-stale FST; the kernel regenerates space info on mount. Just
        // clearing VALID is NOT enough once extents are actually freed.
        let ro = u64at(&sb, 0xB4) & !0x3;
        sb[0xB4..0xBC].copy_from_slice(&ro.to_le_bytes());
        let c = fs.flavor.compute(&sb[0x20..]);
        sb[0..4].copy_from_slice(&c.to_le_bytes());
        sb[4..0x20].fill(0);
        patches.push((off, sb));
    }
    Ok(())
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
            self.f
                .read_exact_at(buf, offset)
                .map_err(|e| bad(format!("read: {e}")))
        }
        fn len(&self) -> u64 {
            self.len
        }
    }

    fn have(cmd: &str) -> bool {
        std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok()
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
        std::fs::File::create(src.join("keep.txt"))
            .ok()?
            .write_all(&[0xCD; 65536])
            .ok()?;
        std::fs::File::create(src.join("secret.log"))
            .ok()?
            .write_all(&[0xAB; 98304])
            .ok()?;
        std::fs::File::create(src.join("cache/big.bin"))
            .ok()?
            .write_all(&[0xAB; 196608])
            .ok()?;
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
            eprintln!(
                "mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Some((img, dir))
    }

    /// `mkfs.btrfs --rootdir` over a tree `build` makes, with `extra` mkfs
    /// arguments (`--subvol …`). `None` when mkfs is missing or refuses.
    fn make_btrfs_from(
        build: impl FnOnce(&Path),
        extra: &[&str],
    ) -> Option<(PathBuf, tempfile::TempDir)> {
        if !have("mkfs.btrfs") {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).ok()?;
        build(&src);
        let img = dir.path().join("fs.img");
        let f = std::fs::File::create(&img).ok()?;
        f.set_len(128 * 1024 * 1024).ok()?;
        drop(f);
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .args(extra)
            .arg(&img)
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!(
                "mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Some((img, dir))
    }

    /// Decision 6/E4 proof on btrfs: a key file hard-linked from outside, a
    /// key directory that is a subvolume (walked as its own tree, where the
    /// name is not on the path) — both are reported, never passed as
    /// excluded. Small key files are inline: zeroed in their leaf, nothing
    /// left.
    #[test]
    fn key_directory_matches_btrfs_cannot_remove_are_reported() {
        let key = crate::seal::local::KEY_DIRECTORY_NAME;
        let Some((img, _keep)) = make_btrfs_from(
            |src| {
                std::fs::create_dir_all(src.join(key)).unwrap();
                std::fs::write(src.join(key).join("a.key"), vec![0x5A; 65536]).unwrap();
                std::fs::hard_link(src.join(key).join("a.key"), src.join("copy.bin")).unwrap();
            },
            &[],
        ) else {
            return;
        };
        let (scan, _) = run_scan(&img, &[]);
        let left = crate::exclusions::key_store_left_behind(&scan.left).expect("reported");
        assert_eq!(left.reason, LeftReason::HardLink);

        let Some((img, _keep)) = make_btrfs_from(
            |src| {
                std::fs::create_dir_all(src.join(key)).unwrap();
                std::fs::write(src.join(key).join("small.key"), vec![0x5A; 200]).unwrap();
                std::fs::write(src.join(key).join("big.key"), vec![0x5B; 65536]).unwrap();
            },
            &[],
        ) else {
            return;
        };
        let (scan, _) = run_scan(&img, &[]);
        assert!(scan.left.is_empty(), "{:?}", scan.left);
        assert_eq!(scan.files_excluded, 3, "the directory and both files");

        let Some((img, _keep)) = make_btrfs_from(
            |src| {
                std::fs::create_dir_all(src.join(key)).unwrap();
                std::fs::write(src.join(key).join("a.key"), vec![0x5A; 65536]).unwrap();
            },
            &["--subvol", key],
        ) else {
            eprintln!("mkfs.btrfs without --subvol — subvolume case skipped");
            return;
        };
        let (scan, _) = run_scan(&img, &[]);
        let left = crate::exclusions::key_store_left_behind(&scan.left).expect("reported");
        assert_eq!(left.reason, LeftReason::Subvolume);
    }

    fn run_scan(img: &Path, patterns: &[&str]) -> (BtrfsExclusionScan, FileVol) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        let scan = scan_btrfs_exclusions(&mut vol, &pats, true, |_| Ok(())).unwrap();
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

    /// Apply the scan to a copy: zero the ranges, apply all patches. Every
    /// applied image also passes the static defect-4 detector below.
    fn apply(img: &Path, scan: &BtrfsExclusionScan, out_path: &Path) {
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
        assert_csums_cover_only_allocated(out_path);
        assert_parent_keys_match(out_path);
    }

    /// Every internal node's key_ptr must equal its child's first key.
    ///
    /// btrfs verifies exactly this on every tree-block read, and a violation is
    /// not a cosmetic one: `tree first key mismatch` fails the read, which
    /// reaches userspace as EIO — as SIGBUS if the page was mapped, which is
    /// how a restored system's init died (`Attempted to kill init!
    /// exitcode=0x00000007`). The defect that caused it lived in a tree this
    /// suite never had a parent node for, so nothing noticed.
    fn assert_parent_keys_match(img: &Path) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let fs = Fs::bootstrap(&mut vol).unwrap();
        let roots: Vec<u64> = fs.tree_roots.values().copied().collect();
        for root in roots {
            let mut stack = vec![root];
            while let Some(logical) = stack.pop() {
                let node = match read_node(&mut vol, &fs.chunks, logical, fs.nodesize) {
                    Ok(n) => n,
                    Err(_) => continue, // not reachable in this image — not our claim
                };
                if node[0x64] == 0 {
                    continue; // leaf
                }
                let nritems = u32at(&node, 0x60) as usize;
                for i in 0..nritems {
                    let off = HEADER + i * 33;
                    let want = read_key(&node, off);
                    let child = u64at(&node, off + 17);
                    let cn = read_node(&mut vol, &fs.chunks, child, fs.nodesize).unwrap();
                    let cn_items = u32at(&cn, 0x60) as usize;
                    assert!(
                        cn_items > 0,
                        "node {child} is empty but {logical} points at it"
                    );
                    // Leaf items and internal key_ptrs both start with the key.
                    let got = read_key(&cn, HEADER);
                    assert_eq!(
                        want, got,
                        "parent {logical} slot {i} expects {want:?} but child {child} starts with {got:?} \
                         — `tree first key mismatch` at the next read"
                    );
                    stack.push(child);
                }
            }
        }
    }

    /// Static detector for the defect that used to be visible only at the
    /// restored system's FIRST WRITE: every checksum-tree entry must lie
    /// inside an allocated data extent. A csum over freed space overlaps
    /// whatever the kernel later allocates there —
    /// `corrupt leaf: csum end range goes beyond the start range of the next
    /// csum item` — and `btrfs check` does not flag the stale entry itself.
    fn assert_csums_cover_only_allocated(img: &Path) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let fs = Fs::bootstrap(&mut vol).unwrap();
        let extent_root = *fs
            .tree_roots
            .get(&crate::btrfs_map::EXTENT_TREE_OBJECTID)
            .unwrap();
        let mut allocated = ByteRanges::new();
        walk_tree(
            &mut vol,
            &fs.chunks,
            extent_root,
            fs.nodesize,
            |node, nritems| {
                for_each_item(node, nritems, |objectid, ktype, koffset, data| {
                    if ktype == EXTENT_ITEM_TYPE
                        && data.len() >= 24
                        && u64at(data, 16) & EXTENT_FLAG_DATA != 0
                    {
                        allocated.add(objectid, objectid + koffset);
                    }
                    Ok(())
                })
            },
        )
        .unwrap();
        allocated.finalize();
        let Some(&csum_root) = fs.tree_roots.get(&CSUM_TREE_OBJECTID) else {
            return;
        };
        walk_tree(
            &mut vol,
            &fs.chunks,
            csum_root,
            fs.nodesize,
            |node, nritems| {
                for i in 0..nritems as usize {
                    let it = HEADER + i * 25;
                    if node[it + 8] != EXTENT_CSUM {
                        continue;
                    }
                    let start = u64at(node, it + 9);
                    let sectors = (u32at(node, it + 21) / 4) as u64;
                    let end = start + sectors * fs.sectorsize;
                    assert!(
                    allocated.covers(start, end),
                    "csum entry [{start},{end}) covers unallocated space — the defect-4 time bomb"
                );
                }
                Ok(())
            },
        )
        .unwrap();
    }

    /// Private-extent files are TRULY removed (Stage 2): the image is
    /// `btrfs check`-clean afterward and the excluded files are gone, the kept
    /// file byte-identical.
    #[test]
    fn stage2_removes_private_files_and_stays_check_clean() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let (scan, _) = run_scan(&img, &["secret.log", "cache"]);
        assert!(scan.files_excluded >= 2, "excluded {}", scan.files_excluded);
        assert_eq!(scan.files_kept_shared, 0);
        assert!(!scan.patches.is_empty(), "no metadata patches");

        let patched = keep.path().join("patched.img");
        apply(&img, &scan, &patched);

        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check failed:\n{log}");

        // Restore: excluded gone, kept file intact.
        let outdir = keep.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let out = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !outdir.join("secret.log").exists(),
            "secret.log still present"
        );
        assert!(!outdir.join("cache").exists(), "cache dir still present");
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(
            kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD),
            "keep.txt damaged"
        );
    }

    /// No patch may leave a live tree node zeroed.
    ///
    /// This is the check that was missing, and its absence cost a restored
    /// system its boot. Three nodes of a real filesystem came back as 16 KiB of
    /// zeros with a *recomputed* header checksum — so `btrfs check` on the
    /// small, flat filesystem this module tests against stayed clean, block
    /// checksums matched, the post-restore comparison matched, and btrfs itself
    /// only found out when it walked into one:
    ///
    /// ```text
    /// BTRFS error: tree first key mismatch, bytenr=611139584
    ///              parent_transid=675 key expected=(199862,84,…) has=(0,0,0)
    /// ```
    ///
    /// A zeroed node with a valid checksum is not something a kernel writes, so
    /// the shape is unmistakable — and cheap to look for directly, whatever the
    /// filesystem's shape.
    #[test]
    fn no_patch_zeroes_a_live_tree_node() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let (scan, _) = run_scan(&img, &["secret.log", "cache"]);
        let patched = keep.path().join("patched.img");
        apply(&img, &scan, &patched);

        // Every 16 KiB node the patches touched, read back from the result.
        let data = std::fs::read(&patched).unwrap();
        const NODE: usize = 16384;
        for (off, bytes) in &scan.patches {
            let start = (*off as usize / NODE) * NODE;
            for node_start in (start..(*off as usize + bytes.len())).step_by(NODE) {
                let Some(node) = data.get(node_start..node_start + NODE) else {
                    continue;
                };
                // A node is "zeroed" when everything past its 32-byte checksum
                // field is zero. The checksum itself will match — that is the
                // whole trap.
                let body_is_zero = node[32..].iter().all(|&b| b == 0);
                assert!(
                    !body_is_zero,
                    "patch wrote an all-zero tree node at {node_start} — btrfs will read it as \
                     key (0,0,0) and abort the transaction"
                );
            }
        }
    }

    /// A filesystem where the excluded directory owns *whole leaves*.
    ///
    /// This is the shape that destroyed a real restore: `/proc`, `/sys`,
    /// `/dev`, `/run`, `/var/cache` on a live system are dense enough that some
    /// FS-tree leaves held nothing else, so every item in them was deleted. The
    /// leaves stayed in the tree, empty, and btrfs aborted the transaction on
    /// the first mount. A small flat filesystem — what the other tests use —
    /// never reaches that state, which is why it went unnoticed.
    ///
    /// Hundreds of files in one directory fill several 16 KiB leaves with
    /// nothing but that directory's items.
    #[test]
    fn excluding_a_dense_directory_never_empties_a_leaf() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        // Long names so each item is fat: fewer files fill a leaf.
        for i in 0..600 {
            let name = format!("cache/entry-{i:04}-{}", "x".repeat(80));
            std::fs::File::create(src.join(&name))
                .unwrap()
                .write_all(&[0xAB; 4096])
                .unwrap();
        }
        let img = dir.path().join("dense.img");
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(512 * 1024 * 1024).unwrap();
        drop(f);
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!(
                "mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        let (scan, _) = run_scan(&img, &["cache"]);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);

        // The filesystem must still be sound. Before the fix this failed with
        // "Wrong key of child node/leaf, wanted: (…), have: (0, 0, 0)".
        let (ok, log) = btrfs_check(&patched, true);
        assert!(
            ok,
            "btrfs check failed after excluding a dense directory:\n{log}"
        );

        // And the survivor is untouched.
        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(
            kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD),
            "keep.txt damaged"
        );
    }

    /// The scan's three products are independent, and the weakest must not
    /// depend on the strongest.
    ///
    /// The file-type breakdown comes from the directory walk, the zeroing from
    /// the extent analysis, the entry removal from tree surgery. When removal
    /// started refusing unsafe cases, it took the other two down with it — the
    /// copy's card reported «Linux 0 B» for a 16.8 GiB Linux volume and
    /// «Исключено 0 B» for files that had been zeroed. What the user sees must
    /// describe what happened, not what the riskiest stage managed.
    #[test]
    fn the_breakdown_and_the_zeroing_do_not_depend_on_entry_removal() {
        let Some((img, _keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let (scan, _) = run_scan(&img, &["secret.log", "cache"]);
        // The walk saw the files that stayed…
        assert!(
            scan.categories.total() > 0,
            "нет разбора по типам: {:?}",
            scan.categories
        );
        // …and the excluded ones are accounted for, however they were handled.
        assert!(scan.bytes_excluded > 0, "исключённые байты не посчитаны");
        assert!(
            !scan.ranges.is_empty(),
            "нечего обнулять — данные попали бы в копию"
        );
    }

    /// Sparing a child must spare its parent.
    ///
    /// The leaf guard takes individual files out of the removal set, and that
    /// quietly broke the rule the removal set is built on: a directory only
    /// goes together with everything inside it. Removing the parent removes the
    /// entries that *name* its children, so a spared child ends up with nothing
    /// pointing at it. On a real copy `btrfs check` counted 608 of them:
    ///
    /// ```text
    /// root 257 inode 66021 errors 2000, link count wrong
    ///   unresolved ref dir 199852 index 2 name downloads … no dir item, no dir index
    /// ```
    ///
    /// Nested dense directories are what triggers it: deep leaves fill with one
    /// subdirectory's items, get spared, while the parent stays removable.
    #[test]
    fn sparing_a_child_spares_its_parent() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        for sub in ["one", "two"] {
            std::fs::create_dir_all(src.join("cache").join(sub)).unwrap();
            for i in 0..400 {
                let name = format!("cache/{sub}/f-{i:04}-{}", "y".repeat(90));
                std::fs::File::create(src.join(&name))
                    .unwrap()
                    .write_all(&[0xAB; 4096])
                    .unwrap();
            }
        }
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("nested.img");
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(768 * 1024 * 1024).unwrap();
        drop(f);
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!(
                "mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        let (scan, _) = run_scan(&img, &["cache"]);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);

        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check found orphaned entries:\n{log}");
        assert!(
            !log.contains("unresolved ref"),
            "запись каталога удалена, а инод остался:\n{log}"
        );
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        let Some((img, _keep)) = make_btrfs() else {
            return;
        };
        let (scan, _) = run_scan(&img, &[]);
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
    }

    /// A file small enough to live INSIDE the tree must lose its contents too.
    ///
    /// btrfs keeps files under ~2 KiB inline, in the B-tree leaf, with no
    /// extent of their own. Stage 1 zeroes extents — so for these it zeroed
    /// nothing, and comparing a real copy against its source turned up 513
    /// excluded files (`~/.cache/mesa_shader_cache/**` and friends) whose
    /// bytes had travelled into the copy intact. The exclusion had removed
    /// their data everywhere except where it actually was.
    #[test]
    fn inline_data_of_an_excluded_file_is_zeroed_too() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).unwrap();
        // Small enough to be stored inline, and unmistakable in a hex dump.
        let secret = b"INLINE-SECRET-THAT-MUST-NOT-TRAVEL".repeat(20); // ~680 B
        std::fs::File::create(src.join("cache/tiny.txt"))
            .unwrap()
            .write_all(&secret)
            .unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }
        let marker = b"INLINE-SECRET-THAT-MUST-NOT-TRAVEL";
        let raw = std::fs::read(&img).unwrap();
        assert!(
            raw.windows(marker.len()).any(|w| w == marker),
            "фикстура не содержит маркер — тест ничего не проверил бы"
        );

        // Stage 1 only (remove_entries = false): the file KEEPS its entry, so
        // this is exactly the case where the data used to survive.
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let scan =
            scan_btrfs_exclusions(&mut vol, &["cache".to_string()], false, |_| Ok(())).unwrap();
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);

        let after = std::fs::read(&patched).unwrap();
        assert!(
            !after.windows(marker.len()).any(|w| w == marker),
            "содержимое исключённого inline-файла осталось в копии"
        );
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check после обнуления inline-данных:\n{log}");

        // …and the file that was not excluded is untouched.
        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(
            kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD),
            "keep.txt damaged"
        );
        let ghost = std::fs::read(outdir.join("cache/tiny.txt")).unwrap();
        assert!(ghost.iter().all(|&b| b == 0), "призрак читается не нулями");
    }

    /// A leaf that would empty is removed, so nothing is left behind at all.
    ///
    /// This number is what the user meets: a ghost is a file that still exists,
    /// still claims its old size and reads as zeros — and one such .deb is
    /// enough to stop `apt upgrade` until someone runs `apt clean`. The guards
    /// first spared every file that touched an emptying leaf (359 ghosts out of
    /// 400 here, 9453 of 11499 on a real volume), then just one per leaf (7),
    /// and now the leaf itself goes.
    #[test]
    fn a_dense_directory_leaves_no_ghosts_at_all() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).unwrap();
        // Long names so the directory fills whole leaves, and 64 KiB of data
        // each so the checksum tree fills too.
        for i in 0..400 {
            let name = format!("cache/f-{i:04}-{}", "y".repeat(80));
            std::fs::File::create(src.join(&name))
                .unwrap()
                .write_all(&[0xAB; 65536])
                .unwrap();
        }
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }

        let (scan, _) = run_scan(&img, &["cache"]);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check:\n{log}");

        // Count what actually survives in the restored tree: a ghost is a file
        // that is still there, and every one of them is an apt complaint.
        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let ghosts = std::fs::read_dir(outdir.join("cache"))
            .map(|d| d.count())
            .unwrap_or(0);
        eprintln!("ПРИЗРАКОВ: {ghosts} из 400");
        assert_eq!(
            ghosts, 0,
            "остались призраки: опустевшие листья должны удаляться, а не щадиться"
        );
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(
            kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD),
            "keep.txt damaged"
        );
    }

    /// The same, on a filesystem mounted with compression.
    ///
    /// A compressed inline extent cannot be overwritten with zeros — the
    /// decompressor would be handed nonsense and the read would fail with EIO
    /// instead of returning an empty file. The item is re-emitted uncompressed
    /// instead, so the data is gone and the file still reads.
    #[test]
    fn compressed_inline_data_is_re_emitted_uncompressed() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).unwrap();
        // Highly compressible and small: mkfs.btrfs --compress stores it as a
        // compressed inline extent.
        let secret = "COMPRESSED-INLINE-SECRET ".repeat(40); // ~1 KiB, compresses well
        std::fs::File::create(src.join("cache/tiny.txt"))
            .unwrap()
            .write_all(secret.as_bytes())
            .unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }
        // mkfs.btrfs 6.6 has no --compress, so the compressed inline extent is
        // built here: the item is re-emitted with a zstd frame and
        // compression=3. The fixture is then PROVEN valid — btrfs itself must
        // read the original text back out of it — before the scan is asked to
        // deal with it. A fixture nobody checked would make this test say
        // whatever we hoped.
        let compressed_img = dir.path().join("compressed.img");
        {
            let f = std::fs::File::open(&img).unwrap();
            let len = f.metadata().unwrap().len();
            let mut vol = FileVol { f, len };
            let fs = Fs::bootstrap(&mut vol).unwrap();
            let root = *fs.tree_roots.get(&FS_TREE_OBJECTID).unwrap();
            let mut target: Option<(u64, Key, Vec<u8>)> = None;
            walk_tree(&mut vol, &fs.chunks, root, fs.nodesize, |node, nritems| {
                let leaf = u64at(node, 0x30);
                for_each_item(node, nritems, |oid, ktype, koff, data| {
                    if ktype == EXTENT_DATA
                        && data.len() > 21
                        && data[20] == 0
                        && data[21..].starts_with(b"COMPRESSED-INLINE-SECRET")
                    {
                        target = Some((leaf, (oid, ktype, koff), data.to_vec()));
                    }
                    Ok(())
                })
            })
            .unwrap();
            let (leaf, key, item) = target.expect("inline-элемент с маркером не найден");
            let packed = zstd::bulk::compress(&item[21..], 3).unwrap();
            assert!(
                packed.len() < item.len() - 21,
                "фикстура: сжатие не уменьшило данные"
            );
            let mut new_item = item[..21].to_vec();
            new_item[16] = 3; // BTRFS_COMPRESS_ZSTD
            new_item.extend_from_slice(&packed);

            let mut edits = EditSet::new(fs.nodesize);
            edits.learn_parents(&mut vol, &fs, root).unwrap();
            edits.delete_item(leaf, key);
            edits.insert_item(leaf, key, new_item);
            let patches = edits.finalize(&mut vol, &fs).unwrap();
            let mut data = std::fs::read(&img).unwrap();
            for (off, bytes) in &patches {
                let o = *off as usize;
                data[o..o + bytes.len()].copy_from_slice(bytes);
            }
            std::fs::write(&compressed_img, &data).unwrap();
        }
        // The fixture must be a real, readable compressed inline extent.
        let (ok, log) = btrfs_check(&compressed_img, true);
        assert!(ok, "фикстура со сжатым inline невалидна:\n{log}");
        let probe = dir.path().join("probe");
        std::fs::create_dir_all(&probe).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&compressed_img)
            .arg(&probe)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore фикстуры: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let back = std::fs::read(probe.join("cache/tiny.txt")).unwrap();
        assert_eq!(
            back,
            secret.as_bytes(),
            "фикстура: btrfs не прочитал сжатые данные обратно"
        );

        let f = std::fs::File::open(&compressed_img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let scan =
            scan_btrfs_exclusions(&mut vol, &["cache".to_string()], false, |_| Ok(())).unwrap();
        let patched = dir.path().join("patched.img");
        apply(&compressed_img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check после пере-эмиссии сжатого inline:\n{log}");

        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let ghost = std::fs::read(outdir.join("cache/tiny.txt")).unwrap();
        assert!(!ghost.is_empty(), "файл должен читаться");
        assert!(
            ghost.iter().all(|&b| b == 0),
            "сжатые inline-данные не вычищены"
        );
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(
            kept.len() == 65536 && kept.iter().all(|&b| b == 0xCD),
            "keep.txt damaged"
        );
    }

    /// A Stage-1 ghost's blocks must stay in the image; only what Stage 2 truly
    /// freed may be dropped.
    ///
    /// This is the difference between an empty file and an unreadable one. A
    /// ghost keeps its entry, and btrfs keeps a checksum for it — the checksum
    /// of zeros. Drop its blocks from the used map and they are never stored,
    /// so a restore writes nothing there and the filesystem reads whatever the
    /// target disk held before: checksum failure, EIO. On a restored Kubuntu
    /// that was apt, unable to open its own excluded .deb cache:
    /// `E: Ошибка чтения - read (5: Ошибка ввода/вывода)`.
    #[test]
    fn a_stage1_ghost_keeps_its_blocks_in_the_image() {
        let Some((img, _keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        // Stage 1 only: nothing is removed, so nothing may be dropped.
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats = vec!["secret.log".to_string(), "cache".to_string()];
        let scan = scan_btrfs_exclusions(&mut vol, &pats, false, |_| Ok(())).unwrap();
        assert!(!scan.ranges.is_empty(), "ghost data is still zeroed");
        assert!(
            scan.droppable.is_empty(),
            "ghost blocks were dropped from the image — the filesystem still reads them"
        );

        // Stage 2: the freed extents may go, and never more than was zeroed.
        let (scan2, _) = run_scan(&img, &["secret.log", "cache"]);
        assert!(!scan2.droppable.is_empty(), "stage 2 frees nothing?");
        for (s, e) in scan2.droppable.iter() {
            assert!(
                scan2.ranges.covers(s, e),
                "droppable range [{s},{e}) is not among the zeroed ones"
            );
        }
    }

    /// Forensic tool, not a test of our code: point it at a real btrfs image
    /// (e.g. a partition carved out of a copy) and it applies the structural
    /// checks `btrfs check` does not — every parent key against its child's
    /// first key, every csum entry against the allocated extents.
    ///
    /// ```sh
    /// OWN_BACKUP_VERIFY_IMG=/tmp/btrfs.img cargo test -p own-backup-backup \
    ///     --lib -- --ignored verify_an_image_from_the_environment --nocapture
    /// ```
    #[test]
    #[ignore = "diagnostic: needs OWN_BACKUP_VERIFY_IMG=<path to a btrfs image>"]
    fn verify_an_image_from_the_environment() {
        let Ok(path) = std::env::var("OWN_BACKUP_VERIFY_IMG") else {
            eprintln!("OWN_BACKUP_VERIFY_IMG not set — skipping");
            return;
        };
        let img = Path::new(&path);
        assert_parent_keys_match(img);
        eprintln!("parent keys: OK");
        assert_csums_cover_only_allocated(img);
        eprintln!("csum coverage: OK");
    }

    /// Many files → a multi-level FS tree (internal nodes), so removing items
    /// exercises the first-key cascade up the parent chain. `btrfs check` must
    /// stay clean and every `.log` gone, every `.keep` intact.
    #[test]
    fn stage2_multi_leaf_tree_cascade_is_clean() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 3000 small files (mixed .log / .keep) — enough to split the FS tree.
        for i in 0..3000 {
            let ext = if i % 3 == 0 { "log" } else { "keep" };
            let mut f = std::fs::File::create(src.join(format!("f{i:05}.{ext}"))).unwrap();
            f.write_all(format!("data-{i}-").repeat(8).as_bytes())
                .unwrap();
        }
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }
        let (scan, _) = run_scan(&img, &["*.log"]);
        assert_eq!(scan.files_excluded, 1000, "expected 1000 .log excluded");

        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check failed on multi-leaf tree:\n{log}");

        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let out = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut logs = 0;
        let mut keeps = 0;
        for e in std::fs::read_dir(&outdir).unwrap().flatten() {
            let n = e.file_name().into_string().unwrap();
            if n.ends_with(".log") {
                logs += 1;
            }
            if n.ends_with(".keep") {
                keeps += 1;
            }
        }
        assert_eq!(logs, 0, "some .log files survived");
        assert_eq!(keeps, 2000, "kept files lost: {keeps}/2000");
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
        assert!(
            scan.files_kept_shared >= 1,
            "expected shared-kept accounting"
        );
    }

    /// The plan to remove a leaf must be rebuilt whenever the removal set moves.
    ///
    /// `spare_parents_of_spared_children` runs LAST in each round and puts
    /// files back, so a leaf marked for removal earlier in that same round may
    /// no longer empty. The mark used to survive into the next round anyway,
    /// its tree block was freed for a leaf that still held items, and
    /// `finalize` — correctly — refused the whole of Stage 2, letting every
    /// excluded file into the copy: the image went from 6.8 to 9.4 GB with
    /// "btrfs exclusions failed — keeping all files" in the log.
    ///
    /// A hard link whose other name is outside the excluded tree is what
    /// triggers it: the file cannot be removed, so its directory is spared
    /// after the fact.
    #[test]
    fn a_spared_parent_cancels_the_plan_to_remove_its_leaf() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).unwrap();
        // Dense enough that whole leaves fill with this directory's items.
        for i in 0..400 {
            let name = format!("cache/f-{i:04}-{}", "y".repeat(80));
            std::fs::File::create(src.join(&name))
                .unwrap()
                .write_all(&[0xAB; 8192])
                .unwrap();
        }
        // …and one file inside it that cannot go: its twin lives outside.
        std::fs::File::create(src.join("cache/hard.bin"))
            .unwrap()
            .write_all(&[0xEE; 8192])
            .unwrap();
        std::fs::hard_link(src.join("cache/hard.bin"), src.join("keep_hard.bin")).unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }

        // The scan must SUCCEED. Failing here means Stage 2 was refused, which
        // in the product means every excluded file travels into the copy.
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let scan = scan_btrfs_exclusions(&mut vol, &["cache".to_string()], true, |_| Ok(()))
            .expect("Stage 2 отменён — план удаления листьев разошёлся с реальностью");
        assert!(scan.files_excluded > 0);

        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check:\n{log}");

        // The hard link's data must survive: its other name was never excluded.
        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let twin = std::fs::read(outdir.join("keep_hard.bin")).unwrap();
        assert!(
            twin.iter().all(|&b| b == 0xEE),
            "жёсткая ссылка снаружи потеряла данные"
        );
    }

    /// When Stage 2 is refused at assembly time, Stage 1 must still happen.
    ///
    /// This is the difference between a copy without the excluded data and a
    /// copy WITH it. A Stage-2 defect discovered while assembling the patches
    /// used to fail the whole scan, and the engine's answer to a failed scan is
    /// to keep every file: the image went from 6.8 to 9.4 GB and the excluded
    /// data travelled after all. The weaker promise — the data goes, the entry
    /// stays — has to survive the stronger one failing.
    #[test]
    fn a_refused_stage2_still_zeroes_the_data() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats = vec!["secret.log".to_string(), "cache".to_string()];

        FAIL_FINALIZE.with(|f| f.set(true));
        let scan = scan_btrfs_exclusions(&mut vol, &pats, true, |_| Ok(()));
        FAIL_FINALIZE.with(|f| f.set(false));
        let scan = scan.expect("скан обязан выжить: Stage 1 не зависит от Stage 2");

        assert!(
            !scan.ranges.is_empty(),
            "данные исключённых файлов не обнулены"
        );
        assert!(
            scan.droppable.is_empty(),
            "призраки нельзя выбрасывать из образа"
        );
        assert!(scan.bytes_excluded > 0, "исключённые байты не посчитаны");

        let patched = keep.path().join("fallback.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check после отката на Stage 1:\n{log}");

        // The files are still listed — as ghosts — and read as zeros.
        let outdir = keep.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let ghost = std::fs::read(outdir.join("secret.log")).unwrap();
        assert!(ghost.iter().all(|&b| b == 0), "призрак читается не нулями");
        let kept = std::fs::read(outdir.join("keep.txt")).unwrap();
        assert!(kept.iter().all(|&b| b == 0xCD), "keep.txt повреждён");
    }

    /// A swapfile keeps its header and its entry; only its contents go.
    ///
    /// `swapfile` is in the default exclusion list, and Stage 2 duly removed
    /// it — so the restored system met `Failed to activate swap
    /// swap-swapfile.swap - /swap/swapfile` on every boot, with `swap.target`
    /// failing behind it, because `/etc/fstab` still names a file that is no
    /// longer there. Zeroing it whole is no better: `swapon` wants the
    /// signature in the first page. The contents are worth nothing (the kernel
    /// never reads a swap page back across a boot), the header is worth the
    /// boot.
    #[test]
    fn a_swapfile_keeps_its_header_and_its_place() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // A real swap area: header page ending in SWAPSPACE2, then "contents".
        let page = 4096usize;
        let pages = 64usize;
        let mut swap = vec![0u8; page * pages];
        swap[1024..1028].copy_from_slice(&1u32.to_le_bytes()); // version
        swap[1028..1032].copy_from_slice(&((pages - 1) as u32).to_le_bytes()); // last_page
        swap[page - 10..page].copy_from_slice(b"SWAPSPACE2");
        for b in swap[page..].iter_mut() {
            *b = 0x5A; // what a used swap area looks like: not zeros
        }
        std::fs::File::create(src.join("swapfile"))
            .unwrap()
            .write_all(&swap)
            .unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xCD; 65536])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }

        let (scan, _) = run_scan(&img, &["swapfile"]);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check:\n{log}");

        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let back =
            std::fs::read(outdir.join("swapfile")).expect("своп-файл удалён — fstab его не найдёт");
        assert_eq!(back.len(), page * pages, "размер своп-файла изменился");
        assert_eq!(
            &back[page - 10..page],
            SWAP_MAGIC,
            "заголовок свопа потерян — swapon откажет"
        );
        assert!(
            back[page..].iter().all(|&b| b == 0),
            "содержимое свопа уехало в копию"
        );
    }

    /// A tree block a snapshot also points at is not ours to free.
    ///
    /// Unlinking an emptied leaf means freeing its tree block, and btrfs
    /// snapshots share tree blocks with the subvolume they were taken from
    /// until one side is written to. Free such a block and the snapshot loses a
    /// leaf. The fixture has a snapshot, so exclusions across it must come out
    /// `btrfs check`-clean — including the check that no parent key was left
    /// pointing at something that is gone.
    #[test]
    fn a_leaf_shared_with_a_snapshot_is_never_freed() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/btrfs_shared.img.zst");
        if !zst.exists() {
            eprintln!("btrfs_shared.img.zst missing — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shared.img");
        std::fs::write(
            &img,
            zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap(),
        )
        .unwrap();

        // Exclude broadly, so the walk crosses the snapshot as well as the
        // subvolume it came from.
        let (scan, _) = run_scan(&img, &["orig.bin", "clone.bin", "cache", "*.bin"]);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched); // parent-key + csum-coverage asserts inside
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check на образе со снапшотом:\n{log}");
        assert!(
            !log.contains("Wrong key"),
            "ключ родителя оставлен висеть:\n{log}"
        );
    }

    /// Set `log_root` (superblock @0x60) to a non-zero value and fix the
    /// superblock checksum — the state an unclean shutdown leaves behind.
    fn dirty_log_image(img: &Path, dir: &Path) -> PathBuf {
        let mut data = std::fs::read(img).unwrap();
        let off = SUPER_OFFSET as usize;
        let sb = &mut data[off..off + SUPER_SIZE as usize];
        let stored = u32::from_le_bytes(sb[0..4].try_into().unwrap());
        let flavor = CsumFlavor::calibrate(&sb[0x20..], stored).unwrap();
        sb[0x60..0x68].copy_from_slice(&0xDEAD_0000u64.to_le_bytes());
        let c = flavor.compute(&sb[0x20..]);
        sb[0..4].copy_from_slice(&c.to_le_bytes());
        sb[4..0x20].fill(0);
        let out = dir.join("dirtylog.img");
        std::fs::write(&out, &data).unwrap();
        out
    }

    /// A non-empty fsync log tree means an unclean shutdown. Its tree blocks
    /// are not in the extent tree (a used-map would drop them from the copy)
    /// and its replay inserts items that would collide with our rewrites — both
    /// entry points must refuse, falling back to copying everything as-is.
    #[test]
    fn a_dirty_log_tree_refuses_scan_and_used_map() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let dirty = dirty_log_image(&img, keep.path());
        let f = std::fs::File::open(&dirty).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let scan = scan_btrfs_exclusions(&mut vol, &["secret.log".to_string()], true, |_| Ok(()));
        assert!(scan.is_err(), "exclusions must refuse a dirty log tree");
        let map = crate::btrfs_map::build_btrfs_used_map(&mut vol, 1 << 20);
        assert!(map.is_err(), "used-map must refuse a dirty log tree");
    }

    /// Stage 1 changes no allocation: bytes_used is right and the free-space
    /// tree is still valid, so the superblocks must stay untouched. (They used
    /// to get their FST feature bits cleared unconditionally.)
    #[test]
    fn stage1_only_leaves_the_superblocks_alone() {
        let Some((img, keep)) = make_btrfs() else {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        };
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats = vec!["secret.log".to_string(), "cache".to_string()];
        let scan = scan_btrfs_exclusions(&mut vol, &pats, false, |_| Ok(())).unwrap();
        assert!(!scan.ranges.is_empty(), "stage 1 still zeroes data");
        for (off, _) in &scan.patches {
            assert!(
                !crate::btrfs_map::SB_MIRRORS.contains(off),
                "stage 1 patched a superblock at {off}"
            );
        }
        // …and the zeroed image still checks clean, data csums included.
        let patched = keep.path().join("stage1.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check after stage-1-only:\n{log}");
    }

    /// Two names sharing one btrfs name hash (crc32c, seed ~1) live in a
    /// single DIR_ITEM. Removing one must not take the innocent other — and
    /// must not leave a stale sub-entry pointing at the removed inode either.
    #[test]
    fn stage2_keeps_the_innocent_name_in_a_hash_collision() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        // Birthday-search a colliding pair (btrfs_name_hash = crc32c(~1, name)).
        // Names must be pseudorandom and of VARYING length: crc is linear, and
        // fixed-length counter-style names never collide within millions.
        let hash = |name: &str| crc32c_raw(0xFFFF_FFFE, name.as_bytes());
        let mut seen: HashMap<u32, String> = HashMap::new();
        let mut pair: Option<(String, String)> = None;
        for i in 0u64..3_000_000 {
            let name = format!("c{:x}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 8);
            if let Some(prev) = seen.insert(hash(&name), name.clone()) {
                pair = Some((prev, name));
                break;
            }
        }
        let (victim, innocent) = pair.expect("no 32-bit hash collision in 3M names");

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::File::create(src.join(&victim))
            .unwrap()
            .write_all(&[0xAB; 8192])
            .unwrap();
        std::fs::File::create(src.join(&innocent))
            .unwrap()
            .write_all(&[0xCD; 8192])
            .unwrap();
        std::fs::File::create(src.join("keep.txt"))
            .unwrap()
            .write_all(&[0xEE; 4096])
            .unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }

        // Sanity: the pair really shares one DIR_ITEM on disk — otherwise this
        // test would silently exercise nothing.
        {
            let f = std::fs::File::open(&img).unwrap();
            let len = f.metadata().unwrap().len();
            let mut vol = FileVol { f, len };
            let fs = Fs::bootstrap(&mut vol).unwrap();
            let fs_root = *fs.tree_roots.get(&FS_TREE_OBJECTID).unwrap();
            let mut collided = false;
            walk_tree(
                &mut vol,
                &fs.chunks,
                fs_root,
                fs.nodesize,
                |node, nritems| {
                    for_each_item(node, nritems, |_oid, ktype, _koff, data| {
                        if ktype == DIR_ITEM {
                            let d = data.to_vec();
                            let has = |n: &str| d.windows(n.len()).any(|w| w == n.as_bytes());
                            if has(&victim) && has(&innocent) {
                                collided = true;
                            }
                        }
                        Ok(())
                    })
                },
            )
            .unwrap();
            assert!(
                collided,
                "{victim} and {innocent} did not share a DIR_ITEM — hash formula drifted?"
            );
        }

        let (scan, _) = run_scan(&img, &[&victim]);
        assert_eq!(scan.files_excluded, 1);
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check after collision split:\n{log}");

        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert!(
            !outdir.join(&victim).exists(),
            "victim {victim} still present"
        );
        let kept = std::fs::read(outdir.join(&innocent)).unwrap();
        assert!(
            kept.len() == 8192 && kept.iter().all(|&b| b == 0xCD),
            "innocent collision twin {innocent} damaged"
        );
    }

    /// Interleaved REAL (non-inline) extents: freeing every third file forces
    /// csum-item head/tail trims and middle SPLITS en masse — the multi-leaf
    /// test's files are inline (data inside the leaf), so this is the static
    /// acceptance for `plan_csum_edits`. Every kept file must survive
    /// byte-identical, every excluded one must be gone or a zero ghost.
    #[test]
    fn stage2_frees_interleaved_real_extents_check_clean() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let fill = |i: u32| (i % 251) as u8 + 1; // never zero
        for i in 0..600u32 {
            let ext = if i % 3 == 0 { "log" } else { "keep" };
            let mut f = std::fs::File::create(src.join(format!("f{i:04}.{ext}"))).unwrap();
            f.write_all(&vec![fill(i); 8192]).unwrap();
        }
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!("mkfs.btrfs failed — skipping");
            return;
        }

        let (scan, _) = run_scan(&img, &["*.log"]);
        assert_eq!(scan.files_excluded, 200);
        assert!(!scan.ranges.is_empty(), "real extents must be dropped");

        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched); // + csum-coverage assert inside
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check after interleaved frees:\n{log}");

        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        for i in 0..600u32 {
            let keep = i % 3 != 0;
            let name = format!("f{i:04}.{}", if keep { "keep" } else { "log" });
            let p = outdir.join(&name);
            if keep {
                let data = std::fs::read(&p).unwrap_or_else(|_| panic!("{name} lost"));
                assert!(
                    data.len() == 8192 && data.iter().all(|&b| b == fill(i)),
                    "{name} damaged"
                );
            } else if p.exists() {
                // A spared file survives only as a zero-filled ghost.
                let data = std::fs::read(&p).unwrap();
                assert!(data.iter().all(|&b| b == 0), "{name} kept non-zero data");
            }
        }
    }

    /// A filesystem big enough for a MULTI-LEVEL checksum tree — the shape
    /// every other fixture here lacks, and the reason a real copy shipped
    /// broken while all 235 tests stayed green.
    ///
    /// Freeing extents deletes and trims csum items, which moves leaves' first
    /// keys. Their parents' key_ptrs must move with them. The csum tree's
    /// linkage was never mapped, so the cascade had nothing to walk and did
    /// nothing at all; on a 130 GiB volume (many csum leaves, a real parent
    /// node) the restored system then read those leaves as
    /// `tree first key mismatch`, and its init took SIGBUS on its own pages:
    /// `Kernel panic — Attempted to kill init! exitcode=0x00000007`.
    /// Every test filesystem here has a single-leaf csum tree, where there is
    /// no parent to get it wrong.
    #[test]
    fn stage2_updates_parent_keys_in_a_multi_level_csum_tree() {
        use std::io::Write;
        if !have("mkfs.btrfs") {
            eprintln!("mkfs.btrfs unavailable — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // ~200 MiB: one csum leaf covers ~16 MiB of data, so this is a dozen
        // leaves plus the internal node that ties them together.
        for i in 0..200u32 {
            let ext = if i % 3 == 0 { "log" } else { "keep" };
            let mut buf = vec![0u8; 1024 * 1024];
            for (j, b) in buf.iter_mut().enumerate() {
                *b = (i as usize).wrapping_mul(31).wrapping_add(j) as u8;
            }
            std::fs::File::create(src.join(format!("f{i:04}.{ext}")))
                .unwrap()
                .write_all(&buf)
                .unwrap();
        }
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(1024 * 1024 * 1024)
            .unwrap();
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-r"])
            .arg(&src)
            .arg(&img)
            .output()
            .unwrap();
        if !out.status.success() {
            eprintln!(
                "mkfs.btrfs failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        // Sanity: without internal nodes in the csum tree this proves nothing.
        {
            let f = std::fs::File::open(&img).unwrap();
            let len = f.metadata().unwrap().len();
            let mut vol = FileVol { f, len };
            let fs = Fs::bootstrap(&mut vol).unwrap();
            let csum_root = *fs.tree_roots.get(&CSUM_TREE_OBJECTID).unwrap();
            let node = read_node(&mut vol, &fs.chunks, csum_root, fs.nodesize).unwrap();
            assert!(
                node[0x64] > 0,
                "csum tree is a single leaf — the fixture is too small to catch the defect"
            );
        }

        let (scan, _) = run_scan(&img, &["*.log"]);
        assert_eq!(scan.files_excluded, 67);
        assert!(!scan.ranges.is_empty());

        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched); // parent-key + csum-coverage asserts inside
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check on a multi-level csum tree:\n{log}");
        assert!(
            !log.contains("Wrong key"),
            "a parent key was left behind:\n{log}"
        );

        // The kept files must still read back byte-identical.
        let outdir = dir.path().join("restored");
        std::fs::create_dir_all(&outdir).unwrap();
        let r = std::process::Command::new("btrfs")
            .arg("restore")
            .arg(&patched)
            .arg(&outdir)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "btrfs restore failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        for i in (0..200u32).filter(|i| i % 3 != 0).take(20) {
            let data = std::fs::read(outdir.join(format!("f{i:04}.keep"))).unwrap();
            assert_eq!(data.len(), 1024 * 1024, "f{i:04}.keep truncated");
            assert_eq!(
                data[777],
                (i as usize).wrapping_mul(31).wrapping_add(777) as u8,
                "f{i:04}.keep damaged"
            );
        }
    }

    /// The dynamic acceptance no static check can replace: mount the patched
    /// image, read everything, WRITE until the freed space is reused, read
    /// again, unmount, re-check. This is exactly how the freed-extent csum
    /// defect surfaced — and the only way to prove it stays gone.
    ///
    /// The fixture is the shared corpus (`exclusion_corpus`) plus what only
    /// btrfs has: a subvolume, a snapshot of it, reflink copies, DUP metadata
    /// (`mkfs -m dup` — every metadata block written twice, so a patch that
    /// forgets the mirror is caught), and 200 MiB of big files so the checksum
    /// tree has real internal nodes. The first version of this test used 3000
    /// tiny files: its csum tree was a single leaf, so a defect that only shows
    /// when a leaf's first key moves under a parent walked straight past it and
    /// into a user's copy.
    ///
    /// Needs root (loop mount):
    ///
    /// ```sh
    /// cargo test -p own-backup-backup --lib --no-run     # build as yourself
    /// sudo <test-binary> --ignored stage2_survives_mount_and_write --nocapture
    /// ```
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn stage2_survives_mount_and_write() {
        use crate::exclusion_corpus as corpus;
        if !corpus::is_root() {
            eprintln!("not root — skipping");
            return;
        }
        if !have("mkfs.btrfs") || !have("btrfs") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(2 * 1024 * 1024 * 1024)
            .unwrap();
        // DUP metadata: every metadata block exists twice, and a patch applied
        // to one copy only leaves the mirror stale.
        let out = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-m", "dup", "-d", "single"])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        // Two names sharing one btrfs name hash live in a single DIR_ITEM: the
        // excluded one must be split out and the innocent one left whole. The
        // static test proves the split; here the kernel gets to read the result.
        let hash = |name: &str| crc32c_raw(0xFFFF_FFFE, name.as_bytes());
        let mut seen: HashMap<u32, String> = HashMap::new();
        let mut pair: Option<(String, String)> = None;
        for i in 0u64..3_000_000 {
            let name = format!("c{:x}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 8);
            if let Some(prev) = seen.insert(hash(&name), name.clone()) {
                pair = Some((prev, name));
                break;
            }
        }
        let (victim, innocent) = pair.expect("no 32-bit hash collision in 3M names");

        let (mut c, subvol_file) = {
            let _m = corpus::mounted(&img, &mnt);
            let c = corpus::populate(&mnt, corpus::Caps::btrfs());
            std::fs::write(mnt.join("keep").join(&victim), corpus::pattern(6001, 8192)).unwrap();
            std::fs::write(
                mnt.join("keep").join(&innocent),
                corpus::pattern(6002, 8192),
            )
            .unwrap();
            // A subvolume with its own files, and a snapshot of it: their trees
            // share leaves with the main one, and a shared leaf may never be
            // freed no matter what was excluded from it.
            let sub = mnt.join("keep/sub");
            let r = std::process::Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&sub)
                .output()
                .unwrap();
            assert!(
                r.status.success(),
                "subvolume create: {}",
                String::from_utf8_lossy(&r.stderr)
            );
            for i in 0..50u32 {
                std::fs::write(
                    sub.join(format!("s{i:03}.bin")),
                    corpus::pattern(5000 + i, 65536),
                )
                .unwrap();
            }
            let _ = std::process::Command::new("sync").status();
            let r = std::process::Command::new("btrfs")
                .args(["subvolume", "snapshot"])
                .arg(&sub)
                .arg(mnt.join("keep/snap"))
                .output()
                .unwrap();
            assert!(
                r.status.success(),
                "snapshot: {}",
                String::from_utf8_lossy(&r.stderr)
            );
            let _ = std::process::Command::new("sync").status();
            (c, sub.join("s000.bin"))
        };

        // Sanity: a fixture proves only what it contains — without internal
        // nodes in the csum tree, and without a mirrored metadata chunk, the
        // two things this test is here for are not present at all.
        {
            let f = std::fs::File::open(&img).unwrap();
            let len = f.metadata().unwrap().len();
            let mut vol = FileVol { f, len };
            let fs = Fs::bootstrap(&mut vol).unwrap();
            let csum_root = *fs.tree_roots.get(&CSUM_TREE_OBJECTID).unwrap();
            let node = read_node(&mut vol, &fs.chunks, csum_root, fs.nodesize).unwrap();
            assert!(
                node[0x64] > 0,
                "csum tree is a single leaf — fixture too small"
            );
            let mirrored = fs
                .chunks
                .chunks
                .iter()
                .filter(|c| c.stripes.len() > 1)
                .count();
            assert!(
                mirrored > 0,
                "mkfs проигнорировал -m dup — зеркал нет, проверять нечего"
            );
            // Every mirror must be patched: the kernel falls back to the second
            // copy when the first fails its checksum, so a stale mirror is a
            // mine that goes off long after the restore.
            let (scan, _) = run_scan(&img, &["cache"]);
            let mut offs: Vec<u64> = scan.patches.iter().map(|(o, _)| *o).collect();
            offs.sort_unstable();
            offs.dedup();
            assert_eq!(
                offs.len(),
                scan.patches.len(),
                "патчи по одному смещению дублируются"
            );
            let node_len = fs.nodesize;
            let mirrored_patches = scan
                .patches
                .iter()
                .filter(|(_, b)| b.len() as u64 == node_len)
                .filter(|(o, b)| scan.patches.iter().any(|(o2, b2)| o2 != o && b2 == b))
                .count();
            assert!(
                mirrored_patches > 0,
                "ни один узел не пропатчен в обеих копиях DUP — зеркала протухнут"
            );
        }

        // The innocent twin is a kept file like any other from here on.
        c.kept.push((
            mnt.join("keep").join(&innocent),
            corpus::pattern(6002, 8192),
        ));

        let (scan, _) = run_scan(&img, &["cache", &victim]);
        assert!(
            scan.files_excluded >= 1400,
            "исключено всего {}",
            scan.files_excluded
        );
        let patched = dir.path().join("patched.img");
        apply(&img, &scan, &patched);
        let (ok, log) = btrfs_check(&patched, true);
        assert!(ok, "btrfs check before mount:\n{log}");

        let before = corpus::dmesg_len();
        {
            let _m = corpus::mounted(&patched, &mnt);
            c.verify_kept("после восстановления");
            assert_eq!(
                std::fs::read(&subvol_file).unwrap(),
                corpus::pattern(5000, 65536),
                "файл в подтоме повреждён"
            );
            assert_eq!(
                std::fs::read(mnt.join("keep/snap/s000.bin")).unwrap(),
                corpus::pattern(5000, 65536),
                "файл в снапшоте повреждён"
            );
            assert!(
                !mnt.join("keep").join(&victim).exists(),
                "исключённое имя {victim} пережило коллизию хешей"
            );
            let written = corpus::Corpus::fill(&mnt, 900);
            assert!(written > 32, "записалось всего {written} МиБ");
            eprintln!("записано {written} МиБ поверх освобождённого");
            // Reading a kept file AFTER the writes is the point: a stale csum,
            // or a csum leaf its parent can no longer address, surfaces here.
            c.verify_kept("после заполнения");
        }

        let (ok, log) = btrfs_check(&patched, true);
        assert!(
            ok,
            "btrfs check AFTER mount-and-write — класс дефекта 4 вернулся:\n{log}"
        );
        let complaints = corpus::kernel_complaints(before, &["BTRFS error", "BTRFS critical"]);
        assert!(
            complaints.is_empty(),
            "ядро ругалось на ФС:\n{}",
            complaints.join("\n")
        );
    }
}
