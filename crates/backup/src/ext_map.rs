//! Used-block map for ext2/3/4, parsed directly from the raw volume.
//!
//! Windows has no native ext driver, so — exactly as [`crate::fat_map`] does for
//! FAT/exFAT — we read the on-disk structures ourselves to learn which blocks
//! hold data and copy only those (plus all filesystem metadata). This is
//! strictly read-only: we read the superblock, the block group descriptors and
//! the per-group block bitmaps, never files.
//!
//! Failure policy mirrors `fat_map`: ANY doubt — bad magic, geometry that does
//! not fit the device, a layout-changing feature we don't handle, a bitmap
//! pointer out of range — is an error, and the engine falls back to
//! sector-by-sector. Missing data in an image is far worse than a bigger image.
//!
//! Subtlety worth calling out: a block group flagged `BLOCK_UNINIT` (empty
//! groups on a filesystem with the uninit_bg/metadata_csum feature) has a
//! stale, never-written on-disk block bitmap. We must NOT read it; instead we
//! copy only that group's metadata (which we locate from the descriptors and
//! the sparse-super layout) and treat the rest as free.

use tracing::info;

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::used_sectors::UsedSectorMap;

/// ext superblock magic (`s_magic`, little-endian at superblock offset 0x38).
const EXT_MAGIC: u64 = 0xEF53;

// s_feature_incompat bits.
const INCOMPAT_64BIT: u64 = 0x80;
const INCOMPAT_META_BG: u64 = 0x10;
const INCOMPAT_JOURNAL_DEV: u64 = 0x8;

// s_feature_ro_compat bits.
const RO_COMPAT_SPARSE_SUPER: u64 = 0x1;
const RO_COMPAT_BIGALLOC: u64 = 0x200;

// s_feature_compat bits.
const COMPAT_SPARSE_SUPER2: u64 = 0x200;

/// bg_flags: this group's block bitmap is uninitialized (group is otherwise
/// empty) — the on-disk bitmap block must not be trusted.
const BG_BLOCK_UNINIT: u64 = 0x2;

/// Sanity cap for the group descriptor table read.
const MAX_STRUCT_BYTES: u64 = 256 * 1024 * 1024;

/// Per-backup-block used map for an ext2/3/4 volume.
pub struct ExtUsedMap {
    block_used: Vec<bool>,
    used_count: u64,
    /// ext filesystem block size in bytes (1 KiB … 64 KiB).
    pub block_size: u32,
}

impl UsedSectorMap for ExtUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.block_used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.used_count
    }
}

impl ExtUsedMap {
    /// Drop every backup block that lies ENTIRELY within `ranges` (excluded file
    /// data, plan D2) so it is not copied — the same "sparse-out excluded
    /// clusters" the NTFS/FAT paths do. `backup_block_size` is the block size the
    /// map was built with (its index unit). A block only partly excluded stays
    /// used and is zeroed by the caller instead.
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
    OwnBackupError::Device(format!("ext map: {msg}"))
}

fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
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

/// Mark `count` filesystem blocks starting at `first` (block numbers) used.
fn mark_blocks(map: &mut [bool], block_size: u32, fs_block: u64, first: u64, count: u64) {
    mark_bytes(map, block_size, first * fs_block, (first + count) * fs_block);
}

/// Whether block group `g` carries a backup superblock + descriptor table.
/// With sparse_super they live only in groups 0, 1 and the odd powers of
/// 3, 5, 7 (matching the kernel's `ext4_group_sparse`).
fn has_super(g: u64, sparse_super: bool) -> bool {
    if !sparse_super || g <= 1 {
        return true;
    }
    if g % 2 == 0 {
        return false;
    }
    is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
}

/// True iff `a == b^k` for some `k >= 1`.
fn is_power_of(mut a: u64, b: u64) -> bool {
    while a > b && a % b == 0 {
        a /= b;
    }
    a == b
}

/// Detect ext2/3/4 on `vol` and build the used map. Errors (→ caller falls
/// back to sector-by-sector) on anything unrecognized or inconsistent.
pub fn build_ext_used_map(vol: &mut dyn RawVolume, block_size: u32) -> Result<ExtUsedMap> {
    // --- Superblock: 1024 bytes at byte offset 1024 ---
    let mut head = [0u8; 2048];
    vol.read_at(0, &mut head)?;
    let sb = &head[1024..];

    if u16at(sb, 0x38) != EXT_MAGIC {
        return Err(bad("no ext superblock magic (0xEF53)"));
    }

    let log_block_size = u32at(sb, 0x18);
    if log_block_size > 6 {
        return Err(bad(format!("implausible s_log_block_size {log_block_size}")));
    }
    let fs_block = 1024u64 << log_block_size;

    let compat = u32at(sb, 0x5C);
    let incompat = u32at(sb, 0x60);
    let ro_compat = u32at(sb, 0x64);
    // Layout-changing features we deliberately do NOT parse: refuse and let the
    // engine copy every sector rather than risk a wrong map.
    if incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return Err(bad("external journal device, not a filesystem"));
    }
    if incompat & INCOMPAT_META_BG != 0 {
        return Err(bad("meta_bg descriptor layout not supported"));
    }
    if ro_compat & RO_COMPAT_BIGALLOC != 0 {
        return Err(bad("bigalloc (cluster) bitmaps not supported"));
    }
    if compat & COMPAT_SPARSE_SUPER2 != 0 {
        return Err(bad("sparse_super2 backup layout not supported"));
    }

    let is_64bit = incompat & INCOMPAT_64BIT != 0;
    let sparse_super = ro_compat & RO_COMPAT_SPARSE_SUPER != 0;

    let first_data_block = u32at(sb, 0x14);
    let blocks_per_group = u32at(sb, 0x20);
    let inodes_per_group = u32at(sb, 0x28);
    let inode_size = u16at(sb, 0x58);
    let reserved_gdt = u16at(sb, 0xCE);
    let blocks_count = u32at(sb, 0x04) | if is_64bit { u32at(sb, 0x150) << 32 } else { 0 };
    let free_count = u32at(sb, 0x0C) | if is_64bit { u32at(sb, 0x158) << 32 } else { 0 };

    let desc_size = if is_64bit {
        let d = u16at(sb, 0xFE);
        if d < 32 {
            return Err(bad(format!("64-bit fs with descriptor size {d}")));
        }
        d
    } else {
        32
    };
    // The high descriptor halves (bitmap/inode-table _hi) only exist when the
    // descriptor is large enough to hold them.
    let has_hi = is_64bit && desc_size >= 64;

    // --- Geometry validation ---
    if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
        return Err(bad("zero blocks_count / blocks_per_group / inodes_per_group"));
    }
    if blocks_per_group > 8 * fs_block {
        return Err(bad(format!(
            "blocks_per_group {blocks_per_group} exceeds one {fs_block}-byte bitmap block"
        )));
    }
    let expected_fdb = if fs_block == 1024 { 1 } else { 0 };
    if first_data_block != expected_fdb {
        return Err(bad(format!(
            "first_data_block {first_data_block} != expected {expected_fdb} for {fs_block}-byte blocks"
        )));
    }
    if inode_size < 128 || !inode_size.is_power_of_two() || inode_size > fs_block {
        return Err(bad(format!("implausible inode_size {inode_size}")));
    }
    if free_count > blocks_count {
        return Err(bad("free blocks exceed total blocks"));
    }
    let fs_bytes = blocks_count
        .checked_mul(fs_block)
        .ok_or_else(|| bad("blocks_count * block_size overflows"))?;
    if fs_bytes > vol.len() {
        return Err(bad(format!(
            "filesystem claims {fs_bytes} bytes but the device is {}",
            vol.len()
        )));
    }

    let num_groups = (blocks_count - first_data_block).div_ceil(blocks_per_group);
    if num_groups == 0 {
        return Err(bad("filesystem has no block groups"));
    }
    let inode_table_blocks = (inodes_per_group * inode_size).div_ceil(fs_block);

    // --- Group descriptor table: starts one block after the superblock ---
    let gdt_byte_len = num_groups
        .checked_mul(desc_size)
        .ok_or_else(|| bad("descriptor table size overflows"))?;
    if gdt_byte_len > MAX_STRUCT_BYTES {
        return Err(bad(format!("descriptor table implausibly large ({gdt_byte_len} bytes)")));
    }
    let gdt_blocks = gdt_byte_len.div_ceil(fs_block);
    let gdt_start = (first_data_block + 1) * fs_block;
    let mut gdt = vec![0u8; (gdt_blocks * fs_block) as usize];
    vol.read_at(gdt_start, &mut gdt)?;

    // --- Build the per-backup-block used map ---
    let total_backup_blocks = vol.len().div_ceil(block_size as u64);
    let mut map = vec![false; total_backup_blocks as usize];

    // Boot block(s) before the first data block (present only with 1 KiB blocks).
    mark_bytes(&mut map, block_size, 0, first_data_block * fs_block);

    let mut data_blocks_from_bitmaps = 0u64;
    for g in 0..num_groups {
        let off = (g * desc_size) as usize;
        let desc = &gdt[off..off + desc_size as usize];

        let block_bitmap = u32at(desc, 0x00) | if has_hi { u32at(desc, 0x20) << 32 } else { 0 };
        let inode_bitmap = u32at(desc, 0x04) | if has_hi { u32at(desc, 0x24) << 32 } else { 0 };
        let inode_table = u32at(desc, 0x08) | if has_hi { u32at(desc, 0x28) << 32 } else { 0 };
        let flags = u16at(desc, 0x12);

        let group_first = first_data_block + g * blocks_per_group;
        let blocks_in_group = blocks_per_group.min(blocks_count - group_first);

        // Metadata pointed to by the descriptors — marked wherever it lives, so
        // flex_bg (which pools several groups' bitmaps/inode tables into one
        // group) is handled without special-casing.
        for (blk, cnt) in [(block_bitmap, 1), (inode_bitmap, 1), (inode_table, inode_table_blocks)] {
            if blk < first_data_block || blk + cnt > blocks_count {
                return Err(bad(format!(
                    "group {g} metadata block {blk}(+{cnt}) outside the filesystem"
                )));
            }
            mark_blocks(&mut map, block_size, fs_block, blk, cnt);
        }

        // Superblock + descriptor table + reserved-GDT backups at the group
        // start (only groups that carry them). Essential for BLOCK_UNINIT
        // groups, harmless (already in the bitmap) for the rest.
        if has_super(g, sparse_super) {
            let meta = (1 + gdt_blocks + reserved_gdt).min(blocks_in_group);
            mark_blocks(&mut map, block_size, fs_block, group_first, meta);
        }

        // Data blocks from this group's allocation bitmap — but never for a
        // BLOCK_UNINIT group, whose on-disk bitmap is stale and whose only used
        // blocks are the metadata already marked above.
        if flags & BG_BLOCK_UNINIT == 0 {
            let mut bmp = vec![0u8; fs_block as usize];
            vol.read_at(block_bitmap * fs_block, &mut bmp)?;
            for i in 0..blocks_in_group {
                if bmp[(i / 8) as usize] & (1 << (i % 8)) != 0 {
                    data_blocks_from_bitmaps += 1;
                    mark_blocks(&mut map, block_size, fs_block, group_first + i, 1);
                }
            }
        }
    }

    // Device slack past the last filesystem block.
    mark_bytes(&mut map, block_size, fs_bytes, vol.len());

    let used_count = map.iter().filter(|&&u| u).count() as u64;

    // Coarse safety net: the blocks we kept must cover at least every allocated
    // filesystem block. Folding into (larger) backup blocks only ever adds
    // coverage, so keeping fewer bytes than the superblock says are allocated
    // means we dropped real data — refuse and fall back to a full copy.
    let allocated_bytes = (blocks_count - free_count) * fs_block;
    let covered_bytes = (used_count * block_size as u64).min(vol.len());
    if covered_bytes < allocated_bytes {
        return Err(bad(format!(
            "kept {covered_bytes} bytes but the filesystem has {allocated_bytes} allocated"
        )));
    }

    info!(
        fs = "ext4",
        fs_block_size = fs_block,
        num_groups,
        blocks_count,
        data_blocks = data_blocks_from_bitmaps,
        used_blocks = used_count,
        total_blocks = total_backup_blocks,
        "ext used-block map ready"
    );
    Ok(ExtUsedMap {
        block_used: map,
        used_count,
        block_size: fs_block as u32,
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

    fn put_u16(img: &mut [u8], off: usize, v: u16) {
        img[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(img: &mut [u8], off: usize, v: u32) {
        img[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Write the common superblock fields (1 KiB blocks) at byte 1024.
    fn put_superblock(
        img: &mut [u8],
        blocks_count: u32,
        free_count: u32,
        blocks_per_group: u32,
        inodes_per_group: u32,
    ) {
        let sb = 1024;
        put_u32(img, sb + 0x04, blocks_count);
        put_u32(img, sb + 0x0C, free_count);
        put_u32(img, sb + 0x14, 1); // s_first_data_block (1 for 1 KiB blocks)
        put_u32(img, sb + 0x18, 0); // s_log_block_size → 1 KiB
        put_u32(img, sb + 0x20, blocks_per_group);
        put_u32(img, sb + 0x28, inodes_per_group);
        put_u16(img, sb + 0x38, 0xEF53); // magic
        put_u16(img, sb + 0x58, 128); // s_inode_size
        put_u32(img, sb + 0x64, RO_COMPAT_SPARSE_SUPER as u32); // sparse_super
    }

    /// Write a 32-byte classic group descriptor at descriptor index `g`
    /// (descriptor table sits in block 2 for a 1 KiB-block filesystem).
    fn put_desc(img: &mut [u8], g: usize, bbitmap: u32, ibitmap: u32, itable: u32, flags: u16) {
        let base = 2 * 1024 + g * 32;
        put_u32(img, base + 0x00, bbitmap);
        put_u32(img, base + 0x04, ibitmap);
        put_u32(img, base + 0x08, itable);
        put_u16(img, base + 0x12, flags);
    }

    /// Single group, 1 KiB blocks, 16 blocks total. Layout: 0 boot, 1 super,
    /// 2 gdt, 3 block-bitmap, 4 inode-bitmap, 5 inode-table, 6.. data. The
    /// bitmap marks blocks 1–6 and 8 allocated.
    fn one_group_fs() -> MemVolume {
        let mut img = vec![0u8; 16 * 1024];
        // 16 blocks, 9 free (blocks 0,7,9..15 → 7 allocated: 1..6 and 8).
        put_superblock(&mut img, 16, 9, 16, 8);
        put_desc(&mut img, 0, 3, 4, 5, 0);
        // Block bitmap in block 3: bit i → block 1+i. Allocate blocks 1..6 (bits
        // 0..5) and block 8 (bit 7) → 0b1011_1111.
        img[3 * 1024] = 0b1011_1111;
        MemVolume(img)
    }

    #[test]
    fn one_group_marks_metadata_and_used_data() {
        let mut vol = one_group_fs();
        // Backup block size == fs block size, so backup block N maps to fs block N.
        let map = build_ext_used_map(&mut vol, 1024).unwrap();
        assert_eq!(map.block_size, 1024);

        for b in 0..=6 {
            assert!(map.is_block_used(b), "block {b} should be used");
        }
        assert!(!map.is_block_used(7), "free block 7");
        assert!(map.is_block_used(8), "allocated data block 8");
        for b in 9..16 {
            assert!(!map.is_block_used(b), "free block {b}");
        }
        assert_eq!(map.used_block_count(), 8);
    }

    /// Two groups (blocks_per_group = 8, 17 blocks). Group 0 is normal; group 1
    /// is flagged BLOCK_UNINIT with a garbage (all-ones) bitmap — its data
    /// blocks must be ignored, only its metadata copied.
    fn uninit_group_fs() -> MemVolume {
        let mut img = vec![0u8; 17 * 1024];
        // 17 blocks; allocated = group0 {1..6}=6 + group1 metadata {9,10,11,12,13}=5
        // = 11, so 6 free.
        put_superblock(&mut img, 17, 6, 8, 8);
        // GDT (block 2) holds two descriptors.
        put_desc(&mut img, 0, 3, 4, 5, 0);
        put_desc(&mut img, 1, 11, 12, 13, BG_BLOCK_UNINIT as u16);
        // Group 0 bitmap (block 3): bit i → block 1+i. Allocate blocks 1..6.
        img[3 * 1024] = 0b0011_1111;
        // Group 1 bitmap (block 11): garbage — all bits set. Must be ignored.
        img[11 * 1024] = 0xFF;
        MemVolume(img)
    }

    #[test]
    fn uninit_group_bitmap_is_ignored() {
        let mut vol = uninit_group_fs();
        let map = build_ext_used_map(&mut vol, 1024).unwrap();

        // Group 0: metadata + data 1..6 used, 7/8 free.
        for b in 0..=6 {
            assert!(map.is_block_used(b), "group0 block {b}");
        }
        assert!(!map.is_block_used(7));
        assert!(!map.is_block_used(8));
        // Group 1 metadata (super 9, gdt 10, bitmap 11, inode-bitmap 12, table 13).
        for b in 9..=13 {
            assert!(map.is_block_used(b), "group1 metadata block {b}");
        }
        // Group 1 data blocks 14..16: the garbage bitmap would mark them if we
        // read it — the UNINIT flag must keep them free.
        for b in 14..17 {
            assert!(!map.is_block_used(b), "uninit data block {b} must stay free");
        }
        // Blocks 0–6 (7) + group-1 metadata 9–13 (5) = 12.
        assert_eq!(map.used_block_count(), 12);
    }

    #[test]
    fn rejects_non_ext_and_bad_geometry() {
        // No magic.
        assert!(build_ext_used_map(&mut MemVolume(vec![0u8; 16 * 1024]), 1024).is_err());

        // Filesystem claims more blocks than the device holds.
        let mut img = one_group_fs().0;
        put_u32(&mut img, 1024 + 0x04, 10_000); // s_blocks_count
        assert!(build_ext_used_map(&mut MemVolume(img), 1024).is_err());

        // Unsupported feature (bigalloc) → fall back.
        let mut img = one_group_fs().0;
        put_u32(&mut img, 1024 + 0x64, (RO_COMPAT_SPARSE_SUPER | RO_COMPAT_BIGALLOC) as u32);
        assert!(build_ext_used_map(&mut MemVolume(img), 1024).is_err());
    }

    #[test]
    fn has_super_matches_sparse_layout() {
        // Sparse: groups 0, 1 and odd powers of 3/5/7 carry backups.
        for g in [0, 1, 3, 5, 7, 9, 25, 27, 49] {
            assert!(has_super(g, true), "group {g} should have a super backup");
        }
        for g in [2, 4, 6, 8, 10, 15, 30] {
            assert!(!has_super(g, true), "group {g} should not");
        }
        // Without sparse_super every group has one.
        assert!(has_super(2, false));
    }
}
