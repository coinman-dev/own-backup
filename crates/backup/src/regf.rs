//! Windows registry hive files (`regf`), read into a tree and written back —
//! enough to edit a BCD store offline (PLAN §6, stage 3), in pure Rust.
//!
//! hivex would do this and is LGPL, which the project does not take; the
//! format is documented well enough (Microsoft's own "Windows registry file
//! format" notes, Maxim Suhanov's specification) to do it here.
//!
//! ## The format, briefly
//!
//! ```text
//! 0      base block, 4 KiB: "regf", two sequence numbers, root cell offset,
//!        size of the hive bins, version, checksum (XOR of the first 508 bytes)
//! 4096   hive bins: "hbin" headers (32 B), each a multiple of 4 KiB, holding
//!        cells. A cell is an i32 size — negative when allocated — and data.
//!        Cell offsets count from the first hbin.
//! ```
//!
//! Cells: `nk` (a key), `vk` (a value), `sk` (a security descriptor, shared
//! by many keys, with a reference count), subkey lists (`lf`, `lh`, `li`, and
//! `ri` — a list of lists), value lists (bare arrays of offsets), and `db`
//! (a value's data split into segments, for data over 16 344 bytes).
//!
//! ## Why rewrite instead of patch
//!
//! Editing cells in place means managing free space, splitting and merging
//! cells and keeping every back-reference right. A BCD store is a few dozen
//! kilobytes, so [`Hive::to_bytes`] simply lays the whole tree out afresh: the
//! result is a compact, valid hive with every key, value, class name,
//! timestamp and security descriptor of the original, and nothing of its
//! free-space history.
//!
//! ## Dirty hives
//!
//! When the two sequence numbers differ, the last write was not completed and
//! the real content is partly in the transaction logs beside the file
//! (`BCD.LOG1`, `BCD.LOG2`). Replaying logs is not implemented: such a hive is
//! refused rather than edited from a stale image. A hive we write has both
//! numbers equal and one higher than before, so logs left beside it carry
//! older sequence numbers and are not replayed over it.

use own_backup_common::{tr, OwnBackupError, Result};

const BASE_BLOCK: usize = 4096;
const HBIN: usize = 4096;
const HBIN_HEADER: usize = 32;
/// The largest value data kept in one cell; longer data goes into `db` segments.
const MAX_CELL_DATA: usize = 16344;
/// Subkeys per `lf`/`lh` list before the writer splits them under an `ri`.
const LEAF_MAX: usize = 500;
/// Depth past which the tree is treated as a loop.
const MAX_DEPTH: usize = 512;

pub const KEY_HIVE_ENTRY: u16 = 0x0004;
pub const KEY_NO_DELETE: u16 = 0x0008;
pub const KEY_COMP_NAME: u16 = 0x0020;
const VALUE_COMP_NAME: u16 = 0x0001;

pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;
pub const REG_MULTI_SZ: u32 = 7;
pub const REG_QWORD: u32 = 11;

fn bad(reason: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::ImageFormat(tr!("regf.bad_hive", reason = reason).into_owned())
}

/// A value: name (empty = the key's default value), type and raw data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub name: String,
    pub kind: u32,
    pub data: Vec<u8>,
}

impl Value {
    /// A REG_SZ/REG_EXPAND_SZ value as text (to its first NUL).
    pub fn as_string(&self) -> Option<String> {
        if self.kind != REG_SZ && self.kind != REG_EXPAND_SZ {
            return None;
        }
        let units = utf16_units(&self.data);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }

    /// A REG_MULTI_SZ value as its strings.
    pub fn as_multi_string(&self) -> Option<Vec<String>> {
        if self.kind != REG_MULTI_SZ {
            return None;
        }
        let units = utf16_units(&self.data);
        Some(
            units
                .split(|&u| u == 0)
                .filter(|s| !s.is_empty())
                .map(String::from_utf16_lossy)
                .collect(),
        )
    }

    pub fn as_dword(&self) -> Option<u32> {
        (self.kind == REG_DWORD && self.data.len() >= 4)
            .then(|| u32::from_le_bytes(self.data[..4].try_into().unwrap()))
    }

    pub fn string(name: &str, text: &str) -> Self {
        let mut data: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        data.extend([0, 0]);
        Self {
            name: name.into(),
            kind: REG_SZ,
            data,
        }
    }

    pub fn dword(name: &str, v: u32) -> Self {
        Self {
            name: name.into(),
            kind: REG_DWORD,
            data: v.to_le_bytes().to_vec(),
        }
    }
}

fn utf16_units(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect()
}

/// A key with everything a hive keeps about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    /// nk flags other than the name encoding (which the writer decides).
    pub flags: u16,
    /// FILETIME of the last write.
    pub last_written: u64,
    /// Raw class name (UTF-16LE), if any.
    pub class: Option<Vec<u8>>,
    /// Index into [`Hive::security`].
    pub security: Option<usize>,
    pub values: Vec<Value>,
    pub subkeys: Vec<Key>,
}

impl Key {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            flags: 0,
            last_written: 0,
            class: None,
            security: None,
            values: Vec::new(),
            subkeys: Vec::new(),
        }
    }

    /// Registry names compare case-insensitively.
    fn same_name(a: &str, b: &str) -> bool {
        a.to_uppercase() == b.to_uppercase()
    }

    pub fn subkey(&self, name: &str) -> Option<&Key> {
        self.subkeys.iter().find(|k| Self::same_name(&k.name, name))
    }

    pub fn subkey_mut(&mut self, name: &str) -> Option<&mut Key> {
        self.subkeys
            .iter_mut()
            .find(|k| Self::same_name(&k.name, name))
    }

    /// Follow a `\`-separated path of subkeys.
    pub fn path(&self, path: &str) -> Option<&Key> {
        path.split('\\')
            .filter(|p| !p.is_empty())
            .try_fold(self, |k, p| k.subkey(p))
    }

    pub fn path_mut(&mut self, path: &str) -> Option<&mut Key> {
        path.split('\\')
            .filter(|p| !p.is_empty())
            .try_fold(self, |k, p| k.subkey_mut(p))
    }

    pub fn value(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|v| Self::same_name(&v.name, name))
    }

    /// Replace or add a value.
    pub fn set_value(&mut self, value: Value) {
        match self
            .values
            .iter_mut()
            .find(|v| Self::same_name(&v.name, &value.name))
        {
            Some(v) => *v = value,
            None => self.values.push(value),
        }
    }

    /// Add a subkey (inheriting this key's security descriptor), or return
    /// the existing one of that name.
    pub fn add_subkey(&mut self, name: &str) -> &mut Key {
        if let Some(i) = self
            .subkeys
            .iter()
            .position(|k| Self::same_name(&k.name, name))
        {
            return &mut self.subkeys[i];
        }
        let mut k = Key::new(name);
        k.security = self.security;
        k.last_written = self.last_written;
        self.subkeys.push(k);
        self.subkeys.last_mut().unwrap()
    }

    pub fn remove_subkey(&mut self, name: &str) -> Option<Key> {
        let i = self
            .subkeys
            .iter()
            .position(|k| Self::same_name(&k.name, name))?;
        Some(self.subkeys.remove(i))
    }
}

/// A whole hive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hive {
    /// The original base block; the writer updates sequence numbers, root
    /// offset, data size and checksum and keeps the rest (version, file name).
    pub base: Vec<u8>,
    /// Security descriptors, shared by index from [`Key::security`].
    pub security: Vec<Vec<u8>>,
    pub root: Key,
}

fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| bad(format!("read past the end at {o:#x}")))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| bad(format!("read past the end at {o:#x}")))
}

fn u64_at(b: &[u8], o: usize) -> Result<u64> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| bad(format!("read past the end at {o:#x}")))
}

/// XOR of the first 127 dwords, with the two values Windows avoids remapped.
fn checksum(base: &[u8]) -> u32 {
    let x = (0..127).fold(0u32, |acc, i| {
        acc ^ u32::from_le_bytes(base[i * 4..i * 4 + 4].try_into().unwrap())
    });
    match x {
        0xFFFF_FFFF => 0xFFFF_FFFE,
        0 => 1,
        x => x,
    }
}

fn decode_name(raw: &[u8], compressed: bool) -> String {
    if compressed {
        raw.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf16_lossy(&utf16_units(raw))
    }
}

struct Reader<'a> {
    data: &'a [u8],
    minor: u32,
    security: Vec<Vec<u8>>,
    security_at: std::collections::HashMap<u32, usize>,
}

impl<'a> Reader<'a> {
    /// The data of the allocated cell at `off`.
    fn cell(&self, off: u32) -> Result<&'a [u8]> {
        let pos = BASE_BLOCK
            .checked_add(off as usize)
            .ok_or_else(|| bad("cell offset overflow"))?;
        let size = u32_at(self.data, pos)? as i32;
        if size >= 0 {
            return Err(bad(format!("cell {off:#x} is free")));
        }
        let len = size.unsigned_abs() as usize;
        if len < 4 {
            return Err(bad(format!("cell {off:#x} is too small")));
        }
        self.data
            .get(pos + 4..pos + len)
            .ok_or_else(|| bad(format!("cell {off:#x} runs past the end")))
    }

    fn security(&mut self, off: u32) -> Result<Option<usize>> {
        if off == 0xFFFF_FFFF {
            return Ok(None);
        }
        if let Some(&i) = self.security_at.get(&off) {
            return Ok(Some(i));
        }
        let c = self.cell(off)?;
        if c.get(0..2) != Some(b"sk") {
            return Err(bad(format!("security cell {off:#x} is not sk")));
        }
        let len = u32_at(c, 16)? as usize;
        let desc = c
            .get(20..20 + len)
            .ok_or_else(|| bad("security descriptor runs past its cell"))?
            .to_vec();
        let i = self.security.len();
        self.security.push(desc);
        self.security_at.insert(off, i);
        Ok(Some(i))
    }

    fn subkey_offsets(&self, off: u32, out: &mut Vec<u32>, depth: usize) -> Result<()> {
        if depth > 4 {
            return Err(bad("nested subkey index lists"));
        }
        let c = self.cell(off)?;
        let count = u16_at(c, 2)? as usize;
        match c.get(0..2) {
            Some(b"lf") | Some(b"lh") => {
                for i in 0..count {
                    out.push(u32_at(c, 4 + i * 8)?);
                }
            }
            Some(b"li") => {
                for i in 0..count {
                    out.push(u32_at(c, 4 + i * 4)?);
                }
            }
            Some(b"ri") => {
                for i in 0..count {
                    self.subkey_offsets(u32_at(c, 4 + i * 4)?, out, depth + 1)?;
                }
            }
            _ => return Err(bad(format!("unknown subkey list at {off:#x}"))),
        }
        Ok(())
    }

    fn value_data(&self, vk: &[u8]) -> Result<Vec<u8>> {
        let raw_len = u32_at(vk, 4)?;
        let off = u32_at(vk, 8)?;
        if raw_len & 0x8000_0000 != 0 {
            let len = (raw_len & 0x7FFF_FFFF) as usize;
            if len > 4 {
                return Err(bad("inline value longer than 4 bytes"));
            }
            return Ok(vk[8..8 + len].to_vec());
        }
        let len = raw_len as usize;
        if len == 0 {
            return Ok(Vec::new());
        }
        let c = self.cell(off)?;
        if len > MAX_CELL_DATA && self.minor >= 4 && c.get(0..2) == Some(b"db") {
            let segments = u16_at(c, 2)? as usize;
            let list = self.cell(u32_at(c, 4)?)?;
            let mut data = Vec::with_capacity(len);
            for i in 0..segments {
                let seg = self.cell(u32_at(list, i * 4)?)?;
                let take = (len - data.len()).min(MAX_CELL_DATA).min(seg.len());
                data.extend_from_slice(&seg[..take]);
            }
            if data.len() != len {
                return Err(bad("big data shorter than its value"));
            }
            return Ok(data);
        }
        c.get(..len)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| bad("value data runs past its cell"))
    }

    fn key(&mut self, off: u32, depth: usize) -> Result<Key> {
        if depth > MAX_DEPTH {
            return Err(bad("key tree too deep (a loop?)"));
        }
        let c = self.cell(off)?;
        if c.get(0..2) != Some(b"nk") {
            return Err(bad(format!("key cell {off:#x} is not nk")));
        }
        let flags = u16_at(c, 2)?;
        let last_written = u64_at(c, 4)?;
        let n_sub = u32_at(c, 20)? as usize;
        let sub_off = u32_at(c, 28)?;
        let n_val = u32_at(c, 36)? as usize;
        let val_off = u32_at(c, 40)?;
        let sk_off = u32_at(c, 44)?;
        let class_off = u32_at(c, 48)?;
        let name_len = u16_at(c, 72)? as usize;
        let class_len = u16_at(c, 74)? as usize;
        let name_raw = c
            .get(76..76 + name_len)
            .ok_or_else(|| bad("key name runs past its cell"))?;
        let name = decode_name(name_raw, flags & KEY_COMP_NAME != 0);

        let class = if class_off != 0xFFFF_FFFF && class_len > 0 {
            Some(
                self.cell(class_off)?
                    .get(..class_len)
                    .ok_or_else(|| bad("class name runs past its cell"))?
                    .to_vec(),
            )
        } else {
            None
        };
        let security = self.security(sk_off)?;

        let mut values = Vec::with_capacity(n_val);
        if n_val > 0 {
            let list = self.cell(val_off)?;
            for i in 0..n_val {
                let vk = self.cell(u32_at(list, i * 4)?)?;
                if vk.get(0..2) != Some(b"vk") {
                    return Err(bad("value cell is not vk"));
                }
                let vname_len = u16_at(vk, 2)? as usize;
                let vflags = u16_at(vk, 16)?;
                let vname = vk
                    .get(20..20 + vname_len)
                    .ok_or_else(|| bad("value name runs past its cell"))?;
                values.push(Value {
                    name: decode_name(vname, vflags & VALUE_COMP_NAME != 0),
                    kind: u32_at(vk, 12)?,
                    data: self.value_data(vk)?,
                });
            }
        }

        let mut subkeys = Vec::with_capacity(n_sub);
        if n_sub > 0 {
            let mut offs = Vec::with_capacity(n_sub);
            self.subkey_offsets(sub_off, &mut offs, 0)?;
            if offs.len() != n_sub {
                return Err(bad(format!(
                    "key {name:?} claims {n_sub} subkeys, its list has {}",
                    offs.len()
                )));
            }
            for o in offs {
                subkeys.push(self.key(o, depth + 1)?);
            }
        }

        Ok(Key {
            name,
            flags: flags & !KEY_COMP_NAME,
            last_written,
            class,
            security,
            values,
            subkeys,
        })
    }
}

impl Hive {
    /// Read a hive file.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < BASE_BLOCK + HBIN || data.get(0..4) != Some(b"regf") {
            return Err(bad("not a registry hive (no regf signature)"));
        }
        let seq1 = u32_at(data, 4)?;
        let seq2 = u32_at(data, 8)?;
        if seq1 != seq2 {
            return Err(OwnBackupError::ImageFormat(
                tr!("regf.dirty", primary = seq1, secondary = seq2).into_owned(),
            ));
        }
        let major = u32_at(data, 20)?;
        let minor = u32_at(data, 24)?;
        if major != 1 || !(2..=6).contains(&minor) {
            return Err(bad(format!("unsupported version {major}.{minor}")));
        }
        let stored = u32_at(data, 508)?;
        if checksum(data) != stored {
            return Err(bad("base block checksum mismatch"));
        }
        let root_off = u32_at(data, 36)?;
        let mut r = Reader {
            data,
            minor,
            security: Vec::new(),
            security_at: Default::default(),
        };
        let root = r.key(root_off, 0)?;
        Ok(Self {
            base: data[..BASE_BLOCK].to_vec(),
            security: r.security,
            root,
        })
    }

    /// A new, empty hive of version 1.3 with a root key `root_name` and one
    /// security descriptor every key will share.
    pub fn new(root_name: &str, security_descriptor: Vec<u8>) -> Self {
        let mut base = vec![0u8; BASE_BLOCK];
        base[0..4].copy_from_slice(b"regf");
        base[20..24].copy_from_slice(&1u32.to_le_bytes());
        base[24..28].copy_from_slice(&3u32.to_le_bytes());
        base[32..36].copy_from_slice(&1u32.to_le_bytes()); // format: direct memory load
        base[44..48].copy_from_slice(&1u32.to_le_bytes()); // clustering factor
        let mut root = Key::new(root_name);
        root.flags = KEY_HIVE_ENTRY | KEY_NO_DELETE;
        root.security = Some(0);
        Self {
            base,
            security: vec![security_descriptor],
            root,
        }
    }

    fn minor(&self) -> u32 {
        u32::from_le_bytes(self.base[24..28].try_into().unwrap())
    }

    /// Lay the whole tree out as a new hive file.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new(self.minor() >= 5);
        // The root key is the first cell of the first bin, as in every hive
        // Windows writes — some readers start there instead of at the offset
        // the base block names. Security descriptors follow: every nk names one.
        let (root_name, _) = encode_name(&self.root.name);
        let root_nk = w.alloc(76 + root_name.len());
        let mut refs = vec![0u32; self.security.len()];
        count_security(&self.root, &mut refs)?;
        let sk: Vec<u32> = self
            .security
            .iter()
            .map(|d| w.alloc(20 + d.len()))
            .collect();
        for (i, d) in self.security.iter().enumerate() {
            let n = sk.len();
            let c = w.cell_mut(sk[i]);
            c[0..2].copy_from_slice(b"sk");
            c[4..8].copy_from_slice(&sk[(i + 1) % n].to_le_bytes()); // flink
            c[8..12].copy_from_slice(&sk[(i + n - 1) % n].to_le_bytes()); // blink
            c[12..16].copy_from_slice(&refs[i].to_le_bytes());
            c[16..20].copy_from_slice(&(d.len() as u32).to_le_bytes());
            c[20..20 + d.len()].copy_from_slice(d);
        }
        let root = w.key(&self.root, 0xFFFF_FFFF, &sk, 0, Some(root_nk))?;
        let bins = w.finish();

        let mut out = self.base.clone();
        let seq = u32::from_le_bytes(out[4..8].try_into().unwrap()).wrapping_add(1);
        out[4..8].copy_from_slice(&seq.to_le_bytes());
        out[8..12].copy_from_slice(&seq.to_le_bytes());
        out[36..40].copy_from_slice(&root.to_le_bytes());
        out[40..44].copy_from_slice(&(bins.len() as u32).to_le_bytes());
        let sum = checksum(&out);
        out[508..512].copy_from_slice(&sum.to_le_bytes());
        out.extend_from_slice(&bins);
        Ok(out)
    }
}

fn count_security(k: &Key, refs: &mut [u32]) -> Result<()> {
    let i = k
        .security
        .ok_or_else(|| bad(format!("key {:?} has no security descriptor", k.name)))?;
    *refs
        .get_mut(i)
        .ok_or_else(|| bad("security index out of range"))? += 1;
    k.subkeys.iter().try_for_each(|s| count_security(s, refs))
}

/// Can `s` be stored in the 8-bit "compressed" form?
fn compressible(s: &str) -> bool {
    s.chars().all(|c| (c as u32) < 0x100)
}

fn encode_name(s: &str) -> (Vec<u8>, bool) {
    if compressible(s) {
        (s.chars().map(|c| c as u8).collect(), true)
    } else {
        (s.encode_utf16().flat_map(u16::to_le_bytes).collect(), false)
    }
}

/// The `lh` hash: base-37 over the upper-cased UTF-16 name.
fn lh_hash(name: &str) -> u32 {
    name.to_uppercase()
        .encode_utf16()
        .fold(0u32, |h, u| h.wrapping_mul(37).wrapping_add(u as u32))
}

/// The `lf` hint: the first four characters, as stored.
fn lf_hint(name: &str) -> u32 {
    let mut b = [0u8; 4];
    for (i, c) in name.chars().take(4).enumerate() {
        b[i] = (c as u32).min(0xFF) as u8;
    }
    u32::from_le_bytes(b)
}

struct Writer {
    bins: Vec<u8>,
    /// Start of the hbin being filled and how far it is used.
    current: usize,
    used: usize,
    lh: bool,
}

impl Writer {
    fn new(lh: bool) -> Self {
        let mut w = Self {
            bins: Vec::new(),
            current: 0,
            used: 0,
            lh,
        };
        w.open_bin(HBIN);
        w
    }

    fn open_bin(&mut self, size: usize) {
        self.current = self.bins.len();
        self.bins.resize(self.current + size, 0);
        let h = &mut self.bins[self.current..];
        h[0..4].copy_from_slice(b"hbin");
        h[4..8].copy_from_slice(&(self.current as u32).to_le_bytes());
        h[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        self.used = HBIN_HEADER;
    }

    fn bin_size(&self) -> usize {
        u32::from_le_bytes(
            self.bins[self.current + 8..self.current + 12]
                .try_into()
                .unwrap(),
        ) as usize
    }

    /// Close the current bin: what is left becomes one free cell.
    fn close_bin(&mut self) {
        let left = self.bin_size() - self.used;
        if left > 0 {
            let at = self.current + self.used;
            self.bins[at..at + 4].copy_from_slice(&(left as i32).to_le_bytes());
        }
    }

    /// Allocate a cell for `len` bytes of data; returns its offset.
    fn alloc(&mut self, len: usize) -> u32 {
        let size = (4 + len).div_ceil(8) * 8;
        if self.used + size > self.bin_size() {
            self.close_bin();
            self.open_bin((HBIN_HEADER + size).div_ceil(HBIN) * HBIN);
        }
        let at = self.current + self.used;
        self.bins[at..at + 4].copy_from_slice(&(-(size as i32)).to_le_bytes());
        self.used += size;
        at as u32
    }

    fn cell_mut(&mut self, off: u32) -> &mut [u8] {
        let at = off as usize;
        let size = (i32::from_le_bytes(self.bins[at..at + 4].try_into().unwrap())).unsigned_abs();
        &mut self.bins[at + 4..at + size as usize]
    }

    fn put(&mut self, data: &[u8]) -> u32 {
        let off = self.alloc(data.len());
        self.cell_mut(off)[..data.len()].copy_from_slice(data);
        off
    }

    fn value(&mut self, v: &Value) -> Result<u32> {
        let (name, compressed) = encode_name(&v.name);
        let vk = self.alloc(20 + name.len());
        let (len_field, data_off) = if v.data.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..v.data.len()].copy_from_slice(&v.data);
            (
                0x8000_0000 | v.data.len() as u32,
                u32::from_le_bytes(inline),
            )
        } else if v.data.len() <= MAX_CELL_DATA {
            (v.data.len() as u32, self.put(&v.data))
        } else {
            return Err(bad(format!(
                "value {:?} is {} bytes; big data is not written",
                v.name,
                v.data.len()
            )));
        };
        let c = self.cell_mut(vk);
        c[0..2].copy_from_slice(b"vk");
        c[2..4].copy_from_slice(&(name.len() as u16).to_le_bytes());
        c[4..8].copy_from_slice(&len_field.to_le_bytes());
        c[8..12].copy_from_slice(&data_off.to_le_bytes());
        c[12..16].copy_from_slice(&v.kind.to_le_bytes());
        let flags = if compressed { VALUE_COMP_NAME } else { 0 };
        c[16..18].copy_from_slice(&flags.to_le_bytes());
        c[20..20 + name.len()].copy_from_slice(&name);
        Ok(vk)
    }

    fn leaf(&mut self, entries: &[(u32, &str)]) -> u32 {
        let mut b = Vec::with_capacity(4 + entries.len() * 8);
        b.extend_from_slice(if self.lh { b"lh" } else { b"lf" });
        b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (off, name) in entries {
            b.extend_from_slice(&off.to_le_bytes());
            let hint = if self.lh {
                lh_hash(name)
            } else {
                lf_hint(name)
            };
            b.extend_from_slice(&hint.to_le_bytes());
        }
        self.put(&b)
    }

    fn key(
        &mut self,
        k: &Key,
        parent: u32,
        sk: &[u32],
        depth: usize,
        nk: Option<u32>,
    ) -> Result<u32> {
        if depth > MAX_DEPTH {
            return Err(bad("key tree too deep"));
        }
        let (name, compressed) = encode_name(&k.name);
        if name.len() > 0xFFFF {
            return Err(bad("key name too long"));
        }
        let nk = nk.unwrap_or_else(|| self.alloc(76 + name.len()));

        let class_off = match &k.class {
            Some(c) if !c.is_empty() => self.put(c),
            _ => 0xFFFF_FFFF,
        };

        let mut value_offs = Vec::with_capacity(k.values.len());
        for v in &k.values {
            value_offs.push(self.value(v)?);
        }
        let values_list = if value_offs.is_empty() {
            0xFFFF_FFFF
        } else {
            let b: Vec<u8> = value_offs.iter().flat_map(|o| o.to_le_bytes()).collect();
            self.put(&b)
        };

        // Subkey lists must be sorted by upper-cased name.
        let mut order: Vec<&Key> = k.subkeys.iter().collect();
        order.sort_by_key(|s| s.name.to_uppercase());
        for w in order.windows(2) {
            if w[0].name.to_uppercase() == w[1].name.to_uppercase() {
                return Err(bad(format!("two subkeys named {:?}", w[0].name)));
            }
        }
        let mut sub = Vec::with_capacity(order.len());
        for s in &order {
            sub.push((self.key(s, nk, sk, depth + 1, None)?, s.name.as_str()));
        }
        let sub_list = if sub.is_empty() {
            0xFFFF_FFFF
        } else if sub.len() <= LEAF_MAX * 2 {
            self.leaf(&sub)
        } else {
            let leaves: Vec<u32> = sub.chunks(LEAF_MAX).map(|c| self.leaf(c)).collect();
            let mut b = b"ri".to_vec();
            b.extend_from_slice(&(leaves.len() as u16).to_le_bytes());
            leaves
                .iter()
                .for_each(|o| b.extend_from_slice(&o.to_le_bytes()));
            self.put(&b)
        };

        let max_sub_name = k
            .subkeys
            .iter()
            .map(|s| s.name.encode_utf16().count() * 2)
            .max();
        let max_sub_class = k
            .subkeys
            .iter()
            .map(|s| s.class.as_ref().map_or(0, Vec::len))
            .max();
        let max_val_name = k
            .values
            .iter()
            .map(|v| v.name.encode_utf16().count() * 2)
            .max();
        let max_val_data = k.values.iter().map(|v| v.data.len()).max();
        let security = k
            .security
            .and_then(|i| sk.get(i).copied())
            .ok_or_else(|| bad(format!("key {:?} has no security descriptor", k.name)))?;

        let c = self.cell_mut(nk);
        c[0..2].copy_from_slice(b"nk");
        let flags = (k.flags & !KEY_COMP_NAME) | if compressed { KEY_COMP_NAME } else { 0 };
        c[2..4].copy_from_slice(&flags.to_le_bytes());
        c[4..12].copy_from_slice(&k.last_written.to_le_bytes());
        c[16..20].copy_from_slice(&parent.to_le_bytes());
        c[20..24].copy_from_slice(&(k.subkeys.len() as u32).to_le_bytes());
        c[28..32].copy_from_slice(&sub_list.to_le_bytes());
        c[32..36].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // volatile list
        c[36..40].copy_from_slice(&(k.values.len() as u32).to_le_bytes());
        c[40..44].copy_from_slice(&values_list.to_le_bytes());
        c[44..48].copy_from_slice(&security.to_le_bytes());
        c[48..52].copy_from_slice(&class_off.to_le_bytes());
        c[52..56].copy_from_slice(&(max_sub_name.unwrap_or(0) as u32).to_le_bytes());
        c[56..60].copy_from_slice(&(max_sub_class.unwrap_or(0) as u32).to_le_bytes());
        c[60..64].copy_from_slice(&(max_val_name.unwrap_or(0) as u32).to_le_bytes());
        c[64..68].copy_from_slice(&(max_val_data.unwrap_or(0) as u32).to_le_bytes());
        c[72..74].copy_from_slice(&(name.len() as u16).to_le_bytes());
        let class_len = k.class.as_ref().map_or(0, Vec::len) as u16;
        c[74..76].copy_from_slice(&class_len.to_le_bytes());
        c[76..76 + name.len()].copy_from_slice(&name);
        Ok(nk)
    }

    fn finish(mut self) -> Vec<u8> {
        self.close_bin();
        self.bins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-relative security descriptor: owner/group SYSTEM, no DACL.
    fn sd() -> Vec<u8> {
        let mut d = vec![1, 0, 0x04, 0x80];
        d.extend_from_slice(&20u32.to_le_bytes()); // owner
        d.extend_from_slice(&32u32.to_le_bytes()); // group
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        let system = [1u8, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        d.extend_from_slice(&system);
        d.extend_from_slice(&system);
        d
    }

    fn sample() -> Hive {
        let mut h = Hive::new("ROOT", sd());
        h.root.last_written = 0x01D9_0000_0000_0000;
        let objects = h.root.add_subkey("Objects");
        let o = objects.add_subkey("{9dea862c-5cdd-4e70-acc1-f32b344d4795}");
        o.add_subkey("Description")
            .set_value(Value::dword("Type", 0x1010_0002));
        let e = o.add_subkey("Elements");
        e.add_subkey("12000004")
            .set_value(Value::string("Element", "Windows Boot Manager"));
        e.add_subkey("11000001").set_value(Value {
            name: "Element".into(),
            kind: REG_BINARY,
            data: (0..88u8).collect(),
        });
        h.root.set_value(Value::string("", "default value"));
        h.root.add_subkey("Ünïcödé ключ").class = Some(vec![0x41, 0, 0x42, 0]);
        h
    }

    #[test]
    fn a_hive_round_trips_through_bytes() {
        let h = sample();
        let bytes = h.to_bytes().unwrap();
        assert_eq!(bytes.len() % 4096, 0);
        assert_eq!(u32_at(&bytes, 36).unwrap(), 0x20, "root is the first cell");
        let back = Hive::parse(&bytes).unwrap();
        assert_eq!(back.root, sample_sorted(&h.root));
        assert_eq!(back.security, h.security);
        let e = back
            .root
            .path("objects\\{9DEA862C-5CDD-4E70-ACC1-F32B344D4795}\\Elements\\12000004")
            .unwrap();
        assert_eq!(
            e.value("element").unwrap().as_string().unwrap(),
            "Windows Boot Manager"
        );
        // Rewriting what we read is stable apart from the sequence number.
        let again = back.to_bytes().unwrap();
        assert_eq!(again[12..508], bytes[12..508]);
        assert_eq!(again[512..], bytes[512..]);
        assert_eq!(u32_at(&again, 4).unwrap(), u32_at(&bytes, 4).unwrap() + 1);
    }

    /// The reader returns subkeys in on-disk (sorted) order.
    fn sample_sorted(k: &Key) -> Key {
        let mut k = k.clone();
        k.subkeys.sort_by_key(|s| s.name.to_uppercase());
        k.subkeys = k.subkeys.iter().map(sample_sorted).collect();
        k
    }

    #[test]
    fn a_dirty_hive_is_refused() {
        let mut bytes = sample().to_bytes().unwrap();
        bytes[4] ^= 1;
        let sum = checksum(&bytes);
        bytes[508..512].copy_from_slice(&sum.to_le_bytes());
        assert!(Hive::parse(&bytes).is_err());
    }

    #[test]
    fn a_damaged_checksum_or_signature_is_refused() {
        let bytes = sample().to_bytes().unwrap();
        let mut b = bytes.clone();
        b[100] ^= 1;
        assert!(Hive::parse(&b).is_err());
        let mut b = bytes.clone();
        b[0] = b'x';
        assert!(Hive::parse(&b).is_err());
        assert!(Hive::parse(&bytes[..4096]).is_err());
    }

    #[test]
    fn garbage_offsets_are_errors_not_panics() {
        let bytes = sample().to_bytes().unwrap();
        // Point the root at every offset in the first bin; none may panic.
        for off in (0..4096u32).step_by(4) {
            let mut b = bytes.clone();
            b[36..40].copy_from_slice(&off.to_le_bytes());
            let sum = checksum(&b);
            b[508..512].copy_from_slice(&sum.to_le_bytes());
            let _ = Hive::parse(&b);
        }
    }

    #[test]
    fn many_subkeys_go_under_an_ri_list_and_come_back_sorted() {
        let mut h = Hive::new("R", sd());
        for i in (0..1500).rev() {
            h.root.add_subkey(&format!("{i:08X}"));
        }
        let back = Hive::parse(&h.to_bytes().unwrap()).unwrap();
        assert_eq!(back.root.subkeys.len(), 1500);
        assert_eq!(back.root.subkeys[0].name, "00000000");
        assert_eq!(back.root.subkeys[1499].name, "000005DB");
    }

    #[test]
    fn version_1_5_writes_lh_lists_with_the_documented_hash() {
        let mut h = sample();
        h.base[24..28].copy_from_slice(&5u32.to_le_bytes());
        let bytes = h.to_bytes().unwrap();
        assert!(bytes.windows(2).any(|w| w == b"lh"));
        Hive::parse(&bytes).unwrap();
        // "A" → 0x41; "ab" → 'A'*37 + 'B'.
        assert_eq!(lh_hash("a"), 0x41);
        assert_eq!(lh_hash("ab"), 0x41 * 37 + 0x42);
    }

    #[test]
    fn duplicate_subkey_names_are_refused() {
        let mut h = Hive::new("R", sd());
        h.root.subkeys.push(Key {
            security: Some(0),
            ..Key::new("x")
        });
        h.root.subkeys.push(Key {
            security: Some(0),
            ..Key::new("X")
        });
        assert!(h.to_bytes().is_err());
    }

    #[test]
    fn a_large_value_spans_its_own_bin() {
        let mut h = Hive::new("R", sd());
        h.root.set_value(Value {
            name: "big".into(),
            kind: REG_BINARY,
            data: vec![7; 10_000],
        });
        let back = Hive::parse(&h.to_bytes().unwrap()).unwrap();
        assert_eq!(back.root.value("BIG").unwrap().data, vec![7; 10_000]);
        h.root.set_value(Value {
            name: "huge".into(),
            kind: REG_BINARY,
            data: vec![7; 20_000],
        });
        assert!(h.to_bytes().is_err(), "big data is refused, not truncated");
    }
    /// hivex as an outside judge (only a test tool on the checking machine,
    /// not a dependency — it is LGPL): `hivexregedit` must read what we write,
    /// and we must read what `hivexsh` writes into it (new keys in fresh
    /// bins, values, a deletion). Needs `libhivex-bin` and
    /// `libwin-hivex-perl`: `cargo test -p own-backup-backup -- --ignored hivex`.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn hivex_reads_ours_and_we_read_hivex() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("BCD");
        std::fs::write(&path, sample().to_bytes().unwrap()).unwrap();

        let out = Command::new("hivexregedit")
            .args(["--export", "--unsafe-printable-strings"])
            .arg(&path)
            .arg("\\")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("Windows Boot Manager"), "{text}");
        assert!(text.contains("\"Type\"=dword:10100002"), "{text}");
        assert!(text.contains("Ünïcödé ключ"), "{text}");

        let script = dir.path().join("edit");
        std::fs::write(
            &script,
            "cd \\Objects\n\
             add {11111111-2222-3333-4444-555555555555}\n\
             cd {11111111-2222-3333-4444-555555555555}\n\
             setval 2\n\
             Name\n\
             string:added by hivex\n\
             Bits\n\
             dword:0x12345678\n\
             cd \\Ünïcödé ключ\n\
             del\n\
             commit\n",
        )
        .unwrap();
        let out = Command::new("hivexsh")
            .arg("-w")
            .arg("-f")
            .arg(&script)
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let back = Hive::parse(&std::fs::read(&path).unwrap()).unwrap();
        let k = back
            .root
            .path("Objects\\{11111111-2222-3333-4444-555555555555}")
            .unwrap();
        assert_eq!(
            k.value("Name").unwrap().as_string().unwrap(),
            "added by hivex"
        );
        assert_eq!(k.value("Bits").unwrap().as_dword(), Some(0x1234_5678));
        assert!(back.root.subkey("Ünïcödé ключ").is_none());
        assert!(back
            .root
            .path("Objects\\{9dea862c-5cdd-4e70-acc1-f32b344d4795}\\Elements\\11000001")
            .is_some());
        // …and rewritten by us, hivex still reads it.
        std::fs::write(&path, back.to_bytes().unwrap()).unwrap();
        let out = Command::new("hivexregedit")
            .args(["--export", "--unsafe-printable-strings"])
            .arg(&path)
            .arg("\\")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("added by hivex"));
    }
}
