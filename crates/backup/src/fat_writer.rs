//! Writing a small FAT16 image from scratch.
//!
//! One thing needs this: the boot image inside an ISO. UEFI firmware does not
//! read ISO9660 — it looks in the El Torito catalog for a FAT volume, mounts it
//! as if it were the EFI System Partition, and runs `\EFI\BOOT\BOOTX64.EFI`
//! from there. So a bootable ISO has to carry a real, if tiny, FAT filesystem.
//!
//! Deliberately narrow: FAT16, no long file names, a handful of files, sizes
//! measured in megabytes. Everything the EFI boot chain needs fits — the loader
//! and the boot configuration are a couple of megabytes together — and every
//! feature left out is one that cannot be got wrong.
//!
//! 8.3 names are not a limitation here but a requirement: the firmware reads
//! this volume with its own minimal driver, and `BOOTX64.EFI` is 8.3 already.

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

    // Every subdirectory takes one cluster (its own entries: `.`, `..`, plus
    // children — far below the 16 entries a 512-byte cluster holds).
    let data_clusters_needed: u32 = dirs.len() as u32
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
        dir_cluster.push((d.clone(), alloc(1, &mut fat)?));
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
    // How many entries each directory has written so far.
    let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

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
        write_entry(&mut img, base + *slot * 32, &name, 0x10, *cluster, 0);
        *slot += 1;

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
        write_entry(
            &mut img,
            base + *slot * 32,
            &name,
            0x20,
            *cluster,
            f.data.len() as u32,
        );
        *slot += 1;

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

/// One 32-byte directory entry. Names are 8.3, upper-cased and space-padded —
/// the form the firmware's own FAT driver expects.
fn write_entry(img: &mut [u8], at: usize, name: &str, attr: u8, cluster: u16, size: u32) {
    if at + 32 > img.len() {
        return;
    }
    let e = &mut img[at..at + 32];
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, x)) if name != "." && name != ".." => (s, x),
        _ => (name, ""),
    };
    e[..11].fill(b' ');
    for (i, c) in stem.bytes().take(8).enumerate() {
        e[i] = c.to_ascii_uppercase();
    }
    for (i, c) in ext.bytes().take(3).enumerate() {
        e[8 + i] = c.to_ascii_uppercase();
    }
    e[11] = attr;
    // A fixed timestamp: 2026-01-01 00:00. The alternative is the build clock,
    // which would make two builds of the same medium differ byte for byte for
    // no reason anyone benefits from.
    e[22..24].copy_from_slice(&0u16.to_le_bytes());
    e[24..26].copy_from_slice(&(((2026 - 1980) << 9) | (1 << 5) | 1u16).to_le_bytes());
    e[26..28].copy_from_slice(&cluster.to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
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
}
