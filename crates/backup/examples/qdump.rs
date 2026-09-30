//! Diagnostic dump of a backup image's boot-critical regions, read through the
//! very reader the iSCSI target serves from: the GPT, every partition it
//! declares, and for an NTFS one the primary and backup boot sectors, the
//! $MFT's first records and how much of its start is not zeros.
//!
//! The question it answers is "is the image sound, or is Windows right to
//! refuse it" — asked whenever a mount of a finished image fails, which is a
//! Windows-only symptom with a cause that can be looked at from anywhere.
//!
//! Usage: cargo run -p own-backup-backup --example qdump -- <image.qcow2> [part_offset]

use own_backup_backup::qcow2::Qcow2RandomReader;

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn summarize(name: &str, sector: &[u8]) {
    let nonzero = sector.iter().filter(|&&b| b != 0).count();
    println!("{name}: nonzero_bytes={nonzero}/{}", sector.len());
    println!("  head: {}", hex(&sector[..32.min(sector.len())]));
    if sector.len() >= 512 {
        println!("  sig @510: {}", hex(&sector[510..512]));
    }
}

/// The partition types this machine's disks are made of, by GUID.
fn part_type(guid: &[u8]) -> &'static str {
    match hex(guid).as_str() {
        "28 73 2a c1 1f f8 d2 11 ba 4b 00 a0 c9 3e c9 3b" => "EFI system",
        "16 e3 c9 e3 5c 0b b8 4d 81 7d f9 2d f0 02 15 ae" => "Microsoft reserved",
        "a2 a0 d0 eb e5 b9 33 44 87 c0 68 b6 b7 26 99 c7" => "Basic data",
        "a4 bb 94 de d1 06 40 4d a1 6a bf d5 01 79 d6 ac" => "Windows recovery",
        "af 3d c6 0f 83 84 72 47 8e 79 3d 69 d8 47 7d e4" => "Linux filesystem",
        _ => "other",
    }
}

fn read(r: &mut Qcow2RandomReader, at: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    r.read_at(at, &mut buf)
        .unwrap_or_else(|e| panic!("read {len} bytes at {at}: {e}"));
    buf
}

/// Everything the NTFS driver looks at before it agrees to mount.
fn ntfs(r: &mut Qcow2RandomReader, part_start: u64, part_len: u64, boot: &[u8]) {
    let bps = u16::from_le_bytes(boot[11..13].try_into().unwrap()) as u64;
    let spc_raw = boot[13] as u64;
    let spc = if spc_raw <= 0x80 {
        spc_raw
    } else {
        1u64 << (0x100 - spc_raw)
    };
    let total_sectors = u64::from_le_bytes(boot[40..48].try_into().unwrap());
    let mft_lcn = u64::from_le_bytes(boot[48..56].try_into().unwrap());
    let cluster = bps * spc;
    println!("  bps={bps} sectors_per_cluster={spc} cluster={cluster}");
    println!(
        "  total_sectors={total_sectors} fs_bytes={}",
        total_sectors * bps
    );
    println!(
        "  partition is {} bytes longer than the filesystem",
        part_len as i64 - (total_sectors * bps) as i64
    );
    println!(
        "  mft_lcn={mft_lcn} mft_at_disk={}",
        part_start + mft_lcn * cluster
    );

    // The backup boot sector: a copy of sector 0 one sector past the
    // filesystem. It is not in the volume bitmap, so it is only in the image
    // if the copy put it there deliberately.
    let backup_at = part_start + total_sectors * bps;
    let backup = read(r, backup_at, 512);
    summarize(&format!("  backup boot @disk {backup_at}"), &backup);
    println!("  equals primary: {}", backup == boot);

    // The $MFT. Its first records are the metadata files, and every one of
    // them starts with "FILE" — a zero here means the copy has a hole where
    // the filesystem's own catalogue should be.
    let mft_at = part_start + mft_lcn * cluster;
    let mft = read(r, mft_at, 16 * 1024);
    let records = mft
        .chunks(1024)
        .take(16)
        .filter(|rec| &rec[..4] == b"FILE")
        .count();
    println!("  $MFT: {records}/16 first records start with FILE");
    println!("  $MFT head: {}", hex(&mft[..32]));

    // How solid the start of the filesystem is: metadata lives there, and a
    // mostly-zero first megabyte means it was never copied.
    let head = read(r, part_start, 1024 * 1024);
    let nonzero_sectors = head
        .chunks(512)
        .filter(|s| s.iter().any(|&b| b != 0))
        .count();
    println!("  first MiB: {nonzero_sectors}/2048 sectors carry data");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: qdump <image.qcow2> [part_offset]");
    let only: Option<u64> = args
        .next()
        .map(|a| a.parse().expect("part_offset must be a number"));
    let mut r = Qcow2RandomReader::open(std::path::Path::new(&path)).expect("open image");
    println!(
        "virtual_size={} cluster_size={}",
        r.virtual_size(),
        r.cluster_size()
    );

    let gpt = read(&mut r, 512, 512);
    println!("GPT header sig: {:?}", String::from_utf8_lossy(&gpt[..8]));
    if &gpt[..8] != b"EFI PART" {
        println!("no GPT — nothing else can be read by layout");
        return;
    }
    let entries_lba = u64::from_le_bytes(gpt[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(gpt[80..84].try_into().unwrap()) as u64;
    let size = u32::from_le_bytes(gpt[84..88].try_into().unwrap()) as u64;
    println!("partition entries: {count} × {size} bytes at LBA {entries_lba}");

    for i in 0..count.min(128) {
        let e = read(&mut r, entries_lba * 512 + i * size, size as usize);
        if e[..16].iter().all(|&b| b == 0) {
            continue; // an empty slot
        }
        let first_lba = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last_lba = u64::from_le_bytes(e[40..48].try_into().unwrap());
        let start = first_lba * 512;
        let len = (last_lba - first_lba + 1) * 512;
        if only.is_some_and(|o| o != start) {
            continue;
        }
        let name: String = e[56..size as usize]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .filter_map(|u| char::from_u32(u as u32))
            .collect();
        println!(
            "\npartition {}: {} «{name}» start={start} len={len} ({} GiB)",
            i + 1,
            part_type(&e[..16]),
            len >> 30
        );
        let boot = read(&mut r, start, 512);
        summarize("  first sector", &boot);
        match &boot[3..11] {
            b"NTFS    " => ntfs(&mut r, start, len, &boot),
            oem => println!("  not NTFS (oem {:?})", String::from_utf8_lossy(oem)),
        }
    }
}
