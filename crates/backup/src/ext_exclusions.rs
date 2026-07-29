//! ext2/3/4 file exclusions — the Linux counterpart of [`crate::exclusions_win`]
//! (NTFS) and [`crate::fat_exclusions`] (FAT).
//!
//! Windows has no ext driver, so — exactly as [`crate::ext_map`] reads the block
//! bitmaps to build the used-map — we walk the directory tree straight from the
//! raw structures, match every entry against the job's glob patterns, and for a
//! matched file resolve its data extents. Two stages, like the NTFS path:
//!
//! * **Stage 1 (content):** the excluded file's data blocks are dropped from the
//!   used-map and their byte ranges zeroed, so the *data* never leaves the
//!   machine. Safe because ext4 data blocks are exclusively owned by one inode
//!   (no CoW / shared extents — unlike btrfs).
//! * **Stage 2 (metadata, true removal):** byte patches that make the file
//!   *disappear* rather than remain a zero-filled ghost — the directory entry is
//!   coalesced away, the inode and its blocks are freed in the bitmaps, the
//!   free counts are fixed, and every affected `metadata_csum` (crc32c) is
//!   recomputed. Applied to the copied image via the same volume-relative patch
//!   channel as FAT ([`crate::block_reader::MultiPatchReader`] / the whole-disk
//!   composite reader).
//!
//! Failure policy (as elsewhere): any parse ambiguity keeps the file in the
//! backup — excluding less is safe, corrupting the image is not.

use std::collections::{HashMap, HashSet};

use tracing::{info, warn};

use own_backup_common::{OwnBackupError, Result};

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes, ExclusionPattern};
use crate::fat_map::RawVolume;

// --- CRC-32C (Castagnoli), the raw running form the ext4 metadata_csum uses:
// the kernel's `crc32c(seed, data)` continues from `seed` with NO final xor.
// (The complemented form lives in vhdx_format; ext4 needs the raw one.)
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

const EXT_MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;

// s_feature_incompat
const INCOMPAT_64BIT: u64 = 0x80;
const INCOMPAT_META_BG: u64 = 0x10;
const INCOMPAT_RECOVER: u64 = 0x4;
const INCOMPAT_JOURNAL_DEV: u64 = 0x8;
const INCOMPAT_INLINE_DATA: u64 = 0x8000;
const INCOMPAT_CSUM_SEED: u64 = 0x2000;
// s_feature_ro_compat
const RO_COMPAT_BIGALLOC: u64 = 0x200;
const RO_COMPAT_METADATA_CSUM: u64 = 0x400;
const RO_COMPAT_GDT_CSUM: u64 = 0x10;
// s_feature_compat
const COMPAT_SPARSE_SUPER2: u64 = 0x200;

// inode i_mode top nibble
const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IFLNK: u16 = 0xA000;
// inode i_flags
const INODE_EXTENTS_FL: u32 = 0x8_0000;

/// Reserved inode past which normal files begin; `<= 10` are FS metadata inodes.
const EXT_FIRST_USER_INO: u32 = 11;
/// Root directory inode.
const ROOT_INO: u32 = 2;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("ext exclusions: {msg}"))
}

fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}

/// Result of an ext exclusion walk (mirrors [`crate::fat_exclusions::FatExclusionScan`]).
pub struct ExtExclusionScan {
    /// Excluded file data byte ranges (VOLUME-relative), zeroed in the image.
    pub ranges: ByteRanges,
    /// The subset of [`Self::ranges`] that may also be DROPPED from the used
    /// map — filled only when Stage 2 produced its patches, so the blocks are
    /// free in the bitmaps too. When Stage 2 is skipped the files stay as
    /// zero-filled ghosts, and a ghost's blocks must be STORED as zeros: drop
    /// them and a restore leaves whatever the target disk held there, inside a
    /// file the filesystem still lists.
    pub droppable: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    pub bytes_excluded: u64,
    pub categories: CategorySizes,
    /// Stage-2 metadata edits (VOLUME-relative offset → bytes, non-overlapping):
    /// coalesced directory entries, freed bitmaps, fixed counts, recomputed
    /// checksums. Empty when Stage 2 could not be produced safely.
    pub patches: Vec<(u64, Vec<u8>)>,
}

impl ExtExclusionScan {
    fn empty() -> Self {
        Self {
            ranges: ByteRanges::new(),
            droppable: ByteRanges::new(),
            files_excluded: 0,
            entries_scanned: 0,
            bytes_excluded: 0,
            categories: CategorySizes::default(),
            patches: Vec::new(),
        }
    }
}

/// Parsed superblock geometry + feature flags needed to walk inodes and to
/// recompute `metadata_csum`.
struct Sb {
    fs_block: u64,
    blocks_count: u64,
    inodes_count: u64,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    first_data_block: u32,
    desc_size: u16,
    has_hi: bool,
    metadata_csum: bool,
    gdt_csum: bool,
    /// crc32c seed for metadata_csum (s_checksum_seed or crc32c(~0, uuid)).
    csum_seed: u32,
    gdt: Vec<u8>,
    /// Byte offset of the group descriptor table on the volume.
    gdt_off: u64,
}

impl Sb {
    /// Group descriptor bytes for group `g`.
    fn desc(&self, g: u64) -> &[u8] {
        let off = (g * self.desc_size as u64) as usize;
        &self.gdt[off..off + self.desc_size as usize]
    }
    fn inode_table(&self, g: u64) -> u64 {
        let d = self.desc(g);
        u32at(d, 0x08) | if self.has_hi { u32at(d, 0x28) << 32 } else { 0 }
    }
    /// Byte offset of inode `ino` (1-based) on the volume.
    fn inode_offset(&self, ino: u32) -> u64 {
        let g = (ino as u64 - 1) / self.inodes_per_group as u64;
        let idx = (ino as u64 - 1) % self.inodes_per_group as u64;
        self.inode_table(g) * self.fs_block + idx * self.inode_size as u64
    }
}

/// Parse and validate the superblock. Refuses (→ caller keeps all files) on
/// anything unrecognized, exactly like [`crate::ext_map`].
fn parse_sb(vol: &mut dyn RawVolume) -> Result<Sb> {
    let mut head = [0u8; 2048];
    vol.read_at(0, &mut head)?;
    let sb = &head[1024..];
    if u16at(sb, 0x38) != EXT_MAGIC as u64 {
        return Err(bad("no ext superblock magic"));
    }
    let log_block_size = u32at(sb, 0x18);
    if log_block_size > 6 {
        return Err(bad("implausible s_log_block_size"));
    }
    let fs_block = 1024u64 << log_block_size;

    let compat = u32at(sb, 0x5C);
    let incompat = u32at(sb, 0x60);
    let ro_compat = u32at(sb, 0x64);
    if incompat & INCOMPAT_JOURNAL_DEV != 0 || incompat & INCOMPAT_META_BG != 0 {
        return Err(bad("journal-dev / meta_bg layout not supported"));
    }
    if incompat & INCOMPAT_RECOVER != 0 {
        // The journal holds committed transactions the on-disk structures do
        // not reflect yet; the kernel replays it on the NEXT mount. Our patches
        // would be walked from stale metadata and then partially overwritten by
        // that replay — the failure only appears when the restored system
        // boots. Same nature as the btrfs freed-extent csum defect: invisible
        // to every static check we can run.
        return Err(bad(
            "journal needs recovery (unclean shutdown) — boot Linux cleanly or run e2fsck first",
        ));
    }
    if ro_compat & RO_COMPAT_BIGALLOC != 0 || compat & COMPAT_SPARSE_SUPER2 != 0 {
        return Err(bad("bigalloc / sparse_super2 not supported"));
    }
    if incompat & INCOMPAT_INLINE_DATA != 0 {
        // inline_data stores small files/dirs inside the inode. We do not walk
        // those, so we cannot reliably match/patch them — refuse the whole scan.
        return Err(bad("inline_data feature not supported for exclusions"));
    }

    let is_64bit = incompat & INCOMPAT_64BIT != 0;
    let first_data_block = u32at(sb, 0x14) as u32;
    let blocks_per_group = u32at(sb, 0x20) as u32;
    let inodes_per_group = u32at(sb, 0x28) as u32;
    let inode_size = u16at(sb, 0x58) as u16;
    let inodes_count = u32at(sb, 0x00);
    let blocks_count = u32at(sb, 0x04) | if is_64bit { u32at(sb, 0x150) << 32 } else { 0 };

    let desc_size = if is_64bit { u16at(sb, 0xFE) as u16 } else { 32 };
    if desc_size < 32 {
        return Err(bad("descriptor size < 32"));
    }
    let has_hi = is_64bit && desc_size >= 64;

    if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 || inode_size < 128 {
        return Err(bad("zero/implausible geometry"));
    }
    if blocks_per_group as u64 > 8 * fs_block {
        return Err(bad("blocks_per_group exceeds one bitmap block"));
    }
    let fs_bytes = blocks_count
        .checked_mul(fs_block)
        .ok_or_else(|| bad("blocks_count * block_size overflows"))?;
    if fs_bytes > vol.len() {
        return Err(bad("filesystem larger than the device"));
    }

    let metadata_csum = ro_compat & RO_COMPAT_METADATA_CSUM != 0;
    if metadata_csum && ro_compat & RO_COMPAT_GDT_CSUM != 0 {
        return Err(bad("both metadata_csum and gdt_csum set"));
    }
    let csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
        u32at(sb, 0x270) as u32
    } else {
        // crc32c(~0, s_uuid[16]) — the running/raw form.
        crc32c_raw(0xFFFF_FFFF, &sb[0x68..0x68 + 16])
    };

    let num_groups = (blocks_count - first_data_block as u64).div_ceil(blocks_per_group as u64);

    let gdt_byte_len = num_groups * desc_size as u64;
    if gdt_byte_len > 256 * 1024 * 1024 {
        return Err(bad("descriptor table implausibly large"));
    }
    let gdt_blocks = gdt_byte_len.div_ceil(fs_block);
    let gdt_start = (first_data_block as u64 + 1) * fs_block;
    let mut gdt = vec![0u8; (gdt_blocks * fs_block) as usize];
    vol.read_at(gdt_start, &mut gdt)?;

    Ok(Sb {
        fs_block,
        blocks_count,
        inodes_count,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        first_data_block,
        desc_size,
        has_hi,
        metadata_csum,
        gdt_csum: ro_compat & RO_COMPAT_GDT_CSUM != 0,
        csum_seed,
        gdt,
        gdt_off: gdt_start,
    })
}

/// One physical data extent of a file: `[first_block, first_block + count)`.
#[derive(Clone, Copy)]
struct Extent {
    first: u64,
    count: u64,
}

/// Resolve a data inode's physical block extents (ext4 extent tree or ext2/3
/// indirect blocks). Bounded by the on-disk block count; returns an error on any
/// inconsistency so the caller keeps the file.
fn inode_extents(vol: &mut dyn RawVolume, sb: &Sb, inode: &[u8]) -> Result<Vec<Extent>> {
    let flags = u32at(inode, 0x20) as u32;
    let mut out = Vec::new();
    if flags & INODE_EXTENTS_FL != 0 {
        walk_extent_node(vol, sb, &inode[0x28..0x28 + 60], &mut out, 0)?;
    } else {
        indirect_blocks(vol, sb, inode, &mut out)?;
    }
    Ok(out)
}

/// Walk one extent tree node (`node` = the 60-byte inode area or a full extent
/// block). Depth-bounded against corrupted trees.
fn walk_extent_node(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    node: &[u8],
    out: &mut Vec<Extent>,
    depth_guard: u32,
) -> Result<()> {
    if depth_guard > 6 {
        return Err(bad("extent tree deeper than 6 levels"));
    }
    if u16at(node, 0x00) != EXTENT_MAGIC as u64 {
        return Err(bad("bad extent header magic"));
    }
    let entries = u16at(node, 0x02) as usize;
    let depth = u16at(node, 0x06);
    let max_entries = (node.len() - 12) / 12;
    if entries > max_entries {
        return Err(bad("extent header entries out of range"));
    }
    for i in 0..entries {
        let e = &node[12 + i * 12..12 + i * 12 + 12];
        if depth == 0 {
            // ext4_extent: ee_block(4) ee_len(2) ee_start_hi(2) ee_start_lo(4)
            let mut len = u16at(e, 0x04);
            if len > 32768 {
                len -= 32768; // uninitialized extent — blocks are still allocated
            }
            let start = u32at(e, 0x08) | (u16at(e, 0x06) << 32);
            if start == 0 || start + len > sb.blocks_count {
                return Err(bad("extent points outside the filesystem"));
            }
            out.push(Extent { first: start, count: len });
        } else {
            // ext4_extent_idx: ei_block(4) ei_leaf_lo(4) ei_leaf_hi(2) unused(2)
            let leaf = u32at(e, 0x04) | (u16at(e, 0x08) << 32);
            if leaf == 0 || leaf >= sb.blocks_count {
                return Err(bad("extent index points outside the filesystem"));
            }
            // The interior extent-tree block is allocated to this inode too, so
            // it must be zeroed and freed with the file — otherwise it stays
            // marked in use in the block bitmap after removal (e2fsck errors).
            out.push(Extent { first: leaf, count: 1 });
            let mut child = vec![0u8; sb.fs_block as usize];
            vol.read_at(leaf * sb.fs_block, &mut child)?;
            walk_extent_node(vol, sb, &child, out, depth_guard + 1)?;
        }
    }
    Ok(())
}

/// ext2/3 indirect-block map (no extents flag). Direct + single/double/triple
/// indirect pointers; a 0 pointer is a hole.
fn indirect_blocks(vol: &mut dyn RawVolume, sb: &Sb, inode: &[u8], out: &mut Vec<Extent>) -> Result<()> {
    let ptrs_per_block = (sb.fs_block / 4) as usize;
    let block = |b: &[u8], i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    let push = |out: &mut Vec<Extent>, b: u64| -> Result<()> {
        if b == 0 {
            return Ok(());
        }
        if b >= sb.blocks_count {
            return Err(bad("indirect block outside the filesystem"));
        }
        out.push(Extent { first: b, count: 1 });
        Ok(())
    };
    let iblock = &inode[0x28..0x28 + 60];
    for i in 0..12 {
        push(out, block(iblock, i))?;
    }
    // A generic recursive walk of an indirect block at `level` (1=single…).
    fn walk(vol: &mut dyn RawVolume, sb: &Sb, ptr: u64, level: u32, ppb: usize, out: &mut Vec<Extent>) -> Result<()> {
        if ptr == 0 {
            return Ok(());
        }
        if ptr >= sb.blocks_count {
            return Err(bad("indirect pointer outside the filesystem"));
        }
        // The indirect block itself belongs to the file (at every level), so it
        // is zeroed and freed along with the data it points at — otherwise it
        // stays marked in use in the block bitmap (e2fsck reports differences).
        out.push(Extent { first: ptr, count: 1 });
        let mut buf = vec![0u8; sb.fs_block as usize];
        vol.read_at(ptr * sb.fs_block, &mut buf)?;
        for i in 0..ppb {
            let b = u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
            if b == 0 {
                continue;
            }
            if level == 1 {
                if b >= sb.blocks_count {
                    return Err(bad("data block outside the filesystem"));
                }
                out.push(Extent { first: b, count: 1 });
            } else {
                walk(vol, sb, b, level - 1, ppb, out)?;
            }
        }
        Ok(())
    }
    walk(vol, sb, block(iblock, 12), 1, ptrs_per_block, out)?;
    walk(vol, sb, block(iblock, 13), 2, ptrs_per_block, out)?;
    walk(vol, sb, block(iblock, 14), 3, ptrs_per_block, out)?;
    Ok(())
}

/// Read inode `ino` into a fresh buffer (its `inode_size` bytes).
fn read_inode(vol: &mut dyn RawVolume, sb: &Sb, ino: u32) -> Result<Vec<u8>> {
    if ino == 0 || ino as u64 > sb.inodes_count {
        return Err(bad("inode number out of range"));
    }
    let mut buf = vec![0u8; sb.inode_size as usize];
    vol.read_at(sb.inode_offset(ino), &mut buf)?;
    Ok(buf)
}

/// A directory entry to remove in Stage 2 (its file's parent survives). We mark
/// it deleted by zeroing its 4-byte inode field — a valid empty slot (exactly
/// what the kernel leaves for the first entry in a block, and what e2fsck skips).
/// Independent per entry (no rec_len shifting), so several removals in one block
/// never interfere.
struct Coalesce {
    dir_block_off: u64,
    rec_off: usize,
    /// Owning directory's inode + generation — seeds the `det_checksum` of the
    /// block after the entry is zeroed (metadata_csum).
    owner_ino: u32,
    owner_gen: u32,
}

/// Everything one full walk of the tree gathers. No decisions live here — the
/// walk only records what exists and which NAMES the patterns excluded.
#[derive(Default)]
struct Collected {
    /// Raw inode bytes of every matched inode (first sighting).
    matched_inodes: HashMap<u32, Vec<u8>>,
    /// Every directory entry seen (`.`/`..` skipped).
    dirents: Vec<CDirEnt>,
    /// ino → how many of its names are excluded (≥ 1 ⇒ matched).
    hit_names: HashMap<u32, u32>,
}

/// One directory entry, with what Stage 2 needs to coalesce it away.
struct CDirEnt {
    parent: u32,
    parent_gen: u32,
    ino: u32,
    is_dir: bool,
    block_off: u64,
    rec_off: usize,
}

/// Walk the tree, decide, then emit — three phases, btrfs-style.
///
/// The one-pass predecessor decided each entry's fate at sighting time, which
/// broke whenever a decision depended on the whole tree: a hard link spared
/// AFTER its parent directory was already freed ended up a nameless inode
/// («unattached inode» — the ext incarnation of btrfs's «unresolved ref»), and
/// the essential-dir guard pushed its subtree twice with contradictory flags,
/// so the emptying it promised never happened. Collect everything first, hold
/// the invariants at a fixpoint, only then emit.
pub fn scan_ext_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<ExtExclusionScan> {
    let mut scan = ExtExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }
    let sb = parse_sb(vol)?;

    // Phase 1 — collect: one walk of the whole tree, no decisions.
    let col = collect_tree(vol, &sb, &parsed, &mut scan, &mut tick)?;
    if col.matched_inodes.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    // Phase 2 — decide: which matched inodes may ACTUALLY be removed.
    let removable = decide_removable(vol, &sb, &col);
    let spared = col.matched_inodes.len() - removable.len();
    if spared > 0 {
        info!(spared, "ext exclusions: файлы оставлены целиком (hardlink/чужой xattr-блок)");
    }

    // Phase 3 — emit: Stage-1 ranges + the Stage-2 work lists.
    let mut freed_inodes: Vec<(u32, bool)> = Vec::new();
    let mut freed_blocks: Vec<u64> = Vec::new();
    let mut coalesce: Vec<Coalesce> = Vec::new();
    // Surviving parent directories that lost a subdirectory (their link count
    // must drop by one per removed subdir).
    let mut dir_parents: Vec<u32> = Vec::new();
    for (&ino, inode) in &col.matched_inodes {
        if !removable.contains(&ino) {
            continue;
        }
        let mode = u16at(inode, 0x00) as u16 & S_IFMT;
        let is_dir = mode == S_IFDIR;
        let size = u32at(inode, 0x04) | (u32at(inode, 0x6C) << 32);
        // The xattr block goes with the inode (sole ownership proven in
        // phase 2) — left behind it would sit marked used with no owner.
        let file_acl = u32at(inode, 0x68) | (u16at(inode, 0x76) << 32);
        if file_acl != 0 {
            scan.ranges.add(file_acl * sb.fs_block, (file_acl + 1) * sb.fs_block);
            freed_blocks.push(file_acl);
        }
        // Whether i_block holds a block map at all. Only regular files,
        // directories and slow symlinks own data blocks. For a char/block
        // device i_block holds the DEVICE NUMBER, for a fast symlink the
        // target text — read either as block pointers and an unrelated block
        // gets zeroed and freed: excluding an on-disk /dev/console (5,1)
        // turned into "free block 0x0501", which on the repro image was a
        // block of the journal.
        let has_block_map = match mode {
            S_IFREG | S_IFDIR => true,
            S_IFLNK => !is_fast_symlink(&sb, inode, size),
            _ => false, // device / fifo / socket: no data blocks
        };
        if has_block_map {
            let extents = inode_extents(vol, &sb, inode)?;
            for e in &extents {
                scan.ranges.add(e.first * sb.fs_block, (e.first + e.count) * sb.fs_block);
                for b in e.first..e.first + e.count {
                    freed_blocks.push(b);
                }
            }
        }
        freed_inodes.push((ino, is_dir));
        scan.files_excluded += 1;
        if !is_dir {
            scan.bytes_excluded += size;
        }
    }
    // Dirents: an entry whose inode goes while its directory survives is
    // coalesced away (a removed directory takes all its entries with its
    // blocks); a removed subdirectory also drops the surviving parent's link
    // count — its `..` was a hard link to the parent. A removable inode has
    // ALL its names excluded (phase 2), so every surviving-parent entry of it
    // is coalesced — a fully-matched hardlink pair loses both names.
    for d in &col.dirents {
        if removable.contains(&d.ino) && !removable.contains(&d.parent) {
            coalesce.push(Coalesce {
                dir_block_off: d.block_off,
                rec_off: d.rec_off,
                owner_ino: d.parent,
                owner_gen: d.parent_gen,
            });
            if d.is_dir {
                dir_parents.push(d.parent);
            }
        }
    }

    scan.ranges.finalize();
    if scan.files_excluded == 0 {
        return Ok(scan);
    }

    // Stage 2 — build metadata patches (best-effort: on any error, drop Stage 2
    // and keep Stage 1, so excluded files remain zero-filled ghosts rather than
    // risking an inconsistent image).
    match build_stage2_patches(vol, &sb, &coalesce, &freed_inodes, &freed_blocks, &dir_parents) {
        Ok(patches) => {
            scan.patches = patches;
            // Stage 2 freed these blocks in the bitmaps, so nothing reads them
            // any more and the image need not carry them.
            for (s, e) in scan.ranges.iter() {
                scan.droppable.add(s, e);
            }
            scan.droppable.finalize();
        }
        Err(e) => warn!(error = %e, "ext exclusions: Stage-2 metadata removal skipped — keeping zero-filled ghosts"),
    }
    info!(
        files = scan.files_excluded,
        bytes = scan.bytes_excluded,
        freed_inodes = freed_inodes.len(),
        freed_blocks = freed_blocks.len(),
        patches = scan.patches.len(),
        "ext exclusions: scan complete"
    );
    Ok(scan)
}

/// Phase 1: walk the whole tree once, recording every entry and every matched
/// inode. Only NAMING decisions happen here — pattern match, subtree closure,
/// the essential-dir guard; nothing about what may actually be removed.
fn collect_tree(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    parsed: &[ExclusionPattern],
    scan: &mut ExtExclusionScan,
    tick: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Collected> {
    let mut col = Collected::default();
    // (dir inode, lowercased path components, inside an excluded subtree).
    let mut stack: Vec<(u32, Vec<String>, bool)> = vec![(ROOT_INO, Vec::new(), false)];
    let mut visited: HashSet<u32> = HashSet::new();
    let mut block = vec![0u8; sb.fs_block as usize];

    while let Some((dir_ino, rel, sub_excluded)) = stack.pop() {
        if !visited.insert(dir_ino) {
            continue;
        }
        let dir_inode = match read_inode(vol, sb, dir_ino) {
            Ok(i) => i,
            Err(e) => {
                warn!(ino = dir_ino, error = %e, "ext exclusions: unreadable directory inode — skipping subtree");
                continue;
            }
        };
        if u16at(&dir_inode, 0x00) as u16 & S_IFMT != S_IFDIR {
            continue;
        }
        let dir_gen = u32at(&dir_inode, 0x64) as u32;
        let extents = match inode_extents(vol, sb, &dir_inode) {
            Ok(e) => e,
            Err(e) => {
                warn!(ino = dir_ino, error = %e, "ext exclusions: cannot map directory blocks — skipping subtree");
                continue;
            }
        };
        for ext in &extents {
            for bi in 0..ext.count {
                let block_off = (ext.first + bi) * sb.fs_block;
                vol.read_at(block_off, &mut block)?;
                let mut off = 0usize;
                while off + 8 <= block.len() {
                    let ino = u32at(&block, off) as u32;
                    let rec_len = u16at(&block, off + 4) as usize;
                    let name_len = block[off + 6] as usize;
                    if rec_len < 8 || off + rec_len > block.len() {
                        break; // malformed / tail padding — stop this block
                    }
                    scan.entries_scanned += 1;
                    if scan.entries_scanned % 4096 == 0 {
                        tick(scan.entries_scanned)?;
                    }
                    if ino != 0 && name_len != 0 && off + 8 + name_len <= block.len() {
                        let name = &block[off + 8..off + 8 + name_len];
                        if name != b"." && name != b".." {
                            let name_str = String::from_utf8_lossy(name).to_string();
                            let name_lower = name_str.to_lowercase();
                            let mut child_rel = rel.to_vec();
                            child_rel.push(name_lower.clone());
                            let mut hit = sub_excluded
                                || parsed.iter().any(|p| p.matches(&name_lower, &child_rel));

                            // An unreadable child inode is recorded anyway: its
                            // dirent must count in phase 2, where an inode we
                            // cannot free automatically spares its parent.
                            let inode = read_inode(vol, sb, ino).ok();
                            let is_dir = inode
                                .as_deref()
                                .map(|i| u16at(i, 0x00) as u16 & S_IFMT == S_IFDIR)
                                .unwrap_or(false);
                            if hit && is_dir && crate::exclusions::is_essential_linux_dir(&child_rel) {
                                // `/proc`, `/sys`, `/dev`, `/run`, `/tmp` are
                                // emptied, never removed: systemd mounts on
                                // them before it can do anything, and a root
                                // without them panics the kernel.
                                warn!(
                                    dir = %name_str,
                                    "exclusions: kept an essential mount point (contents still excluded)"
                                );
                                hit = false;
                                stack.push((ino, child_rel.clone(), true));
                            } else if is_dir {
                                stack.push((ino, child_rel.clone(), hit));
                            }
                            col.dirents.push(CDirEnt {
                                parent: dir_ino,
                                parent_gen: dir_gen,
                                ino,
                                is_dir,
                                block_off,
                                rec_off: off,
                            });
                            if let Some(inode) = inode {
                                if hit {
                                    *col.hit_names.entry(ino).or_default() += 1;
                                    col.matched_inodes.entry(ino).or_insert(inode);
                                } else if u16at(&inode, 0x00) as u16 & S_IFMT == S_IFREG {
                                    // Kept file — size for the dashboard.
                                    let size =
                                        u32at(&inode, 0x04) | (u32at(&inode, 0x6C) << 32);
                                    scan.categories.add(&child_rel, size);
                                }
                            }
                        }
                    }
                    off += rec_len;
                }
            }
        }
    }
    Ok(col)
}

/// Phase 2: the removable set — matched inodes that may ACTUALLY be removed,
/// with the two invariants the one-pass scanner used to break:
///
/// 1. A non-directory goes only if EVERY one of its names is excluded.
///    `i_links_count` says how many names it has; the walk counted how many
///    are excluded. Freeing an inode from under a surviving name is how
///    «unattached inode» happens. (The btrfs analogue: `refs == matched_count`.)
/// 2. A directory goes only with everything inside it: a spared child spares
///    its parent, up the chain, to a fixpoint. (The btrfs analogue:
///    `spare_parents_of_spared_children`.)
///
/// A shared xattr block (refcount > 1) also spares its file — decrementing the
/// refcount is a patch this pass does not carry yet (plan P8), and excluding
/// less is always safe.
fn decide_removable(vol: &mut dyn RawVolume, sb: &Sb, col: &Collected) -> HashSet<u32> {
    let mut removable: HashSet<u32> = HashSet::new();
    for (&ino, inode) in &col.matched_inodes {
        // Never touch the reserved metadata inodes (unreachable — guard).
        if ino < EXT_FIRST_USER_INO && ino != ROOT_INO {
            continue;
        }
        let mode = u16at(inode, 0x00) as u16 & S_IFMT;
        let links = u16at(inode, 0x1A) as u32;
        // A directory's link count is structural (self + parent + subdirs) —
        // its fate hangs on its children, restored by the fixpoint below.
        if mode != S_IFDIR {
            let hits = col.hit_names.get(&ino).copied().unwrap_or(0);
            if links > hits {
                continue; // a name survives somewhere — keep the whole file
            }
        }
        let file_acl = u32at(inode, 0x68) | (u16at(inode, 0x76) << 32);
        if file_acl != 0 && !xattr_block_is_private(vol, sb, file_acl) {
            continue;
        }
        removable.insert(ino);
    }
    loop {
        let mut changed = false;
        for d in &col.dirents {
            if removable.contains(&d.parent) && !removable.contains(&d.ino) {
                removable.remove(&d.parent);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    removable
}

/// True when `file_acl` points at a valid xattr block owned by exactly one
/// inode (header magic 0xEA020000, refcount 1).
fn xattr_block_is_private(vol: &mut dyn RawVolume, sb: &Sb, file_acl: u64) -> bool {
    if file_acl >= sb.blocks_count {
        return false;
    }
    let mut hdr = [0u8; 8];
    if vol.read_at(file_acl * sb.fs_block, &mut hdr).is_err() {
        return false;
    }
    u32::from_le_bytes(hdr[0..4].try_into().unwrap()) == 0xEA02_0000
        && u32::from_le_bytes(hdr[4..8].try_into().unwrap()) == 1
}

/// e2fsprogs' `ext2fs_is_fast_symlink`: the target lives inside i_block iff the
/// inode owns no blocks besides its (optional) xattr block. `i_blocks` counts
/// 512-byte sectors, and an xattr block accounts for `fs_block/512` of them —
/// so the old `i_blocks == 0` test sent a fast symlink WITH an xattr block down
/// the block-map walker, which read the target text as block pointers.
fn is_fast_symlink(sb: &Sb, inode: &[u8], size: u64) -> bool {
    if size >= 60 {
        return false; // target too long for i_block — a real data block
    }
    let i_blocks = u32at(inode, 0x1C);
    // i_file_acl (xattr block): lo @0x68, high @0x76 (linux2.l_i_file_acl_high).
    let file_acl = u32at(inode, 0x68) | (u16at(inode, 0x76) << 32);
    let acl_sectors = if file_acl != 0 { sb.fs_block / 512 } else { 0 };
    i_blocks == acl_sectors
}

// --- Stage 2 checksum helpers (all metadata_csum = crc32c) ----------------

/// Group-descriptor checksum (`bg_checksum`, metadata_csum): crc32c over the
/// descriptor with the 2-byte checksum field treated as zero, seeded with the
/// group number.
fn desc_csum(sb: &Sb, group: u64, desc: &[u8]) -> u16 {
    let mut c = crc32c_raw(sb.csum_seed, &(group as u32).to_le_bytes());
    c = crc32c_raw(c, &desc[0..0x1E]);
    c = crc32c_raw(c, &[0u8, 0u8]); // dummy bg_checksum
    if sb.desc_size as usize > 0x20 {
        c = crc32c_raw(c, &desc[0x20..sb.desc_size as usize]);
    }
    (c & 0xFFFF) as u16
}

/// Bitmap block checksum (metadata_csum): crc32c over the first `size` bytes.
fn bitmap_csum(sb: &Sb, bitmap: &[u8], size: usize) -> u32 {
    crc32c_raw(sb.csum_seed, &bitmap[..size.min(bitmap.len())])
}

/// Directory-block checksum (`det_checksum`, metadata_csum): crc32c over the
/// block minus its 12-byte tail, seeded with the directory's inode + generation.
fn dir_block_csum(sb: &Sb, dir_ino: u32, dir_gen: u32, block: &[u8]) -> u32 {
    let seed = crc32c_raw(sb.csum_seed, &dir_ino.to_le_bytes());
    let seed = crc32c_raw(seed, &dir_gen.to_le_bytes());
    crc32c_raw(seed, &block[..block.len() - 12])
}

/// Superblock checksum (`s_checksum`, metadata_csum): crc32c(~0) over the first
/// 0x3FC bytes (everything before the checksum field itself).
fn sb_csum(sb_bytes: &[u8]) -> u32 {
    crc32c_raw(0xFFFF_FFFF, &sb_bytes[0..0x3FC])
}

/// Set an inode's `metadata_csum` (i_checksum_lo @0x7C, i_checksum_hi @0x82 when
/// i_extra_isize covers it). Modifies `inode` in place; caller has already
/// applied the field edits (e.g. a link-count change).
fn set_inode_csum(sb: &Sb, ino: u32, inode: &mut [u8]) {
    if !sb.metadata_csum {
        return;
    }
    let has_hi = sb.inode_size > 128
        && u16::from_le_bytes(inode[0x80..0x82].try_into().unwrap()) >= 4;
    // crc over the inode with the checksum fields zeroed, seeded by ino + gen.
    let gen = u32::from_le_bytes(inode[0x64..0x68].try_into().unwrap());
    let mut tmp = inode.to_vec();
    put_u16(&mut tmp, 0x7C, 0);
    if has_hi {
        put_u16(&mut tmp, 0x82, 0);
    }
    let seed = crc32c_raw(sb.csum_seed, &ino.to_le_bytes());
    let seed = crc32c_raw(seed, &gen.to_le_bytes());
    let c = crc32c_raw(seed, &tmp);
    put_u16(inode, 0x7C, c as u16);
    if has_hi {
        put_u16(inode, 0x82, (c >> 16) as u16);
    }
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Add `delta` to a 16-bit (optionally +16 high) little-endian count in `d`.
fn add_split(d: &mut [u8], lo: usize, hi: usize, has_hi: bool, delta: u64) {
    let cur = u16::from_le_bytes(d[lo..lo + 2].try_into().unwrap()) as u64
        | if has_hi { (u16::from_le_bytes(d[hi..hi + 2].try_into().unwrap()) as u64) << 16 } else { 0 };
    let v = cur + delta;
    put_u16(d, lo, v as u16);
    if has_hi {
        put_u16(d, hi, (v >> 16) as u16);
    }
}
fn sub_split(d: &mut [u8], lo: usize, hi: usize, has_hi: bool, delta: u64) {
    let cur = u16::from_le_bytes(d[lo..lo + 2].try_into().unwrap()) as u64
        | if has_hi { (u16::from_le_bytes(d[hi..hi + 2].try_into().unwrap()) as u64) << 16 } else { 0 };
    let v = cur.saturating_sub(delta);
    put_u16(d, lo, v as u16);
    if has_hi {
        put_u16(d, hi, (v >> 16) as u16);
    }
}

/// Build the Stage-2 metadata patches (volume-relative offset → bytes). Each
/// patch is a whole modified structure (a bitmap block, an inode-table slot, a
/// group descriptor, a directory block, the primary superblock); the structures
/// never overlap, so the patches are disjoint. Only `metadata_csum` filesystems
/// are handled — a crc16-only (`gdt_csum`) fs falls back to Stage-1 ghosts.
fn build_stage2_patches(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    coalesce: &[Coalesce],
    freed_inodes: &[(u32, bool)],
    freed_blocks: &[u64],
    dir_parents: &[u32],
) -> Result<Vec<(u64, Vec<u8>)>> {
    if sb.gdt_csum && !sb.metadata_csum {
        return Err(bad("crc16 (gdt_csum-only) descriptors not supported"));
    }
    let fsb = sb.fs_block;
    let mut patches: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
    let mut gdt = sb.gdt.clone();

    let block_bitmap_ptr = |g: u64| -> u64 {
        let d = sb.desc(g);
        u32at(d, 0x00) | if sb.has_hi { u32at(d, 0x20) << 32 } else { 0 }
    };
    let inode_bitmap_ptr = |g: u64| -> u64 {
        let d = sb.desc(g);
        u32at(d, 0x04) | if sb.has_hi { u32at(d, 0x24) << 32 } else { 0 }
    };
    let read_block = |vol: &mut dyn RawVolume, off: u64| -> Result<Vec<u8>> {
        let mut b = vec![0u8; fsb as usize];
        vol.read_at(off, &mut b)?;
        Ok(b)
    };

    // Per-group deltas to apply to descriptors + the superblock at the end.
    let mut freed_blk_ct: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut freed_ino_ct: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut freed_dir_ct: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut touched: std::collections::BTreeSet<u64> = Default::default();

    // --- Block bitmaps: clear each freed block's bit; recompute bitmap csum. ---
    let mut by_group: std::collections::BTreeMap<u64, Vec<u64>> = Default::default();
    for &b in freed_blocks {
        if b < sb.first_data_block as u64 {
            continue;
        }
        let g = (b - sb.first_data_block as u64) / sb.blocks_per_group as u64;
        by_group.entry(g).or_default().push(b);
    }
    for (&g, blks) in &by_group {
        let off = block_bitmap_ptr(g) * fsb;
        let mut bm = read_block(vol, off)?;
        let group_first = sb.first_data_block as u64 + g * sb.blocks_per_group as u64;
        for &b in blks {
            let bit = (b - group_first) as usize;
            if bit / 8 < bm.len() && bm[bit / 8] & (1 << (bit % 8)) != 0 {
                bm[bit / 8] &= !(1u8 << (bit % 8));
                *freed_blk_ct.entry(g).or_default() += 1;
            }
        }
        if sb.metadata_csum {
            let c = bitmap_csum(sb, &bm, (sb.blocks_per_group / 8) as usize);
            let base = (g * sb.desc_size as u64) as usize;
            put_u16(&mut gdt[base..], 0x18, c as u16);
            if sb.has_hi {
                put_u16(&mut gdt[base..], 0x38, (c >> 16) as u16);
            }
        }
        patches.insert(off, bm);
        touched.insert(g);
    }

    // --- Inode bitmaps: clear each freed inode's bit; recompute bitmap csum. ---
    let mut ino_by_group: std::collections::BTreeMap<u64, Vec<(u32, bool)>> = Default::default();
    for &(ino, is_dir) in freed_inodes {
        let g = (ino as u64 - 1) / sb.inodes_per_group as u64;
        ino_by_group.entry(g).or_default().push((ino, is_dir));
    }
    for (&g, inos) in &ino_by_group {
        let off = inode_bitmap_ptr(g) * fsb;
        let mut bm = read_block(vol, off)?;
        for &(ino, is_dir) in inos {
            let bit = ((ino as u64 - 1) % sb.inodes_per_group as u64) as usize;
            if bit / 8 < bm.len() && bm[bit / 8] & (1 << (bit % 8)) != 0 {
                bm[bit / 8] &= !(1u8 << (bit % 8));
                *freed_ino_ct.entry(g).or_default() += 1;
                if is_dir {
                    *freed_dir_ct.entry(g).or_default() += 1;
                }
            }
        }
        if sb.metadata_csum {
            let c = bitmap_csum(sb, &bm, (sb.inodes_per_group / 8) as usize);
            let base = (g * sb.desc_size as u64) as usize;
            // bg_inode_bitmap_csum_lo @0x1A, _hi @0x3A (NOT 0x1C/0x3C, which are
            // bg_itable_unused_lo / bg_block_bitmap_csum_hi is 0x38).
            put_u16(&mut gdt[base..], 0x1A, c as u16);
            if sb.has_hi {
                put_u16(&mut gdt[base..], 0x3A, (c >> 16) as u16);
            }
        }
        patches.insert(off, bm);
        touched.insert(g);
    }

    // --- Inode table: zero each freed inode slot (a clean never-used inode;
    //     a free inode's checksum is not validated by e2fsck). ---
    let freed_set: std::collections::HashSet<u32> = freed_inodes.iter().map(|&(i, _)| i).collect();
    for &(ino, _) in freed_inodes {
        patches.insert(sb.inode_offset(ino), vec![0u8; sb.inode_size as usize]);
    }

    // --- Surviving parent directories: drop i_links_count by the number of
    //     removed subdirectories, recompute the inode checksum. ---
    let mut parent_dec: std::collections::BTreeMap<u32, u16> = Default::default();
    for &p in dir_parents {
        *parent_dec.entry(p).or_default() += 1;
    }
    for (&pino, &dec) in &parent_dec {
        if freed_set.contains(&pino) {
            continue; // parent is itself being removed — nothing to fix
        }
        let off = sb.inode_offset(pino);
        let mut inode = vec![0u8; sb.inode_size as usize];
        vol.read_at(off, &mut inode)?;
        let links = u16::from_le_bytes(inode[0x1A..0x1C].try_into().unwrap());
        put_u16(&mut inode, 0x1A, links.saturating_sub(dec));
        set_inode_csum(sb, pino, &mut inode);
        patches.insert(off, inode);
    }

    // --- Descriptors: apply free-count deltas + recompute bg_checksum. ---
    for &g in &touched {
        let base = (g * sb.desc_size as u64) as usize;
        let mut d = gdt[base..base + sb.desc_size as usize].to_vec();
        add_split(&mut d, 0x0C, 0x2C, sb.has_hi, *freed_blk_ct.get(&g).unwrap_or(&0));
        add_split(&mut d, 0x0E, 0x2E, sb.has_hi, *freed_ino_ct.get(&g).unwrap_or(&0));
        sub_split(&mut d, 0x10, 0x30, sb.has_hi, *freed_dir_ct.get(&g).unwrap_or(&0));
        if sb.metadata_csum {
            let c = desc_csum(sb, g, &d);
            put_u16(&mut d, 0x1E, c);
        }
        patches.insert(sb.gdt_off + base as u64, d);
    }

    // --- Directory blocks: zero each removed entry's inode field; recompute the
    //     block's det_checksum. Several removals share one block; all entries in
    //     a block share one owning directory (inode + generation). ---
    let mut dirs: std::collections::BTreeMap<u64, (u32, u32, Vec<usize>)> = Default::default();
    for c in coalesce {
        let e = dirs.entry(c.dir_block_off).or_insert((c.owner_ino, c.owner_gen, Vec::new()));
        e.2.push(c.rec_off);
    }
    for (&block_off, (dino, dgen, offs)) in &dirs {
        let mut block = read_block(vol, block_off)?;
        for &o in offs {
            if o + 8 <= block.len() {
                // Empty slot: inode = 0, and erase the residual name (privacy —
                // the excluded file's name must not linger in the image) by
                // zeroing name_len and the name bytes; rec_len is kept intact.
                let name_len = block[o + 6] as usize;
                put_u32(&mut block, o, 0);
                block[o + 6] = 0;
                let name_end = (o + 8 + name_len).min(block.len());
                block[o + 8..name_end].fill(0);
            }
        }
        if sb.metadata_csum {
            let c = dir_block_csum(sb, *dino, *dgen, &block);
            let n = block.len();
            put_u32(&mut block, n - 4, c);
        }
        patches.insert(block_off, block);
    }

    // --- Primary superblock: bump free counts + recompute s_checksum. ---
    let total_blk: u64 = freed_blk_ct.values().sum();
    let total_ino: u64 = freed_ino_ct.values().sum();
    let mut sbb = read_block(vol, 1024)?; // 1024-byte SB region (block is ≥1 KiB)
    sbb.truncate(1024);
    // s_free_blocks_count (lo 0x0C, hi 0x158 for 64-bit).
    let is_64 = sb.has_hi || (u32::from_le_bytes(sbb[0x60..0x64].try_into().unwrap()) & INCOMPAT_64BIT as u32 != 0);
    {
        let cur = u32::from_le_bytes(sbb[0x0C..0x10].try_into().unwrap()) as u64
            | if is_64 { (u32::from_le_bytes(sbb[0x158..0x15C].try_into().unwrap()) as u64) << 32 } else { 0 };
        let v = cur + total_blk;
        put_u32(&mut sbb, 0x0C, v as u32);
        if is_64 {
            put_u32(&mut sbb, 0x158, (v >> 32) as u32);
        }
    }
    // s_free_inodes_count (0x10, 32-bit).
    let fino = u32::from_le_bytes(sbb[0x10..0x14].try_into().unwrap()) as u64 + total_ino;
    put_u32(&mut sbb, 0x10, fino as u32);
    if sb.metadata_csum {
        let c = sb_csum(&sbb);
        put_u32(&mut sbb, 0x3FC, c);
    }
    patches.insert(1024, sbb);

    Ok(patches.into_iter().collect())
}

#[test]
fn crc32c_raw_matches_known_vector() {
    // The complemented CRC-32C of "123456789" is 0xE3069283; the raw running
    // form (no final xor) is its complement.
    assert_eq!(!crc32c_raw(0xFFFF_FFFF, b"123456789"), 0xE306_9283);
}

// Integration test against a real ext4 image (needs mke2fs; Linux only).
#[cfg(target_os = "linux")]
#[cfg(test)]
mod ext_integration {
    use super::*;
    use std::os::unix::fs::FileExt;

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

    /// Build a populated ext4 image with `mke2fs -d`; returns its path (in a
    /// leaked tempdir) or None if the tools are unavailable.
    fn make_ext4(block_size: u32) -> Option<(std::path::PathBuf, tempfile::TempDir)> {
        use std::io::Write;
        if std::process::Command::new("mke2fs").arg("-V").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("cache")).ok()?;
        // keep.txt (0xCD), secret.log (0xAB, 8 KiB), cache/big.bin (0xAB, 16 KiB).
        std::fs::File::create(src.join("keep.txt")).ok()?.write_all(&[0xCD; 4096]).ok()?;
        std::fs::File::create(src.join("secret.log")).ok()?.write_all(&[0xAB; 8192]).ok()?;
        std::fs::File::create(src.join("cache/big.bin")).ok()?.write_all(&[0xAB; 16384]).ok()?;
        let img = dir.path().join("fs.img");
        let out = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-b"])
            .arg(block_size.to_string())
            .arg("-d")
            .arg(&src)
            .arg(&img)
            .arg("16384") // blocks
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("mke2fs failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some((img, dir))
    }

    fn run_scan(img: &std::path::Path, patterns: &[&str]) -> (ExtExclusionScan, FileVol) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        let scan = scan_ext_exclusions(&mut vol, &pats, |_| Ok(())).unwrap();
        (scan, vol)
    }

    #[test]
    fn stage1_finds_excluded_file_data() {
        for bs in [1024u32, 4096] {
            let Some((img, _keep)) = make_ext4(bs) else {
                eprintln!("mke2fs unavailable — skipping ext4 integration test");
                return;
            };
            // Exclude two whole entries by name: a file and a directory.
            // Naming the directory is what removes it — `cache/**` would ask
            // only for its contents and leave the directory standing.
            // The image is verified by reading the file directly (below), so the
            // volume handle itself is not needed here.
            let (scan, _vol) = run_scan(&img, &["secret.log", "cache"]);
            // secret.log + cache dir + cache/big.bin.
            assert_eq!(scan.files_excluded, 3, "bs={bs}");
            assert!(!scan.ranges.is_empty(), "bs={bs}: no ranges");
            // Find the files' data on disk by their long fill runs. Testing a
            // byte VALUE inside the excluded ranges would be wrong: those ranges
            // legitimately include the excluded directory's own block, whose
            // dir-entry tail ends in a crc32c that is random per mke2fs run
            // (random fs UUID) — it hits any given byte on ~1.6% of images.
            // Only a long run is real file content.
            let img_bytes = std::fs::read(&img).unwrap();
            let runs = |fill: u8, min_len: usize| -> Vec<(u64, u64)> {
                let mut out = Vec::new();
                let (mut start, mut len) = (0usize, 0usize);
                for (i, &b) in img_bytes.iter().chain(std::iter::once(&0u8)).enumerate() {
                    if b == fill {
                        if len == 0 { start = i; }
                        len += 1;
                    } else {
                        if len >= min_len { out.push((start as u64, (start + len) as u64)); }
                        len = 0;
                    }
                }
                out
            };
            // keep.txt = 4 KiB of 0xCD; secret.log + cache/big.bin = 8 + 16 KiB of 0xAB.
            let keep_runs = runs(0xCD, 4096);
            let ab_runs = runs(0xAB, 4096);
            assert_eq!(
                keep_runs.iter().map(|(s, e)| e - s).sum::<u64>(), 4096,
                "bs={bs}: keep.txt data not found in the image",
            );
            assert_eq!(
                ab_runs.iter().map(|(s, e)| e - s).sum::<u64>(), 8192 + 16384,
                "bs={bs}: excluded files' data not found in the image",
            );

            // Safety property 1: no excluded range may touch the KEPT file.
            for &(ks, ke) in &keep_runs {
                for (start, end) in scan.ranges.iter() {
                    assert!(
                        end <= ks || start >= ke,
                        "bs={bs}: excluded range [{start},{end}) overlaps the KEPT file [{ks},{ke})",
                    );
                }
            }
            // Safety property 2: every byte of the excluded files IS covered.
            for &(as_, ae) in &ab_runs {
                let covered: u64 = scan
                    .ranges
                    .iter()
                    .map(|(rs, re)| re.min(ae).saturating_sub(rs.max(as_)))
                    .sum();
                assert_eq!(covered, ae - as_, "bs={bs}: excluded data [{as_},{ae}) not fully covered");
            }
            assert_eq!(scan.bytes_excluded, 8192 + 16384, "bs={bs}");
        }
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        let Some((img, _keep)) = make_ext4(4096) else { return };
        let (scan, _) = run_scan(&img, &[]);
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
    }

    /// The heart of Stage 2: apply the metadata patches to a real image and let
    /// `e2fsck` judge — a clean exit proves every bitmap/count/checksum is right
    /// and the excluded files are gone.
    #[test]
    fn stage2_patched_image_is_fsck_clean() {
        if std::process::Command::new("e2fsck").arg("-V").output().is_err() {
            eprintln!("e2fsck unavailable — skipping Stage-2 fsck test");
            return;
        }
        for bs in [1024u32, 4096] {
            let Some((img, keep)) = make_ext4(bs) else { return };
            // Exclude the file, the whole cache subtree — leave keep.txt.
            let (scan, _) = run_scan(&img, &["secret.log", "cache"]);
            assert!(!scan.patches.is_empty(), "bs={bs}: Stage 2 produced no patches");

            // Apply the metadata patches to a copy of the image.
            let mut data = std::fs::read(&img).unwrap();
            for (off, bytes) in &scan.patches {
                let o = *off as usize;
                assert!(o + bytes.len() <= data.len(), "bs={bs}: patch out of range");
                data[o..o + bytes.len()].copy_from_slice(bytes);
            }
            let patched = keep.path().join(format!("patched_{bs}.img"));
            std::fs::write(&patched, &data).unwrap();

            // e2fsck -fn: force a full check, answer no (make no changes). Exit 0
            // means the filesystem is already consistent.
            let out = std::process::Command::new("e2fsck")
                .args(["-fn"])
                .arg(&patched)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "bs={bs}: e2fsck found problems (exit {:?}):\n{stdout}\n{stderr}",
                out.status.code()
            );

            // And the excluded entries are gone (their names erased) while the
            // kept one survives — a path lookup for the excluded file must fail.
            let ls = std::process::Command::new("debugfs")
                .args(["-R", "ls -l /"])
                .arg(&patched)
                .output()
                .unwrap();
            let listing = String::from_utf8_lossy(&ls.stdout);
            assert!(listing.contains("keep.txt"), "bs={bs}: keep.txt missing:\n{listing}");
            assert!(!listing.contains("secret.log"), "bs={bs}: secret.log name still present:\n{listing}");
            assert!(!listing.contains("cache"), "bs={bs}: cache name still present:\n{listing}");
            let stat = std::process::Command::new("debugfs")
                .args(["-R", "stat /secret.log"])
                .arg(&patched)
                .output()
                .unwrap();
            let stat_out = format!(
                "{}{}",
                String::from_utf8_lossy(&stat.stdout),
                String::from_utf8_lossy(&stat.stderr)
            );
            assert!(stat_out.contains("not found") || stat_out.contains("File not found"),
                "bs={bs}: /secret.log still resolvable:\n{stat_out}");
        }
    }

    /// ext2/ext3 image (no extents): a file bigger than the 12 direct pointers
    /// needs an *indirect* block, which belongs to the file just like its data.
    /// `file_kib` KiB of 0xAB at `/big.bin`, plus a small kept file.
    fn make_ext2(bs: u32, file_kib: usize) -> Option<(std::path::PathBuf, tempfile::TempDir)> {
        use std::io::Write;
        if std::process::Command::new("mke2fs").arg("-V").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).ok()?;
        std::fs::File::create(src.join("keep.txt")).ok()?.write_all(&[0xCD; 4096]).ok()?;
        std::fs::File::create(src.join("big.bin"))
            .ok()?
            .write_all(&vec![0xAB; file_kib * 1024])
            .ok()?;
        let img = dir.path().join("ext2.img");
        let out = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext2", "-b"])
            .arg(bs.to_string())
            .arg("-d")
            .arg(&src)
            .arg(&img)
            .arg("16384") // blocks
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("mke2fs (ext2) failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some((img, dir))
    }

    /// Regression: on ext2/ext3 the indirect block is part of the file's
    /// allocation. If it is not zeroed and freed with the data, it stays marked
    /// in use in the block bitmap after removal — `e2fsck` then reports block
    /// bitmap differences. 64 KiB at 1 KiB blocks = 64 data blocks: 12 direct +
    /// 52 reached through exactly one single-indirect block.
    #[test]
    fn stage2_ext2_indirect_block_is_freed() {
        if std::process::Command::new("e2fsck").arg("-V").output().is_err() {
            eprintln!("e2fsck unavailable — skipping ext2 indirect-block test");
            return;
        }
        let Some((img, keep)) = make_ext2(1024, 64) else { return };
        let (scan, _vol) = run_scan(&img, &["big.bin"]);
        assert_eq!(scan.files_excluded, 1);

        // Stage 1 must cover the data blocks AND the indirect block.
        let blocks: u64 = scan.ranges.iter().map(|(s, e)| (e - s) / 1024).sum();
        assert_eq!(blocks, 64 + 1, "64 data blocks + 1 single-indirect block");

        // Stage 2 must leave the filesystem consistent.
        let mut data = std::fs::read(&img).unwrap();
        for (off, bytes) in &scan.patches {
            let o = *off as usize;
            data[o..o + bytes.len()].copy_from_slice(bytes);
        }
        let patched = keep.path().join("patched_ext2.img");
        std::fs::write(&patched, &data).unwrap();
        let out = std::process::Command::new("e2fsck").args(["-fn"]).arg(&patched).output().unwrap();
        assert!(
            out.status.success(),
            "e2fsck found problems (exit {:?}):\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // --- debugfs-built fixtures (device nodes, xattr blocks, journal flags) ---

    fn have_tool(cmd: &str) -> bool {
        std::process::Command::new(cmd).arg("-V").output().is_ok()
    }

    /// Feed a script of commands to a read-write debugfs session.
    fn debugfs_w(img: &std::path::Path, script: &str) -> String {
        use std::io::Write;
        let mut child = std::process::Command::new("debugfs")
            .arg("-w")
            .arg(img)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(script.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn debugfs_r(img: &std::path::Path, req: &str) -> String {
        let out = std::process::Command::new("debugfs").args(["-R", req]).arg(img).output().unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// `e2fsck -fn` → (clean, log).
    fn fsck_ro(img: &std::path::Path) -> (bool, String) {
        let out = std::process::Command::new("e2fsck").args(["-fn"]).arg(img).output().unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), log)
    }

    /// debugfs leaves stale group counters behind — let e2fsck repair them,
    /// then insist the baseline is clean so failures below indict the scanner
    /// alone, not the fixture.
    fn fsck_normalize(img: &std::path::Path) -> bool {
        let _ = std::process::Command::new("e2fsck").args(["-fy"]).arg(img).output();
        fsck_ro(img).0
    }

    fn apply_patches(img: &std::path::Path, scan: &ExtExclusionScan, out: &std::path::Path) {
        let mut data = std::fs::read(img).unwrap();
        for (off, bytes) in &scan.patches {
            let o = *off as usize;
            assert!(o + bytes.len() <= data.len(), "patch out of range");
            data[o..o + bytes.len()].copy_from_slice(bytes);
        }
        std::fs::write(out, &data).unwrap();
    }

    /// Regression for a confirmed corruption: a char device's i_block holds the
    /// DEVICE NUMBER, not a block map, and the scanner read it as one — zeroing
    /// and freeing an unrelated block. (5,1) old-encodes to i_block[0] = 0x0501,
    /// and on the repro image "block 1281" belonged to the journal.
    #[test]
    fn excluding_a_device_node_leaves_foreign_blocks_alone() {
        if !have_tool("debugfs") || !have_tool("e2fsck") {
            eprintln!("debugfs/e2fsck unavailable — skipping");
            return;
        }
        let Some((img, keep)) = make_ext4(4096) else { return };
        debugfs_w(&img, "mkdir /devdir\ncd /devdir\nmknod console c 5 1\n");
        if !fsck_normalize(&img) {
            eprintln!("debugfs left the image inconsistent — skipping");
            return;
        }
        let stat = debugfs_r(&img, "stat /devdir/console");
        if !stat.contains("character special") {
            eprintln!("debugfs could not create the device node — skipping:\n{stat}");
            return;
        }

        let (scan, _) = run_scan(&img, &["devdir"]);
        assert_eq!(scan.files_excluded, 2, "devdir + console");
        // The device number, read as a block pointer, must NOT be excluded.
        let dev_block = 0x0501u64;
        for (s, e) in scan.ranges.iter() {
            assert!(
                e <= dev_block * 4096 || s >= (dev_block + 1) * 4096,
                "device number (5,1) was read as block {dev_block}: range [{s},{e})"
            );
        }
        // Stage 2 removes the node; e2fsck judges the aftermath.
        let patched = keep.path().join("patched_dev.img");
        apply_patches(&img, &scan, &patched);
        let (ok, log) = fsck_ro(&patched);
        assert!(ok, "e2fsck after device removal:\n{log}");
        assert!(!debugfs_r(&patched, "ls -l /").contains("devdir"), "devdir survived");
    }

    /// A fast symlink with an EXTERNAL xattr block has i_blocks != 0, and the
    /// old fast-symlink test (`i_blocks == 0`) sent its TARGET TEXT down the
    /// block-map walker. The only block such an inode owns is the xattr block —
    /// solely owned, it is freed together with the inode.
    #[test]
    fn excluding_a_fast_symlink_with_an_xattr_block() {
        if !have_tool("debugfs") || !have_tool("e2fsck") {
            eprintln!("debugfs/e2fsck unavailable — skipping");
            return;
        }
        let Some((img, keep)) = make_ext4(4096) else { return };
        let big = "A".repeat(300); // does not fit in-inode → external xattr block
        debugfs_w(&img, &format!("symlink /link shorttarget\nea_set /link user.big {big}\n"));
        if !fsck_normalize(&img) {
            eprintln!("debugfs left the image inconsistent — skipping");
            return;
        }
        let stat = debugfs_r(&img, "stat /link");
        let acl: u64 = stat
            .lines()
            .find_map(|l| l.trim().strip_prefix("File ACL: "))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if acl == 0 {
            eprintln!("xattr stayed in-inode — skipping:\n{stat}");
            return;
        }

        let (scan, _) = run_scan(&img, &["link"]);
        assert_eq!(scan.files_excluded, 1);
        // Exactly the xattr block and nothing else — the old check produced
        // bogus block pointers out of the target text.
        let all: Vec<_> = scan.ranges.iter().collect();
        assert_eq!(
            all,
            vec![(acl * 4096, (acl + 1) * 4096)],
            "excluded ranges must be exactly the symlink's xattr block"
        );

        let patched = keep.path().join("patched_sym.img");
        apply_patches(&img, &scan, &patched);
        let (ok, log) = fsck_ro(&patched);
        assert!(ok, "e2fsck after symlink removal:\n{log}");
        assert!(!debugfs_r(&patched, "ls -l /").contains("link"), "symlink survived");
    }

    /// Populated ext4 image from a caller-built source tree.
    fn make_ext4_from(
        bs: u32,
        build: impl FnOnce(&std::path::Path),
    ) -> Option<(std::path::PathBuf, tempfile::TempDir)> {
        if std::process::Command::new("mke2fs").arg("-V").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).ok()?;
        build(&src);
        let img = dir.path().join("fs.img");
        let out = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-b"])
            .arg(bs.to_string())
            .arg("-d")
            .arg(&src)
            .arg(&img)
            .arg("16384")
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("mke2fs failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some((img, dir))
    }

    /// A hard link with a surviving name spares the whole file — and its parent
    /// directory, and the chain up (the invariant `decide_removable` restores).
    /// The other children still go, their entries coalesced from the SPARED
    /// directory's surviving blocks. The one-pass scanner freed the parent
    /// first and left the spared child a nameless «unattached inode».
    #[test]
    fn a_surviving_hardlink_name_spares_file_and_parent_chain() {
        if !have_tool("e2fsck") || !have_tool("debugfs") {
            eprintln!("e2fsck/debugfs unavailable — skipping");
            return;
        }
        let Some((img, keep)) = make_ext4_from(4096, |src| {
            std::fs::create_dir_all(src.join("cache")).unwrap();
            std::fs::write(src.join("cache/hard_a.bin"), vec![0xAB; 8192]).unwrap();
            std::fs::hard_link(src.join("cache/hard_a.bin"), src.join("hard_b.bin")).unwrap();
            std::fs::write(src.join("cache/other.bin"), vec![0xEE; 8192]).unwrap();
        }) else {
            return;
        };
        let (scan, _) = run_scan(&img, &["cache"]);
        assert_eq!(scan.files_excluded, 1, "only other.bin is removable");
        assert_eq!(
            scan.ranges.iter().map(|(s, e)| e - s).sum::<u64>(),
            8192,
            "exactly other.bin's data — the hardlink and the spared dir stay"
        );

        let patched = keep.path().join("patched.img");
        apply_patches(&img, &scan, &patched);
        let (ok, log) = fsck_ro(&patched);
        assert!(ok, "e2fsck after a spared hardlink:\n{log}");
        let ls = debugfs_r(&patched, "ls -l /cache");
        assert!(ls.contains("hard_a.bin"), "spared hardlink name lost:\n{ls}");
        assert!(!ls.contains("other.bin"), "other.bin entry survived:\n{ls}");
    }

    /// A hardlink pair with BOTH names excluded goes entirely — the ext
    /// counterpart of btrfs's `refs == matched_count`: inode freed once, both
    /// entries gone, data zeroed and counted once.
    #[test]
    fn a_fully_matched_hardlink_pair_is_removed() {
        if !have_tool("e2fsck") || !have_tool("debugfs") {
            eprintln!("e2fsck/debugfs unavailable — skipping");
            return;
        }
        let Some((img, keep)) = make_ext4_from(4096, |src| {
            std::fs::create_dir_all(src.join("c1")).unwrap();
            std::fs::create_dir_all(src.join("c2")).unwrap();
            std::fs::write(src.join("c1/hard_a.bin"), vec![0xAB; 8192]).unwrap();
            std::fs::hard_link(src.join("c1/hard_a.bin"), src.join("c2/hard_b.bin")).unwrap();
        }) else {
            return;
        };
        let (scan, _) = run_scan(&img, &["c1", "c2"]);
        assert_eq!(scan.files_excluded, 3, "c1 + c2 + the shared inode");
        assert_eq!(scan.bytes_excluded, 8192, "the shared inode counts once");

        let patched = keep.path().join("patched.img");
        apply_patches(&img, &scan, &patched);
        let (ok, log) = fsck_ro(&patched);
        assert!(ok, "e2fsck after removing a hardlink pair:\n{log}");
        let ls = debugfs_r(&patched, "ls -l /");
        assert!(!ls.contains("c1") && !ls.contains("c2"), "dirs survived:\n{ls}");
    }

    /// A hand-written `tmp` pattern keeps the mount point and EMPTIES it — the
    /// promise the old double-push never delivered (the second push won and the
    /// contents stayed untouched while the log claimed otherwise).
    #[test]
    fn an_essential_dir_is_emptied_but_kept() {
        if !have_tool("e2fsck") || !have_tool("debugfs") {
            eprintln!("e2fsck/debugfs unavailable — skipping");
            return;
        }
        let Some((img, keep)) = make_ext4_from(4096, |src| {
            std::fs::create_dir_all(src.join("tmp")).unwrap();
            std::fs::write(src.join("tmp/junk1.bin"), vec![0xAB; 8192]).unwrap();
            std::fs::write(src.join("tmp/junk2.bin"), vec![0xAB; 4096]).unwrap();
        }) else {
            return;
        };
        let (scan, _) = run_scan(&img, &["tmp"]);
        assert_eq!(scan.files_excluded, 2, "contents go, the mount point stays");
        assert_eq!(scan.bytes_excluded, 8192 + 4096);

        let patched = keep.path().join("patched.img");
        apply_patches(&img, &scan, &patched);
        let (ok, log) = fsck_ro(&patched);
        assert!(ok, "e2fsck after emptying /tmp:\n{log}");
        let root = debugfs_r(&patched, "ls -l /");
        assert!(root.contains("tmp"), "/tmp must survive:\n{root}");
        let inside = debugfs_r(&patched, "ls /tmp");
        assert!(!inside.contains("junk"), "contents must be gone:\n{inside}");
    }

    /// An unreplayed journal makes both products liars: the on-disk structures
    /// are older than the committed state, and the kernel's replay at first
    /// boot overwrites whatever was patched. Both entry points must refuse.
    #[test]
    fn a_dirty_journal_refuses_scan_and_used_map() {
        if !have_tool("debugfs") {
            eprintln!("debugfs unavailable — skipping");
            return;
        }
        let Some((img, _keep)) = make_ext4(4096) else { return };
        debugfs_w(&img, "feature needs_recovery\n");
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let scan = scan_ext_exclusions(&mut vol, &["secret.log".to_string()], |_| Ok(()));
        assert!(scan.is_err(), "exclusions must refuse a dirty journal");
        let map = crate::ext_map::build_ext_used_map(&mut vol, 1 << 20);
        assert!(map.is_err(), "used-map must refuse a dirty journal");
    }
}
