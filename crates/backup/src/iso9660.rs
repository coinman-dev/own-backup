//! Writing a bootable ISO, without oscdimg and without the ADK.
//!
//! The medium's files have to reach the user as one file they can burn, mount in
//! a virtual machine, or write to a stick. That file is an ISO, and making one
//! needs three things layered together:
//!
//! * **ISO9660** — the filesystem the boot loader reads `\sources\boot.wim`
//!   from. Its names are 8.3-ish and upper-case, which every file in our boot
//!   chain already is.
//! * **Joliet** — a second directory tree over the same file data, with real
//!   names. The WebView2 runtime has hundreds of files that ISO9660 would
//!   mangle beyond recognition; Windows reads Joliet in preference.
//! * **El Torito** — the boot catalog. UEFI firmware does not read ISO9660 at
//!   all: it looks here for a FAT volume, mounts it as an EFI System Partition
//!   and runs `\EFI\BOOT\BOOTX64.EFI` from it. That volume is built by
//!   [`crate::fat_writer`].
//!
//! Everything is laid out in one pass: sizes are known before anything is
//! written, so the file is produced sequentially and never seeked backwards.

use own_backup_common::{OwnBackupError, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SECTOR: usize = 2048;
/// The first 16 sectors belong to nobody — historically boot code, now zeros.
const SYSTEM_AREA_SECTORS: u32 = 16;

/// A file to place on the medium.
pub struct IsoFile {
    /// Path on the medium, `/`-separated, no leading slash:
    /// `EFI/BOOT/BOOTX64.EFI`.
    pub path: String,
    pub source: PathBuf,
}

/// Which file in the medium is the El Torito boot image.
///
/// It is a normal file in the tree, not raw data hidden between the extents:
/// firmware only needs the address from the catalog, but every tool that looks
/// at the finished ISO — and every person who mounts it — expects to find the
/// image where the catalog says it is. xorriso says
/// "-not-found-any-more-by-lba=" about the alternative.
pub struct IsoBoot {
    /// Path inside the ISO, e.g. `boot/efisys.bin`.
    pub image_path: String,
}

#[derive(Default)]
struct DirNode {
    /// Path components from the root; empty for the root itself.
    path: Vec<String>,
    /// Index into the directory list of this one's parent (the root is its own).
    parent: usize,
    files: Vec<FileEntry>,
    children: Vec<usize>,
    /// Assigned when the layout is computed.
    iso_lba: u32,
    iso_len: u32,
    joliet_lba: u32,
    joliet_len: u32,
}

struct FileEntry {
    name: String,
    source: PathBuf,
    size: u32,
    lba: u32,
}

/// Write `files` as a bootable ISO at `out`.
///
/// `boot` is optional only so the writer can be tested without one; a medium
/// without a boot catalog is not bootable by UEFI firmware.
pub fn write_iso(
    files: &[IsoFile],
    volume_id: &str,
    boot: Option<IsoBoot>,
    out: &Path,
) -> Result<u64> {
    let mut dirs: Vec<DirNode> = vec![DirNode { parent: 0, ..Default::default() }];

    // --- build the tree ----------------------------------------------------
    for f in files {
        let parts: Vec<&str> = f.path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        let Some((name, dir_parts)) = parts.split_last() else { continue };
        let mut cur = 0usize;
        for part in dir_parts {
            let existing = dirs[cur]
                .children
                .iter()
                .copied()
                .find(|&c| dirs[c].path.last().map(|s| s.as_str()) == Some(*part));
            cur = match existing {
                Some(c) => c,
                None => {
                    let mut path = dirs[cur].path.clone();
                    path.push((*part).to_string());
                    dirs.push(DirNode { path, parent: cur, ..Default::default() });
                    let idx = dirs.len() - 1;
                    dirs[cur].children.push(idx);
                    idx
                }
            };
        }
        let size = std::fs::metadata(&f.source)
            .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", f.source.display())))?
            .len();
        if size > u32::MAX as u64 {
            return Err(OwnBackupError::ImageFormat(format!(
                "{}: файл больше 4 ГБ — в ISO9660 такой не поместится",
                f.source.display()
            )));
        }
        dirs[cur].files.push(FileEntry {
            name: (*name).to_string(),
            source: f.source.clone(),
            size: size as u32,
            lba: 0,
        });
    }

    // Directory order is the path-table order: by level, then by parent, then
    // by name. Records inside a directory must be sorted too, or a reader is
    // entitled to give up on them.
    let order = path_table_order(&dirs);
    for d in dirs.iter_mut() {
        d.files.sort_by(|a, b| iso_name(&a.name, false).cmp(&iso_name(&b.name, false)));
    }

    // --- work out where everything goes ------------------------------------
    let boot_present = boot.is_some();
    let mut lba = SYSTEM_AREA_SECTORS
        + if boot_present { 4 } else { 3 }; // PVD [+ boot record] + Joliet + terminator
    let boot_catalog_lba = boot_present.then(|| {
        let l = lba;
        lba += 1;
        l
    });

    // Built twice on purpose. The tables' SIZE depends only on the names, so it
    // is known now and the layout can be computed; their CONTENT is the address
    // of every directory, which is not known until the layout is done. Writing
    // the first build would put a table of zeroes on the medium — which is what
    // happened, and only an outside reader caught it: our own lookups walk the
    // directory records and never consult the path table at all.
    let iso_pt = build_path_table(&dirs, &order, false);
    let joliet_pt = build_path_table(&dirs, &order, true);
    let iso_pt_lba = lba;
    lba += sectors_for(iso_pt.0.len() as u32) * 2; // L and M tables
    let joliet_pt_lba = lba;
    lba += sectors_for(joliet_pt.0.len() as u32) * 2;

    for &i in &order {
        let len = dir_extent_len(&dirs, i, false);
        dirs[i].iso_lba = lba;
        dirs[i].iso_len = len;
        lba += sectors_for(len);
    }
    for &i in &order {
        let len = dir_extent_len(&dirs, i, true);
        dirs[i].joliet_lba = lba;
        dirs[i].joliet_len = len;
        lba += sectors_for(len);
    }
    // File data is shared by both trees — one copy, two sets of names.
    for &i in &order {
        for f in dirs[i].files.iter_mut() {
            f.lba = lba;
            lba += sectors_for(f.size.max(1));
        }
    }
    let total_sectors = lba;

    // Now that every directory has an address, the tables can say so.
    let iso_pt = build_path_table(&dirs, &order, false);
    let joliet_pt = build_path_table(&dirs, &order, true);

    // --- write --------------------------------------------------------------
    let mut w = std::io::BufWriter::new(
        std::fs::File::create(out)
            .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", out.display())))?,
    );
    write_zeros(&mut w, SYSTEM_AREA_SECTORS as usize * SECTOR)?;

    let root_iso = dir_record(&dirs, 0, false, RecordKind::SelfRef);
    let root_joliet = dir_record(&dirs, 0, true, RecordKind::SelfRef);
    w.write_all(&volume_descriptor(
        1,
        volume_id,
        total_sectors,
        iso_pt.0.len() as u32,
        iso_pt_lba,
        &root_iso,
        false,
    ))?;
    if let Some(cat) = boot_catalog_lba {
        w.write_all(&boot_record_descriptor(cat))?;
    }
    w.write_all(&volume_descriptor(
        2,
        volume_id,
        total_sectors,
        joliet_pt.0.len() as u32,
        joliet_pt_lba,
        &root_joliet,
        true,
    ))?;
    w.write_all(&terminator_descriptor())?;

    if let (Some(_), Some(b)) = (boot_catalog_lba, boot.as_ref()) {
        let (img_lba, img_size) = locate(&dirs, &order, &b.image_path).ok_or_else(|| {
            OwnBackupError::ImageFormat(format!(
                "{}: загрузочный образ должен быть среди файлов носителя",
                b.image_path
            ))
        })?;
        w.write_all(&boot_catalog(img_lba, img_size))?;
    }

    for (table, big_endian) in [(&iso_pt.0, false), (&iso_pt.1, true)] {
        let _ = big_endian;
        w.write_all(table)?;
        write_zeros(&mut w, pad_to_sector(table.len()))?;
    }
    for table in [&joliet_pt.0, &joliet_pt.1] {
        w.write_all(table)?;
        write_zeros(&mut w, pad_to_sector(table.len()))?;
    }

    for joliet in [false, true] {
        for &i in &order {
            let ext = directory_extent(&dirs, i, joliet);
            w.write_all(&ext)?;
            write_zeros(&mut w, pad_to_sector(ext.len()))?;
        }
    }

    let mut buf = vec![0u8; 1 << 20];
    for &i in &order {
        for f in &dirs[i].files {
            let mut src = std::fs::File::open(&f.source)
                .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", f.source.display())))?;
            let mut left = f.size as usize;
            while left > 0 {
                let take = left.min(buf.len());
                let n = src.read(&mut buf[..take])?;
                if n == 0 {
                    return Err(OwnBackupError::ImageFormat(format!(
                        "{}: файл кончился раньше, чем обещал его размер",
                        f.source.display()
                    )));
                }
                w.write_all(&buf[..n])?;
                left -= n;
            }
            write_zeros(&mut w, pad_to_sector(f.size.max(1) as usize))?;
        }
    }
    w.flush()?;
    drop(w);

    let len = std::fs::metadata(out)?.len();
    Ok(len)
}

/// LBA and size of a file already laid out, by its path on the medium.
fn locate(dirs: &[DirNode], order: &[usize], path: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let (name, dir_parts) = parts.split_last()?;
    for &i in order {
        if dirs[i].path.len() == dir_parts.len()
            && dirs[i].path.iter().zip(dir_parts).all(|(a, b)| a == b)
        {
            let f = dirs[i].files.iter().find(|f| f.name == *name)?;
            return Some((f.lba, f.size));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// layout helpers
// ---------------------------------------------------------------------------

fn sectors_for(bytes: u32) -> u32 {
    (bytes as usize).div_ceil(SECTOR) as u32
}
fn pad_to_sector(len: usize) -> usize {
    (SECTOR - len % SECTOR) % SECTOR
}
fn write_zeros(w: &mut impl Write, n: usize) -> Result<()> {
    const Z: [u8; 4096] = [0; 4096];
    let mut left = n;
    while left > 0 {
        let take = left.min(Z.len());
        w.write_all(&Z[..take])?;
        left -= take;
    }
    Ok(())
}

/// Directories in path-table order: the root, then each level, parents before
/// children. Numbering follows this order and the records refer to it.
fn path_table_order(dirs: &[DirNode]) -> Vec<usize> {
    let mut out = vec![0usize];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        let mut kids = dirs[cur].children.clone();
        kids.sort_by(|&a, &b| {
            iso_name(dirs[a].path.last().unwrap(), true)
                .cmp(&iso_name(dirs[b].path.last().unwrap(), true))
        });
        out.extend(kids);
        i += 1;
    }
    out
}

/// Both path tables (little-endian, big-endian) for one tree.
fn build_path_table(dirs: &[DirNode], order: &[usize], joliet: bool) -> (Vec<u8>, Vec<u8>) {
    let mut number = vec![0u16; dirs.len()];
    for (n, &i) in order.iter().enumerate() {
        number[i] = n as u16 + 1;
    }
    let (mut le, mut be) = (Vec::new(), Vec::new());
    for &i in order {
        let name: Vec<u8> = if i == 0 {
            vec![0]
        } else {
            encode_name(dirs[i].path.last().unwrap(), joliet, true)
        };
        for (buf, big) in [(&mut le, false), (&mut be, true)] {
            buf.push(name.len() as u8);
            buf.push(0);
            let lba = if joliet { dirs[i].joliet_lba } else { dirs[i].iso_lba };
            buf.extend_from_slice(&if big { lba.to_be_bytes() } else { lba.to_le_bytes() });
            let parent = number[dirs[i].parent];
            buf.extend_from_slice(&if big { parent.to_be_bytes() } else { parent.to_le_bytes() });
            buf.extend_from_slice(&name);
            if name.len() % 2 == 1 {
                buf.push(0);
            }
        }
    }
    (le, be)
}

enum RecordKind {
    SelfRef,
    Parent,
    Child(usize),
    File(usize),
}

/// Length of a directory's extent. Measured by building it, so the layout and
/// the writing can never disagree about the padding below — the record sizes do
/// not depend on the addresses, so this is exact before the addresses exist.
fn dir_extent_len(dirs: &[DirNode], idx: usize, joliet: bool) -> u32 {
    directory_extent(dirs, idx, joliet).len() as u32
}

fn directory_extent(dirs: &[DirNode], idx: usize, joliet: bool) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // "Each Directory Record shall be recorded entirely within one Logical
    // Sector." A record that straddles the boundary turns everything after it
    // into rubbish — readers then see impossible flags and loop, or refuse the
    // image outright. Directories only exceed 2048 bytes once they hold a few
    // dozen entries, so a small tree never notices; the WebView2 runtime has
    // hundreds of files in one folder and did.
    let mut push = |rec: Vec<u8>, out: &mut Vec<u8>| {
        let used = out.len() % SECTOR;
        if used + rec.len() > SECTOR {
            out.resize(out.len() + (SECTOR - used), 0);
        }
        out.extend(rec);
    };

    push(dir_record(dirs, idx, joliet, RecordKind::SelfRef), &mut out);
    push(dir_record(dirs, idx, joliet, RecordKind::Parent), &mut out);
    let mut kids = dirs[idx].children.clone();
    kids.sort_by(|&a, &b| {
        encode_name(dirs[a].path.last().unwrap(), joliet, true)
            .cmp(&encode_name(dirs[b].path.last().unwrap(), joliet, true))
    });
    for c in kids {
        push(dir_record(dirs, c, joliet, RecordKind::Child(c)), &mut out);
    }
    for f in 0..dirs[idx].files.len() {
        push(dir_record(dirs, idx, joliet, RecordKind::File(f)), &mut out);
    }
    out
}

/// One directory record. `.` and `..` are identifiers of a single zero byte and
/// a single 0x01 byte — the format's way of saying "here" and "up".
fn dir_record(dirs: &[DirNode], idx: usize, joliet: bool, kind: RecordKind) -> Vec<u8> {
    let (name, lba, len, is_dir) = match kind {
        RecordKind::SelfRef => (
            vec![0u8],
            if joliet { dirs[idx].joliet_lba } else { dirs[idx].iso_lba },
            if joliet { dirs[idx].joliet_len } else { dirs[idx].iso_len },
            true,
        ),
        RecordKind::Parent => {
            let p = dirs[idx].parent;
            (
                vec![1u8],
                if joliet { dirs[p].joliet_lba } else { dirs[p].iso_lba },
                if joliet { dirs[p].joliet_len } else { dirs[p].iso_len },
                true,
            )
        }
        RecordKind::Child(c) => (
            encode_name(dirs[c].path.last().unwrap(), joliet, true),
            if joliet { dirs[c].joliet_lba } else { dirs[c].iso_lba },
            if joliet { dirs[c].joliet_len } else { dirs[c].iso_len },
            true,
        ),
        RecordKind::File(f) => {
            let fe = &dirs[idx].files[f];
            (encode_name(&fe.name, joliet, false), fe.lba, fe.size, false)
        }
    };

    let mut rec = vec![0u8; 33 + name.len()];
    rec[0] = rec.len() as u8;
    if rec.len() % 2 == 1 {
        rec.push(0);
        rec[0] = rec.len() as u8;
    }
    rec[2..6].copy_from_slice(&lba.to_le_bytes());
    rec[6..10].copy_from_slice(&lba.to_be_bytes());
    rec[10..14].copy_from_slice(&len.to_le_bytes());
    rec[14..18].copy_from_slice(&len.to_be_bytes());
    // A fixed date, so two builds of the same medium are the same file.
    rec[18] = 126; // 2026 - 1900
    rec[19] = 1;
    rec[20] = 1;
    rec[25] = if is_dir { 0x02 } else { 0x00 };
    rec[28..30].copy_from_slice(&1u16.to_le_bytes());
    rec[30..32].copy_from_slice(&1u16.to_be_bytes());
    rec[32] = name.len() as u8;
    rec[33..33 + name.len()].copy_from_slice(&name);
    rec
}

/// ISO9660 identifiers are upper-case and drawn from a small alphabet; files
/// carry a `;1` version suffix. Joliet is UCS-2 big-endian and keeps the name
/// as it is — which is the whole reason for having a second tree.
fn encode_name(name: &str, joliet: bool, is_dir: bool) -> Vec<u8> {
    if joliet {
        // No `;1` here. ISO9660 requires that version suffix; Joliet does not,
        // and adding it makes every file show up in Explorer as "boot.wim;1".
        // Readers look for the plain name and do not find it.
        let s: String = name.chars().take(64).collect();
        return s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    }
    iso_name(name, is_dir).into_bytes()
}

fn iso_name(name: &str, is_dir: bool) -> String {
    let clean = |s: &str, n: usize| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .take(n)
            .collect()
    };
    if is_dir {
        return clean(name, 31);
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{}.{};1", clean(stem, 27), clean(ext, 3)),
        None => format!("{};1", clean(name, 30)),
    }
}

// ---------------------------------------------------------------------------
// volume descriptors
// ---------------------------------------------------------------------------

fn volume_descriptor(
    kind: u8,
    volume_id: &str,
    total_sectors: u32,
    path_table_size: u32,
    path_table_lba: u32,
    root_record: &[u8],
    joliet: bool,
) -> Vec<u8> {
    let mut d = vec![0u8; SECTOR];
    d[0] = kind;
    d[1..6].copy_from_slice(b"CD001");
    d[6] = 1;
    put_str(&mut d[8..40], "", joliet);
    put_str(&mut d[40..72], volume_id, joliet);
    d[80..84].copy_from_slice(&total_sectors.to_le_bytes());
    d[84..88].copy_from_slice(&total_sectors.to_be_bytes());
    if joliet {
        // UCS-2 level 3 — the escape sequence is how a reader knows this tree
        // is Joliet rather than a plain second descriptor.
        d[88..91].copy_from_slice(b"%/E");
    }
    d[120..122].copy_from_slice(&1u16.to_le_bytes());
    d[122..124].copy_from_slice(&1u16.to_be_bytes());
    d[124..126].copy_from_slice(&1u16.to_le_bytes());
    d[126..128].copy_from_slice(&1u16.to_be_bytes());
    d[128..130].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    d[130..132].copy_from_slice(&(SECTOR as u16).to_be_bytes());
    d[132..136].copy_from_slice(&path_table_size.to_le_bytes());
    d[136..140].copy_from_slice(&path_table_size.to_be_bytes());
    d[140..144].copy_from_slice(&path_table_lba.to_le_bytes());
    d[148..152].copy_from_slice(&(path_table_lba + sectors_for(path_table_size)).to_be_bytes());
    d[156..156 + root_record.len().min(34)]
        .copy_from_slice(&root_record[..root_record.len().min(34)]);
    for r in [190..318, 318..446, 446..574, 574..702] {
        put_str(&mut d[r], "", joliet);
    }
    let date = b"2026010100000000\0";
    for r in [813..830, 830..847] {
        d[r].copy_from_slice(date);
    }
    for r in [847..864, 864..881] {
        d[r][..16].copy_from_slice(b"0000000000000000");
    }
    d[881] = 1;
    d
}

/// Text fields are a-characters padded with spaces; in a Joliet descriptor the
/// same fields are UCS-2, so the padding is a wide space.
fn put_str(field: &mut [u8], s: &str, joliet: bool) {
    if joliet {
        let mut i = 0;
        for u in s.encode_utf16() {
            if i + 2 > field.len() {
                break;
            }
            field[i..i + 2].copy_from_slice(&u.to_be_bytes());
            i += 2;
        }
        while i + 2 <= field.len() {
            field[i..i + 2].copy_from_slice(&0x0020u16.to_be_bytes());
            i += 2;
        }
    } else {
        field.fill(b' ');
        for (i, c) in s.bytes().take(field.len()).enumerate() {
            field[i] = c.to_ascii_uppercase();
        }
    }
}

fn boot_record_descriptor(catalog_lba: u32) -> Vec<u8> {
    let mut d = vec![0u8; SECTOR];
    d[0] = 0;
    d[1..6].copy_from_slice(b"CD001");
    d[6] = 1;
    let id = b"EL TORITO SPECIFICATION";
    d[7..7 + id.len()].copy_from_slice(id);
    d[71..75].copy_from_slice(&catalog_lba.to_le_bytes());
    d
}

fn terminator_descriptor() -> Vec<u8> {
    let mut d = vec![0u8; SECTOR];
    d[0] = 0xFF;
    d[1..6].copy_from_slice(b"CD001");
    d[6] = 1;
    d
}

/// The boot catalog: a validation entry saying "this is for EFI", then the
/// entry pointing at the FAT image. Platform 0xEF with no legacy entry is what
/// a UEFI-only medium looks like — the same shape oscdimg produces for
/// `-bootdata:1#pEF,e,b…`.
fn boot_catalog(image_lba: u32, image_bytes: u32) -> Vec<u8> {
    let mut c = vec![0u8; SECTOR];
    c[0] = 1; // header id
    c[1] = 0xEF; // platform: EFI
    c[30] = 0x55;
    c[31] = 0xAA;
    // The 16-bit words of the validation entry must sum to zero.
    let sum: u16 = c[..32]
        .chunks_exact(2)
        .map(|w| u16::from_le_bytes([w[0], w[1]]))
        .fold(0u16, |a, b| a.wrapping_add(b));
    c[28..30].copy_from_slice(&(0u16.wrapping_sub(sum)).to_le_bytes());

    c[32] = 0x88; // bootable
    c[33] = 0; // no emulation
    // Sector count is in 512-byte units; firmware reads the FAT's own header for
    // the rest, but a truthful number here costs nothing.
    let sectors = (image_bytes.div_ceil(512)).min(0xFFFF) as u16;
    c[38..40].copy_from_slice(&sectors.to_le_bytes());
    c[40..44].copy_from_slice(&image_lba.to_le_bytes());
    c
}

/// Read back the LBA and size of one file, by walking the primary tree. Used by
/// the tests, and by anyone who needs to check a finished ISO without mounting
/// it.
pub fn find_file(iso: &Path, path: &str) -> Result<Option<(u32, u32)>> {
    let mut f = std::fs::File::open(iso)?;
    let mut pvd = vec![0u8; SECTOR];
    f.seek(SeekFrom::Start(SYSTEM_AREA_SECTORS as u64 * SECTOR as u64))?;
    f.read_exact(&mut pvd)?;
    if &pvd[1..6] != b"CD001" || pvd[0] != 1 {
        return Err(OwnBackupError::ImageFormat("не ISO9660".into()));
    }
    let mut lba = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let mut len = u32::from_le_bytes(pvd[166..170].try_into().unwrap());

    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    for (depth, part) in parts.iter().enumerate() {
        let want = iso_name(part, depth + 1 < parts.len());
        let mut buf = vec![0u8; len as usize];
        f.seek(SeekFrom::Start(lba as u64 * SECTOR as u64))?;
        f.read_exact(&mut buf)?;
        let mut at = 0usize;
        let mut hit = None;
        while at < buf.len() {
            // A zero length byte means "no more records in THIS sector"; the
            // next one may still hold entries.
            if buf[at] == 0 {
                let next = (at / SECTOR + 1) * SECTOR;
                if next >= buf.len() {
                    break;
                }
                at = next;
                continue;
            }
            let rl = buf[at] as usize;
            let nl = buf[at + 32] as usize;
            let name = String::from_utf8_lossy(&buf[at + 33..at + 33 + nl]).to_string();
            if name == want {
                hit = Some((
                    u32::from_le_bytes(buf[at + 2..at + 6].try_into().unwrap()),
                    u32::from_le_bytes(buf[at + 10..at + 14].try_into().unwrap()),
                ));
                break;
            }
            at += rl;
        }
        match hit {
            Some((l, s)) => {
                lba = l;
                len = s;
            }
            None => return Ok(None),
        }
    }
    Ok(Some((lba, len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> Vec<IsoFile> {
        let mk = |rel: &str, bytes: &[u8]| {
            let p = dir.join(rel.replace('/', "_"));
            std::fs::write(&p, bytes).unwrap();
            p
        };
        vec![
            IsoFile {
                path: "EFI/BOOT/BOOTX64.EFI".into(),
                source: mk("bootx64", &vec![0xAA; 4096]),
            },
            IsoFile { path: "boot/boot.sdi".into(), source: mk("sdi", &vec![0xBB; 2048]) },
            IsoFile {
                path: "sources/boot.wim".into(),
                source: mk("wim", &vec![0xCC; 5000]),
            },
            // A name ISO9660 cannot represent — this is why Joliet is there.
            IsoFile {
                path: "OwnBackup/WebView2/msedgewebview2.exe".into(),
                source: mk("wv2", &vec![0xDD; 100]),
            },
        ]
    }

    #[test]
    fn the_descriptors_are_where_a_reader_looks_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("t.iso");
        write_iso(&sample(dir.path()), "OWNBACKUP", None, &iso).unwrap();
        let data = std::fs::read(&iso).unwrap();
        assert_eq!(&data[16 * SECTOR..16 * SECTOR + 6], b"\x01CD001");
        assert_eq!(&data[17 * SECTOR..17 * SECTOR + 6], b"\x02CD001", "Joliet descriptor");
        assert_eq!(&data[18 * SECTOR..18 * SECTOR + 6], b"\xFFCD001", "terminator");
        assert_eq!(&data[17 * SECTOR + 88..17 * SECTOR + 91], b"%/E", "Joliet escape");
    }

    #[test]
    fn a_boot_catalog_is_added_when_a_boot_image_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("b.iso");
        let mut files = sample(dir.path());
        let efisys = dir.path().join("efisys");
        std::fs::write(&efisys, vec![0x11; 4096]).unwrap();
        files.push(IsoFile { path: "boot/efisys.bin".into(), source: efisys });
        let boot = IsoBoot { image_path: "boot/efisys.bin".into() };
        write_iso(&files, "OWNBACKUP", Some(boot), &iso).unwrap();
        let data = std::fs::read(&iso).unwrap();
        // Boot record descriptor sits between the primary and Joliet ones.
        assert_eq!(&data[17 * SECTOR..17 * SECTOR + 6], b"\x00CD001");
        assert_eq!(&data[17 * SECTOR + 7..17 * SECTOR + 30], b"EL TORITO SPECIFICATION");
        let cat = u32::from_le_bytes(data[17 * SECTOR + 71..17 * SECTOR + 75].try_into().unwrap());
        let c = &data[cat as usize * SECTOR..cat as usize * SECTOR + 64];
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 0xEF, "the platform must say EFI or firmware ignores it");
        assert_eq!((c[30], c[31]), (0x55, 0xAA));
        // The validation entry's words must sum to zero.
        let sum = c[..32]
            .chunks_exact(2)
            .map(|w| u16::from_le_bytes([w[0], w[1]]))
            .fold(0u16, |a, b| a.wrapping_add(b));
        assert_eq!(sum, 0, "bad checksum — firmware will reject the catalog");
        assert_eq!(c[32], 0x88, "entry must be marked bootable");
        assert_eq!(c[33], 0, "no-emulation is what a FAT boot image needs");
        // The catalog must point at the file, not at some hole in the image.
        let img_lba = u32::from_le_bytes(c[40..44].try_into().unwrap());
        let (file_lba, _) = find_file(&iso, "boot/efisys.bin").unwrap().unwrap();
        assert_eq!(img_lba, file_lba, "the boot image must be a real file in the tree");
    }

    #[test]
    fn files_come_back_out_where_the_records_say() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("f.iso");
        write_iso(&sample(dir.path()), "OWNBACKUP", None, &iso).unwrap();
        let (lba, size) = find_file(&iso, "sources/boot.wim").unwrap().unwrap();
        assert_eq!(size, 5000);
        let data = std::fs::read(&iso).unwrap();
        let at = lba as usize * SECTOR;
        assert_eq!(&data[at..at + 5000], &vec![0xCC; 5000][..]);
        assert!(find_file(&iso, "sources/missing.wim").unwrap().is_none());
    }

    /// The boot chain's own paths must survive ISO9660's name rules unchanged,
    /// because the loader asks for them by name.
    #[test]
    fn the_boot_paths_are_not_mangled() {
        assert_eq!(iso_name("BOOTX64.EFI", false), "BOOTX64.EFI;1");
        assert_eq!(iso_name("boot.wim", false), "BOOT.WIM;1");
        assert_eq!(iso_name("BCD", false), "BCD;1");
        assert_eq!(iso_name("EFI", true), "EFI");
        assert_eq!(iso_name("sources", true), "SOURCES");
    }

    /// The path table is a second, independent index of the same directories.
    /// Readers use it instead of walking; ours did not, so a table full of
    /// zeroes went unnoticed until an outside reader choked on it.
    #[test]
    fn the_path_table_points_at_the_same_directories_as_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("pt.iso");
        write_iso(&sample(dir.path()), "OWNBACKUP", None, &iso).unwrap();
        let data = std::fs::read(&iso).unwrap();
        let pvd = &data[16 * SECTOR..17 * SECTOR];
        let pt_size = u32::from_le_bytes(pvd[132..136].try_into().unwrap()) as usize;
        let pt_lba = u32::from_le_bytes(pvd[140..144].try_into().unwrap()) as usize;
        let pt = &data[pt_lba * SECTOR..pt_lba * SECTOR + pt_size];

        let mut at = 0usize;
        let mut seen = 0;
        while at < pt.len() {
            let nl = pt[at] as usize;
            if nl == 0 && seen > 0 {
                break;
            }
            let lba = u32::from_le_bytes(pt[at + 2..at + 6].try_into().unwrap());
            assert!(lba >= 16, "path table entry {seen} points at LBA {lba}");
            // Whatever it points at must actually be a directory extent: the
            // first record there describes itself and carries the same address.
            let rec = &data[lba as usize * SECTOR..];
            let self_lba = u32::from_le_bytes(rec[2..6].try_into().unwrap());
            assert_eq!(self_lba, lba, "entry {seen} does not land on its own extent");
            assert_eq!(rec[25] & 0x02, 0x02, "entry {seen} is not a directory");
            at += 8 + nl + (nl % 2);
            seen += 1;
        }
        assert!(seen >= 4, "expected the root and its subdirectories, saw {seen}");
    }

    /// Joliet exists so names survive; a version suffix there defeats the point.
    #[test]
    fn joliet_names_are_the_real_names() {
        let n = encode_name("boot.wim", true, false);
        let s = String::from_utf16(
            &n.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(s, "boot.wim", "no ;1, and the case is kept");
        let d = encode_name("Microsoft", true, true);
        let s = String::from_utf16(
            &d.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(s, "Microsoft");
    }

    /// The bug that made a real medium unreadable: a folder with hundreds of
    /// files fills more than one sector, and a record allowed to straddle the
    /// boundary corrupts the rest of the directory. xorriso called the image
    /// "interleaved"; pycdlib recursed until it gave up.
    #[test]
    fn a_directory_larger_than_a_sector_stays_readable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload");
        std::fs::write(&src, b"x").unwrap();
        let mut files: Vec<IsoFile> = (0..300)
            .map(|i| IsoFile {
                path: format!("OwnBackup/WebView2/file_number_{i:03}.dll"),
                source: src.clone(),
            })
            .collect();
        files.push(IsoFile { path: "sources/boot.wim".into(), source: src.clone() });
        let iso = dir.path().join("big.iso");
        write_iso(&files, "OWNBACKUP", None, &iso).unwrap();

        let data = std::fs::read(&iso).unwrap();
        let (lba, len) = {
            // Walk to the crowded directory through its parent.
            let (l, s) = find_file(&iso, "OwnBackup/WebView2/file_number_000.dll")
                .unwrap()
                .expect("first file");
            assert_eq!(s, 1);
            (l, s)
        };
        assert!(lba > 16 && len == 1);
        // Every file must still be findable — the last one is past the boundary.
        for i in [0usize, 42, 299] {
            assert!(
                find_file(&iso, &format!("OwnBackup/WebView2/file_number_{i:03}.dll"))
                    .unwrap()
                    .is_some(),
                "file {i} lost after the sector boundary"
            );
        }
        // And no record may cross a sector: check the directory extent itself.
        let (dlba, dlen) = dir_extent_of(&data, "OWNBACKUP/WEBVIEW2");
        let ext = &data[dlba * SECTOR..dlba * SECTOR + dlen];
        let mut at = 0usize;
        while at < ext.len() {
            let rl = ext[at] as usize;
            if rl == 0 {
                at = (at / SECTOR + 1) * SECTOR;
                continue;
            }
            assert!(
                at % SECTOR + rl <= SECTOR,
                "a record at {at} crosses the sector boundary"
            );
            at += rl;
        }
    }

    /// Address and length of a directory, by walking the primary tree.
    fn dir_extent_of(data: &[u8], path: &str) -> (usize, usize) {
        let pvd = &data[16 * SECTOR..17 * SECTOR];
        let mut lba = u32::from_le_bytes(pvd[158..162].try_into().unwrap()) as usize;
        let mut len = u32::from_le_bytes(pvd[166..170].try_into().unwrap()) as usize;
        for part in path.split('/') {
            let ext = &data[lba * SECTOR..lba * SECTOR + len];
            let mut at = 0usize;
            loop {
                assert!(at < ext.len(), "{part} not found");
                let rl = ext[at] as usize;
                if rl == 0 {
                    at = (at / SECTOR + 1) * SECTOR;
                    continue;
                }
                let nl = ext[at + 32] as usize;
                if &ext[at + 33..at + 33 + nl] == part.as_bytes() {
                    lba = u32::from_le_bytes(ext[at + 2..at + 6].try_into().unwrap()) as usize;
                    len = u32::from_le_bytes(ext[at + 10..at + 14].try_into().unwrap()) as usize;
                    break;
                }
                at += rl;
            }
        }
        (lba, len)
    }

    #[test]
    fn every_extent_starts_on_a_sector_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("s.iso");
        let len = write_iso(&sample(dir.path()), "OWNBACKUP", None, &iso).unwrap();
        assert_eq!(len as usize % SECTOR, 0, "the image must be whole sectors");
        for p in ["EFI/BOOT/BOOTX64.EFI", "boot/boot.sdi", "sources/boot.wim"] {
            let (lba, _) = find_file(&iso, p).unwrap().unwrap();
            assert!(lba >= 16, "{p} landed inside the system area");
        }
    }
}
