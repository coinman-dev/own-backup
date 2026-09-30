//! Forensic inspector for a parent VHDX + child AVHDX pair (MS-VHDX format).
//!
//! Answers, for one backup block: what is physically stored in the child,
//! what the parent holds, what the chain would return, and how that compares
//! to the sidecar checksums. Used to debug the persistent verify mismatch on
//! the volume's last block.
//!
//! Usage:
//!   cargo run -p own-backup-backup --example inspect_vhdx -- \
//!       <parent.vhdx> <child.avhdx> <volume_block_number>

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use own_backup_backup::checksum::compute_block_checksum;
use own_backup_backup::sidecar::{read_sidecar, sidecar_path_for};

const MIB: u64 = 1024 * 1024;

// Well-known VHDX GUIDs, already in on-disk byte order.
const GUID_BAT: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
const GUID_METADATA: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];
const GUID_FILE_PARAMS: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
const GUID_VDISK_SIZE: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
const GUID_SECTOR_SIZE: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

struct Vhdx {
    file: File,
    bat_offset: u64,
    block_size: u32,
    sector_size: u32,
    #[allow(dead_code)] // printed at open time
    virtual_size: u64,
    #[allow(dead_code)]
    has_parent: bool,
    chunk_ratio: u64,
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.read_exact(&mut buf).expect("read");
    buf
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Vhdx {
    fn open(path: &str) -> Self {
        let mut file = File::open(path).expect("open vhdx");

        let sig = read_at(&mut file, 0, 8);
        assert_eq!(&sig, b"vhdxfile", "not a VHDX file: {path}");

        // Region table (primary copy at 192 KiB).
        let region = read_at(&mut file, 192 * 1024, 64 * 1024);
        assert_eq!(&region[0..4], b"regi", "bad region table");
        let entry_count = u32le(&region[8..12]) as usize;

        let mut bat_offset = 0u64;
        let mut meta_offset = 0u64;
        let mut meta_len = 0usize;
        for i in 0..entry_count {
            let e = &region[16 + i * 32..16 + (i + 1) * 32];
            let guid: [u8; 16] = e[0..16].try_into().unwrap();
            let offset = u64le(&e[16..24]);
            let length = u32le(&e[24..28]) as usize;
            if guid == GUID_BAT {
                bat_offset = offset;
            } else if guid == GUID_METADATA {
                meta_offset = offset;
                meta_len = length;
            }
        }
        assert!(
            bat_offset > 0 && meta_offset > 0,
            "missing BAT/metadata region"
        );

        // Metadata table.
        let meta = read_at(&mut file, meta_offset, meta_len.min(1024 * 1024));
        assert_eq!(&meta[0..8], b"metadata", "bad metadata table");
        let meta_count = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;

        let mut block_size = 0u32;
        let mut has_parent = false;
        let mut virtual_size = 0u64;
        let mut sector_size = 0u32;
        for i in 0..meta_count {
            let e = &meta[32 + i * 32..32 + (i + 1) * 32];
            let guid: [u8; 16] = e[0..16].try_into().unwrap();
            let off = u32le(&e[16..20]) as usize;
            if guid == GUID_FILE_PARAMS {
                block_size = u32le(&meta[off..off + 4]);
                has_parent = u32le(&meta[off + 4..off + 8]) & 0x2 != 0;
            } else if guid == GUID_VDISK_SIZE {
                virtual_size = u64le(&meta[off..off + 8]);
            } else if guid == GUID_SECTOR_SIZE {
                sector_size = u32le(&meta[off..off + 4]);
            }
        }
        assert!(block_size > 0 && sector_size > 0 && virtual_size > 0);
        let chunk_ratio = (8388608u64 * sector_size as u64) / block_size as u64;

        println!(
            "{path}\n  block_size={block_size} sector_size={sector_size} \
             virtual_size={virtual_size} has_parent={has_parent} chunk_ratio={chunk_ratio}"
        );

        Self {
            file,
            bat_offset,
            block_size,
            sector_size,
            virtual_size,
            has_parent,
            chunk_ratio,
        }
    }

    /// (state, file_offset) of a payload block by virtual-block index.
    fn payload_entry(&mut self, vblock: u64) -> (u8, u64) {
        let bat_index = vblock + vblock / self.chunk_ratio;
        let raw = u64le(&read_at(&mut self.file, self.bat_offset + bat_index * 8, 8));
        ((raw & 0x7) as u8, (raw >> 20) * MIB)
    }

    /// (state, file_offset) of the sector-bitmap block covering `vblock`.
    fn bitmap_entry(&mut self, vblock: u64) -> (u8, u64) {
        let chunk = vblock / self.chunk_ratio;
        let bat_index = (chunk + 1) * self.chunk_ratio + chunk;
        let raw = u64le(&read_at(&mut self.file, self.bat_offset + bat_index * 8, 8));
        ((raw & 0x7) as u8, (raw >> 20) * MIB)
    }

    fn read_block(&mut self, file_offset: u64) -> Vec<u8> {
        read_at(&mut self.file, file_offset, self.block_size as usize)
    }

    /// The 1-bit-per-sector presence bitmap slice for `vblock` (LSB-first).
    fn read_block_bitmap(&mut self, vblock: u64, bitmap_file_offset: u64) -> Vec<u8> {
        let within_chunk = vblock % self.chunk_ratio;
        let sectors_per_block = self.block_size as u64 / self.sector_size as u64;
        let byte_off = within_chunk * sectors_per_block / 8;
        read_at(
            &mut self.file,
            bitmap_file_offset + byte_off,
            (sectors_per_block / 8) as usize,
        )
    }
}

fn state_name(s: u8) -> &'static str {
    match s {
        0 => "NOT_PRESENT",
        1 => "UNDEFINED",
        2 => "ZERO",
        3 => "UNMAPPED",
        6 => "FULLY_PRESENT",
        7 => "PARTIALLY_PRESENT",
        _ => "??",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (parent_path, child_path, vol_block): (&str, &str, u64) = (
        &args[1],
        &args[2],
        args.get(3).map(|s| s.parse().unwrap()).unwrap_or(130948),
    );

    // Sidecar checksums for the block.
    for (label, img) in [("parent", parent_path), ("child", child_path)] {
        let sc = read_sidecar(&sidecar_path_for(std::path::Path::new(img))).expect("sidecar");
        let entry = sc.checksums.iter().find(|c| c.block_number == vol_block);
        println!(
            "{label} sidecar: role={:?} data_offset={} block {} hash={}",
            sc.info.chain_role,
            sc.info.volume_data_offset,
            vol_block,
            entry
                .map(|e| format!("{:016x}", e.checksum))
                .unwrap_or("ABSENT".into())
        );
    }

    let mut parent = Vhdx::open(parent_path);
    let mut child = Vhdx::open(child_path);
    assert_eq!(parent.block_size, child.block_size);

    // volume data offset is 1 MiB == one VHDX block → virtual block index.
    let data_offset = 1 * MIB;
    let vblock = (data_offset + vol_block * MIB) / parent.block_size as u64;
    println!("\nvolume block {vol_block} → virtual VHDX block {vblock}");

    // Parent state + content.
    let (p_state, p_off) = parent.payload_entry(vblock);
    println!(
        "parent payload: state={} file_offset={p_off}",
        state_name(p_state)
    );
    let p_data = match p_state {
        6 => parent.read_block(p_off),
        2 | 0 => vec![0u8; parent.block_size as usize],
        s => panic!("unexpected parent state {s}"),
    };
    println!(
        "parent block hash = {:016x}",
        compute_block_checksum(&p_data)
    );

    // Child state + content.
    let (c_state, c_off) = child.payload_entry(vblock);
    println!(
        "child payload:  state={} file_offset={c_off}",
        state_name(c_state)
    );

    let sectors_per_block = (child.block_size / child.sector_size) as usize;
    let composed: Vec<u8> = match c_state {
        0 => {
            println!("child: block not present → chain returns parent content");
            p_data.clone()
        }
        6 => {
            let c = child.read_block(c_off);
            println!(
                "child block (full) hash = {:016x}",
                compute_block_checksum(&c)
            );
            c
        }
        7 => {
            let c = child.read_block(c_off);
            let (bm_state, bm_off) = child.bitmap_entry(vblock);
            println!("child sector-bitmap: state={bm_state} file_offset={bm_off}");
            let bm = child.read_block_bitmap(vblock, bm_off);
            let present: Vec<usize> = (0..sectors_per_block)
                .filter(|&i| bm[i / 8] & (1 << (i % 8)) != 0)
                .collect();
            println!(
                "child has {} of {} sectors; ranges: {:?}",
                present.len(),
                sectors_per_block,
                compress_ranges(&present)
            );
            // Compose chain view: child sector where present, else parent.
            let ss = child.sector_size as usize;
            let mut out = p_data.clone();
            for &i in &present {
                out[i * ss..(i + 1) * ss].copy_from_slice(&c[i * ss..(i + 1) * ss]);
            }
            out
        }
        s => panic!("unexpected child state {s}"),
    };

    println!(
        "chain-composed hash    = {:016x}",
        compute_block_checksum(&composed)
    );

    // Where do composed and parent differ?
    let ss = child.sector_size as usize;
    let diff: Vec<usize> = (0..sectors_per_block)
        .filter(|&i| composed[i * ss..(i + 1) * ss] != p_data[i * ss..(i + 1) * ss])
        .collect();
    println!(
        "sectors differing from parent: {:?}",
        compress_ranges(&diff)
    );

    for &i in diff.iter().take(4) {
        let sec = &composed[i * ss..(i + 1) * ss];
        let nonzero = sec.iter().filter(|&&b| b != 0).count();
        println!(
            "  sector {i}: nonzero_bytes={nonzero} first16=[{}] sig@3={:?}",
            hex(&sec[..16]),
            std::str::from_utf8(&sec[3..7]).unwrap_or("----")
        );
    }

    // Tail structure of the composed block (last 4096 bytes).
    let tail = &composed[composed.len() - 4096..];
    let tail_nonzero = tail.iter().filter(|&&b| b != 0).count();
    println!(
        "composed tail4096: nonzero={tail_nonzero} last512_first16=[{}]",
        hex(&tail[3584..3600])
    );

    // Volume sector 0 (boot sector) from the parent chain for comparison.
    let (b_state, b_off) = parent.payload_entry(data_offset / MIB);
    if b_state == 6 {
        let blk = parent.read_block(b_off);
        let boot = &blk[..ss];
        println!(
            "volume sector0: first16=[{}] equals composed last sector: {}",
            hex(&boot[..16]),
            boot == &composed[composed.len() - ss..]
        );
        // NTFS BPB: BytesPerSector u16 @ 0x0B, TotalSectors u64 @ 0x28.
        // The backup boot sector lives AT TotalSectors * BytesPerSector — the
        // first sector past the filesystem end (the volume/partition is a few
        // sectors longer than the filesystem).
        let bps = u16::from_le_bytes(boot[0x0B..0x0D].try_into().unwrap()) as u64;
        let total_sectors = u64le(&boot[0x28..0x30]);
        let backup_boot_offset = total_sectors * bps;
        println!(
            "NTFS BPB: bytes_per_sector={bps} total_sectors={total_sectors} \
             → backup boot @ volume offset {backup_boot_offset} \
             (block {} sector {})",
            backup_boot_offset / MIB,
            (backup_boot_offset % MIB) / bps,
        );
    }
}

fn compress_ranges(idx: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &i in idx {
        match out.last_mut() {
            Some(last) if last.1 + 1 == i => last.1 = i,
            _ => out.push((i, i)),
        }
    }
    out
}
