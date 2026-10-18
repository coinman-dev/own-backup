//! xfs file exclusions (Stage 1) — the reflink-aware zeroing pass.
//!
//! xfs is big-endian on disk and, unlike btrfs, does **not** checksum file data
//! (only metadata on v5). So Stage 1 needs no checksum fix-ups and no metadata
//! edits at all: we walk the directory trees, match the patterns, resolve each
//! matched file's data extents, and zero the ones that are **exclusively
//! owned** — dropping them from the used-map. xfs supports reflink (shared
//! extents), so before zeroing an extent we consult the AG's **refcount
//! B+tree**: any block with refcount > 1 is shared (a reflink copy or CoW), and
//! zeroing it would corrupt the sharer — so it is kept. Read-only; `xfs_repair`
//! stays clean because only file *data* blocks are zeroed.
//!
//! Stage 2 (catalog true-removal — free-space/inode-btree/directory surgery) is
//! deliberately deferred: xfs has no rebuild shortcut like btrfs's free-space
//! tree, so it is the highest-risk of the three and xfs is the least-used.
//!
//! Scope: single filesystem, extents/btree inode forks, shortform + block/leaf/
//! node (dir2 data-block) directories. Btree-format directories, non-crc (v4)
//! filesystems, or any structural doubt keep the file — a bigger image is always
//! better than a corrupted one.

use std::collections::{HashMap, HashSet};

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use crate::exclusions::{parse_patterns, ByteRanges, CategorySizes};
use crate::fat_map::RawVolume;

const SB_MAGIC: u32 = 0x5846_5342; // "XFSB"
const DINODE_MAGIC: u16 = 0x494E; // "IN"
const AGF_MAGIC: u32 = 0x5841_4746; // "XAGF"
const DIR3_DATA_MAGIC: u32 = 0x5844_4433; // "XDD3"
const DIR3_BLOCK_MAGIC: u32 = 0x5844_4233; // "XDB3"
const BMBT_CRC_MAGIC: u32 = 0x424D_4133; // "BMA3"
const REFC_CRC_MAGIC: u32 = 0x5233_4643; // "R3FC"

// di_format
const FMT_LOCAL: u8 = 1;
const FMT_EXTENTS: u8 = 2;
const FMT_BTREE: u8 = 3;

const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;

/// v5 inode: 176-byte core, then the literal (fork) area.
const DINODE_CORE_V3: usize = 176;
/// v5 dir data block header (xfs_dir3_data_hdr).
const DIR3_DATA_HDR: usize = 64;
/// v5 short-form btree block header (bmbt / refcount).
const BTREE_HDR_V5: usize = 56;
const BMBT_HDR_V5: usize = 72;

/// refcount record COW-staging flag (top bit of rc_startblock).
const REFC_COW_FLAG: u32 = 1 << 31;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs exclusions: {msg}"))
}

/// Incompatible features whose on-disk layout this code reads correctly:
/// FTYPE, SPINODES, META_UUID, BIGTIME and NREXT64. Anything else (exchange
/// ranges, parent pointers, a metadata directory…) may move what we read, and
/// a volume we would misread is refused rather than half-understood.
const KNOWN_INCOMPAT: u64 = 0x1 | 0x2 | 0x4 | 0x8 | 0x20;

/// `XFS_DIFLAG2_NREXT64` in `di_flags2`: the inode keeps its data-fork extent
/// count in the 64-bit `di_big_nextents` at 0x18, and the old 32-bit field at
/// 0x4C means the attribute fork's. `mkfs.xfs` 6.x sets `nrext64` by default;
/// until 2026-10-17 this code read 0x4C regardless, saw no extents even in
/// directories, and on such a volume excluded nothing at all, silently.
const DIFLAG2_NREXT64: u64 = 1 << 4;

/// Does this v3 inode use the large extent counters?
pub(crate) fn nrext64(inode: &[u8]) -> bool {
    inode.len() >= 0x80 && be64(inode, 0x78) & DIFLAG2_NREXT64 != 0
}

/// The data fork's extent count, wherever this inode keeps it.
pub(crate) fn data_nextents(inode: &[u8]) -> u64 {
    if nrext64(inode) {
        be64(inode, 0x18)
    } else {
        be32(inode, 0x4C)
    }
}

fn be16(b: &[u8], o: usize) -> u64 {
    u16::from_be_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn be32(b: &[u8], o: usize) -> u64 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Result of an xfs exclusion walk. `patches` is empty unless Stage 2 ran and
/// every candidate passed all three of its steps.
pub struct XfsExclusionScan {
    /// PHYSICAL volume byte ranges of exclusive excluded extents — zeroed in
    /// the image.
    pub ranges: ByteRanges,
    /// Blocks Stage 2 actually returned to the free pool — safe to leave out of
    /// the image entirely, because the filesystem no longer reads them. Empty
    /// when Stage 2 is off or spared the file: a zero-filled ghost is still
    /// listed in the catalog, so its sectors must be stored as the zeros they
    /// became.
    pub droppable: ByteRanges,
    pub files_excluded: u64,
    pub entries_scanned: u64,
    pub bytes_excluded: u64,
    /// Files matched but kept because their data is shared — a reflink/CoW
    /// extent, or a hard link (the other name is not excluded).
    pub files_kept_shared: u64,
    pub categories: CategorySizes,
    pub patches: Vec<(u64, Vec<u8>)>,
    /// Кого Stage 2 пощадил и почему: файл остался в каталоге нулевым
    /// призраком. Наружу это идёт предупреждением, а не тишиной.
    pub stage2_spared: Vec<(u64, String)>,
    /// Файлы, чьи данные Stage 1 занулил — то есть кандидаты на НАСТОЯЩЕЕ
    /// удаление (этап D). Stage 1 сам их не трогает структурно.
    pub(crate) removable: Vec<RemovableFile>,
}

impl XfsExclusionScan {
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
            stage2_spared: Vec::new(),
            removable: Vec::new(),
        }
    }
}

pub(crate) struct Sb {
    pub(crate) blocksize: u64,
    /// sb_logstart / sb_logblocks — the internal log, or 0 for an external one.
    pub(crate) logstart: u64,
    pub(crate) logblocks: u64,
    pub(crate) inodesize: u64,
    pub(crate) agblocks: u64,
    pub(crate) agcount: u64,
    pub(crate) agblklog: u32,
    pub(crate) inopblog: u32,
    pub(crate) rootino: u64,
    pub(crate) sectsize: u64,
    pub(crate) reflink: bool,
    pub(crate) ftype: bool,
    /// Разреженные чанки инодов (`XFS_SB_FEAT_INCOMPAT_SPINODES`, бит 1) —
    /// меняют раскладку записи INOBT. Включены современным mkfs по умолчанию
    /// (проверено: `incompat=0xb` на mkfs.xfs 6.x).
    pub(crate) spinodes: bool,
}

impl Sb {
    /// Byte offset of inode `ino` on the volume.
    pub(crate) fn inode_offset(&self, ino: u64) -> u64 {
        let mask = (1u64 << (self.agblklog + self.inopblog)) - 1;
        let agino = ino & mask;
        let ag = ino >> (self.agblklog + self.inopblog);
        let agblock = agino >> self.inopblog;
        let off = agino & ((1u64 << self.inopblog) - 1);
        (ag * self.agblocks + agblock) * self.blocksize + off * self.inodesize
    }
    /// Physical byte offset of an fs-block number (agno<<agblklog | agblock).
    pub(crate) fn fsblock_to_phys(&self, fsblock: u64) -> u64 {
        let ag = fsblock >> self.agblklog;
        let agblock = fsblock & ((1u64 << self.agblklog) - 1);
        (ag * self.agblocks + agblock) * self.blocksize
    }
}

pub(crate) fn parse_sb(vol: &mut dyn RawVolume) -> Result<Sb> {
    let mut sb = [0u8; 512];
    vol.read_at(0, &mut sb)?;
    if be32(&sb, 0) as u32 != SB_MAGIC {
        return Err(bad("no XFSB superblock magic"));
    }
    let versionnum = be16(&sb, 0x64);
    if versionnum & 0xF != 5 {
        return Err(bad("not a v5 (crc) xfs — exclusions unsupported"));
    }
    let blocksize = be32(&sb, 0x04);
    let inodesize = be16(&sb, 0x68);
    let agblocks = be32(&sb, 0x54);
    let agcount = be32(&sb, 0x58);
    let rootino = be64(&sb, 0x38);
    let logstart = be64(&sb, 0x30);
    let logblocks = be32(&sb, 0x60);
    let sectsize = be16(&sb, 0x66);
    let inopblog = sb[0x7B] as u32;
    let agblklog = sb[0x7C] as u32;
    let ro_compat = be32(&sb, 0xD4);
    let incompat = be32(&sb, 0xD8);
    if !(512..=65536).contains(&blocksize) || !blocksize.is_power_of_two() {
        return Err(bad("implausible block size"));
    }
    if inodesize < 256 || !inodesize.is_power_of_two() {
        return Err(bad("implausible inode size"));
    }
    if agblocks == 0 || agcount == 0 {
        return Err(bad("zero AG geometry"));
    }
    if incompat & !KNOWN_INCOMPAT != 0 {
        return Err(bad(format!(
            "unsupported incompatible features 0x{incompat:x}"
        )));
    }
    if blocksize * agblocks * agcount > vol.len() {
        return Err(bad("filesystem larger than the device"));
    }
    Ok(Sb {
        blocksize,
        logstart,
        logblocks,
        inodesize,
        agblocks,
        agcount,
        agblklog,
        inopblog,
        rootino,
        sectsize,
        reflink: ro_compat & 0x4 != 0, // XFS_SB_FEAT_RO_COMPAT_REFLINK
        ftype: incompat & 0x1 != 0,    // XFS_SB_FEAT_INCOMPAT_FTYPE
        spinodes: incompat & 0x2 != 0, // XFS_SB_FEAT_INCOMPAT_SPINODES
    })
}

/// XFS has no "needs recovery" flag: the only way to know whether the log
/// holds work is to look at the log.
///
/// It matters here more than the flag does on ext4. Our xfs pass reads the
/// inode and directory structures to decide which blocks belong to an excluded
/// file, and zeroes them. If the log holds transactions that moved those
/// blocks to some other file, the replay at first mount hands the survivor a
/// hole full of zeros. So: read the log, find the record with the highest LSN,
/// and require it to be the unmount record a clean shutdown leaves behind.
/// Anything we cannot read confidently counts as dirty — refusing costs a few
/// gigabytes in the copy, guessing costs someone else's file.
fn log_is_clean(vol: &mut dyn RawVolume, sb: &Sb) -> Result<bool> {
    if sb.logstart == 0 {
        return Ok(false); // external log device — not ours to inspect
    }
    xfs_log_is_clean_at(
        vol,
        sb.fsblock_to_phys(sb.logstart),
        sb.logblocks * sb.blocksize,
        sb.sectsize,
    )
}

/// The scan itself, over an already-located log. Kept free of the exclusion
/// scanner's superblock struct so the used-map builder — which accepts older
/// filesystems this module refuses outright — can ask the same question.
pub(crate) fn xfs_log_is_clean_at(
    vol: &mut dyn RawVolume,
    log_off: u64,
    log_len: u64,
    sectsize: u64,
) -> Result<bool> {
    const XLOG_MAGIC: u32 = 0xFEED_BABE;
    const XLOG_UNMOUNT_TYPE: u16 = 0x556E; // "Un"

    if log_len == 0 || log_off + log_len > vol.len() {
        return Ok(false);
    }
    let sect = sectsize.max(512) as usize;
    let mut best: Option<(u64, u64)> = None; // (lsn, absolute offset)
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < log_len {
        let want = ((log_len - pos) as usize).min(buf.len());
        vol.read_at(log_off + pos, &mut buf[..want])?;
        let mut o = 0usize;
        while o + sect <= want {
            if be32(&buf, o) as u32 == XLOG_MAGIC {
                let lsn = be64(&buf, o + 0x10);
                if best.map(|(b, _)| lsn > b).unwrap_or(true) {
                    best = Some((lsn, log_off + pos + o as u64));
                }
            }
            o += sect;
        }
        pos += want as u64;
    }

    let Some((_, head)) = best else {
        return Ok(true); // never written: a fresh mkfs log holds nothing
    };
    // The unmount record is one operation long, and its payload says so.
    let mut rec = vec![0u8; sect + 16];
    vol.read_at(head, &mut rec)?;
    let num_logops = be32(&rec, 0x28);
    if num_logops != 1 {
        return Ok(false);
    }
    let magic_off = sect + 12; // record header, then xlog_op_header (12 B)
    if magic_off + 2 > rec.len() {
        return Ok(false);
    }
    let be = u16::from_be_bytes(rec[magic_off..magic_off + 2].try_into().unwrap());
    let le = u16::from_le_bytes(rec[magic_off..magic_off + 2].try_into().unwrap());
    Ok(be == XLOG_UNMOUNT_TYPE || le == XLOG_UNMOUNT_TYPE)
}

/// A file Stage 1 zeroed: everything etап D needs to remove it for real.
pub(crate) struct RemovableFile {
    pub(crate) ino: u64,
    /// The directory holding the excluded name (Stage 2 has to edit it).
    pub(crate) parent: u64,
    /// The name to take out of that directory — Stage 2 removes entries by
    /// name, and the inode number alone does not identify one.
    pub(crate) name: String,
    pub(crate) extents: Vec<Extent>,
    pub(crate) size: u64,
}

/// A resolved data extent (fs-block start + block count).
#[derive(Clone, Copy)]
pub(crate) struct Extent {
    pub(crate) startblock: u64,
    pub(crate) count: u64,
}

/// Decode one 16-byte bmbt record (packed 128-bit) → (startoff, extent).
fn decode_bmbt_rec(b: &[u8], o: usize) -> (u64, Extent) {
    let l0 = be64(b, o);
    let l1 = be64(b, o + 8);
    let startoff = (l0 >> 9) & ((1u64 << 54) - 1);
    let startblock = ((l0 & 0x1FF) << 43) | (l1 >> 21);
    let count = l1 & ((1u64 << 21) - 1);
    (startoff, Extent { startblock, count })
}

/// Read inode `ino` (its `inodesize` bytes).
pub(crate) fn read_inode(vol: &mut dyn RawVolume, sb: &Sb, ino: u64) -> Result<Vec<u8>> {
    let off = sb.inode_offset(ino);
    if off + sb.inodesize > vol.len() {
        return Err(bad("inode past device"));
    }
    let mut buf = vec![0u8; sb.inodesize as usize];
    vol.read_at(off, &mut buf)?;
    if be16(&buf, 0) as u16 != DINODE_MAGIC {
        return Err(bad("bad inode magic"));
    }
    Ok(buf)
}

/// The data fork literal area of an inode (`[176, 176 + forkoff*8)` or, when
/// forkoff==0, to the end of the inode).
pub(crate) fn data_fork(inode: &[u8], sb: &Sb) -> (usize, usize) {
    let forkoff = inode[0x52] as usize;
    let end = if forkoff == 0 {
        sb.inodesize as usize
    } else {
        DINODE_CORE_V3 + forkoff * 8
    };
    (DINODE_CORE_V3, end.min(inode.len()))
}

/// Resolve a data/attr fork of format EXTENTS or BTREE into extents. `nextents`
/// bounds the extents-format reads; BTREE walks child bmbt blocks.
fn fork_extents(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    inode: &[u8],
    format: u8,
    nextents: u64,
) -> Result<Vec<Extent>> {
    Ok(fork_extents_at(vol, sb, inode, format, nextents)?
        .into_iter()
        .map(|(_, e)| e)
        .collect())
}

/// Same, keeping each extent's LOGICAL offset in the fork. Directory work needs
/// it: a dirent's address is derived from the logical block number, so an
/// extent list without offsets cannot locate the matching hash entry.
pub(crate) fn fork_extents_at(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    inode: &[u8],
    format: u8,
    nextents: u64,
) -> Result<Vec<(u64, Extent)>> {
    let (fs, fe) = data_fork(inode, sb);
    let mut out = Vec::new();
    match format {
        FMT_EXTENTS => {
            for i in 0..nextents as usize {
                let o = fs + i * 16;
                if o + 16 > fe {
                    return Err(bad("extent list exceeds fork"));
                }
                let (off, e) = decode_bmbt_rec(inode, o);
                if e.count > 0 {
                    out.push((off, e));
                }
            }
        }
        FMT_BTREE => {
            // Inode bmdr root: bb_level(2) bb_numrecs(2) then keys[be64] + ptrs[be64].
            let level = be16(inode, fs);
            let numrecs = be16(inode, fs + 2) as usize;
            // Pointers sit in the second half of the fork's usable area.
            let maxrecs = ((fe - fs - 4) / 16).max(1);
            let ptr_base = fs + 4 + maxrecs * 8;
            let mut ptrs = Vec::new();
            for i in 0..numrecs {
                if ptr_base + i * 8 + 8 > fe {
                    return Err(bad("bmbt root ptr out of range"));
                }
                ptrs.push(be64(inode, ptr_base + i * 8));
            }
            for p in ptrs {
                walk_bmbt(vol, sb, p, level, &mut out, 0)?;
            }
        }
        _ => return Err(bad("unsupported fork format")),
    }
    Ok(out)
}

/// Walk a bmbt btree block (fs-block `fsblock`) collecting leaf extents.
fn walk_bmbt(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    fsblock: u64,
    _parent_level: u64,
    out: &mut Vec<(u64, Extent)>,
    depth: u32,
) -> Result<()> {
    if depth > 16 {
        return Err(bad("bmbt too deep"));
    }
    let mut blk = vec![0u8; sb.blocksize as usize];
    vol.read_at(sb.fsblock_to_phys(fsblock), &mut blk)?;
    if be32(&blk, 0) as u32 != BMBT_CRC_MAGIC {
        return Err(bad("bad bmbt block magic"));
    }
    let level = be16(&blk, 4);
    let numrecs = be16(&blk, 6) as usize;
    if level == 0 {
        for i in 0..numrecs {
            let o = BMBT_HDR_V5 + i * 16;
            if o + 16 > blk.len() {
                return Err(bad("bmbt leaf rec out of range"));
            }
            let (off, e) = decode_bmbt_rec(&blk, o);
            if e.count > 0 {
                out.push((off, e));
            }
        }
    } else {
        // keys[numrecs] (be64) then ptrs[numrecs] (be64).
        let maxrecs = (sb.blocksize as usize - BMBT_HDR_V5) / 16;
        let ptr_base = BMBT_HDR_V5 + maxrecs * 8;
        for i in 0..numrecs {
            if ptr_base + i * 8 + 8 > blk.len() {
                return Err(bad("bmbt node ptr out of range"));
            }
            let child = be64(&blk, ptr_base + i * 8);
            walk_bmbt(vol, sb, child, level - 1, out, depth + 1)?;
        }
    }
    Ok(())
}

/// One directory entry.
struct DirEnt {
    ino: u64,
    name: String,
    is_dir: bool,
}

/// Enumerate a directory inode's entries (shortform or dir2 data blocks).
/// Returns an error for btree-format directories (caller keeps the subtree).
fn read_dir(vol: &mut dyn RawVolume, sb: &Sb, dir_inode: &[u8]) -> Result<Vec<DirEnt>> {
    let format = dir_inode[5];
    let mut out = Vec::new();
    match format {
        FMT_LOCAL => {
            let (fs, fe) = data_fork(dir_inode, sb);
            let sf = &dir_inode[fs..fe];
            if sf.len() < 2 {
                return Ok(out);
            }
            let count = sf[0] as usize;
            let i8 = sf[1] != 0;
            let ino_sz = if i8 { 8 } else { 4 };
            let mut p = 2 + ino_sz; // skip parent
            for _ in 0..count {
                if p + 3 > sf.len() {
                    break;
                }
                let namelen = sf[p] as usize;
                // namelen(1) offset(2) name ftype(1?) inumber
                let name_off = p + 3;
                if name_off + namelen > sf.len() {
                    break;
                }
                let name = String::from_utf8_lossy(&sf[name_off..name_off + namelen]).to_string();
                let mut q = name_off + namelen;
                let ftype = if sb.ftype {
                    let t = sf.get(q).copied().unwrap_or(0);
                    q += 1;
                    t
                } else {
                    0
                };
                if q + ino_sz > sf.len() {
                    break;
                }
                let ino = if i8 { be64(sf, q) } else { be32(sf, q) };
                out.push(DirEnt {
                    ino,
                    name,
                    is_dir: ftype == 2,
                });
                p = q + ino_sz; // advance to the next shortform entry
            }
        }
        FMT_EXTENTS | FMT_BTREE => {
            let nextents = data_nextents(dir_inode);
            let extents = fork_extents(vol, sb, dir_inode, format, nextents)?;
            let mut blk = vec![0u8; sb.blocksize as usize];
            for e in extents {
                for bi in 0..e.count {
                    vol.read_at(sb.fsblock_to_phys(e.startblock + bi), &mut blk)?;
                    let magic = be32(&blk, 0) as u32;
                    if magic != DIR3_DATA_MAGIC && magic != DIR3_BLOCK_MAGIC {
                        continue; // leaf/node/free block — skip
                    }
                    parse_dir2_data(&blk, sb, &mut out)?;
                }
            }
        }
        _ => return Err(bad("unsupported directory format")),
    }
    Ok(out)
}

/// Parse one dir2 data block's entries.
fn parse_dir2_data(blk: &[u8], sb: &Sb, out: &mut Vec<DirEnt>) -> Result<()> {
    let mut p = DIR3_DATA_HDR;
    // A block-format single dir has a leaf/tail; we stop when entries run out.
    while p + 8 <= blk.len() {
        // Unused entry: freetag 0xFFFF then length(2).
        if be16(blk, p) == 0xFFFF {
            let len = be16(blk, p + 2) as usize;
            if len == 0 {
                break;
            }
            p += len;
            continue;
        }
        // xfs_dir2_data_entry: inumber(8) namelen(1) name[] ftype(1?) tag(2).
        let ino = be64(blk, p);
        let namelen = blk[p + 8] as usize;
        if namelen == 0 || p + 9 + namelen > blk.len() {
            break;
        }
        let name = String::from_utf8_lossy(&blk[p + 9..p + 9 + namelen]).to_string();
        let ftype = if sb.ftype {
            blk.get(p + 9 + namelen).copied().unwrap_or(0)
        } else {
            0
        };
        if name != "." && name != ".." {
            out.push(DirEnt {
                ino,
                name,
                is_dir: ftype == 2,
            });
        }
        // rec len = 8 + 1 + namelen + (ftype?1:0) + 2, rounded up to 8.
        let raw = 8 + 1 + namelen + if sb.ftype { 1 } else { 0 } + 2;
        p += (raw + 7) & !7;
    }
    Ok(())
}

/// Collect the shared block ranges (refcount > 1, non-COW) of every AG by
/// walking each AG's refcount B+tree. Ranges are in fs-block numbers.
fn collect_shared(vol: &mut dyn RawVolume, sb: &Sb) -> Result<ByteRanges> {
    let mut shared = ByteRanges::new();
    for ag in 0..sb.agcount {
        // AGF is sector 1 of the AG.
        let agf_off = ag * sb.agblocks * sb.blocksize + sb.sectsize;
        let mut agf = vec![0u8; sb.sectsize as usize];
        vol.read_at(agf_off, &mut agf)?;
        if be32(&agf, 0) as u32 != AGF_MAGIC {
            return Err(bad("bad AGF magic"));
        }
        let root = be32(&agf, 0x58);
        let level = be32(&agf, 0x5C);
        if root == 0 || level == 0 {
            continue; // no refcount btree in this AG
        }
        walk_refcount(vol, sb, ag, root, &mut shared)?;
    }
    shared.finalize();
    Ok(shared)
}

fn walk_refcount(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    ag: u64,
    agblock: u64,
    shared: &mut ByteRanges,
) -> Result<()> {
    let phys = (ag * sb.agblocks + agblock) * sb.blocksize;
    let mut blk = vec![0u8; sb.blocksize as usize];
    vol.read_at(phys, &mut blk)?;
    if be32(&blk, 0) as u32 != REFC_CRC_MAGIC {
        return Err(bad("bad refcount btree magic"));
    }
    let level = be16(&blk, 4);
    let numrecs = be16(&blk, 6) as usize;
    if level == 0 {
        for i in 0..numrecs {
            let o = BTREE_HDR_V5 + i * 12;
            if o + 12 > blk.len() {
                return Err(bad("refcount rec out of range"));
            }
            let start = be32(&blk, o) as u32;
            let count = be32(&blk, o + 4);
            let refc = be32(&blk, o + 8);
            if start & REFC_COW_FLAG != 0 {
                continue; // CoW staging extent, not a shared data extent
            }
            if refc > 1 {
                // fs-block range → mark shared (as byte range in fs-block units).
                let s = (ag << sb.agblklog) | start as u64;
                shared.add(s, s + count);
            }
        }
    } else {
        // keys[numrecs] (xfs_refcount_key: startblock be32, 4 bytes) then
        // ptrs[numrecs] (be32 agblock).
        let maxrecs = (sb.blocksize as usize - BTREE_HDR_V5) / 8;
        let ptr_base = BTREE_HDR_V5 + maxrecs * 4;
        for i in 0..numrecs {
            if ptr_base + i * 4 + 4 > blk.len() {
                return Err(bad("refcount node ptr out of range"));
            }
            let child = be32(&blk, ptr_base + i * 4);
            walk_refcount(vol, sb, ag, child, shared)?;
        }
    }
    Ok(())
}

/// Walk the directory tree, match `patterns`, and zero exclusively-owned data
/// extents of matched files (shared extents kept — reflink safety).
/// `stage2` включает НАСТОЯЩЕЕ удаление (шаги D2–D5): записи каталогов, иноды
/// и блоки. Без него остаётся Stage 1 — данные занулены, файл в каталоге.
pub fn scan_xfs_exclusions(
    vol: &mut dyn RawVolume,
    patterns: &[String],
    stage2: bool,
    mut tick: impl FnMut(u64) -> Result<()>,
) -> Result<XfsExclusionScan> {
    let mut scan = XfsExclusionScan::empty();
    let parsed = parse_patterns(patterns);
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }
    let sb = parse_sb(vol)?;
    if !log_is_clean(vol, &sb)? {
        return Err(bad(tr!("xfs_exclusions.log_dirty")));
    }
    // Shared block set (fs-block ranges) — empty when reflink is off.
    let shared = if sb.reflink {
        collect_shared(vol, &sb)?
    } else {
        ByteRanges::new()
    };

    // BFS the directory tree from the root inode.
    let mut visited: HashSet<u64> = HashSet::new();
    let mut stack: Vec<(u64, Vec<String>, bool)> = vec![(sb.rootino, Vec::new(), false)];
    // Per matched file: its extents (to zero if exclusive).
    // ino → (extents, size, parent dir ino)
    let mut matched_files: HashMap<u64, (Vec<Extent>, u64, u64, String)> = HashMap::new();

    while let Some((ino, rel, sub_excluded)) = stack.pop() {
        if !visited.insert(ino) {
            continue;
        }
        let inode = match read_inode(vol, &sb, ino) {
            Ok(i) => i,
            Err(e) => {
                warn!(ino, error = %e, "xfs exclusions: unreadable inode — skipping");
                continue;
            }
        };
        if be16(&inode, 2) as u16 & S_IFMT != S_IFDIR {
            continue;
        }
        let ents = match read_dir(vol, &sb, &inode) {
            Ok(e) => e,
            Err(e) => {
                warn!(ino, error = %e, "xfs exclusions: cannot read directory — keeping subtree");
                continue;
            }
        };
        for e in ents {
            scan.entries_scanned += 1;
            if scan.entries_scanned % 4096 == 0 {
                tick(scan.entries_scanned)?;
            }
            let name_lower = e.name.to_lowercase();
            let mut crel = rel.clone();
            crel.push(name_lower.clone());
            let mut hit = sub_excluded || parsed.iter().any(|p| p.matches(&name_lower, &crel));
            // `/proc`, `/sys`, `/dev`, `/run`, `/tmp` are emptied, never
            // removed: systemd mounts on them before it can do anything, and a
            // root without them panics the kernel instead of booting.
            if hit && e.is_dir && crate::exclusions::is_essential_linux_dir(&crel) {
                warn!(dir = %e.name, "exclusions: kept an essential mount point (contents still excluded)");
                hit = false;
                stack.push((e.ino, crel, true));
                continue;
            }
            if e.is_dir {
                stack.push((e.ino, crel, hit));
            } else if hit {
                if let Ok(fi) = read_inode(vol, &sb, e.ino) {
                    let mode = be16(&fi, 2) as u16;
                    if mode & S_IFMT != S_IFREG {
                        continue; // symlinks/devices: no data blocks to zero
                    }
                    // A hard-linked file is kept whole: its other name is not
                    // excluded, and zeroing the shared inode's data would
                    // silently empty that innocent file — xfs does not checksum
                    // data, so nothing would even notice. Same rule as
                    // ext4/btrfs/NTFS.
                    if be32(&fi, 0x10) > 1 {
                        scan.files_kept_shared += 1;
                        continue;
                    }
                    let size = be64(&fi, 0x38);
                    let format = fi[5];
                    let nextents = data_nextents(&fi);
                    if let Ok(exts) = fork_extents(vol, &sb, &fi, format, nextents) {
                        matched_files.insert(e.ino, (exts, size, ino, e.name.clone()));
                    }
                }
            } else if let Ok(fi) = read_inode(vol, &sb, e.ino) {
                let size = be64(&fi, 0x38);
                scan.categories.add(&crel, size);
            }
        }
    }

    // Zero exclusively-owned extents; keep any that overlap a shared range.
    scan.files_excluded = matched_files.len() as u64;
    for (&fino, (exts, size, parent, name)) in &matched_files {
        let mut any_shared = false;
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for e in exts {
            let s = e.startblock;
            if shared.overlaps(s, s + e.count) {
                any_shared = true;
                break;
            }
            ranges.push((
                sb.fsblock_to_phys(s),
                sb.fsblock_to_phys(s) + e.count * sb.blocksize,
            ));
        }
        if any_shared {
            scan.files_kept_shared += 1;
            continue;
        }
        for (a, b) in ranges {
            scan.ranges.add(a, b);
        }
        scan.bytes_excluded += size;
        scan.removable.push(RemovableFile {
            ino: fino,
            parent: *parent,
            name: name.clone(),
            extents: exts.clone(),
            size: *size,
        });
    }
    scan.ranges.finalize();

    // Stage 2: настоящее удаление. Патчи появляются только если весь набор
    // прошёл все три шага; пощажённые файлы остаются нулевыми призраками, и их
    // блоки обязаны остаться в образе — ФС всё ещё их читает.
    if stage2 && !scan.removable.is_empty() {
        match crate::xfs_stage2::plan_stage2(vol, &sb, &scan.removable) {
            Ok(outcome) => {
                let removed: std::collections::HashSet<u64> =
                    outcome.removed.iter().copied().collect();
                for f in &scan.removable {
                    if !removed.contains(&f.ino) {
                        continue;
                    }
                    for e in &f.extents {
                        let a = sb.fsblock_to_phys(e.startblock);
                        scan.droppable.add(a, a + e.count * sb.blocksize);
                    }
                }
                scan.droppable.finalize();
                // По одной строке на файл — это тысячи строк; журналу нужна
                // сводка по причинам, а подробности лежат в самом результате.
                let mut tally: std::collections::BTreeMap<&str, u64> = Default::default();
                for (_, why) in &outcome.spared {
                    *tally.entry(why.as_str()).or_default() += 1;
                }
                for (why, n) in &tally {
                    warn!(files = n, reason = %why, "xfs stage2: файлы оставлены на Stage 1");
                }
                scan.stage2_spared = outcome.spared;
                info!(
                    removed = outcome.removed.len(),
                    spared = scan.stage2_spared.len(),
                    blocks_freed = outcome.blocks_freed,
                    patches = outcome.patches.len(),
                    "xfs exclusions: Stage 2 complete"
                );
                scan.patches = outcome.patches;
            }
            Err(e) => {
                // Ни одного патча наружу: Stage 1 уже полноценный результат.
                eprintln!("ОШИБКА plan_stage2: {e}");
                warn!(error = %e, "xfs stage2: настоящее удаление не удалось — остаётся зануление");
            }
        }
    }

    info!(
        files = scan.files_excluded,
        kept_shared = scan.files_kept_shared,
        bytes = scan.bytes_excluded,
        ranges = scan.ranges.iter().count(),
        patches = scan.patches.len(),
        "xfs exclusions: scan complete"
    );
    Ok(scan)
}

#[cfg(all(test, target_os = "linux"))]
mod xfs_integration {
    use super::*;
    use std::os::unix::fs::FileExt;
    use std::path::Path;

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

    /// Тот же прогон, но с НАСТОЯЩИМ удалением (Stage 2).
    fn run2(img: &Path, patterns: &[&str]) -> XfsExclusionScan {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        scan_xfs_exclusions(&mut vol, &pats, true, |_| Ok(())).unwrap()
    }

    fn run(img: &Path, patterns: &[&str]) -> (XfsExclusionScan, FileVol) {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        let scan = scan_xfs_exclusions(&mut vol, &pats, false, |_| Ok(())).unwrap();
        (scan, vol)
    }

    /// The reflink+private fixture is generated by tests/data/gen_xfs.sh (needs
    /// root/mount) and stored compressed. private.bin → zeroed; the reflinked
    /// shared file → kept; xfs_repair stays clean after zeroing.
    #[test]
    fn stage1_zeroes_private_keeps_shared_repair_clean() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xfs_reflink.img.zst");
        if !zst.exists() {
            eprintln!("xfs_reflink.img.zst missing — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        let raw = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        std::fs::write(&img, &raw).unwrap();

        // Exclude the shared file (kept) AND the private one (zeroed).
        let (scan, mut vol) = run(&img, &["secret.log", "cache"]);
        assert!(
            scan.files_kept_shared >= 1,
            "shared file not detected as shared"
        );
        assert!(!scan.ranges.is_empty(), "private file not zeroed");

        // No zeroed byte may be the kept file's 0xCD; must cover the private 0xAB.
        let mut buf = vec![0u8; 65536];
        for (s, e) in scan.ranges.iter() {
            let mut o = s;
            while o < e {
                let n = ((e - o) as usize).min(buf.len());
                vol.read_at(o, &mut buf[..n]).unwrap();
                assert!(!buf[..n].contains(&0xCD), "zeroed a kept file's data");
                o += n as u64;
            }
        }

        // Apply zeroing → xfs_repair -n must stay clean (only data touched).
        if std::process::Command::new("xfs_repair")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let mut data = std::fs::read(&img).unwrap();
        for (s, e) in scan.ranges.iter() {
            data[s as usize..e as usize].fill(0);
        }
        let patched = dir.path().join("patched.img");
        std::fs::write(&patched, &data).unwrap();
        let out = std::process::Command::new("xfs_repair")
            .arg("-n")
            .arg(&patched)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "xfs_repair found errors:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// XFS keeps no "needs recovery" flag, so the log itself is the only
    /// witness. The fixture was unmounted cleanly, so it must read as clean;
    /// overwrite the unmount record's magic and it must read as dirty — and
    /// both entry points must then refuse rather than work from stale btrees.
    #[test]
    fn a_dirty_log_refuses_scan_and_used_map() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xfs_reflink.img.zst");
        if !zst.exists() {
            eprintln!("xfs_reflink.img.zst missing — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        let raw = zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        std::fs::write(&img, &raw).unwrap();

        // Sanity: a cleanly unmounted filesystem must not read as dirty, or the
        // guard would simply switch xfs exclusions off for everyone.
        let (scan, _) = run(&img, &["cache"]);
        assert!(
            scan.files_excluded > 0,
            "чистый лог принят за грязный — исключения бы отключились"
        );

        // Find the unmount record and break its magic.
        let mut data = raw.clone();
        let sect = u16::from_be_bytes(data[0x66..0x68].try_into().unwrap()) as usize;
        let bs = u32::from_be_bytes(data[0x04..0x08].try_into().unwrap()) as u64;
        let logstart = u64::from_be_bytes(data[0x30..0x38].try_into().unwrap());
        let agblocks = u32::from_be_bytes(data[0x54..0x58].try_into().unwrap()) as u64;
        let agblklog = data[0x7C] as u32;
        let log_off =
            ((logstart >> agblklog) * agblocks + (logstart & ((1u64 << agblklog) - 1))) * bs;
        let logblocks = u32::from_be_bytes(data[0x60..0x64].try_into().unwrap()) as u64;
        let mut best: Option<(u64, usize)> = None;
        let mut o = log_off as usize;
        while o + sect <= (log_off + logblocks * bs) as usize {
            if u32::from_be_bytes(data[o..o + 4].try_into().unwrap()) == 0xFEED_BABE {
                let lsn = u64::from_be_bytes(data[o + 0x10..o + 0x18].try_into().unwrap());
                if best.map(|(b, _)| lsn > b).unwrap_or(true) {
                    best = Some((lsn, o));
                }
            }
            o += sect;
        }
        let (_, head) = best.expect("в фикстуре не нашлось ни одной записи лога");
        let magic_off = head + sect + 12;
        data[magic_off] ^= 0xFF; // no longer an unmount record
        let dirty = dir.path().join("dirty.img");
        std::fs::write(&dirty, &data).unwrap();

        let f = std::fs::File::open(&dirty).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        assert!(
            scan_xfs_exclusions(&mut vol, &["cache".to_string()], false, |_| Ok(())).is_err(),
            "исключения должны отказать на грязном логе"
        );
        assert!(
            crate::xfs_map::build_xfs_used_map(&mut vol, 1 << 20).is_err(),
            "карта занятых блоков должна отказать на грязном логе"
        );
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xfs_reflink.img.zst");
        if !zst.exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::write(
            &img,
            zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap(),
        )
        .unwrap();
        let (scan, _) = run(&img, &[]);
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.ranges.is_empty());
    }

    /// A hard-linked file must be kept whole: its other name is not excluded,
    /// and zeroing the shared inode's data would silently empty that innocent
    /// file. (The refcount btree — reflink sharing — knows nothing about NAME
    /// sharing, which is exactly why a separate guard is needed.)
    ///
    /// The pair is real: `gen_xfs.sh` makes `hard_a.bin`/`hard_b.bin` one inode
    /// under two names. Until 2026-08-05 the committed fixture predated that
    /// and the test had to fake the link count with `xfs_db write core.nlinkv2`
    /// — which proved the nlink guard fires, but not that a second NAME is what
    /// raises it.
    #[test]
    fn a_hard_linked_file_is_kept_whole() {
        let zst = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xfs_reflink.img.zst");
        if !zst.exists() {
            eprintln!("xfs_reflink.img.zst missing — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::write(
            &img,
            zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap(),
        )
        .unwrap();

        // Sanity: a file with ONE name is zeroed — otherwise the assert below
        // would pass on a scanner that excludes nothing at all.
        let (scan, _) = run(&img, &["big.bin"]);
        assert!(
            !scan.ranges.is_empty(),
            "sanity: big.bin should be zeroable when unlinked"
        );

        // One name of the pair excluded, the other not: nothing may be zeroed.
        let (scan, _) = run(&img, &["hard_a.bin"]);
        assert!(scan.ranges.is_empty(), "hard-linked file was zeroed");
        assert_eq!(scan.files_kept_shared, 1, "kept-hardlink accounting");
    }

    /// The dynamic half of the acceptance: mount what a restore would produce,
    /// read every kept file, write until the space is reused, read again,
    /// unmount, `xfs_repair -n`. XFS is Stage-1-only today (data zeroed, no
    /// metadata edits), so what this guards is the zeroing itself — an extent
    /// zeroed under a file that still owns it is invisible to `xfs_repair`,
    /// which checks structure and never looks at file contents.
    ///
    /// The fixture is the shared corpus (`exclusion_corpus`): dense and
    /// nested-dense directories, hard links inside and across the exclusion,
    /// device nodes, FIFOs, sockets, symlinks, xattrs and a reflink copy whose
    /// extents are shared with an excluded file.
    ///
    /// Needs root (loop mount):
    ///
    /// ```sh
    /// cargo test -p own-backup-backup --lib --no-run     # build as yourself
    /// sudo <test-binary> --ignored xfs_survives_mount_and_write --nocapture
    /// ```
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn xfs_survives_mount_and_write() {
        use crate::exclusion_corpus as corpus;
        if !corpus::is_root() {
            eprintln!("not root — skipping");
            return;
        }
        for t in ["mkfs.xfs", "xfs_repair"] {
            if std::process::Command::new(t).arg("-V").output().is_err() {
                eprintln!("{t} unavailable — skipping");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(768 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(&img)
            .status()
            .unwrap()
            .success());
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        let c = {
            let _m = corpus::mounted(&img, &mnt);
            corpus::populate(&mnt, corpus::Caps::xfs())
        };

        let (scan, _) = run(&img, &["cache"]);
        assert!(
            scan.files_excluded >= 1400,
            "исключено всего {}",
            scan.files_excluded
        );
        // The reflink clone and both hard-linked files are shared — nothing of
        // theirs may be zeroed, and the scanner has to say so out loud.
        assert!(
            scan.files_kept_shared >= 3,
            "разделённых файлов найдено {}",
            scan.files_kept_shared
        );
        let patched = dir.path().join("patched.img");
        let mut data = std::fs::read(&img).unwrap();
        for (s, e) in scan.ranges.iter() {
            data[s as usize..e as usize].fill(0);
        }
        std::fs::write(&patched, &data).unwrap();

        let before = corpus::dmesg_len();
        {
            let _m = corpus::mounted(&patched, &mnt);
            c.verify_kept("после восстановления");
            let written = corpus::Corpus::fill(&mnt, 512);
            assert!(written > 32, "записалось всего {written} МиБ");
            eprintln!("записано {written} МиБ поверх зануленного");
            c.verify_kept("после заполнения");
        }

        let out = std::process::Command::new("xfs_repair")
            .arg("-n")
            .arg(&patched)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "xfs_repair ПОСЛЕ монтирования и записи:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let complaints = corpus::kernel_complaints(before, &["XFS", "xfs"]);
        let bad: Vec<&String> = complaints
            .iter()
            .filter(|l| l.contains("error") || l.contains("corrupt"))
            .collect();
        assert!(bad.is_empty(), "ядро ругалось на ФС:\n{bad:?}");
    }

    /// Приёмка Stage 2 целиком, через тот же вход, что зовёт движок.
    ///
    /// Отличие от `xfs_survives_mount_and_write` (Stage 1) в том, что здесь
    /// файлы обязаны ИСЧЕЗНУТЬ из каталога, а не остаться нулевыми призраками,
    /// и `xfs_repair` обязан молчать после всей операции.
    ///
    /// Корпус даёт полторы тысячи исключённых файлов в плотных и вложенных
    /// каталогах — это не демонстрация на трёх файлах, а нагрузка, на которой
    /// и проявляются щажения.
    ///
    /// **Замер 2026-08-20: удаляется 129–306 файлов из 1403** (три прогона
    /// подряд; разброс от того, как ядро разложило корпус). Потолок ставит
    /// RMAPBT: массовое удаление уводит его листья ниже минимума, а слияния
    /// листьев ещё нет. Остальные остаются нулевыми призраками — то есть на
    /// Stage 1: данных в копии нет, имя в каталоге есть. Ограничение
    /// КОЛИЧЕСТВЕННОЕ, а не качественное: образ согласован в любом случае, и
    /// проверяет тест именно это.
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn stage2_removes_the_files_and_repair_stays_clean() {
        use crate::exclusion_corpus as corpus;
        use std::os::unix::fs::FileExt;
        if !corpus::is_root() {
            eprintln!("не root — пропускаю");
            return;
        }
        for t in ["mkfs.xfs", "xfs_repair"] {
            if std::process::Command::new(t).arg("-V").output().is_err() {
                eprintln!("нет {t} — пропускаю");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(768 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(&img)
            .status()
            .unwrap()
            .success());
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let c = {
            let _m = corpus::mounted(&img, &mnt);
            corpus::populate(&mnt, corpus::Caps::xfs())
        };

        let scan = run2(&img, &["cache"]);
        assert!(
            scan.files_excluded >= 1400,
            "исключено всего {}",
            scan.files_excluded
        );
        assert!(!scan.patches.is_empty(), "Stage 2 не дал ни одного патча");
        assert!(
            !scan.droppable.is_empty(),
            "ничего не вернулось в свободное место — значит удаления не было"
        );
        eprintln!(
            "  исключено {}, патчей {}, разделено {}",
            scan.files_excluded,
            scan.patches.len(),
            scan.files_kept_shared
        );

        // Сначала зануление данных, поверх — правка метаданных: тот же порядок,
        // в каком это видит движок при чтении.
        let patched = dir.path().join("patched.img");
        let mut data = std::fs::read(&img).unwrap();
        for (a, b) in scan.ranges.iter() {
            data[a as usize..b as usize].fill(0);
        }
        for (off, bytes) in &scan.patches {
            let o = *off as usize;
            data[o..o + bytes.len()].copy_from_slice(bytes);
        }
        std::fs::write(&patched, &data).unwrap();
        drop(data);

        let repair = |p: &Path| -> String {
            let o = std::process::Command::new("xfs_repair")
                .arg("-n")
                .arg(p)
                .output()
                .unwrap();
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };
        let judge = |t: &str| -> Vec<String> {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .filter(|l| {
                    !l.starts_with("Phase")
                        && !l.starts_with('-')
                        && !l.starts_with("No modify flag set")
                        && !l.contains("host filesystem geometry")
                        && !l.contains("Repair may fail")
                        && !l.contains("sector size mismatch")
                        && !l.contains("the image and the host filesystem")
                })
                .map(|l| l.to_string())
                .collect()
        };
        let found = judge(&repair(&patched));
        assert!(
            found.is_empty(),
            "xfs_repair после Stage 2 сказал:\n{}",
            found.join("\n")
        );

        let before = corpus::dmesg_len();
        let left = {
            let _m = corpus::mounted(&patched, &mnt);
            c.verify_kept("после восстановления");
            let left = walk_count(&mnt.join("cache"));
            let written = corpus::Corpus::fill(&mnt, 512);
            assert!(written > 32, "записалось всего {written} МиБ");
            c.verify_kept("после заполнения");
            left
        };
        let mut tally: std::collections::BTreeMap<&str, u64> = Default::default();
        for (_, why) in &scan.stage2_spared {
            *tally.entry(why.as_str()).or_default() += 1;
        }
        let removed = scan.files_excluded - scan.stage2_spared.len() as u64;
        eprintln!(
            "  удалено по-настоящему {removed} из {}, в cache/ осталось имён {left}",
            scan.files_excluded
        );
        for (why, n) in &tally {
            let short: String = why.chars().take(80).collect();
            eprintln!("    {n:5} × {short}");
        }
        // Каждое щажение обязано быть названо: «не смогли» без причины в этом
        // коде — ошибка, а не результат.
        assert!(
            scan.stage2_spared.iter().all(|(_, w)| !w.is_empty()),
            "нашлось щажение без причины"
        );
        // Порога здесь намеренно нет. Сколько удастся удалить, зависит от
        // раскладки, которую делает ядро при наполнении корпуса: три прогона
        // подряд дали 129, 266 и 306 из 1403. Порог на такой величине был бы
        // не сторожем, а генератором ложных падений. Проверяем то, что
        // устойчиво: удаление произошло, и образ согласован.
        assert!(
            removed > 0,
            "не удалено ни одного файла — Stage 2 перестал работать"
        );
        assert!(
            left + removed <= scan.files_excluded + 16,
            "имён осталось {left} при {removed} удалённых — счёт не сходится"
        );

        let found = judge(&repair(&patched));
        assert!(
            found.is_empty(),
            "xfs_repair после монтирования и записи сказал:\n{}",
            found.join("\n")
        );
        let complaints = corpus::kernel_complaints(before, &["XFS", "xfs"]);
        let bad: Vec<&String> = complaints
            .iter()
            .filter(|l| l.contains("error") || l.contains("corrupt"))
            .collect();
        assert!(bad.is_empty(), "ядро ругалось на ФС:\n{bad:?}");
    }

    /// Сколько ФАЙЛОВ осталось в поддереве (каталоги Stage 2 не удаляет).
    fn walk_count(root: &Path) -> u64 {
        let mut n = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    n += 1;
                }
            }
        }
        n
    }
}
