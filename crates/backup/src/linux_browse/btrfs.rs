//! A btrfs filesystem, read for its files: what `ext4-view` is for ext4.
//!
//! The view is the system as it boots: the subvolume `/etc/fstab` mounts at
//! `/` is the root, and those it mounts elsewhere (`@home` at `/home`,
//! `@swap` at `/swap`) are in their places — the fstab of the subvolume
//! itself says so, for this filesystem (its UUID). Without such an fstab the
//! view is the top-level subvolume (id 5), subvolumes as the folders they
//! are in it (`@`, `@home`). The
//! structures come from the backup engine's btrfs code (`btrfs_map`: the
//! superblock's chunks, the chunk tree, tree blocks); what is added here is
//! looking things up by key instead of walking whole trees, so a listing
//! costs a few tree blocks, not a pass over the filesystem.
//!
//! Read as last committed. The fsync log of an unclean shutdown is not
//! replayed: its few seconds of writes are missing, the rest is consistent.
//! Single-device filesystems only, SINGLE or DUP (as `btrfs_map`); data
//! compressed with zlib, LZO or zstd is decompressed.

use std::collections::HashMap;
use std::rc::Rc;

use own_backup_common::{OwnBackupError, Result};

use super::{names, Entry};
use crate::btrfs_map::{
    for_each_item, parse_chunk, read_node, walk_tree, ChunkMap, CHUNK_ITEM, HEADER, MAGIC,
    ROOT_ITEM, SUPER_OFFSET, SUPER_SIZE,
};
use crate::fat_map::RawVolume;

const INODE_ITEM: u8 = 1;
const DIR_INDEX: u8 = 96;
const EXTENT_DATA: u8 = 108;
const TOP_LEVEL: u64 = 5;
/// The root directory of every subvolume.
const ROOT_DIR: u64 = 256;
const FILE_EXTENT_INLINE: u8 = 0;
const FILE_EXTENT_PREALLOC: u8 = 2;
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
/// Tree blocks kept in memory (16 KiB each by default: 32 MiB).
const NODE_CACHE: usize = 2048;
/// Symbolic links followed in one path at most.
const MAX_LINKS: usize = 32;
/// No sane extent decompresses to more than this (btrfs caps it at 128 KiB).
const MAX_EXTENT: u64 = 16 << 20;

type Key = (u64, u8, u64);

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("btrfs: {msg}"))
}

fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn key_at(b: &[u8], o: usize) -> Key {
    (u64at(b, o), b[o + 8], u64at(b, o + 9))
}

/// Where an inode is: the tree (subvolume) and its number in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ino {
    tree: u64,
    ino: u64,
}

/// What an inode item says, as far as listing and reading need.
#[derive(Debug, Clone, Copy)]
struct Inode {
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_nsec: u32,
}

impl Inode {
    fn kind(&self) -> u32 {
        self.mode & S_IFMT
    }
}

pub struct Btrfs {
    vol: Box<dyn RawVolume>,
    chunks: ChunkMap,
    nodesize: u64,
    label: String,
    /// Subvolume (tree) id → its root block.
    trees: HashMap<u64, u64>,
    nodes: HashMap<u64, Rc<Vec<u8>>>,
    /// The extent last decompressed: (disk bytenr, its bytes).
    unpacked: Option<(u64, Rc<Vec<u8>>)>,
    /// The filesystem's UUID, as fstab names it.
    fsid: String,
    /// What `/` is: the root subvolume's root directory, or the top level's.
    view_root: Ino,
    /// Subvolumes mounted elsewhere in the view: their path, their root.
    mounts: Vec<(Vec<u8>, Ino)>,
    /// The view comes from fstab (an absolute link starts at `/`); without
    /// it a link starts at the root of the subvolume it lies in.
    from_fstab: bool,
}

impl Btrfs {
    /// Superblock → chunk map → root tree.
    pub fn load(mut vol: Box<dyn RawVolume>) -> Result<Self> {
        if SUPER_OFFSET + SUPER_SIZE > vol.len() {
            return Err(bad("too small"));
        }
        let mut sb = vec![0u8; SUPER_SIZE as usize];
        vol.read_at(SUPER_OFFSET, &mut sb)?;
        if &sb[0x40..0x48] != MAGIC {
            return Err(bad("no superblock"));
        }
        let root = u64at(&sb, 0x50);
        let chunk_root = u64at(&sb, 0x58);
        let num_devices = u64at(&sb, 0x88);
        let nodesize = u32at(&sb, 0x94);
        let sys_array_size = u32at(&sb, 0xA0) as usize;
        let our_devid = u64at(&sb, 0xC9);
        if num_devices != 1 {
            return Err(bad("the filesystem spans several devices"));
        }
        if !(4096..=65536).contains(&nodesize) || !nodesize.is_power_of_two() {
            return Err(bad("implausible node size"));
        }
        if sys_array_size == 0 || 0x32B + sys_array_size > sb.len() {
            return Err(bad("implausible system chunk array"));
        }
        let label = {
            let raw = &sb[0x12B..0x12B + 256];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };

        let mut boot = ChunkMap::new();
        let sys = &sb[0x32B..0x32B + sys_array_size];
        let mut pos = 0usize;
        while pos + 17 <= sys.len() {
            if sys[pos + 8] != CHUNK_ITEM {
                return Err(bad("unexpected item in the system chunk array"));
            }
            let logical = u64at(sys, pos + 9);
            let cd = &sys[pos + 17..];
            if cd.len() < 48 {
                return Err(bad("truncated system chunk"));
            }
            let len = 48 + 32 * u16at(cd, 44) as usize;
            if len > cd.len() {
                return Err(bad("system chunk past the array"));
            }
            boot.push(parse_chunk(logical, &cd[..len], our_devid)?);
            pos += 17 + len;
        }
        let nodesize = nodesize as u64;
        let mut chunks = ChunkMap::new();
        walk_tree(&mut *vol, &boot, chunk_root, nodesize, |node, n| {
            for_each_item(node, n, |_, kind, offset, data| {
                if kind == CHUNK_ITEM {
                    chunks.push(parse_chunk(offset, data, our_devid)?);
                }
                Ok(())
            })
        })?;
        let mut trees = HashMap::new();
        walk_tree(&mut *vol, &chunks, root, nodesize, |node, n| {
            for_each_item(node, n, |id, kind, _, data| {
                if kind == ROOT_ITEM && data.len() >= 184 {
                    trees.insert(id, u64at(data, 176));
                }
                Ok(())
            })
        })?;
        if !trees.contains_key(&TOP_LEVEL) {
            return Err(bad("no top-level subvolume"));
        }
        let fsid = uuid_text(&sb[0x20..0x30]);
        let mut fs = Self {
            vol,
            chunks,
            nodesize,
            label,
            trees,
            nodes: HashMap::new(),
            unpacked: None,
            fsid,
            view_root: Ino {
                tree: TOP_LEVEL,
                ino: ROOT_DIR,
            },
            mounts: Vec::new(),
            from_fstab: false,
        };
        fs.arrange_as_booted();
        Ok(fs)
    }

    /// Make the view the system as it boots, when a subvolume's own fstab
    /// says which is `/` and where the others go. `@` is asked first.
    fn arrange_as_booted(&mut self) {
        let top = Ino {
            tree: TOP_LEVEL,
            ino: ROOT_DIR,
        };
        let Ok(entries) = self.dir_entries(top) else {
            return;
        };
        let mut subvols: Vec<(Vec<u8>, Ino)> = entries
            .into_iter()
            .filter(|(_, i)| i.tree != TOP_LEVEL)
            .collect();
        subvols.sort_by_key(|(n, _)| n.as_slice() != b"@");
        for (name, at) in &subvols {
            self.view_root = *at;
            self.mounts.clear();
            let Ok((ino, node, _)) = self.resolve(b"/etc/fstab") else {
                continue;
            };
            if node.kind() != S_IFREG {
                continue;
            }
            let Ok(text) = self.read_inode(ino, &node, 0, 64 * 1024) else {
                continue;
            };
            let lines = fstab_subvols(&text, &self.fsid);
            let is_this = |l: &FstabSubvol| {
                l.subvolid == Some(at.tree)
                    || l.subvol
                        .as_deref()
                        .map(|s| s.trim_start_matches('/').as_bytes())
                        == Some(name.as_slice())
            };
            if !lines.iter().any(|l| l.mountpoint == b"/" && is_this(l)) {
                continue;
            }
            let mut mounts = Vec::new();
            for l in lines.iter().filter(|l| l.mountpoint != b"/") {
                let root = match (l.subvolid, &l.subvol) {
                    (Some(id), _) if self.trees.contains_key(&id) => Some(Ino {
                        tree: id,
                        ino: ROOT_DIR,
                    }),
                    (_, Some(s)) => {
                        let s = s.trim_start_matches('/').as_bytes();
                        subvols
                            .iter()
                            .find(|(n, _)| n.as_slice() == s)
                            .map(|(_, i)| *i)
                    }
                    _ => None,
                };
                if let Some(root) = root {
                    mounts.push((l.mountpoint.clone(), root));
                }
            }
            tracing::info!(
                root = %String::from_utf8_lossy(name),
                mounts = mounts.len(),
                "btrfs: shown as booted, from the subvolume's fstab"
            );
            self.mounts = mounts;
            self.from_fstab = true;
            return;
        }
        self.view_root = top;
        self.mounts.clear();
        self.from_fstab = false;
    }

    pub fn label(&self) -> String {
        self.label.clone()
    }

    fn node(&mut self, logical: u64) -> Result<Rc<Vec<u8>>> {
        if let Some(n) = self.nodes.get(&logical) {
            return Ok(n.clone());
        }
        let n = Rc::new(read_node(
            &mut *self.vol,
            &self.chunks,
            logical,
            self.nodesize,
        )?);
        if self.nodes.len() >= NODE_CACHE {
            self.nodes.clear();
        }
        self.nodes.insert(logical, n.clone());
        Ok(n)
    }

    /// Visit the items of subvolume `tree` from `start` on, in key order,
    /// until `f` says stop.
    fn scan(
        &mut self,
        tree: u64,
        start: Key,
        mut f: impl FnMut(Key, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let root = *self
            .trees
            .get(&tree)
            .ok_or_else(|| bad(format!("no subvolume {tree}")))?;
        // The path down: (block, index of the child taken).
        let mut path: Vec<(Rc<Vec<u8>>, usize)> = Vec::new();
        let mut node = self.node(root)?;
        loop {
            let n = u32at(&node, 0x60) as usize;
            if node[0x64] == 0 {
                if HEADER + n * 25 > node.len() {
                    return Err(bad("leaf items past the block"));
                }
                // The first item at or after `start`.
                let first = (0..n)
                    .find(|&i| key_at(&node, HEADER + i * 25) >= start)
                    .unwrap_or(n);
                for i in first..n {
                    let it = HEADER + i * 25;
                    let off = HEADER + u32at(&node, it + 17) as usize;
                    let len = u32at(&node, it + 21) as usize;
                    let data = node
                        .get(off..off + len)
                        .ok_or_else(|| bad("item data past the block"))?;
                    if !f(key_at(&node, it), data)? {
                        return Ok(());
                    }
                }
                // On to the next leaf: up to the first block with a child
                // to the right, then down its leftmost side.
                loop {
                    let Some((parent, taken)) = path.pop() else {
                        return Ok(());
                    };
                    let pn = u32at(&parent, 0x60) as usize;
                    if taken + 1 < pn {
                        let child = u64at(&parent, HEADER + (taken + 1) * 33 + 17);
                        path.push((parent, taken + 1));
                        node = self.node(child)?;
                        break;
                    }
                }
                // Descend leftmost from here.
                while node[0x64] != 0 {
                    let child = u64at(&node, HEADER + 17);
                    path.push((node.clone(), 0));
                    node = self.node(child)?;
                }
                // The rest of the keys are all after `start`.
                return self.continue_from(path, node, f);
            }
            if HEADER + n * 33 > node.len() || n == 0 {
                return Err(bad("node pointers past the block"));
            }
            // The last child whose first key is not after `start`.
            let i = (0..n)
                .rev()
                .find(|&i| key_at(&node, HEADER + i * 33) <= start)
                .unwrap_or(0);
            let child = u64at(&node, HEADER + i * 33 + 17);
            path.push((node.clone(), i));
            node = self.node(child)?;
        }
    }

    /// The rest of [`Self::scan`] once it has moved past its first leaf:
    /// every item of every leaf from `node` on.
    fn continue_from(
        &mut self,
        mut path: Vec<(Rc<Vec<u8>>, usize)>,
        mut node: Rc<Vec<u8>>,
        mut f: impl FnMut(Key, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        loop {
            let n = u32at(&node, 0x60) as usize;
            if HEADER + n * 25 > node.len() {
                return Err(bad("leaf items past the block"));
            }
            for i in 0..n {
                let it = HEADER + i * 25;
                let off = HEADER + u32at(&node, it + 17) as usize;
                let len = u32at(&node, it + 21) as usize;
                let data = node
                    .get(off..off + len)
                    .ok_or_else(|| bad("item data past the block"))?;
                if !f(key_at(&node, it), data)? {
                    return Ok(());
                }
            }
            loop {
                let Some((parent, taken)) = path.pop() else {
                    return Ok(());
                };
                if taken + 1 < u32at(&parent, 0x60) as usize {
                    let child = u64at(&parent, HEADER + (taken + 1) * 33 + 17);
                    path.push((parent, taken + 1));
                    node = self.node(child)?;
                    break;
                }
            }
            while node[0x64] != 0 {
                let child = u64at(&node, HEADER + 17);
                path.push((node.clone(), 0));
                node = self.node(child)?;
            }
        }
    }

    fn inode(&mut self, at: Ino) -> Result<Inode> {
        let mut found = None;
        self.scan(at.tree, (at.ino, INODE_ITEM, 0), |k, data| {
            if k.0 == at.ino && k.1 == INODE_ITEM && data.len() >= 160 {
                found = Some(Inode {
                    size: u64at(data, 16),
                    mode: u32at(data, 52) as u32,
                    mtime: u64at(data, 136) as i64,
                    mtime_nsec: u32at(data, 144) as u32,
                });
            }
            Ok(false)
        })?;
        found.ok_or_else(|| bad(format!("no inode {} in subvolume {}", at.ino, at.tree)))
    }

    /// The entries of directory `dir`: raw name and where it leads.
    fn dir_entries(&mut self, dir: Ino) -> Result<Vec<(Vec<u8>, Ino)>> {
        let mut out = Vec::new();
        self.scan(dir.tree, (dir.ino, DIR_INDEX, 0), |k, data| {
            if k.0 != dir.ino || k.1 != DIR_INDEX {
                return Ok(false);
            }
            // btrfs_dir_item: location key (17), transid (8), data_len (2),
            // name_len (2), type (1), name.
            if data.len() >= 30 {
                let (oid, kind, _) = key_at(data, 0);
                let name_len = u16at(data, 27) as usize;
                if let Some(name) = data.get(30..30 + name_len) {
                    let to = if kind == ROOT_ITEM {
                        // A subvolume: its own tree, from its root directory.
                        Ino {
                            tree: oid,
                            ino: ROOT_DIR,
                        }
                    } else {
                        Ino {
                            tree: dir.tree,
                            ino: oid,
                        }
                    };
                    out.push((name.to_vec(), to));
                }
            }
            Ok(true)
        })?;
        Ok(out)
    }

    /// The root of the subvolume mounted at `path` in the view, if any.
    fn mount_at(&self, path: &[u8]) -> Option<Ino> {
        self.mounts.iter().find(|(p, _)| p == path).map(|(_, i)| *i)
    }

    /// Follow `path` (absolute, bytes) to an inode, symbolic links and the
    /// subvolumes mounted in the view included; also the path it ends at.
    /// A link to an absolute path starts at `/` of the view — or, when the
    /// view is the top level, at the root of the subvolume the link is in
    /// (on Ubuntu `@` is what was `/`).
    fn resolve(&mut self, path: &[u8]) -> Result<(Ino, Inode, Vec<u8>)> {
        let mut todo: Vec<Vec<u8>> = split(path).into_iter().rev().collect();
        // The directories walked and their paths, for `..`.
        let mut trail: Vec<(Ino, Vec<u8>)> = vec![(self.view_root, Vec::new())];
        let mut links = 0;
        while let Some(part) = todo.pop() {
            let (here, here_path) = trail.last().cloned().unwrap();
            if part == b"." {
                continue;
            }
            if part == b".." {
                if trail.len() > 1 {
                    trail.pop();
                }
                continue;
            }
            let mut child_path = here_path;
            child_path.push(b'/');
            child_path.extend_from_slice(&part);
            let next = match self.mount_at(&child_path) {
                Some(root) => root,
                None => self
                    .dir_entries(here)?
                    .into_iter()
                    .find(|(n, _)| *n == part)
                    .map(|(_, to)| to)
                    .ok_or_else(|| bad("not found"))?,
            };
            let node = self.inode(next)?;
            if node.kind() == S_IFLNK {
                links += 1;
                if links > MAX_LINKS {
                    return Err(bad("too many symbolic links"));
                }
                let target = self.read_inode(next, &node, 0, 4096)?;
                if target.starts_with(b"/") {
                    let keep = if self.from_fstab {
                        0
                    } else {
                        // The first directory of the walk in the link's tree.
                        trail
                            .iter()
                            .position(|(i, _)| i.tree == here.tree)
                            .unwrap_or(0)
                    };
                    trail.truncate(keep + 1);
                }
                todo.extend(split(&target).into_iter().rev());
                continue;
            }
            trail.push((next, child_path));
        }
        let (at, at_path) = trail.pop().unwrap();
        let node = self.inode(at)?;
        Ok((at, node, at_path))
    }

    fn entry(name: &[u8], node: &Inode) -> Entry {
        let dir = node.kind() == S_IFDIR;
        Entry {
            name: names::to_display(name),
            dir,
            size: if dir { 0 } else { node.size },
            modified: node.mtime,
            nanos: node.mtime_nsec,
        }
    }

    /// The entries of the directory at `path`, links shown as what they
    /// point to; links to nowhere and special files left out.
    pub fn list(&mut self, path: &[u8]) -> std::result::Result<Vec<Entry>, String> {
        let (dir, node, dir_path) = self.resolve(path).map_err(|e| e.to_string())?;
        if node.kind() != S_IFDIR {
            return Err("not a directory".into());
        }
        let mut out = Vec::new();
        for (name, to) in self.dir_entries(dir).map_err(|e| e.to_string())? {
            // A folder another subvolume is mounted on shows that subvolume.
            let mut child = dir_path.clone();
            child.push(b'/');
            child.extend_from_slice(&name);
            let to = self.mount_at(&child).unwrap_or(to);
            let Ok(mut node) = self.inode(to) else {
                continue;
            };
            if node.kind() == S_IFLNK {
                let mut full = path.to_vec();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&name);
                match self.resolve(&full) {
                    Ok((_, target, _)) => node = target,
                    Err(_) => continue,
                }
            }
            if node.kind() != S_IFDIR && node.kind() != S_IFREG {
                continue;
            }
            out.push(Self::entry(&name, &node));
        }
        out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    pub fn stat(&mut self, path: &[u8]) -> std::result::Result<Entry, String> {
        let (_, node, _) = self.resolve(path).map_err(|e| e.to_string())?;
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or_default();
        Ok(Self::entry(name, &node))
    }

    pub fn read(
        &mut self,
        path: &[u8],
        offset: u64,
        len: usize,
    ) -> std::result::Result<Vec<u8>, String> {
        let (at, node, _) = self.resolve(path).map_err(|e| e.to_string())?;
        if node.kind() != S_IFREG {
            return Err("not a file".into());
        }
        self.read_inode(at, &node, offset, len)
            .map_err(|e| e.to_string())
    }

    /// Up to `len` bytes of the inode's data from `offset`: its extents,
    /// holes read as zeros.
    fn read_inode(&mut self, at: Ino, node: &Inode, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset >= node.size {
            return Ok(Vec::new());
        }
        let end = node.size.min(offset + len as u64);
        let mut out = vec![0u8; (end - offset) as usize];
        // The extents that may cover [offset, end): the last one starting at
        // or before `offset`, and every one after it that starts before `end`.
        let mut extents: Vec<(u64, Vec<u8>)> = Vec::new();
        self.scan(at.tree, (at.ino, EXTENT_DATA, 0), |k, data| {
            if k.0 != at.ino || k.1 != EXTENT_DATA || k.2 >= end {
                return Ok(false);
            }
            if k.2 <= offset {
                extents.clear();
            }
            extents.push((k.2, data.to_vec()));
            Ok(true)
        })?;
        for (start, item) in extents {
            self.copy_extent(start, &item, offset, &mut out)?;
        }
        Ok(out)
    }

    /// Copy what the extent item at file offset `start` holds of the range
    /// `out` stands for (from file offset `from`).
    fn copy_extent(&mut self, start: u64, item: &[u8], from: u64, out: &mut [u8]) -> Result<()> {
        if item.len() < 21 {
            return Err(bad("short extent item"));
        }
        let ram_bytes = u64at(item, 8);
        let compression = item[16];
        if item[17] != 0 || u16at(item, 18) != 0 {
            return Err(bad("encrypted or encoded extents are not read"));
        }
        let kind = item[20];
        let to = from + out.len() as u64;
        // Where the extent's bytes come from: the logical bytes it covers in
        // the file, and how to get them.
        let (covers, bytes): (u64, Rc<Vec<u8>>) = if kind == FILE_EXTENT_INLINE {
            let data = &item[21..];
            let plain = if compression == 0 {
                Rc::new(data.to_vec())
            } else {
                Rc::new(decompress(compression, data, ram_bytes)?)
            };
            (ram_bytes.min(plain.len() as u64), plain)
        } else {
            if item.len() < 53 {
                return Err(bad("short extent item"));
            }
            let disk = u64at(item, 21);
            let disk_len = u64at(item, 29);
            let in_extent = u64at(item, 37);
            let num = u64at(item, 45);
            let lo = start.max(from);
            let hi = (start + num).min(to);
            if lo >= hi || disk == 0 || kind == FILE_EXTENT_PREALLOC {
                // Outside the range, a hole, or allocated and never written:
                // zeros, which `out` already is.
                return Ok(());
            }
            if compression == 0 {
                let at = (lo - from) as usize;
                let n = (hi - lo) as usize;
                return self.read_logical(disk + in_extent + (lo - start), &mut out[at..at + n]);
            }
            let plain = match &self.unpacked {
                Some((d, b)) if *d == disk => b.clone(),
                _ => {
                    if disk_len > MAX_EXTENT || ram_bytes > MAX_EXTENT {
                        return Err(bad("implausibly large compressed extent"));
                    }
                    let mut packed = vec![0u8; disk_len as usize];
                    self.read_logical(disk, &mut packed)?;
                    let b = Rc::new(decompress(compression, &packed, ram_bytes)?);
                    self.unpacked = Some((disk, b.clone()));
                    b
                }
            };
            // The file's part of the decompressed extent begins `in_extent` in.
            let skip = in_extent as usize;
            let tail = plain.get(skip..).unwrap_or_default();
            (num.min(tail.len() as u64), Rc::new(tail.to_vec()))
        };
        let lo = start.max(from);
        let hi = (start + covers).min(to);
        if lo < hi {
            let src = &bytes[(lo - start) as usize..(hi - start) as usize];
            out[(lo - from) as usize..(hi - from) as usize].copy_from_slice(src);
        }
        Ok(())
    }

    /// `buf.len()` bytes at logical address `logical`, across chunks (the
    /// first copy of a DUP chunk).
    fn read_logical(&mut self, logical: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = logical + done as u64;
            let c = self
                .chunks
                .chunks
                .iter()
                .find(|c| at >= c.logical && at < c.logical + c.length)
                .ok_or_else(|| bad(format!("logical {at} in no chunk")))?;
            let n = ((c.logical + c.length - at) as usize).min(buf.len() - done);
            let phys = c.stripes[0] + (at - c.logical);
            self.vol.read_at(phys, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }
}

/// A UUID as fstab writes it: `8-4-4-4-12`, lower case.
fn uuid_text(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// An fstab line that mounts a subvolume of a btrfs filesystem.
#[derive(Debug, PartialEq, Eq)]
struct FstabSubvol {
    mountpoint: Vec<u8>,
    subvol: Option<String>,
    subvolid: Option<u64>,
}

/// The lines of `fstab` mounting subvolumes of the btrfs filesystem `fsid`.
/// A line naming its filesystem by UUID must name this one; a line naming
/// it otherwise (`LABEL=`, a device) is taken only when no line names a
/// btrfs by UUID — then there is nothing to tell them apart by.
fn fstab_subvols(fstab: &[u8], fsid: &str) -> Vec<FstabSubvol> {
    let text = String::from_utf8_lossy(fstab);
    let mut by_uuid = Vec::new();
    let mut other = Vec::new();
    let mut any_uuid = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 || f[2] != "btrfs" {
            continue;
        }
        let spec = f[0].trim_matches('"');
        let uuid = spec
            .strip_prefix("UUID=")
            .or_else(|| spec.strip_prefix("/dev/disk/by-uuid/"))
            .map(|u| u.trim_matches('"').to_ascii_lowercase());
        let (mut subvol, mut subvolid) = (None, None);
        for o in f[3].split(',') {
            if let Some(v) = o.strip_prefix("subvol=") {
                subvol = Some(v.to_string());
            } else if let Some(v) = o.strip_prefix("subvolid=") {
                subvolid = v.parse().ok();
            }
        }
        if subvol.is_none() && subvolid.is_none() {
            continue;
        }
        // fstab writes a space in a path as \040.
        let mountpoint = f[1].replace("\\040", " ");
        let mountpoint = mountpoint.trim_end_matches('/');
        let mountpoint = if mountpoint.is_empty() {
            "/"
        } else {
            mountpoint
        };
        let line = FstabSubvol {
            mountpoint: mountpoint.as_bytes().to_vec(),
            subvol,
            subvolid,
        };
        match uuid {
            Some(u) => {
                any_uuid = true;
                if u == fsid {
                    by_uuid.push(line);
                }
            }
            None => other.push(line),
        }
    }
    if any_uuid {
        by_uuid
    } else {
        other
    }
}

fn split(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/')
        .filter(|p| !p.is_empty())
        .map(|p| p.to_vec())
        .collect()
}

/// Decompress a btrfs extent: 1 zlib, 2 LZO (btrfs' own framing), 3 zstd.
fn decompress(kind: u8, data: &[u8], ram_bytes: u64) -> Result<Vec<u8>> {
    let size = ram_bytes as usize;
    match kind {
        1 => {
            use std::io::Read;
            let mut out = Vec::with_capacity(size);
            flate2::read::ZlibDecoder::new(data)
                .take(ram_bytes)
                .read_to_end(&mut out)
                .map_err(|e| bad(format!("zlib: {e}")))?;
            Ok(out)
        }
        2 => lzo(data, size),
        3 => {
            let mut out = Vec::with_capacity(size);
            // One frame: the extent is padded with zeros to a whole sector,
            // and read on they look like a broken next frame.
            let mut d = zstd::stream::read::Decoder::new(data)
                .map_err(|e| bad(format!("zstd: {e}")))?
                .single_frame();
            use std::io::Read;
            // A btrfs zstd extent may stop without the frame's end mark
            // once it has produced ram_bytes: read what is needed only.
            let mut chunk = vec![0u8; 64 * 1024];
            while out.len() < size {
                let n = d.read(&mut chunk).map_err(|e| bad(format!("zstd: {e}")))?;
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&chunk[..n.min(size - out.len())]);
            }
            Ok(out)
        }
        other => Err(bad(format!("compression {other} is not known"))),
    }
}

/// btrfs' LZO framing: the total length (u32), then segments of a length
/// (u32) and LZO1X data, a segment header never crossing a 4 KiB boundary
/// (padding up to it is skipped). Each segment decompresses to at most a
/// page.
fn lzo(data: &[u8], size: usize) -> Result<Vec<u8>> {
    const PAGE: usize = 4096;
    if data.len() < 4 {
        return Err(bad("lzo: no header"));
    }
    let total = (u32at(data, 0) as usize).min(data.len());
    let mut pos = 4;
    let mut out = Vec::with_capacity(size);
    while pos < total && out.len() < size {
        // A segment header does not straddle a page: skip to the next one.
        if PAGE - pos % PAGE < 4 {
            pos = pos.next_multiple_of(PAGE);
            continue;
        }
        if pos + 4 > data.len() {
            break;
        }
        let seg = u32at(data, pos) as usize;
        pos += 4;
        let src = data
            .get(pos..pos + seg)
            .ok_or_else(|| bad("lzo: segment past the extent"))?;
        let want = PAGE.min(size - out.len());
        let piece = lzokay_native::decompress_all(src, Some(want))
            .map_err(|e| bad(format!("lzo: {e:?}")))?;
        out.extend_from_slice(&piece[..piece.len().min(want)]);
        pos += seg;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    #[test]
    fn fstab_says_which_subvolume_goes_where() {
        let fstab = format!(
            "# /etc/fstab: static file system information.\n\
             UUID={ID} /     btrfs defaults,subvol=@     0 1\n\
             UUID={ID} /home btrfs defaults,subvol=/@home 0 2\n\
             UUID={ID} /swap btrfs defaults,subvolid=258 0 0\n\
             UUID=AAAA-BBBB /boot/efi vfat umask=0077 0 1\n\
             UUID=11111111-2222-3333-4444-555555555555 /data btrfs subvol=@data 0 0\n\
             UUID={ID} /mnt/no\\040sub btrfs defaults 0 0\n"
        );
        let lines = fstab_subvols(fstab.as_bytes(), ID);
        let points: Vec<&[u8]> = lines.iter().map(|l| l.mountpoint.as_slice()).collect();
        assert_eq!(
            points,
            [&b"/"[..], b"/home", b"/swap"],
            "another filesystem's and a line without a subvolume left out"
        );
        assert_eq!(lines[1].subvol.as_deref(), Some("/@home"));
        assert_eq!(lines[2].subvolid, Some(258));
    }

    #[test]
    fn without_uuids_the_btrfs_lines_are_taken() {
        let lines = fstab_subvols(
            b"LABEL=root / btrfs subvol=@ 0 1\n/dev/sda2 /home/ btrfs subvol=@home 0 2\n",
            ID,
        );
        let points: Vec<&[u8]> = lines.iter().map(|l| l.mountpoint.as_slice()).collect();
        assert_eq!(points, [&b"/"[..], b"/home"]);
    }

    #[test]
    fn a_uuid_reads_as_fstab_writes_it() {
        let raw: Vec<u8> = (0..16u8).collect();
        assert_eq!(uuid_text(&raw), "00010203-0405-0607-0809-0a0b0c0d0e0f");
    }
}
