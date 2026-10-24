//! Writing a small FAT16 image from scratch.
//!
//! One thing needs this: the boot image inside an ISO. UEFI firmware does not
//! read ISO9660 — it looks in the El Torito catalog for a FAT volume, mounts it
//! as if it were the EFI System Partition, and runs `\EFI\BOOT\BOOTX64.EFI`
//! from there. So a bootable ISO has to carry a real, if tiny, FAT filesystem.
//!
//! Deliberately narrow: FAT16, a handful of files, sizes measured in
//! megabytes. Everything the EFI boot chain needs fits — the loader and the
//! boot configuration are a couple of megabytes together — and every feature
//! left out is one that cannot be got wrong.
//!
//! A name that is not already an upper-case 8.3 one gets a VFAT long name
//! beside a unique short alias (`MICROS~1`), as Windows writes it. Until
//! 2026-10-17 names were cut to 8.3 instead: `EFI\Microsoft` was `EFI\MICROSOF`
//! on the image, which a lookup by the real name does not find. The firmware's
//! FAT driver (UEFI requires long names) and Windows both read the long name.

use own_backup_common::{tr, OwnBackupError, Result};

const SECTOR: usize = 512;
/// One cluster per sector keeps the arithmetic trivial and wastes nothing at
/// these sizes.
const SECTORS_PER_CLUSTER: u8 = 1;
const RESERVED_SECTORS: u16 = 1;
const FAT_COUNT: u8 = 2;
/// 512 entries — the customary root size, and far more than we put in it.
const ROOT_ENTRIES: u16 = 512;
/// FAT16 is only valid above 4085 clusters; below that a driver is entitled to
/// read the volume as FAT12 and find nonsense. Padding to a safe margin costs
/// a couple of megabytes on a medium measured in hundreds.
const MIN_CLUSTERS: u32 = 4200;

/// A file to place in the image. Directories are created from the paths.
pub struct FatFile {
    /// Path inside the image, `\`-separated: `EFI\BOOT\BOOTX64.EFI`.
    pub path: String,
    pub data: Vec<u8>,
}

/// Build a FAT16 image holding `files`, with `label` as the volume label.
///
/// The image is sized to fit and rounded up to whole clusters, plus the minimum
/// FAT16 needs.
pub fn build_fat16(files: &[FatFile], label: &str) -> Result<Vec<u8>> {
    // --- work out the shape of the tree -----------------------------------
    let mut dirs: Vec<String> = Vec::new(); // parent-first, "" is the root
    for f in files {
        let parts: Vec<&str> = f.path.split('\\').collect();
        let mut cur = String::new();
        for p in &parts[..parts.len().saturating_sub(1)] {
            if !cur.is_empty() {
                cur.push('\\');
            }
            cur.push_str(p);
            if !dirs.contains(&cur) {
                dirs.push(cur.clone());
            }
        }
    }

    // Each directory's entries: `.`, `..` (or the label in the root) and one
    // short entry per child, plus that child's long-name entries.
    let mut entries: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    entries.insert(String::new(), 1);
    for d in &dirs {
        entries.insert(d.clone(), 2);
    }
    for path in dirs.iter().chain(files.iter().map(|f| &f.path)) {
        let (parent, name) = split_parent(path);
        *entries.entry(parent).or_insert(0) += 1 + long_entries(&name);
    }
    if entries[""] > ROOT_ENTRIES as usize {
        return Err(OwnBackupError::ImageFormat(
            tr!("fat_writer.image_full").into_owned(),
        ));
    }
    let dir_clusters = |d: &String| entries[d].div_ceil(SECTOR / 32) as u32;
    let data_clusters_needed: u32 = dirs.iter().map(dir_clusters).sum::<u32>()
        + files
            .iter()
            .map(|f| {
                let c = f.data.len().div_ceil(SECTOR * SECTORS_PER_CLUSTER as usize) as u32;
                c.max(1)
            })
            .sum::<u32>();
    let clusters = (data_clusters_needed + 2).max(MIN_CLUSTERS);

    let fat_sectors = ((clusters as usize + 2) * 2).div_ceil(SECTOR) as u16;
    let root_sectors = (ROOT_ENTRIES as usize * 32).div_ceil(SECTOR) as u16;
    let data_start =
        RESERVED_SECTORS as u32 + FAT_COUNT as u32 * fat_sectors as u32 + root_sectors as u32;
    let total_sectors = data_start + clusters * SECTORS_PER_CLUSTER as u32;

    let mut img = vec![0u8; total_sectors as usize * SECTOR];

    // --- boot sector -------------------------------------------------------
    {
        let b = &mut img[..SECTOR];
        b[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]); // jump, as a real one has
        b[3..11].copy_from_slice(b"MSWIN4.1");
        b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        b[13] = SECTORS_PER_CLUSTER;
        b[14..16].copy_from_slice(&RESERVED_SECTORS.to_le_bytes());
        b[16] = FAT_COUNT;
        b[17..19].copy_from_slice(&ROOT_ENTRIES.to_le_bytes());
        // "Total sectors" lives in the 16-bit field when it fits, else in the
        // 32-bit one — a driver reads whichever is non-zero.
        if total_sectors < 0x1_0000 {
            b[19..21].copy_from_slice(&(total_sectors as u16).to_le_bytes());
        } else {
            b[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        }
        b[21] = 0xF8; // fixed disk
        b[22..24].copy_from_slice(&fat_sectors.to_le_bytes());
        b[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors per track
        b[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
        b[36] = 0x80; // drive number
        b[38] = 0x29; // extended boot signature: the three fields below are real
        b[39..43].copy_from_slice(&0x4F42_4B55u32.to_le_bytes()); // volume serial
        let mut lab = [b' '; 11];
        for (i, c) in label.bytes().take(11).enumerate() {
            lab[i] = c.to_ascii_uppercase();
        }
        b[43..54].copy_from_slice(&lab);
        b[54..62].copy_from_slice(b"FAT16   ");
        b[510] = 0x55;
        b[511] = 0xAA;
    }

    // --- allocate clusters --------------------------------------------------
    let mut fat: Vec<u16> = vec![0; clusters as usize + 2];
    fat[0] = 0xFFF8;
    fat[1] = 0xFFFF;
    let mut next_free: u16 = 2;
    let mut alloc = |count: u32, fat: &mut Vec<u16>| -> Result<u16> {
        if count == 0 {
            return Ok(0);
        }
        let first = next_free;
        for i in 0..count {
            let c = next_free;
            if (c as u32) >= clusters + 2 {
                return Err(OwnBackupError::ImageFormat(
                    tr!("fat_writer.image_full").into_owned(),
                ));
            }
            fat[c as usize] = if i + 1 == count { 0xFFFF } else { c + 1 };
            next_free += 1;
        }
        Ok(first)
    };

    // Directories first, so their clusters are contiguous and easy to reason
    // about when reading the image back.
    let mut dir_cluster: Vec<(String, u16)> = Vec::new();
    for d in &dirs {
        dir_cluster.push((d.clone(), alloc(dir_clusters(d), &mut fat)?));
    }
    let mut file_cluster: Vec<(usize, u16)> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let n = (f.data.len().div_ceil(SECTOR * SECTORS_PER_CLUSTER as usize) as u32).max(1);
        file_cluster.push((i, alloc(n, &mut fat)?));
    }

    // --- write both FATs ----------------------------------------------------
    for copy in 0..FAT_COUNT as usize {
        let off = (RESERVED_SECTORS as usize + copy * fat_sectors as usize) * SECTOR;
        for (i, v) in fat.iter().enumerate() {
            let p = off + i * 2;
            if p + 2 <= img.len() {
                img[p..p + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    // --- directory entries --------------------------------------------------
    let root_off = (RESERVED_SECTORS as usize + FAT_COUNT as usize * fat_sectors as usize) * SECTOR;
    let cluster_off = |c: u16| -> usize {
        (data_start as usize + (c as usize - 2) * SECTORS_PER_CLUSTER as usize) * SECTOR
    };
    // How many entries each directory has written so far, and its short names.
    let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut taken: std::collections::HashMap<String, Vec<[u8; 11]>> =
        std::collections::HashMap::new();

    // Volume label, first entry of the root — that is where a driver looks.
    write_entry(&mut img, root_off, label, 0x08, 0, 0);
    used.insert(String::new(), 1);

    for (path, cluster) in &dir_cluster {
        let (parent, name) = split_parent(path);
        let parent_cluster = dir_cluster
            .iter()
            .find(|(p, _)| *p == parent)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        let slot = used.entry(parent.clone()).or_insert(0);
        let base = if parent.is_empty() {
            root_off
        } else {
            cluster_off(parent_cluster)
        };
        let siblings = taken.entry(parent.clone()).or_default();
        for e in named_entries(&name, 0x10, *cluster, 0, siblings) {
            img[base + *slot * 32..base + *slot * 32 + 32].copy_from_slice(&e);
            *slot += 1;
        }

        // `.` and `..` — a directory without them is not a directory.
        let here = cluster_off(*cluster);
        write_entry(&mut img, here, ".", 0x10, *cluster, 0);
        write_entry(&mut img, here + 32, "..", 0x10, parent_cluster, 0);
        used.insert(path.clone(), 2);
    }

    for (i, cluster) in &file_cluster {
        let f = &files[*i];
        let (parent, name) = split_parent(&f.path);
        let parent_cluster = dir_cluster
            .iter()
            .find(|(p, _)| *p == parent)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        let slot = used.entry(parent.clone()).or_insert(0);
        let base = if parent.is_empty() {
            root_off
        } else {
            cluster_off(parent_cluster)
        };
        let siblings = taken.entry(parent.clone()).or_default();
        for e in named_entries(&name, 0x20, *cluster, f.data.len() as u32, siblings) {
            img[base + *slot * 32..base + *slot * 32 + 32].copy_from_slice(&e);
            *slot += 1;
        }

        let at = cluster_off(*cluster);
        img[at..at + f.data.len()].copy_from_slice(&f.data);
    }

    Ok(img)
}

/// Split `EFI\BOOT\BOOTX64.EFI` into (`EFI\BOOT`, `BOOTX64.EFI`).
fn split_parent(path: &str) -> (String, String) {
    match path.rfind('\\') {
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// The label, `.` and `..`: written as they are, space-padded, never a long name.
fn write_entry(img: &mut [u8], at: usize, name: &str, attr: u8, cluster: u16, size: u32) {
    if at + 32 > img.len() {
        return;
    }
    let mut short = [b' '; 11];
    for (i, c) in name.bytes().take(11).enumerate() {
        short[i] = c.to_ascii_uppercase();
    }
    img[at..at + 32].copy_from_slice(&short_entry(&short, attr, cluster, size));
}

/// Characters an 8.3 name may hold besides upper-case letters and digits.
const SHORT_EXTRA: &[u8] = b"!#$%&'()-@^_`{}~";

fn short_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || SHORT_EXTRA.contains(&c)
}

/// The 11 bytes of `name` when it already is an upper-case 8.3 name.
fn exact_short(name: &str) -> Option<[u8; 11]> {
    let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
    let fits = (1..=8).contains(&stem.len())
        && ext.len() <= 3
        && !(name.ends_with('.'))
        && stem.bytes().chain(ext.bytes()).all(short_char);
    if !fits {
        return None;
    }
    let mut short = [b' '; 11];
    short[..stem.len()].copy_from_slice(stem.as_bytes());
    short[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Some(short)
}

/// A short alias no sibling has: `Microsoft` → `MICROS~1`, `boot.stl` →
/// `BOOT~1.STL`, as Windows makes them.
fn short_alias(name: &str, taken: &[[u8; 11]]) -> [u8; 11] {
    let clean = |s: &str| -> Vec<u8> {
        s.bytes()
            .filter(|&c| c != b' ' && c != b'.')
            .map(|c| c.to_ascii_uppercase())
            .map(|c| if short_char(c) { c } else { b'_' })
            .collect()
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, x)) if !s.is_empty() => (clean(s), clean(x)),
        _ => (clean(name), Vec::new()),
    };
    for n in 1u32.. {
        let tail = format!("~{n}");
        let keep = stem.len().min(8 - tail.len());
        let mut short = [b' '; 11];
        short[..keep].copy_from_slice(&stem[..keep]);
        short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        let x = ext.len().min(3);
        short[8..8 + x].copy_from_slice(&ext[..x]);
        if !taken.contains(&short) {
            return short;
        }
    }
    unreachable!("a directory holds fewer than 2^32 entries")
}

/// Long-name entries `name` needs before its short entry.
fn long_entries(name: &str) -> usize {
    if exact_short(name).is_some() {
        0
    } else {
        name.encode_utf16().count().div_ceil(13)
    }
}

/// The entries a child takes in its directory, in order: its long-name
/// entries (last part first), then the short entry they belong to.
fn named_entries(
    name: &str,
    attr: u8,
    cluster: u16,
    size: u32,
    taken: &mut Vec<[u8; 11]>,
) -> Vec<[u8; 32]> {
    if let Some(short) = exact_short(name) {
        taken.push(short);
        return vec![short_entry(&short, attr, cluster, size)];
    }
    let short = short_alias(name, taken);
    taken.push(short);
    let sum = short
        .iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
    let units: Vec<u16> = name.encode_utf16().collect();
    let parts = units.len().div_ceil(13);
    let mut out = Vec::with_capacity(parts + 1);
    for part in (0..parts).rev() {
        let mut e = [0u8; 32];
        e[0] = (part + 1) as u8 | if part + 1 == parts { 0x40 } else { 0 };
        e[11] = 0x0F; // read-only | hidden | system | volume: a long-name entry
        e[13] = sum;
        // Thirteen UTF-16 units in three runs; after the name one 0x0000,
        // then 0xFFFF padding.
        const AT: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (i, at) in AT.iter().enumerate() {
            let k = part * 13 + i;
            let unit = match k.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[k],
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            e[*at..*at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.push(e);
    }
    out.push(short_entry(&short, attr, cluster, size));
    out
}

/// One 32-byte short directory entry.
fn short_entry(short: &[u8; 11], attr: u8, cluster: u16, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(short);
    e[11] = attr;
    // A fixed timestamp: 2026-01-01 00:00. The alternative is the build clock,
    // which would make two builds of the same medium differ byte for byte for
    // no reason anyone benefits from.
    e[22..24].copy_from_slice(&0u16.to_le_bytes());
    e[24..26].copy_from_slice(&(((2026 - 1980) << 9) | (1 << 5) | 1u16).to_le_bytes());
    e[26..28].copy_from_slice(&cluster.to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn efi_tree() -> Vec<FatFile> {
        vec![
            FatFile {
                path: r"EFI\BOOT\BOOTX64.EFI".into(),
                data: vec![0xAA; 1_681_904],
            },
            FatFile {
                path: r"EFI\Microsoft\Boot\BCD".into(),
                data: vec![0xBB; 16_384],
            },
        ]
    }

    #[test]
    fn the_boot_sector_says_fat16() {
        let img = build_fat16(&efi_tree(), "OWNBOOT").unwrap();
        assert_eq!(&img[54..62], b"FAT16   ");
        assert_eq!(img[510], 0x55);
        assert_eq!(img[511], 0xAA);
        assert_eq!(&img[43..50], b"OWNBOOT");
    }

    /// Below 4085 clusters a driver may read the volume as FAT12 and see
    /// nothing but nonsense, so a small image is padded rather than shrunk.
    #[test]
    fn a_tiny_image_is_still_a_valid_fat16() {
        let img = build_fat16(
            &[FatFile {
                path: "A.TXT".into(),
                data: b"hi".to_vec(),
            }],
            "T",
        )
        .unwrap();
        let total = u16::from_le_bytes([img[19], img[20]]) as u32;
        let reserved = u16::from_le_bytes([img[14], img[15]]) as u32;
        let fats = img[16] as u32 * u16::from_le_bytes([img[22], img[23]]) as u32;
        let root = (u16::from_le_bytes([img[17], img[18]]) as u32 * 32).div_ceil(512);
        let clusters = (total - reserved - fats - root) / img[13] as u32;
        assert!(
            clusters > 4085,
            "only {clusters} clusters — a driver may read this as FAT12"
        );
    }

    #[test]
    fn files_land_where_the_directory_entries_point() {
        let files = efi_tree();
        let img = build_fat16(&files, "OWNBOOT").unwrap();
        let reserved = u16::from_le_bytes([img[14], img[15]]) as usize;
        let fat_sectors = u16::from_le_bytes([img[22], img[23]]) as usize;
        let root_off = (reserved + img[16] as usize * fat_sectors) * SECTOR;
        // Root: volume label, then the EFI directory.
        assert_eq!(
            &img[root_off + 11..root_off + 12],
            &[0x08],
            "label entry first"
        );
        assert_eq!(&img[root_off + 32..root_off + 35], b"EFI");
        assert_eq!(img[root_off + 32 + 11], 0x10, "EFI must be a directory");
    }

    #[test]
    fn an_oversized_request_fails_instead_of_writing_a_broken_image() {
        // One file larger than the clusters the header can describe.
        let huge = FatFile {
            path: "BIG.BIN".into(),
            data: vec![0; 8],
        };
        let mut files = vec![huge];
        for i in 0..10 {
            files.push(FatFile {
                path: format!("F{i}.BIN"),
                data: vec![0; 8],
            });
        }
        // Not an error at this size — the point is that it builds cleanly.
        assert!(build_fat16(&files, "T").is_ok());
    }

    #[test]
    fn short_names_and_their_aliases() {
        assert_eq!(&exact_short("BOOTX64.EFI").unwrap(), b"BOOTX64 EFI");
        assert_eq!(&exact_short("BCD").unwrap(), b"BCD        ");
        for long in [
            "Microsoft",
            "boot.stl",
            "a.b.c",
            "NAME.TOOLONG",
            "X.",
            "has space",
        ] {
            assert!(exact_short(long).is_none(), "{long}");
        }
        let mut taken = Vec::new();
        assert_eq!(&short_alias("Microsoft", &taken), b"MICROS~1   ");
        taken.push(short_alias("Microsoft", &taken));
        assert_eq!(&short_alias("Microsoft", &taken), b"MICROS~2   ");
        assert_eq!(&short_alias("boot.stl", &[]), b"BOOT~1  STL");
        assert_eq!(&short_alias("a+b.json", &[]), b"A_B~1   JSO");
    }

    /// Every name in the image as a reader sees it, long name first, with
    /// the long-name checksum checked against the short entry it precedes.
    fn listing(img: &[u8]) -> Vec<String> {
        let reserved = u16::from_le_bytes([img[14], img[15]]) as usize;
        let fat_sectors = u16::from_le_bytes([img[22], img[23]]) as usize;
        let root_entries = u16::from_le_bytes([img[17], img[18]]) as usize;
        let root_off = (reserved + img[16] as usize * fat_sectors) * SECTOR;
        let data_start = root_off + root_entries * 32;
        let fat = |c: u16| {
            let at = reserved * SECTOR + c as usize * 2;
            u16::from_le_bytes([img[at], img[at + 1]])
        };
        fn walk(
            img: &[u8],
            entries: Vec<&[u8]>,
            prefix: &str,
            data_start: usize,
            fat: &dyn Fn(u16) -> u16,
            out: &mut Vec<String>,
        ) {
            let mut long: Vec<(usize, Vec<u16>, u8)> = Vec::new();
            for e in entries {
                if e[0] == 0 {
                    break;
                }
                if e[11] == 0x0F {
                    let units = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                        .iter()
                        .map(|&at| u16::from_le_bytes([e[at], e[at + 1]]))
                        .take_while(|&u| u != 0 && u != 0xFFFF)
                        .collect();
                    long.push(((e[0] & 0x1F) as usize, units, e[13]));
                    continue;
                }
                if e[11] & 0x08 != 0 || e[0] == b'.' {
                    long.clear();
                    continue;
                }
                let short: &[u8; 11] = e[..11].try_into().unwrap();
                let name = if long.is_empty() {
                    let stem = String::from_utf8_lossy(&short[..8]).trim_end().to_string();
                    let ext = String::from_utf8_lossy(&short[8..]).trim_end().to_string();
                    if ext.is_empty() {
                        stem
                    } else {
                        format!("{stem}.{ext}")
                    }
                } else {
                    let sum = short
                        .iter()
                        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
                    assert!(long.iter().all(|(_, _, s)| *s == sum), "long-name checksum");
                    long.sort_by_key(|(n, _, _)| *n);
                    let units: Vec<u16> = long.iter().flat_map(|(_, u, _)| u.clone()).collect();
                    String::from_utf16(&units).unwrap()
                };
                long.clear();
                let path = format!("{prefix}/{name}");
                if e[11] & 0x10 != 0 {
                    let mut chunks = Vec::new();
                    let mut c = u16::from_le_bytes([e[26], e[27]]);
                    while (2..0xFFF8).contains(&c) {
                        let at = data_start + (c as usize - 2) * SECTOR;
                        chunks.extend(img[at..at + SECTOR].chunks(32));
                        c = fat(c);
                    }
                    walk(img, chunks, &path, data_start, fat, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        let root = img[root_off..data_start].chunks(32).collect();
        walk(img, root, "", data_start, &fat, &mut out);
        out.sort();
        out
    }

    fn boot_tree() -> Vec<FatFile> {
        let mut files = efi_tree();
        files.push(FatFile {
            path: r"EFI\Microsoft\Boot\boot.stl".into(),
            data: vec![0xCC; 4000],
        });
        files.push(FatFile {
            path: r"boot\boot.sdi".into(),
            data: vec![0xDD; 3_170_304],
        });
        // More than one cluster of entries in one directory.
        for i in 0..20 {
            files.push(FatFile {
                path: format!(r"EFI\Microsoft\Boot\ru-RU\bootmgr.efi.mui.{i}"),
                data: vec![i as u8; 100],
            });
        }
        files
    }

    #[test]
    fn long_names_are_read_back_whole() {
        let img = build_fat16(&boot_tree(), "OWNBOOT").unwrap();
        let names = listing(&img);
        for want in [
            "/EFI/BOOT/BOOTX64.EFI",
            "/EFI/Microsoft/Boot/BCD",
            "/EFI/Microsoft/Boot/boot.stl",
            "/boot/boot.sdi",
            "/EFI/Microsoft/Boot/ru-RU/bootmgr.efi.mui.19",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
        }
        assert_eq!(names.len(), 24, "{names:?}");
    }

    /// The image as dosfstools sees it, where it is installed (the Linux
    /// build machine): clean, and every long name intact.
    #[test]
    fn fsck_fat_finds_the_image_clean() {
        if std::process::Command::new("fsck.fat")
            .arg("--help")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("efisys.bin");
        std::fs::write(&path, build_fat16(&boot_tree(), "OWNBOOT").unwrap()).unwrap();
        let out = std::process::Command::new("fsck.fat")
            .args(["-n", "-l", "-v"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("/EFI/Microsoft/Boot/boot.stl"), "{text}");
        assert!(
            text.contains("/EFI/Microsoft/Boot/ru-RU/bootmgr.efi.mui.19"),
            "{text}"
        );
    }
}
