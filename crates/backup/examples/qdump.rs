//! Diagnostic dump of the boot-critical regions of a backup qcow2 image:
//! GPT header, partition entry, NTFS primary boot sector, the patched backup
//! boot sector (at TotalSectors), and the partition's last sector — the spots
//! the NTFS driver reads/writes while (failing to) RW-mount the image.
//!
//! Usage: cargo run -p own-backup-backup --example qdump -- <image.qcow2>

use own_backup_backup::qcow2::Qcow2RandomReader;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn summarize(name: &str, sector: &[u8]) {
    let nonzero = sector.iter().filter(|&&b| b != 0).count();
    println!("{name}: nonzero_bytes={nonzero}/{}", sector.len());
    println!("  head: {}", hex(&sector[..32.min(sector.len())]));
    if sector.len() >= 512 {
        println!("  sig @510: {}", hex(&sector[510..512]));
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: qdump <image.qcow2>");
    let mut r = Qcow2RandomReader::open(std::path::Path::new(&path)).expect("open image");
    let vsize = r.virtual_size();
    println!("virtual_size={vsize} cluster_size={}", r.cluster_size());

    // GPT header (LBA 1).
    let mut gpt = vec![0u8; 512];
    r.read_at(512, &mut gpt).unwrap();
    println!("GPT header sig: {:?}", String::from_utf8_lossy(&gpt[..8]));

    // First GPT partition entry (LBA 2).
    let mut pe = vec![0u8; 128];
    r.read_at(1024, &mut pe).unwrap();
    let first_lba = u64::from_le_bytes(pe[32..40].try_into().unwrap());
    let last_lba = u64::from_le_bytes(pe[40..48].try_into().unwrap());
    let attrs = u64::from_le_bytes(pe[48..56].try_into().unwrap());
    let part_start = first_lba * 512;
    let part_len = (last_lba - first_lba + 1) * 512;
    println!("partition: first_lba={first_lba} last_lba={last_lba} attrs={attrs:#x}");
    println!("  start={part_start} len={part_len} end={}", part_start + part_len);

    // NTFS primary boot sector.
    let mut boot = vec![0u8; 512];
    r.read_at(part_start, &mut boot).unwrap();
    summarize("primary boot @part_start", &boot);
    let oem = String::from_utf8_lossy(&boot[3..11]).to_string();
    let bps = u16::from_le_bytes(boot[11..13].try_into().unwrap()) as u64;
    let total_sectors = u64::from_le_bytes(boot[40..48].try_into().unwrap());
    println!("  oem={oem:?} bps={bps} total_sectors={total_sectors}");
    println!(
        "  fs_bytes={} (partition is {} bytes longer)",
        total_sectors * bps,
        part_len as i64 - (total_sectors * bps) as i64
    );

    // Backup boot sector at TotalSectors (what the engine patches).
    let backup_at = part_start + total_sectors * bps;
    let mut backup = vec![0u8; 512];
    r.read_at(backup_at, &mut backup).unwrap();
    summarize(&format!("backup boot @TotalSectors (disk {backup_at})"), &backup);
    println!("  equals primary: {}", backup == boot);

    // Partition's last sector (where the mount was observed writing).
    let last_at = part_start + part_len - 512;
    let mut last = vec![0u8; 512];
    r.read_at(last_at, &mut last).unwrap();
    summarize(&format!("partition last sector (disk {last_at})"), &last);
    println!("  equals primary: {}", last == boot);

    // Sectors between FS end and partition end (the slack NTFS may probe).
    let slack_sectors = (part_len - total_sectors * bps) / 512;
    println!("slack sectors past FS end: {slack_sectors}");
    for i in 0..slack_sectors.min(8) {
        let at = backup_at + i * 512;
        let mut s = vec![0u8; 512];
        r.read_at(at, &mut s).unwrap();
        let nonzero = s.iter().filter(|&&b| b != 0).count();
        println!("  slack[{i}] @disk {at}: nonzero={nonzero} eq_primary={}", s == boot);
    }
}
