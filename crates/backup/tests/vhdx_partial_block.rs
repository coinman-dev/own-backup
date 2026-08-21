//! A partially-present payload block must resolve sector by sector.
//!
//! `PAYLOAD_BLOCK_PARTIALLY_PRESENT` (state 7) means only SOME sectors of the
//! block live in this file; the chunk's sector bitmap says which, and the rest
//! still come from the parent. Our reader used to treat state 7 like "fully
//! present" and hand back the whole block from the child file, where the
//! parent's sectors are zeros.
//!
//! That is not theoretical: on 2026-08-21 a transient mount of an incremental
//! image let the Windows FAT32 driver write three sectors (the FAT32 boot
//! sector at LBA 34, 35 and 66) into block 0 of the child. Block 0 also holds
//! the GPT header at LBA 1, which belongs to the parent — so every later read
//! of that image got zeros there and the whole image was rejected with
//! "disk has no GPT (MBR-only disks are not supported)". The image itself was
//! intact; only our parser was wrong.
//!
//! The fixtures here are built byte by byte because that is the only way to
//! produce state 7 on Linux: Windows writes it, we never do.

use std::path::Path;

use own_backup_backup::vhdx_bat::{read_vhdx_guest_at, VhdxGuestReader};

const REGION1_OFF: usize = 192 * 1024;
const META_OFF: u64 = 2 * 1024 * 1024;
const BAT_OFF: u64 = 3 * 1024 * 1024;
const DATA_OFF: u64 = 4 * 1024 * 1024; // first payload/bitmap block
const BLOCK: u64 = 1024 * 1024;
const SECTOR: u64 = 512;
const VSIZE: u64 = 4 * BLOCK;
/// (2^23 * sector) / block — the number of payload entries per sector-bitmap
/// entry in the BAT, 4096 for 512-byte sectors and 1 MiB blocks.
const CHUNK_RATIO: u64 = (1 << 23) * SECTOR / BLOCK;

fn guid_le(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    let b: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect();
    let mut o = [0u8; 16];
    o[0..4].copy_from_slice(&[b[3], b[2], b[1], b[0]]);
    o[4..6].copy_from_slice(&[b[5], b[4]]);
    o[6..8].copy_from_slice(&[b[7], b[6]]);
    o[8..16].copy_from_slice(&b[8..16]);
    o
}

fn put(buf: &mut [u8], off: usize, bytes: &[u8]) {
    buf[off..off + bytes.len()].copy_from_slice(bytes);
}

/// Build a VHDX whose BAT entries are given as `(bat_index, state, file_offset)`
/// and whose file body is `body` (placed at [`DATA_OFF`]).
fn build_vhdx(path: &Path, entries: &[(u64, u64, u64)], body: &[(u64, Vec<u8>)], parent: Option<&str>) {
    let mut file = vec![0u8; (DATA_OFF + 8 * BLOCK) as usize];
    put(&mut file, 0, b"vhdxfile");

    // Region table: BAT + metadata.
    let r = REGION1_OFF;
    put(&mut file, r, b"regi");
    put(&mut file, r + 8, &2u32.to_le_bytes());
    put(&mut file, r + 16, &guid_le("2dc27766-f623-4200-9d64-115e9bfd4a08"));
    put(&mut file, r + 32, &BAT_OFF.to_le_bytes());
    put(&mut file, r + 40, &(1024 * 1024u32).to_le_bytes());
    put(&mut file, r + 48, &guid_le("8b7ca206-4790-4b9a-b8fe-575f050f886e"));
    put(&mut file, r + 64, &META_OFF.to_le_bytes());
    put(&mut file, r + 72, &(1024 * 1024u32).to_le_bytes());

    // Metadata region: item offsets are relative to the region start.
    let m = META_OFF as usize;
    put(&mut file, m, b"metadata");
    let item_count: u16 = if parent.is_some() { 4 } else { 3 };
    put(&mut file, m + 10, &item_count.to_le_bytes());
    let mut items: Vec<([u8; 16], u32, u32)> = vec![
        (guid_le("caa16737-fa36-4d43-b3b6-33f0aa44e76b"), 0x1000, 8),
        (guid_le("2fa54224-cd1b-4876-b211-5dbed83bf4b8"), 0x1010, 8),
        (guid_le("8141bf1d-a96f-4709-ba47-f233a8faab5f"), 0x1020, 4),
    ];
    put(&mut file, m + 0x1000, &(BLOCK as u32).to_le_bytes()); // block size
    // FileParameters flags: bit 1 = has-parent, set only for a real child.
    let flags: u32 = if parent.is_some() { 2 } else { 0 };
    put(&mut file, m + 0x1004, &flags.to_le_bytes());
    put(&mut file, m + 0x1010, &VSIZE.to_le_bytes());
    put(&mut file, m + 0x1020, &(SECTOR as u32).to_le_bytes());

    if let Some(parent_name) = parent {
        // Parent locator: one "relative_path" key/value pair, UTF-16LE.
        let base = 0x2000usize;
        let key: Vec<u8> = "relative_path".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let val: Vec<u8> = parent_name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let key_off = 32u32;
        let val_off = key_off + key.len() as u32;
        put(&mut file, m + base + 18, &1u16.to_le_bytes()); // key/value count
        put(&mut file, m + base + 20, &key_off.to_le_bytes());
        put(&mut file, m + base + 24, &val_off.to_le_bytes());
        put(&mut file, m + base + 28, &(key.len() as u16).to_le_bytes());
        put(&mut file, m + base + 30, &(val.len() as u16).to_le_bytes());
        put(&mut file, m + base + key_off as usize, &key);
        put(&mut file, m + base + val_off as usize, &val);
        items.push((
            guid_le("a8d35f2d-b30b-454d-abf7-d3d84834ab0c"),
            base as u32,
            (val_off as usize + val.len()) as u32,
        ));
    }
    for (i, (guid, ioff, ilen)) in items.iter().enumerate() {
        let e = m + 32 + i * 32;
        put(&mut file, e, guid);
        put(&mut file, e + 16, &ioff.to_le_bytes());
        put(&mut file, e + 20, &ilen.to_le_bytes());
    }

    for (index, state, file_offset) in entries {
        let entry = (file_offset & 0xFFFF_FFFF_FFF0_0000) | (state & 7);
        put(&mut file, (BAT_OFF + index * 8) as usize, &entry.to_le_bytes());
    }
    for (off, bytes) in body {
        put(&mut file, *off as usize, bytes);
    }
    std::fs::write(path, &file).unwrap();
}

/// Parent: block 0 fully present, carrying a recognizable byte per sector.
fn parent_block0() -> Vec<u8> {
    let mut b = vec![0u8; BLOCK as usize];
    for s in 0..(BLOCK / SECTOR) {
        b[(s * SECTOR) as usize] = 0xAA;
    }
    b[(1 * SECTOR) as usize..(1 * SECTOR) as usize + 8].copy_from_slice(b"EFI PART");
    b
}

/// Child: same block, but only sectors 34, 35 and 66 are really its own.
fn child_block0_and_bitmap() -> (Vec<u8>, Vec<u8>) {
    let mut data = vec![0u8; BLOCK as usize];
    for s in [34u64, 35, 66] {
        data[(s * SECTOR) as usize] = 0xCC;
    }
    let mut bitmap = vec![0u8; BLOCK as usize];
    for s in [34u64, 35, 66] {
        bitmap[(s / 8) as usize] |= 1 << (s % 8);
    }
    (data, bitmap)
}

fn fixture(dir: &Path) {
    build_vhdx(
        &dir.join("parent.vhdx"),
        &[(0, 6, DATA_OFF)],
        &[(DATA_OFF, parent_block0())],
        None,
    );
    let (data, bitmap) = child_block0_and_bitmap();
    build_vhdx(
        &dir.join("child.avhdx"),
        &[
            (0, 7, DATA_OFF),                       // block 0: PARTIALLY present
            (CHUNK_RATIO, 6, DATA_OFF + 2 * BLOCK), // its chunk's sector bitmap
        ],
        &[(DATA_OFF, data), (DATA_OFF + 2 * BLOCK, bitmap)],
        Some("parent.vhdx"),
    );
}

/// Every sector of a partially-present block: ours where the bitmap says so,
/// the parent's everywhere else.
fn assert_resolves(mut read: impl FnMut(u64, &mut [u8])) {
    let mut buf = vec![0u8; BLOCK as usize];
    read(0, &mut buf);
    for s in 0..(BLOCK / SECTOR) {
        if s == 1 {
            continue; // holds the GPT signature, checked below
        }
        let first = buf[(s * SECTOR) as usize];
        let expected = if matches!(s, 34 | 35 | 66) { 0xCC } else { 0xAA };
        assert_eq!(
            first, expected,
            "sector {s} came from the wrong file (got {first:#04x}, want {expected:#04x})"
        );
    }
    // The GPT header lives at LBA 1 and belongs to the PARENT — the exact byte
    // range whose loss produced "disk has no GPT".
    assert_eq!(&buf[SECTOR as usize..SECTOR as usize + 8], b"EFI PART");
}

#[test]
fn stateless_reader_resolves_a_partially_present_block() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let child = dir.path().join("child.avhdx");
    assert_resolves(|off, buf| read_vhdx_guest_at(&child, off, buf).unwrap());
}

#[test]
fn cached_bat_reader_resolves_a_partially_present_block() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let mut r = VhdxGuestReader::open(&dir.path().join("child.avhdx")).unwrap();
    assert_resolves(|off, buf| r.read_at(off, buf).unwrap());
}

/// A read that does NOT start on a sector boundary must still follow the
/// bitmap — the embedded sidecar is read from `meta_offset + 12 + json_len`,
/// which is an arbitrary byte offset.
#[test]
fn unaligned_read_across_a_sector_boundary_follows_the_bitmap() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let child = dir.path().join("child.avhdx");
    // Start 100 bytes into sector 33 (parent's) and run into sector 34 (child's).
    let start = 33 * SECTOR + 100;
    let mut buf = vec![0u8; (SECTOR + 8) as usize];
    read_vhdx_guest_at(&child, start, &mut buf).unwrap();
    let into_34 = (SECTOR - 100) as usize;
    assert_eq!(buf[into_34], 0xCC, "the byte at the start of sector 34 is the child's");
    assert_eq!(buf[into_34 - 1], 0x00, "the tail of sector 33 is the parent's padding");
    // And the parent's marker byte is where sector 33 began, before our window.
    let mut one = [0u8; 1];
    read_vhdx_guest_at(&child, 33 * SECTOR, &mut one).unwrap();
    assert_eq!(one[0], 0xAA, "sector 33 belongs to the parent");
}

/// A read that starts mid-block must still follow the bitmap.
#[test]
fn partial_block_resolves_from_an_offset_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let child = dir.path().join("child.avhdx");
    let mut buf = vec![0u8; (4 * SECTOR) as usize];
    read_vhdx_guest_at(&child, 33 * SECTOR, &mut buf).unwrap();
    assert_eq!(buf[0], 0xAA, "sector 33 belongs to the parent");
    assert_eq!(buf[SECTOR as usize], 0xCC, "sector 34 belongs to the child");
    assert_eq!(buf[(2 * SECTOR) as usize], 0xCC, "sector 35 belongs to the child");
    assert_eq!(buf[(3 * SECTOR) as usize], 0xAA, "sector 36 belongs to the parent");
}

// ---------------------------------------------------------------------------
// Writing a differencing child in userspace
// ---------------------------------------------------------------------------
//
// The engine writes VHDX images without attaching them (no AttachVirtualDisk,
// so no ~180 s PnP detach veto). That puts the whole burden of "what does an
// untouched block read as" on our own writer — and the answer differs between
// a parentless image (zeros) and a differencing child (the PARENT's data).
// Both tests below fail if the writer forgets that difference, and both are
// silent data loss in production: the image looks fine and restores stale
// bytes.

use own_backup_backup::vhdx_format::{VhdxWriter, VhdxReader};

/// Build a parent + an empty differencing child over it, both 1 MiB blocks.
fn diff_pair(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let parent = dir.join("p.vhdx");
    let child = dir.join("c.avhdx");
    build_vhdx(&parent, &[(0, 6, DATA_OFF)], &[(DATA_OFF, parent_block0())], None);
    // No BAT entries at all: every block of the child comes from the parent.
    build_vhdx(&child, &[], &[], Some("p.vhdx"));
    (parent, child)
}

/// Writing zeros into a child must ALLOCATE the block. Skipping the write —
/// correct for a parentless image, where an absent block already reads as
/// zeros — leaves the parent's old bytes showing through: a file deleted (or a
/// region emptied) since the last backup would come back on restore.
#[test]
fn zeros_written_to_a_child_hide_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let (_parent, child) = diff_pair(dir.path());

    let mut w = VhdxWriter::open_existing(&child).unwrap();
    w.write_at(0, &vec![0u8; BLOCK as usize]).unwrap();
    w.finish().unwrap();

    let mut buf = vec![0u8; BLOCK as usize];
    read_vhdx_guest_at(&child, 0, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "the child was written all-zero, but the parent still shows through — \
         the zero-write shortcut must not apply to a differencing image"
    );
}

/// A partial write into a block the child does not own yet must keep the rest
/// of that block coming from the parent. The block becomes FULLY_PRESENT, so
/// anything not seeded reads back as zeros — losing the parent's data.
#[test]
fn a_partial_write_to_a_child_keeps_the_parents_rest_of_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let (_parent, child) = diff_pair(dir.path());

    // Touch one sector in the middle of block 0; the parent owns the rest.
    let mut w = VhdxWriter::open_existing(&child).unwrap();
    w.write_at(100 * SECTOR, &vec![0x5Au8; SECTOR as usize]).unwrap();
    w.finish().unwrap();

    let mut buf = vec![0u8; BLOCK as usize];
    read_vhdx_guest_at(&child, 0, &mut buf).unwrap();
    assert_eq!(buf[(100 * SECTOR) as usize], 0x5A, "our own write is missing");
    assert_eq!(
        &buf[SECTOR as usize..SECTOR as usize + 8],
        b"EFI PART",
        "the GPT header at LBA 1 belongs to the parent and must survive a \
         partial write elsewhere in the same block"
    );
    for s in [2u64, 50, 99, 101, 500] {
        assert_eq!(
            buf[(s * SECTOR) as usize],
            0xAA,
            "sector {s} lost the parent's data to a partial write"
        );
    }
}

/// The same write on a PARENTLESS image keeps the cheap behaviour: an all-zero
/// write allocates nothing, because an absent block already reads as zeros.
#[test]
fn zero_writes_to_a_parentless_image_stay_free() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.vhdx");
    let mut w = VhdxWriter::create(&path, 8 * BLOCK, BLOCK, SECTOR as u32).unwrap();
    w.write_at(0, &vec![0u8; BLOCK as usize]).unwrap();
    w.finish().unwrap();

    let mut r = VhdxReader::open(&path).unwrap();
    let mut buf = vec![0u8; BLOCK as usize];
    r.read_at(0, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len < 5 * BLOCK, "an all-zero block should not be allocated (file is {len} bytes)");
}
