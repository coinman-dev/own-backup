//! Pure-Rust reader/writer for dynamic **VHDX** images (no OS API), so Linux can
//! produce images that Windows mounts natively and read them back for verify /
//! restore. Implements the subset of MS-VHDX we need: one header, the region
//! table (BAT + metadata), the five required metadata items, and a dynamic BAT
//! whose payload blocks are all fully-present (free space simply has no block).
//!
//! Self-consistency (write → read → identical, absent blocks read as zero) is
//! unit-tested here; native Windows mounting is validated on a Windows host.
//!
//! All multi-byte integers are little-endian; GUIDs use the Windows/VHDX mixed-
//! endian layout (`Uuid::to_bytes_le`). Header/region-table checksums are
//! CRC-32C (Castagnoli).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use uuid::Uuid;

use own_backup_common::{OwnBackupError, Result};

const MB: u64 = 1024 * 1024;

// Fixed offsets of the header section (first 1 MiB).
const HEADER1_OFF: u64 = 64 * 1024;
const REGION1_OFF: u64 = 192 * 1024;
const REGION2_OFF: u64 = 256 * 1024;
// Regions after the header section + reserved log.
const LOG_OFF: u64 = MB;
const LOG_LEN: u64 = MB;
const META_OFF: u64 = 2 * MB;
const META_LEN: u64 = MB;
const BAT_OFF: u64 = 3 * MB;
/// Metadata items are placed 64 KiB into the metadata region (spec minimum).
const META_ITEMS_OFF: u32 = 64 * 1024;

// Region / metadata-item GUIDs (as written by Uuid::to_bytes_le).
const BAT_GUID: &str = "2dc27766-f623-4200-9d64-115e9bfd4a08";
const META_GUID: &str = "8b7ca206-4790-4b9a-b8fe-575f050f886e";
const MI_FILE_PARAMS: &str = "caa16737-fa36-4d43-b3b6-33f0aa44e76b";
const MI_VDISK_SIZE: &str = "2fa54224-cd1b-4876-b211-5dbed83bf4b8";
const MI_VDISK_ID: &str = "beca12ab-b2e6-4523-93ef-c309e000c746";
const MI_LOG_SECTOR: &str = "8141bf1d-a96f-4709-ba47-f233a8faab5f";
const MI_PHYS_SECTOR: &str = "cda348c7-445d-4471-9cc9-e9885251c556";

const PAYLOAD_BLOCK_FULLY_PRESENT: u64 = 6;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Vhdx(format!("vhdx: {msg}"))
}

fn guid_le(s: &str) -> [u8; 16] {
    Uuid::parse_str(s)
        .expect("valid GUID literal")
        .to_bytes_le()
}

// --- CRC-32C (Castagnoli), reflected, init/xorout 0xFFFFFFFF ---
fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Number of payload blocks per sector-bitmap block.
fn chunk_ratio(block_size: u64, sector_size: u32) -> u64 {
    (1u64 << 23) * sector_size as u64 / block_size
}

/// BAT index of payload block `i` (sector-bitmap entries are interleaved every
/// `chunk_ratio` payload blocks).
fn bat_index(i: u64, chunk_ratio: u64) -> u64 {
    i + i / chunk_ratio
}

/// Total BAT entries for a dynamic disk.
fn total_bat_entries(data_blocks: u64, chunk_ratio: u64) -> u64 {
    if data_blocks == 0 {
        0
    } else {
        data_blocks + (data_blocks - 1) / chunk_ratio
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Writes a dynamic VHDX. Call [`Self::write_at`] with virtual-disk byte ranges
/// (blocks are allocated on first touch), then [`Self::finish`].
pub struct VhdxWriter {
    file: File,
    virtual_size: u64,
    block_size: u64,
    sector_size: u32,
    chunk_ratio: u64,
    /// Per BAT index: allocated file offset in bytes (0 = not allocated). Only
    /// payload indices are ever set; sector-bitmap indices stay 0.
    bat: Vec<u64>,
    /// Where the next allocated payload block lands (MiB-aligned).
    next_data_off: u64,
    /// Offset of the BAT region in the VHDX file.
    bat_off: u64,
    /// This file is a DIFFERENCING child, so an unallocated block does not read
    /// as zeros — it reads as the parent's data. Two shortcuts that are correct
    /// for a parentless image are silent data loss here, and both are guarded
    /// in [`Self::write_at`]: skipping an all-zero write, and leaving the rest
    /// of a block untouched when a write covers only part of it.
    has_parent: bool,
    /// The image as it read BEFORE this writer touched it (parent chain and
    /// all), opened lazily. Used to seed a freshly allocated block so the bytes
    /// the caller did not write keep coming from the parent.
    seed: Option<crate::vhdx_bat::VhdxGuestReader>,
    /// Own path — the seed reader opens the same file a second time.
    path: std::path::PathBuf,
}

impl VhdxWriter {
    /// Create a dynamic VHDX of `virtual_size` bytes. `block_size` (payload
    /// block, e.g. 2 MiB) must be a power-of-two multiple of 1 MiB.
    pub fn create(
        path: &Path,
        virtual_size: u64,
        block_size: u64,
        sector_size: u32,
    ) -> Result<Self> {
        if block_size < MB || block_size % MB != 0 || !block_size.is_power_of_two() {
            return Err(bad(format!(
                "block size {block_size} must be a power-of-two ≥ 1 MiB"
            )));
        }
        let chunk_ratio = chunk_ratio(block_size, sector_size);
        let data_blocks = virtual_size.div_ceil(block_size);
        let bat_entries = total_bat_entries(data_blocks, chunk_ratio);
        let bat_len = (bat_entries * 8).max(MB).div_ceil(MB) * MB;
        let data_start = BAT_OFF + bat_len;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| bad(format!("create {}: {e}", path.display())))?;
        file.set_len(data_start).map_err(bad)?;

        let mut w = Self {
            file,
            virtual_size,
            block_size,
            sector_size,
            chunk_ratio,
            bat: vec![0u64; bat_entries as usize],
            next_data_off: data_start,
            bat_off: BAT_OFF,
            // A freshly created image has no parent: absent blocks read as zeros.
            has_parent: false,
            seed: None,
            path: path.to_path_buf(),
        };
        w.write_file_id()?;
        w.write_header()?;
        w.write_region_table(bat_len)?;
        w.write_metadata()?;
        Ok(w)
    }

    /// Open an existing dynamic or differencing VHDX image (e.g. pre-created by
    /// `create_differencing_vhdx` or `create_dynamic_vhdx`) to write payload
    /// blocks in userspace without attaching to the Windows kernel.
    pub fn open_existing(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| bad(format!("open {}: {e}", path.display())))?;

        // Region table: locate BAT and metadata offsets.
        let mut region = vec![0u8; 64 * 1024];
        file.seek(SeekFrom::Start(REGION1_OFF)).map_err(bad)?;
        file.read_exact(&mut region).map_err(bad)?;
        if &region[0..4] != b"regi" {
            return Err(bad("no region table signature"));
        }
        let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
        let (mut bat_off, mut bat_len, mut meta_off) = (0u64, 0u64, 0u64);
        for slot in 0..entry_count.min(2047) {
            let base = 16 + slot * 32;
            let guid = &region[base..base + 16];
            let off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
            let len = u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
            if guid == guid_le(BAT_GUID) {
                bat_off = off;
                bat_len = len;
            } else if guid == guid_le(META_GUID) {
                meta_off = off;
            }
        }
        if bat_off == 0 || meta_off == 0 {
            return Err(bad("region table missing BAT or metadata"));
        }

        // Metadata: virtual disk size, block size, sector size.
        let mut meta = vec![0u8; META_LEN as usize];
        file.seek(SeekFrom::Start(meta_off)).map_err(bad)?;
        file.read_exact(&mut meta).map_err(bad)?;
        if &meta[0..8] != b"metadata" {
            return Err(bad("no metadata signature"));
        }
        let mcount = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
        let (mut vsize, mut bsize, mut ssize) = (0u64, 0u64, 512u32);
        let mut has_parent = false;
        for slot in 0..mcount.min(2046) {
            let e = 32 + slot * 32;
            let guid = &meta[e..e + 16];
            let ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
            let read_u32 = |m: &[u8], o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
            let read_u64 = |m: &[u8], o: usize| u64::from_le_bytes(m[o..o + 8].try_into().unwrap());
            if guid == guid_le(MI_VDISK_SIZE) {
                vsize = read_u64(&meta, ioff);
            } else if guid == guid_le(MI_FILE_PARAMS) {
                bsize = read_u32(&meta, ioff) as u64;
                // FileParameters: bit 0 leave-blocks-allocated, bit 1 has-parent.
                has_parent = read_u32(&meta, ioff + 4) & 2 != 0;
            } else if guid == guid_le(MI_LOG_SECTOR) {
                ssize = read_u32(&meta, ioff);
            }
        }
        if vsize == 0 || bsize == 0 {
            return Err(bad("metadata missing virtual/block size"));
        }

        let chunk_ratio = chunk_ratio(bsize, ssize);
        let entries = (bat_len / 8) as usize;
        let mut raw = vec![0u8; entries * 8];
        file.seek(SeekFrom::Start(bat_off)).map_err(bad)?;
        file.read_exact(&mut raw).map_err(bad)?;
        let bat: Vec<u64> = raw
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();

        // Determine where new payload blocks will be appended.
        let mut max_off = bat_off + bat_len;
        for &entry in &bat {
            if entry & 7 == PAYLOAD_BLOCK_FULLY_PRESENT {
                let file_off = (entry >> 20) * MB;
                if file_off + bsize > max_off {
                    max_off = file_off + bsize;
                }
            }
        }
        let file_len = file.metadata().map_err(bad)?.len();
        let next_data_off = max_off.max(file_len);

        Ok(Self {
            file,
            virtual_size: vsize,
            block_size: bsize,
            sector_size: ssize,
            chunk_ratio,
            bat,
            next_data_off,
            bat_off,
            has_parent,
            seed: None,
            path: path.to_path_buf(),
        })
    }

    pub fn length(&self) -> u64 {
        self.virtual_size
    }

    fn at(&mut self, off: u64, data: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(off)).map_err(bad)?;
        self.file.write_all(data).map_err(bad)
    }

    fn write_file_id(&mut self) -> Result<()> {
        let mut b = vec![0u8; 64 * 1024];
        b[..8].copy_from_slice(b"vhdxfile");
        // Creator: UTF-16LE, 512 bytes at offset 8.
        for (i, u) in "OwnBackup".encode_utf16().enumerate() {
            b[8 + i * 2..8 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        self.at(0, &b)
    }

    fn write_header(&mut self) -> Result<()> {
        let mut h = vec![0u8; 4096];
        h[0..4].copy_from_slice(b"head");
        // 4..8 checksum (filled below)
        h[8..16].copy_from_slice(&1u64.to_le_bytes()); // SequenceNumber
        h[16..32].copy_from_slice(&Uuid::new_v4().to_bytes_le()); // FileWriteGuid
        h[32..48].copy_from_slice(&Uuid::new_v4().to_bytes_le()); // DataWriteGuid
                                                                  // 48..64 LogGuid = 0 (empty log)
                                                                  // LogVersion(64..66)=0, Version(66..68)=1
        h[66..68].copy_from_slice(&1u16.to_le_bytes());
        h[68..72].copy_from_slice(&(LOG_LEN as u32).to_le_bytes()); // LogLength
        h[72..80].copy_from_slice(&LOG_OFF.to_le_bytes()); // LogOffset
        let crc = crc32c(&h);
        h[4..8].copy_from_slice(&crc.to_le_bytes());
        // Two copies for resiliency (identical is fine for a fresh file).
        self.at(HEADER1_OFF, &h)?;
        self.at(HEADER1_OFF + 64 * 1024, &h)
    }

    fn write_region_table(&mut self, bat_len: u64) -> Result<()> {
        let mut r = vec![0u8; 64 * 1024];
        r[0..4].copy_from_slice(b"regi");
        // 4..8 checksum
        r[8..12].copy_from_slice(&2u32.to_le_bytes()); // EntryCount
        let mut entry = |slot: usize, guid: &str, off: u64, len: u64| {
            let base = 16 + slot * 32;
            r[base..base + 16].copy_from_slice(&guid_le(guid));
            r[base + 16..base + 24].copy_from_slice(&off.to_le_bytes());
            r[base + 24..base + 28].copy_from_slice(&(len as u32).to_le_bytes());
            r[base + 28..base + 32].copy_from_slice(&1u32.to_le_bytes()); // Required
        };
        entry(0, BAT_GUID, BAT_OFF, bat_len);
        entry(1, META_GUID, META_OFF, META_LEN);
        let crc = crc32c(&r);
        r[4..8].copy_from_slice(&crc.to_le_bytes());
        self.at(REGION1_OFF, &r)?;
        self.at(REGION2_OFF, &r)
    }

    fn write_metadata(&mut self) -> Result<()> {
        let mut m = vec![0u8; META_LEN as usize];
        // Metadata table header.
        m[0..8].copy_from_slice(b"metadata");
        m[10..12].copy_from_slice(&5u16.to_le_bytes()); // EntryCount

        // Item payloads, placed 64 KiB in.
        let items_off = META_ITEMS_OFF as usize;
        let mut cur = items_off;
        let push_item =
            |m: &mut [u8], slot: usize, guid: &str, flags: u32, payload: &[u8], cur: &mut usize| {
                let e = 32 + slot * 32; // entry in the table
                m[e..e + 16].copy_from_slice(&guid_le(guid));
                m[e + 16..e + 20].copy_from_slice(&(*cur as u32).to_le_bytes());
                m[e + 20..e + 24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
                m[e + 24..e + 28].copy_from_slice(&flags.to_le_bytes());
                m[*cur..*cur + payload.len()].copy_from_slice(payload);
                *cur += payload.len();
            };

        // File Parameters: BlockSize + flags(0 = not leave-allocated, no parent).
        let mut fp = [0u8; 8];
        fp[0..4].copy_from_slice(&(self.block_size as u32).to_le_bytes());
        push_item(&mut m, 0, MI_FILE_PARAMS, 0x4, &fp, &mut cur); // IsRequired
                                                                  // Virtual Disk Size.
        push_item(
            &mut m,
            1,
            MI_VDISK_SIZE,
            0x6,
            &self.virtual_size.to_le_bytes(),
            &mut cur,
        );
        // Virtual Disk ID.
        push_item(
            &mut m,
            2,
            MI_VDISK_ID,
            0x6,
            &Uuid::new_v4().to_bytes_le(),
            &mut cur,
        );
        // Logical + Physical sector size.
        push_item(
            &mut m,
            3,
            MI_LOG_SECTOR,
            0x6,
            &self.sector_size.to_le_bytes(),
            &mut cur,
        );
        let phys = std::cmp::max(self.sector_size, 4096);
        push_item(
            &mut m,
            4,
            MI_PHYS_SECTOR,
            0x6,
            &phys.to_le_bytes(),
            &mut cur,
        );

        self.at(META_OFF, &m)
    }

    /// Write `data` at virtual-disk byte `offset`, allocating payload blocks on
    /// first touch.
    pub fn write_at(&mut self, offset: u64, mut data: &[u8]) -> Result<()> {
        let mut off = offset;
        while !data.is_empty() {
            let block = off / self.block_size;
            let in_block = off % self.block_size;
            let take = std::cmp::min(self.block_size - in_block, data.len() as u64) as usize;

            let bi = bat_index(block, self.chunk_ratio) as usize;
            if bi >= self.bat.len() {
                self.bat.resize(bi + 1, 0);
            }
            let entry = self.bat[bi];
            let is_allocated =
                (entry & 7) == PAYLOAD_BLOCK_FULLY_PRESENT || (entry != 0 && (entry & 7) == 0);

            // Zeros into a block nobody has touched cost a megabyte of file for
            // nothing: an unallocated block already reads as zeros. This is what
            // keeps excluded-file "ghosts" free — their blocks stay in the used
            // map (the filesystem still reads them) but carry no data.
            //
            // ONLY for a parentless image. In a differencing child an
            // unallocated block reads as the PARENT's data, so dropping a write
            // of zeros would leave the previous version's bytes showing through
            // — a region emptied on the source since the last backup would come
            // back from the dead on restore.
            if !is_allocated && !self.has_parent && data[..take].iter().all(|&b| b == 0) {
                off += take as u64;
                data = &data[take..];
                continue;
            }

            let file_off = if is_allocated {
                if (entry & 7) == PAYLOAD_BLOCK_FULLY_PRESENT {
                    (entry >> 20) * MB
                } else {
                    entry
                }
            } else {
                let alloc_off = self.next_data_off;
                self.next_data_off += self.block_size;
                self.file.set_len(self.next_data_off).map_err(bad)?;
                // A newly allocated block is written whole (FULLY_PRESENT), so
                // the parent stops showing through it — every byte of it must
                // therefore be filled, not just the part this call carries.
                // Seed it with what the image reads right now: the parent's
                // block, or the parent/child merge of a partially-present one.
                // The on-disk BAT still says "absent" (we only write the BAT in
                // finish), which is exactly the pre-write state wanted here.
                let block_start = block * self.block_size;
                let block_len = (self.virtual_size - block_start).min(self.block_size);
                if self.has_parent && (in_block != 0 || take as u64 != block_len) {
                    let mut seed = vec![0u8; block_len as usize];
                    {
                        if self.seed.is_none() {
                            let path = self.path.clone();
                            self.seed = Some(crate::vhdx_bat::VhdxGuestReader::open(&path)?);
                        }
                        self.seed
                            .as_mut()
                            .unwrap()
                            .read_at(block_start, &mut seed)?;
                    }
                    self.at(alloc_off, &seed)?;
                }
                self.bat[bi] = alloc_off;
                alloc_off
            };

            self.at(file_off + in_block, &data[..take])?;
            off += take as u64;
            data = &data[take..];
        }
        Ok(())
    }

    /// Write the BAT and flush. Consumes the writer.
    pub fn finish(mut self) -> Result<()> {
        let mut bat = vec![0u8; self.bat.len() * 8];
        for (i, &entry) in self.bat.iter().enumerate() {
            let encoded = if (entry & 7) == PAYLOAD_BLOCK_FULLY_PRESENT {
                entry
            } else if entry != 0 {
                ((entry / MB) << 20) | PAYLOAD_BLOCK_FULLY_PRESENT
            } else {
                0
            };
            bat[i * 8..i * 8 + 8].copy_from_slice(&encoded.to_le_bytes());
        }
        self.at(self.bat_off, &bat)?;
        self.file.sync_all().map_err(bad)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Reads a dynamic VHDX produced by [`VhdxWriter`] (and, best-effort, other
/// dynamic VHDX files): maps virtual-disk offsets to file offsets via the BAT.
pub struct VhdxReader {
    file: File,
    virtual_size: u64,
    block_size: u64,
    chunk_ratio: u64,
    bat: Vec<u64>,
}

impl VhdxReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file =
            File::open(path).map_err(|e| bad(format!("open {}: {e}", path.display())))?;

        // Region table: find BAT + metadata offsets.
        let mut region = vec![0u8; 64 * 1024];
        file.seek(SeekFrom::Start(REGION1_OFF)).map_err(bad)?;
        file.read_exact(&mut region).map_err(bad)?;
        if &region[0..4] != b"regi" {
            return Err(bad("no region table signature"));
        }
        let entry_count = u32::from_le_bytes(region[8..12].try_into().unwrap()) as usize;
        let (mut bat_off, mut bat_len, mut meta_off) = (0u64, 0u64, 0u64);
        for slot in 0..entry_count.min(2047) {
            let base = 16 + slot * 32;
            let guid = &region[base..base + 16];
            let off = u64::from_le_bytes(region[base + 16..base + 24].try_into().unwrap());
            let len = u32::from_le_bytes(region[base + 24..base + 28].try_into().unwrap()) as u64;
            if guid == guid_le(BAT_GUID) {
                bat_off = off;
                bat_len = len;
            } else if guid == guid_le(META_GUID) {
                meta_off = off;
            }
        }
        if bat_off == 0 || meta_off == 0 {
            return Err(bad("region table missing BAT or metadata"));
        }

        // Metadata: virtual size, block size, sector size.
        let mut meta = vec![0u8; META_LEN as usize];
        file.seek(SeekFrom::Start(meta_off)).map_err(bad)?;
        file.read_exact(&mut meta).map_err(bad)?;
        if &meta[0..8] != b"metadata" {
            return Err(bad("no metadata signature"));
        }
        let mcount = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
        let (mut vsize, mut bsize, mut ssize) = (0u64, 0u64, 512u32);
        for slot in 0..mcount.min(2046) {
            let e = 32 + slot * 32;
            let guid = &meta[e..e + 16];
            let ioff = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
            let read_u32 = |m: &[u8], o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
            let read_u64 = |m: &[u8], o: usize| u64::from_le_bytes(m[o..o + 8].try_into().unwrap());
            if guid == guid_le(MI_VDISK_SIZE) {
                vsize = read_u64(&meta, ioff);
            } else if guid == guid_le(MI_FILE_PARAMS) {
                bsize = read_u32(&meta, ioff) as u64;
            } else if guid == guid_le(MI_LOG_SECTOR) {
                ssize = read_u32(&meta, ioff);
            }
        }
        if vsize == 0 || bsize == 0 {
            return Err(bad("metadata missing virtual/block size"));
        }

        let chunk_ratio = chunk_ratio(bsize, ssize);
        let entries = (bat_len / 8) as usize;
        let mut raw = vec![0u8; entries * 8];
        file.seek(SeekFrom::Start(bat_off)).map_err(bad)?;
        file.read_exact(&mut raw).map_err(bad)?;
        let bat = raw
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();

        Ok(Self {
            file,
            virtual_size: vsize,
            block_size: bsize,
            chunk_ratio,
            bat,
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    /// Read `buf.len()` bytes at virtual-disk `offset`; regions with no allocated
    /// block read as zero.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.virtual_size {
            return Err(bad("read past virtual disk end"));
        }
        let mut done = 0usize;
        let mut off = offset;
        while done < buf.len() {
            let block = off / self.block_size;
            let in_block = off % self.block_size;
            let take =
                std::cmp::min(self.block_size - in_block, (buf.len() - done) as u64) as usize;

            let bi = bat_index(block, self.chunk_ratio) as usize;
            let entry = self.bat.get(bi).copied().unwrap_or(0);
            if entry & 7 == PAYLOAD_BLOCK_FULLY_PRESENT {
                let file_off = (entry >> 20) * MB + in_block;
                self.file.seek(SeekFrom::Start(file_off)).map_err(bad)?;
                self.file
                    .read_exact(&mut buf[done..done + take])
                    .map_err(bad)?;
            } else {
                buf[done..done + take].fill(0); // absent block = zeros
            }
            done += take;
            off += take as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn write_read_round_trip_with_sparsity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.vhdx");
        let block = 2 * MB;
        let vsize = 10 * MB; // 5 payload blocks

        let mut w = VhdxWriter::create(&path, vsize, block, 512).unwrap();
        // Block 0 (partial), block 2, and a write spanning blocks 3→4.
        let a: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        w.write_at(0, &a).unwrap();
        let b: Vec<u8> = (0..1000u32).map(|i| (i * 3) as u8).collect();
        w.write_at(2 * block + 100, &b).unwrap();
        let span: Vec<u8> = (0..8192u32).map(|i| (i * 7 + 1) as u8).collect();
        w.write_at(4 * block - 4096, &span).unwrap();
        w.finish().unwrap();

        let mut r = VhdxReader::open(&path).unwrap();
        assert_eq!(r.virtual_size(), vsize);

        let mut buf = vec![0u8; a.len()];
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, a);

        let mut buf = vec![0u8; b.len()];
        r.read_at(2 * block + 100, &mut buf).unwrap();
        assert_eq!(buf, b);

        let mut buf = vec![0u8; span.len()];
        r.read_at(4 * block - 4096, &mut buf).unwrap();
        assert_eq!(buf, span);

        // Block 1 was never written → reads back as zeros.
        let mut buf = vec![0u8; 8192];
        r.read_at(block, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&x| x == 0),
            "unallocated block must read zero"
        );

        // The file is sparse: allocated blocks (3) ≪ virtual size.
        let actual = std::fs::metadata(&path).unwrap().len();
        assert!(actual < vsize + BAT_OFF + 4 * block);
    }

    #[test]
    fn open_existing_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t2.vhdx");
        let block = 2 * MB;
        let vsize = 10 * MB;

        let mut w = VhdxWriter::create(&path, vsize, block, 512).unwrap();
        let data1 = vec![0x11u8; 4096];
        w.write_at(0, &data1).unwrap();
        w.finish().unwrap();

        // Re-open existing VHDX and write to block 1
        let mut w2 = VhdxWriter::open_existing(&path).unwrap();
        assert_eq!(w2.length(), vsize);
        let data2 = vec![0x22u8; 4096];
        w2.write_at(block, &data2).unwrap();
        w2.finish().unwrap();

        let mut r = VhdxReader::open(&path).unwrap();
        let mut buf1 = vec![0u8; 4096];
        r.read_at(0, &mut buf1).unwrap();
        assert_eq!(buf1, data1);

        let mut buf2 = vec![0u8; 4096];
        r.read_at(block, &mut buf2).unwrap();
        assert_eq!(buf2, data2);
    }
}
