//! An LVM2 physical volume, as far as a whole-disk copy needs to know it.
//!
//! A partition that is a PV holds three kinds of bytes: the PV label and the
//! volume group's metadata area at its head (and sometimes a second metadata
//! area at its end), the extents of its logical volumes, and free extents.
//! A copy of such a partition reads every active LV through its own snapshot
//! and the rest from the partition itself; the free extents are not copied at
//! all — LVM never reads them.
//!
//! What is here is pure: the on-disk label (`LABELONE` / `LVM2 001`), the
//! metadata area header and its circular text buffer, the text metadata format
//! itself, and the allocation of one PV read from that text. And the record a
//! snapshot of a PV leaves for the engine ([`PvSnapshot`]), shared by the
//! snapshot crate, which writes it, and the engine, which reads it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The label is in one of the first four sectors.
pub const LABEL_SCAN_BYTES: usize = 4 * 512;
const SECTOR: u64 = 512;
const LABEL_ID: &[u8; 8] = b"LABELONE";
const LABEL_TYPE: &[u8; 8] = b"LVM2 001";
const MDA_MAGIC: &[u8; 16] = b" LVM2 x[5A%r0N*>";
const MDA_HEADER_SIZE: u64 = 512;
/// `raw_locn.flags`: this metadata area is not used.
const RAW_LOCN_IGNORED: u32 = 1;
/// No metadata text this large is plausible; a larger one is a damaged header.
pub const MAX_METADATA_AREA: u64 = 256 << 20;

/// The PV label: the PV's UUID and where its data and metadata lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvLabel {
    /// 32 characters, without the dashes the tools print.
    pub uuid: String,
    pub device_size: u64,
    /// `(offset, size)` in bytes; the first offset is `pe_start`. A size of 0
    /// means "to the end of the device".
    pub data_areas: Vec<(u64, u64)>,
    /// `(offset, size)` in bytes of every metadata area.
    pub metadata_areas: Vec<(u64, u64)>,
}

impl PvLabel {
    /// Where the first extent starts, in bytes.
    pub fn pe_start(&self) -> Option<u64> {
        self.data_areas.first().map(|a| a.0)
    }
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The PV label in `head` (the first [`LABEL_SCAN_BYTES`] of the device), or
/// `None` when the device is no LVM2 physical volume.
pub fn parse_label(head: &[u8]) -> Option<PvLabel> {
    for sector in 0..(LABEL_SCAN_BYTES as u64 / SECTOR) {
        let at = (sector * SECTOR) as usize;
        let Some(s) = head.get(at..at + SECTOR as usize) else {
            break;
        };
        if &s[..8] != LABEL_ID || &s[24..32] != LABEL_TYPE || u64_at(s, 8)? != sector {
            continue;
        }
        let pv = u32_at(s, 20)? as usize;
        if pv + 40 > s.len() {
            return None;
        }
        let uuid = std::str::from_utf8(&s[pv..pv + 32]).ok()?.to_string();
        let device_size = u64_at(s, pv + 32)?;
        let mut lists = [Vec::new(), Vec::new()];
        let mut at = pv + 40;
        for list in &mut lists {
            loop {
                let (offset, size) = (u64_at(s, at)?, u64_at(s, at + 8)?);
                at += 16;
                if offset == 0 {
                    break;
                }
                list.push((offset, size));
            }
        }
        let [data_areas, metadata_areas] = lists;
        return Some(PvLabel {
            uuid,
            device_size,
            data_areas,
            metadata_areas,
        });
    }
    None
}

/// The current metadata text of a metadata area: `area` is the area itself,
/// read from its offset (its header first). `None` when the header is not an
/// LVM2 metadata area header, the area is marked unused, or the text does not
/// fit it.
pub fn metadata_text(area: &[u8]) -> Option<String> {
    if area.get(4..20)? != MDA_MAGIC {
        return None;
    }
    let size = u64_at(area, 32)?.min(area.len() as u64);
    // The first raw location is the committed metadata.
    let (offset, len, flags) = (u64_at(area, 40)?, u64_at(area, 48)?, u32_at(area, 60)?);
    if flags & RAW_LOCN_IGNORED != 0 || len == 0 || offset < MDA_HEADER_SIZE || offset >= size {
        return None;
    }
    let mut text = Vec::with_capacity(len as usize);
    let first = len.min(size - offset);
    text.extend_from_slice(area.get(offset as usize..(offset + first) as usize)?);
    // The buffer is circular: what does not fit continues after the header.
    let rest = len - first;
    if rest > 0 {
        let end = MDA_HEADER_SIZE.checked_add(rest)?;
        if end > offset {
            return None;
        }
        text.extend_from_slice(area.get(MDA_HEADER_SIZE as usize..end as usize)?);
    }
    if let Some(nul) = text.iter().position(|&b| b == 0) {
        text.truncate(nul);
    }
    String::from_utf8(text).ok()
}

// ---------------------------------------------------------------------------
// The text format
// ---------------------------------------------------------------------------

/// A value of the LVM text format.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Int(i64),
    Str(String),
    List(Vec<Value>),
    Section(Vec<(String, Value)>),
}

impl Value {
    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Section(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    fn sections(&self) -> impl Iterator<Item = (&str, &Value)> {
        let items: &[(String, Value)] = match self {
            Value::Section(items) => items,
            _ => &[],
        };
        items
            .iter()
            .filter(|(_, v)| matches!(v, Value::Section(_)))
            .map(|(k, v)| (k.as_str(), v))
    }

    /// Does `name` appear as a string anywhere in this value?
    fn mentions(&self, name: &str) -> bool {
        match self {
            Value::Str(s) => s == name,
            Value::List(v) => v.iter().any(|v| v.mentions(name)),
            Value::Section(items) => items.iter().any(|(_, v)| v.mentions(name)),
            Value::Int(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Str(String),
    Punct(char),
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '{' | '}' | '[' | ']' | '=' | ',' => {
                out.push(Token::Punct(c));
                chars.next();
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => return Err("unterminated string".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e) => s.push(e),
                            None => return Err("unterminated string".into()),
                        },
                        Some(c) => s.push(c),
                    }
                }
                out.push(Token::Str(s));
            }
            c if c.is_ascii_alphanumeric() || "_.+-".contains(c) => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || "_.+-".contains(c) {
                        w.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Token::Word(w));
            }
            other => return Err(format!("unexpected character {other:?}")),
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    /// Items up to `}` (inside a section) or the end (at the top).
    fn items(&mut self, nested: bool) -> Result<Vec<(String, Value)>, String> {
        let mut items = Vec::new();
        loop {
            match self.next() {
                None if !nested => return Ok(items),
                None => return Err("unterminated section".into()),
                Some(Token::Punct('}')) if nested => return Ok(items),
                Some(Token::Word(key)) => match self.next() {
                    Some(Token::Punct('=')) => {
                        let v = self.value()?;
                        items.push((key, v));
                    }
                    Some(Token::Punct('{')) => {
                        let inner = self.items(true)?;
                        items.push((key, Value::Section(inner)));
                    }
                    other => return Err(format!("after {key}: {other:?}")),
                },
                Some(other) => return Err(format!("unexpected {other:?}")),
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Str(s)) => Ok(Value::Str(s)),
            Some(Token::Word(w)) => w
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| format!("not a number: {w}")),
            Some(Token::Punct('[')) => {
                let mut list = Vec::new();
                if self.peek() == Some(&Token::Punct(']')) {
                    self.next();
                    return Ok(Value::List(list));
                }
                loop {
                    list.push(self.value()?);
                    match self.next() {
                        Some(Token::Punct(',')) => {}
                        Some(Token::Punct(']')) => return Ok(Value::List(list)),
                        other => return Err(format!("in a list: {other:?}")),
                    }
                }
            }
            other => Err(format!("expected a value, got {other:?}")),
        }
    }
}

fn parse_text(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        tokens: tokenize(text)?,
        at: 0,
    };
    Ok(Value::Section(p.items(false)?))
}

// ---------------------------------------------------------------------------
// Allocation of one PV
// ---------------------------------------------------------------------------

/// Which extents of one PV its volume group gives to logical volumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    /// Volume group name.
    pub vg: String,
    pub seqno: u64,
    /// Bytes.
    pub extent_size: u64,
    /// Bytes from the start of the PV.
    pub pe_start: u64,
    pub pe_count: u64,
    /// Allocated extents: `(first, count)`, sorted, merged.
    pub used: Vec<(u64, u64)>,
    /// The volume group has physical volumes besides this one.
    pub other_pvs: bool,
}

impl Allocation {
    /// The byte range `[start, end)` on the PV of extents `first..first+count`.
    fn bytes(&self, first: u64, count: u64) -> (u64, u64) {
        (
            self.pe_start + first * self.extent_size,
            self.pe_start + (first + count) * self.extent_size,
        )
    }

    /// Allocated extents as byte ranges `[start, end)` on the PV.
    pub fn used_ranges(&self) -> Vec<(u64, u64)> {
        self.used.iter().map(|&(f, n)| self.bytes(f, n)).collect()
    }

    /// Free extents as byte ranges `[start, end)` on the PV.
    pub fn free_ranges(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(first, count) in &self.used {
            if first > at {
                out.push(self.bytes(at, first - at));
            }
            at = at.max(first + count);
        }
        if at < self.pe_count {
            out.push(self.bytes(at, self.pe_count - at));
        }
        out
    }
}

fn nonneg(v: Option<i64>, what: &str) -> Result<u64, String> {
    match v {
        Some(n) if n >= 0 => Ok(n as u64),
        _ => Err(format!("no {what}")),
    }
}

/// The allocation of the PV `pv_uuid` (as in its label, no dashes) in the
/// metadata `text`. An error when the text does not describe that PV, or
/// puts on it a segment type whose extents are not plain stripes (a pvmove
/// in progress, a mirror log): the caller then copies the PV whole.
pub fn allocation(text: &str, pv_uuid: &str) -> Result<Allocation, String> {
    let root = parse_text(text)?;
    let (vg_name, vg) = root
        .sections()
        .find(|(_, s)| s.get("physical_volumes").is_some())
        .ok_or("no volume group in the metadata")?;
    let extent_size = nonneg(vg.int("extent_size"), "extent_size")? * SECTOR;
    if extent_size == 0 {
        return Err("extent size 0".into());
    }
    let seqno = nonneg(vg.int("seqno"), "seqno")?;
    let pvs = vg.get("physical_volumes").ok_or("no physical_volumes")?;
    let mut ours = None;
    let mut count = 0;
    for (name, pv) in pvs.sections() {
        count += 1;
        let id: String = pv.str("id").unwrap_or_default().replace('-', "");
        if id == pv_uuid {
            ours = Some((name, pv));
        }
    }
    let (pv_name, pv) = ours.ok_or("this PV is not in its volume group's metadata")?;
    let pe_start = nonneg(pv.int("pe_start"), "pe_start")? * SECTOR;
    let pe_count = nonneg(pv.int("pe_count"), "pe_count")?;

    let mut used: Vec<(u64, u64)> = Vec::new();
    if let Some(lvs) = vg.get("logical_volumes") {
        for (lv_name, lv) in lvs.sections() {
            for (seg_name, seg) in lv.sections() {
                if !seg_name.starts_with("segment") {
                    continue;
                }
                let kind = seg.str("type").unwrap_or_default();
                if kind != "striped" {
                    if seg.mentions(pv_name) {
                        return Err(format!("{lv_name}/{seg_name}: a {kind} segment on this PV"));
                    }
                    continue;
                }
                let extents = nonneg(seg.int("extent_count"), "extent_count")?;
                let stripes = match seg.int("stripe_count") {
                    None => 1,
                    Some(n) if n > 0 => n as u64,
                    Some(_) => return Err(format!("{lv_name}/{seg_name}: bad stripe_count")),
                };
                if extents % stripes != 0 {
                    return Err(format!("{lv_name}/{seg_name}: uneven stripes"));
                }
                let per = extents / stripes;
                let Some(Value::List(list)) = seg.get("stripes") else {
                    return Err(format!("{lv_name}/{seg_name}: no stripes"));
                };
                if list.len() as u64 != stripes * 2 {
                    return Err(format!("{lv_name}/{seg_name}: stripes do not match"));
                }
                for pair in list.chunks(2) {
                    match pair {
                        [Value::Str(pv), Value::Int(start)] if pv == pv_name && *start >= 0 => {
                            used.push((*start as u64, per));
                        }
                        [Value::Str(_), Value::Int(_)] => {}
                        _ => return Err(format!("{lv_name}/{seg_name}: odd stripe")),
                    }
                }
            }
        }
    }
    used.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (first, n) in used {
        if n == 0 {
            continue;
        }
        if first + n > pe_count {
            return Err(format!("extents {first}+{n} past the PV's {pe_count}"));
        }
        match merged.last_mut() {
            Some(last) if first < last.0 + last.1 => {
                return Err(format!("extent {first} given twice"));
            }
            Some(last) if first == last.0 + last.1 => last.1 += n,
            _ => merged.push((first, n)),
        }
    }
    Ok(Allocation {
        vg: vg_name.to_string(),
        seqno,
        extent_size,
        pe_start,
        pe_count,
        used: merged,
        other_pvs: count > 1,
    })
}

// ---------------------------------------------------------------------------
// What a snapshot of a PV leaves for the engine
// ---------------------------------------------------------------------------

/// Where part of a logical volume lies on the PV. Bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PvSegment {
    /// From the start of the PV (the partition).
    pub pv_offset: u64,
    /// From the start of the logical volume.
    pub lv_offset: u64,
    pub length: u64,
}

/// One logical volume of the PV, as its snapshot holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PvSnapshotVolume {
    /// Its device-mapper name (`ubuntu--vg-ubuntu--lv`).
    pub name: String,
    /// The volume itself (`/dev/dm-1`): where the key store rule is asked.
    pub origin: String,
    /// The snapshot device (`/dev/dm-7`): what the copy reads.
    pub snapshot: String,
    /// The snapshot's own id (`obk-snap-<id>`, `obk-real-<id>`).
    pub dm_id: String,
    /// Bytes.
    pub size: u64,
    /// Its parts on this PV; parts on other PVs are not listed.
    pub segments: Vec<PvSegment>,
}

/// The snapshot of a PV in use: every active logical volume on it held at
/// one point in time, and the bytes that must not change while the rest of
/// the PV is read from the partition itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PvSnapshot {
    /// The partition (the PV) as it was named.
    pub device: String,
    pub volumes: Vec<PvSnapshotVolume>,
    /// `(offset, length)` on the partition: the label and every metadata
    /// area. A volume group change rewrites them.
    pub guard: Vec<(u64, u64)>,
    /// Their digest when the snapshot was taken ([`guard_digest`]).
    pub guard_digest: String,
}

impl PvSnapshot {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// The byte ranges whose change means the volume group changed: everything
/// before the first extent (the label, the first metadata area) and every
/// metadata area, within `device_size`.
pub fn guard_ranges(label: &PvLabel, device_size: u64) -> Vec<(u64, u64)> {
    let mut ranges: BTreeMap<u64, u64> = BTreeMap::new();
    let head = label.pe_start().unwrap_or(LABEL_SCAN_BYTES as u64);
    ranges.insert(0, head.max(LABEL_SCAN_BYTES as u64).min(device_size));
    for &(offset, size) in &label.metadata_areas {
        if offset < device_size {
            let len = size.min(device_size - offset).min(MAX_METADATA_AREA);
            let e = ranges.entry(offset).or_insert(0);
            *e = (*e).max(len);
        }
    }
    // Merge overlaps.
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (offset, len) in ranges {
        match out.last_mut() {
            Some(last) if offset <= last.0 + last.1 => {
                last.1 = last.1.max(offset + len - last.0);
            }
            _ => out.push((offset, len)),
        }
    }
    out.retain(|r| r.1 > 0);
    out
}

/// A digest of the guarded bytes, in order. Not cryptographic: it tells
/// whether LVM rewrote its metadata during a copy, nothing more.
pub fn guard_digest<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> String {
    // FNV-1a, 64 bits, twice with different bases: stable across builds.
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x84222325_cbf29ce4;
    for chunk in chunks {
        for &byte in chunk {
            a = (a ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            b = (b ^ u64::from(byte))
                .wrapping_mul(0x0000_0100_0000_01b3)
                .rotate_left(5);
        }
    }
    format!("{a:016x}{b:016x}")
}

/// Builders of PV labels, metadata areas and metadata text, for the tests of
/// this crate and of the engine.
#[doc(hidden)]
pub mod fixtures {
    use super::*;

    pub const UUID: &str = "AbCdEf0123456789AbCdEf0123456789";

    /// A label sector as `pvcreate` writes it, in sector 1.
    pub fn label(device_size: u64, pe_start: u64, mdas: &[(u64, u64)]) -> Vec<u8> {
        let mut head = vec![0u8; LABEL_SCAN_BYTES];
        let s = &mut head[512..1024];
        s[..8].copy_from_slice(LABEL_ID);
        s[8..16].copy_from_slice(&1u64.to_le_bytes());
        s[20..24].copy_from_slice(&32u32.to_le_bytes());
        s[24..32].copy_from_slice(LABEL_TYPE);
        s[32..64].copy_from_slice(UUID.as_bytes());
        s[64..72].copy_from_slice(&device_size.to_le_bytes());
        let mut at = 72;
        let mut put = |s: &mut [u8], a: u64, b: u64| {
            s[at..at + 8].copy_from_slice(&a.to_le_bytes());
            s[at + 8..at + 16].copy_from_slice(&b.to_le_bytes());
            at += 16;
        };
        put(s, pe_start, 0);
        put(s, 0, 0);
        for &(o, l) in mdas {
            put(s, o, l);
        }
        put(s, 0, 0);
        head
    }

    /// A metadata area of `size` bytes holding `text` at `offset` (wrapping
    /// round past its end when it must).
    pub fn mda(size: u64, offset: u64, text: &str) -> Vec<u8> {
        let mut area = vec![0u8; size as usize];
        area[4..20].copy_from_slice(MDA_MAGIC);
        area[20..24].copy_from_slice(&1u32.to_le_bytes());
        area[24..32].copy_from_slice(&4096u64.to_le_bytes());
        area[32..40].copy_from_slice(&size.to_le_bytes());
        let bytes = text.as_bytes();
        area[40..48].copy_from_slice(&offset.to_le_bytes());
        area[48..56].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        for (i, &b) in bytes.iter().enumerate() {
            let mut at = offset + i as u64;
            if at >= size {
                at = at - size + MDA_HEADER_SIZE;
            }
            area[at as usize] = b;
        }
        area
    }

    /// A segment: `(type, [(pv, first extent)…], extent_count)`.
    pub type Segment<'a> = (&'a str, Vec<(&'a str, u64)>, u64);
    /// A logical volume: `(name, segments)`.
    pub type Lv<'a> = (&'a str, Vec<Segment<'a>>);

    /// Metadata of a VG `vg0` with this PV as `pv0` (`pe_count` extents of
    /// 4 MiB from 1 MiB) and the LVs given.
    pub fn vg_text(pe_count: u64, lvs: &[Lv<'_>]) -> String {
        let mut t = String::from(
            "vg0 {\nid = \"aaaaaa-bbbb-cccc-dddd-eeee-ffff-gggggg\"\nseqno = 7\nformat = \"lvm2\" # x\n\
             status = [\"RESIZEABLE\", \"READ\", \"WRITE\"]\nflags = []\nextent_size = 8192\nmax_lv = 0\n\n\
             physical_volumes {\n\npv0 {\nid = \"AbCdEf-0123-4567-89Ab-CdEf-0123-456789\"\n\
             device = \"/dev/vda3\"\nstatus = [\"ALLOCATABLE\"]\nflags = []\ndev_size = 999\n\
             pe_start = 2048\n",
        );
        t.push_str(&format!(
            "pe_count = {pe_count}\n}}\n}}\n\nlogical_volumes {{\n"
        ));
        for (name, segs) in lvs {
            t.push_str(&format!(
                "\n{name} {{\nid = \"x\"\nstatus = [\"READ\", \"WRITE\", \"VISIBLE\"]\nsegment_count = {}\n",
                segs.len()
            ));
            let mut start = 0;
            for (i, (kind, stripes, count)) in segs.iter().enumerate() {
                t.push_str(&format!(
                    "\nsegment{} {{\nstart_extent = {start}\nextent_count = {count}\n\ntype = \"{kind}\"\n",
                    i + 1
                ));
                if *kind == "striped" {
                    t.push_str(&format!(
                        "stripe_count = {}\n\nstripes = [\n",
                        stripes.len()
                    ));
                    let pairs: Vec<String> = stripes
                        .iter()
                        .map(|(pv, first)| format!("\"{pv}\", {first}"))
                        .collect();
                    t.push_str(&pairs.join(",\n"));
                    t.push_str("\n]\n}\n");
                } else {
                    let names: Vec<String> = stripes
                        .iter()
                        .map(|(pv, f)| format!("\"{pv}\", {f}"))
                        .collect();
                    t.push_str(&format!("mirrors = [\n{}\n]\n}}\n", names.join(", ")));
                }
                start += count;
            }
            t.push_str("}\n");
        }
        t.push_str(
            "}\n}\n# Generated by LVM2\n\ncontents = \"Text Format Volume Group\"\nversion = 1\n\n\
             description = \"\"\n\ncreation_host = \"host\" # Linux host\ncreation_time = 1700000000 # Tue\n",
        );
        t
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn the_label_says_where_data_and_metadata_are() {
        let head = label(64 << 20, 1 << 20, &[(4096, 1044480)]);
        let l = parse_label(&head).unwrap();
        assert_eq!(l.uuid, UUID);
        assert_eq!(l.device_size, 64 << 20);
        assert_eq!(l.pe_start(), Some(1 << 20));
        assert_eq!(l.metadata_areas, vec![(4096, 1044480)]);

        // Not a PV: no label, a label of another kind, one in the wrong sector.
        assert!(parse_label(&[0u8; LABEL_SCAN_BYTES]).is_none());
        let mut other = head.clone();
        other[512 + 24..512 + 32].copy_from_slice(b"LVM1 001");
        assert!(parse_label(&other).is_none());
        let mut moved = head.clone();
        moved[512 + 8..512 + 16].copy_from_slice(&2u64.to_le_bytes());
        assert!(parse_label(&moved).is_none());
        assert!(parse_label(&head[..600]).is_none(), "cut short");
    }

    #[test]
    fn metadata_text_is_read_even_round_the_end_of_its_buffer() {
        let text = vg_text(10, &[]);
        let area = mda(64 << 10, 8192, &text);
        assert_eq!(metadata_text(&area).unwrap(), text);
        // Starts 100 bytes before the end of the area, continues after the header.
        let size = 16 << 10;
        let wrapped = mda(size, size - 100, &text);
        assert_eq!(metadata_text(&wrapped).unwrap(), text);

        let mut ignored = area.clone();
        ignored[60..64].copy_from_slice(&RAW_LOCN_IGNORED.to_le_bytes());
        assert!(metadata_text(&ignored).is_none());
        let mut bad = area;
        bad[4] = b'X';
        assert!(metadata_text(&bad).is_none());
    }

    #[test]
    fn the_allocation_of_this_pv_and_nothing_else() {
        let text = vg_text(
            100,
            &[
                ("root", vec![("striped", vec![("pv0", 0)], 20)]),
                // Two segments: 30..40 and, after a free gap, 60..70.
                (
                    "home",
                    vec![
                        ("striped", vec![("pv0", 30)], 10),
                        ("striped", vec![("pv0", 60)], 10),
                    ],
                ),
                // On another PV: not ours.
                ("far", vec![("striped", vec![("pv1", 0)], 50)]),
                // A thin volume allocates nothing itself.
                ("thin1", vec![("thin", vec![], 5)]),
            ],
        );
        let a = allocation(&text, UUID).unwrap();
        assert_eq!(a.vg, "vg0");
        assert_eq!(a.seqno, 7);
        assert_eq!(a.extent_size, 4 << 20);
        assert_eq!(a.pe_start, 1 << 20);
        assert_eq!(a.used, vec![(0, 20), (30, 10), (60, 10)]);
        assert!(!a.other_pvs, "only pv0 is listed");
        let mib = 1u64 << 20;
        assert_eq!(
            a.free_ranges(),
            vec![
                (mib + 80 * mib, mib + 120 * mib),
                (mib + 160 * mib, mib + 240 * mib),
                (mib + 280 * mib, mib + 400 * mib)
            ]
        );
        assert_eq!(a.used_ranges()[0], (mib, mib + 80 * mib));

        assert!(allocation(&text, "someone-else").is_err());
        // A pvmove holds this PV's extents in a mirror: not plain stripes.
        let moving = vg_text(100, &[("pvmove0", vec![("mirror", vec![("pv0", 0)], 20)])]);
        assert!(allocation(&moving, UUID).is_err());
        // Two volumes on one extent: the metadata is not believed.
        let twice = vg_text(
            100,
            &[
                ("a", vec![("striped", vec![("pv0", 0)], 20)]),
                ("b", vec![("striped", vec![("pv0", 10)], 20)]),
            ],
        );
        assert!(allocation(&twice, UUID).is_err());
        let past = vg_text(10, &[("a", vec![("striped", vec![("pv0", 5)], 20)])]);
        assert!(allocation(&past, UUID).is_err());
        assert!(allocation("not { metadata", UUID).is_err());
        // Two stripes, one of them here.
        let striped = vg_text(
            100,
            &[("s", vec![("striped", vec![("pv0", 50), ("pv1", 0)], 20)])],
        );
        assert_eq!(allocation(&striped, UUID).unwrap().used, vec![(50, 10)]);
    }

    #[test]
    fn the_guard_is_the_head_and_every_metadata_area() {
        let l = PvLabel {
            uuid: UUID.into(),
            device_size: 100 << 20,
            data_areas: vec![(1 << 20, 0)],
            metadata_areas: vec![(4096, 1044480), ((100 << 20) - 65536, 65536)],
        };
        assert_eq!(
            guard_ranges(&l, 100 << 20),
            vec![(0, 1 << 20), ((100 << 20) - 65536, 65536)]
        );
        // A metadata area past the end of a smaller device is not read.
        assert_eq!(guard_ranges(&l, 50 << 20), vec![(0, 1 << 20)]);
        let d1 = guard_digest([&b"abc"[..], b"def"]);
        assert_eq!(d1, guard_digest([&b"abcdef"[..]]));
        assert_ne!(d1, guard_digest([&b"abcdeg"[..]]));
        assert_eq!(d1.len(), 32);
    }

    #[test]
    fn a_snapshot_record_round_trips() {
        let s = PvSnapshot {
            device: "/dev/vda3".into(),
            volumes: vec![PvSnapshotVolume {
                name: "vg0-root".into(),
                origin: "/dev/dm-1".into(),
                snapshot: "/dev/dm-7".into(),
                dm_id: "0123456789abcdef0123456789abcdef".into(),
                size: 80 << 20,
                segments: vec![PvSegment {
                    pv_offset: 1 << 20,
                    lv_offset: 0,
                    length: 80 << 20,
                }],
            }],
            guard: vec![(0, 1 << 20)],
            guard_digest: "00".into(),
        };
        assert_eq!(PvSnapshot::from_json(&s.to_json()), Some(s));
        assert_eq!(PvSnapshot::from_json("{"), None);
    }
}
