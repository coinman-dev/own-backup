//! Pure-Rust reader/writer for dynamic **VHDX** images (no OS API), so Linux can
//! produce images that Windows mounts natively and read them back for verify /
//! restore. Implements the subset of MS-VHDX we need: one header, the region
//! table (BAT + metadata), the five required metadata items, and a dynamic BAT
//! whose payload blocks are all fully-present (free space simply has no block).
//! A differencing child ([`VhdxWriter::create_differencing`]) adds the
//! HasParent flag and a parent locator; it still only ever writes whole blocks.
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
const MI_PARENT_LOCATOR: &str = "a8d35f2d-b30b-454d-abf7-d3d84834ab0c";
/// LocatorType of the VHDX parent locator (MS-VHDX §2.6.2.6) — the only type
/// defined, and the one Windows looks for.
const PARENT_LOCATOR_TYPE_VHDX: &str = "b04aefb7-d19e-4a81-b789-25b8e9445913";
/// Metadata table entry flags (MS-VHDX §2.6.1.2): bit 1 IsVirtualDisk, bit 2
/// IsRequired.
const META_REQUIRED: u32 = 0x4;
const META_VIRTUAL_DISK_REQUIRED: u32 = 0x6;

/// Payload block size of a differencing child — the 1 MiB that
/// `create_differencing_vhdx` asks Windows for (vhdx.rs), so a chain grown on
/// either system allocates the same way per touched block.
pub const CHILD_BLOCK_SIZE: u64 = MB;

const PAYLOAD_BLOCK_FULLY_PRESENT: u64 = 6;
/// The file-offset half of a BAT entry (bits 20..64, i.e. MiB-aligned bytes).
const BAT_OFFSET_MASK: u64 = 0xFFFF_FFFF_FFF0_0000;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Vhdx(format!("vhdx: {msg}"))
}

fn guid_le(s: &str) -> [u8; 16] {
    Uuid::parse_str(s)
        .expect("valid GUID literal")
        .to_bytes_le()
}

// CRC-32C (Castagnoli), reflected, init/xorout 0xFFFFFFFF — shared with the
// readers, which check it on every region table.
use crate::vhdx_bat::crc32c;

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

/// Total BAT entries for a differencing disk (MS-VHDX §2.5): every chunk,
/// the last one included, carries its sector-bitmap entry, because a child may
/// hold any block partially. Sizing a child's BAT like a dynamic disk's comes
/// up one entry per chunk short.
fn total_bat_entries_differencing(data_blocks: u64, chunk_ratio: u64) -> u64 {
    data_blocks.div_ceil(chunk_ratio) * (chunk_ratio + 1)
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
        let physical = std::cmp::max(sector_size, 4096);
        Self::create_inner(path, virtual_size, block_size, sector_size, physical, None)
    }

    /// Create a **differencing** VHDX at `path` over `parent`, in userspace —
    /// what `vhdx::create_differencing_vhdx` gets from `CreateVirtualDisk` on
    /// Windows, without the Windows API. The file must not exist yet (the
    /// engine creates children under a private name and publishes them without
    /// replacing anything; `CreateVirtualDisk` refuses an existing file too).
    ///
    /// The child copies the parent's virtual size and logical/physical sector
    /// sizes, uses [`CHILD_BLOCK_SIZE`] blocks, sets FileParameters.HasParent
    /// and carries a parent locator (see `parent_locator_for` for what it
    /// holds and what Windows needs from it). The returned writer knows the
    /// image has a parent, so zero writes are never skipped and a partially
    /// written block is seeded from the parent chain.
    pub fn create_differencing(path: &Path, parent: &Path) -> Result<Self> {
        let geometry = crate::vhdx_bat::vhdx_geometry(parent)?;
        let locator = parent_locator_for(path, parent)?;
        Self::create_inner(
            path,
            geometry.virtual_size,
            CHILD_BLOCK_SIZE,
            geometry.logical_sector_size,
            geometry.physical_sector_size,
            Some(&locator),
        )
    }

    fn create_inner(
        path: &Path,
        virtual_size: u64,
        block_size: u64,
        sector_size: u32,
        physical_sector_size: u32,
        parent_locator: Option<&[u8]>,
    ) -> Result<Self> {
        if block_size < MB || block_size % MB != 0 || !block_size.is_power_of_two() {
            return Err(bad(format!(
                "block size {block_size} must be a power-of-two ≥ 1 MiB"
            )));
        }
        let has_parent = parent_locator.is_some();
        let chunk_ratio = chunk_ratio(block_size, sector_size);
        let data_blocks = virtual_size.div_ceil(block_size);
        let bat_entries = if has_parent {
            total_bat_entries_differencing(data_blocks, chunk_ratio)
        } else {
            total_bat_entries(data_blocks, chunk_ratio)
        };
        let bat_len = (bat_entries * 8).max(MB).div_ceil(MB) * MB;
        let data_start = BAT_OFF + bat_len;

        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if has_parent {
            // A child is always a new file: never overwrite whatever has the name.
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let file = options
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
            // Without a parent, absent blocks read as zeros; with one, they
            // read as the parent's data.
            has_parent,
            seed: None,
            path: path.to_path_buf(),
        };
        w.write_file_id()?;
        w.write_header()?;
        w.write_region_table(bat_len)?;
        w.write_metadata(physical_sector_size, parent_locator)?;
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
        let region = crate::vhdx_bat::read_region_table(&mut file)?;
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
        // Payload blocks live at whole-MiB offsets (a BAT entry cannot say
        // anything else), so a file whose length is not a MiB multiple gets
        // its next block at the following MiB, not at its last byte.
        let next_data_off = max_off.max(file_len).div_ceil(MB) * MB;

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

    fn write_metadata(
        &mut self,
        physical_sector_size: u32,
        parent_locator: Option<&[u8]>,
    ) -> Result<()> {
        let mut m = vec![0u8; META_LEN as usize];
        // Metadata table header.
        m[0..8].copy_from_slice(b"metadata");
        let count: u16 = if parent_locator.is_some() { 6 } else { 5 };
        m[10..12].copy_from_slice(&count.to_le_bytes()); // EntryCount

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

        // File Parameters: BlockSize + flags (bit 0 leave-blocks-allocated, never
        // set here; bit 1 has-parent, set for a differencing child).
        let mut fp = [0u8; 8];
        fp[0..4].copy_from_slice(&(self.block_size as u32).to_le_bytes());
        let fp_flags: u32 = if parent_locator.is_some() { 2 } else { 0 };
        fp[4..8].copy_from_slice(&fp_flags.to_le_bytes());
        push_item(&mut m, 0, MI_FILE_PARAMS, META_REQUIRED, &fp, &mut cur);
        // Virtual Disk Size.
        push_item(
            &mut m,
            1,
            MI_VDISK_SIZE,
            META_VIRTUAL_DISK_REQUIRED,
            &self.virtual_size.to_le_bytes(),
            &mut cur,
        );
        // Virtual Disk ID.
        push_item(
            &mut m,
            2,
            MI_VDISK_ID,
            META_VIRTUAL_DISK_REQUIRED,
            &Uuid::new_v4().to_bytes_le(),
            &mut cur,
        );
        // Logical + Physical sector size.
        push_item(
            &mut m,
            3,
            MI_LOG_SECTOR,
            META_VIRTUAL_DISK_REQUIRED,
            &self.sector_size.to_le_bytes(),
            &mut cur,
        );
        push_item(
            &mut m,
            4,
            MI_PHYS_SECTOR,
            META_VIRTUAL_DISK_REQUIRED,
            &physical_sector_size.to_le_bytes(),
            &mut cur,
        );
        if let Some(locator) = parent_locator {
            if cur + locator.len() > m.len() {
                return Err(bad("parent locator does not fit the metadata region"));
            }
            // Parent Locator: IsRequired, not a property of the virtual disk.
            push_item(
                &mut m,
                5,
                MI_PARENT_LOCATOR,
                META_REQUIRED,
                locator,
                &mut cur,
            );
        }

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
            // `bat` holds on-disk BAT entries: what was read, plus blocks this
            // writer allocated (stored FULLY_PRESENT). Only a fully present
            // block can be written in place; every other state — absent, zero,
            // unmapped, or PARTIALLY present (state 7, whose sector bitmap says
            // some sectors still belong to the parent) — gets a new whole block
            // seeded with what the image reads there now.
            let entry = self.bat[bi];
            let is_allocated = (entry & 7) == PAYLOAD_BLOCK_FULLY_PRESENT;

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
                entry & BAT_OFFSET_MASK
            } else {
                let alloc_off = self.next_data_off;
                debug_assert_eq!(alloc_off % MB, 0, "payload blocks sit at whole MiB");
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
                self.bat[bi] = alloc_off | PAYLOAD_BLOCK_FULLY_PRESENT;
                alloc_off
            };

            self.at(file_off + in_block, &data[..take])?;
            off += take as u64;
            data = &data[take..];
        }
        Ok(())
    }

    /// Flush the payload written so far, before [`Self::finish`] writes the
    /// BAT that points at it. A caller that must survive a crash between the
    /// two (the userspace fold, `vhdx_chain`) needs the order; a backup does
    /// not — its unfinished image is never published.
    pub fn flush_payload(&mut self) -> Result<()> {
        self.file.sync_data().map_err(bad)
    }

    /// Write the BAT and flush. Consumes the writer.
    ///
    /// Entries this writer did not allocate go back exactly as they were read.
    /// (They used to be re-encoded as "fully present at offset/1 MiB", which
    /// turned an untouched PARTIALLY_PRESENT block into a whole block of the
    /// child — the parent's sectors in it then read as whatever the child file
    /// held there — and a ZERO/UNMAPPED entry into a block at file offset 0.)
    pub fn finish(mut self) -> Result<()> {
        let mut bat = vec![0u8; self.bat.len() * 8];
        for (i, &entry) in self.bat.iter().enumerate() {
            bat[i * 8..i * 8 + 8].copy_from_slice(&entry.to_le_bytes());
        }
        self.at(self.bat_off, &bat)?;
        self.file.sync_all().map_err(bad)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Parent locator
// ---------------------------------------------------------------------------

/// The parent locator item a differencing `child` carries to name `parent`
/// (MS-VHDX §2.6.2.6): LocatorType `{B04AEFB7-D19E-4A81-B789-25B8E9445913}`
/// followed by UTF-16LE key/value pairs.
///
/// What Windows needs to open the child as a chain:
/// - `parent_linkage` (required): the parent's current **DataWriteGuid**,
///   braced. The VHD stack compares it with the parent it finds and refuses
///   the chain on a mismatch (`parent_linkage2` is an optional second accepted
///   GUID; a fresh child has none).
/// - at least one path that leads to that parent: `relative_path` (relative
///   to the child's folder), `volume_path` or `absolute_win32_path`; a path
///   that does not resolve, or resolves to a file with another DataWriteGuid,
///   does not count. Our own readers prefer `relative_path` (see
///   `vhdx_bat::parse_parent_locator`). We always write `relative_path` in Windows
///   form — `.\name` for a parent in the same folder, which is how every
///   chain the engine makes is laid out (the child's private name and its
///   public one sit beside the parent), otherwise `..\`-style — and add
///   `absolute_win32_path` (`\\?\C:\…`) only where one can be formed, i.e. on
///   Windows. A Linux absolute path is meaningless to Windows, so it is never
///   written; a folder moved between systems keeps working through the
///   relative path alone. `volume_path` (`\\?\Volume{GUID}\…`) is not
///   written: only Windows knows volume GUIDs.
pub(crate) fn parent_locator_for(child: &Path, parent: &Path) -> Result<Vec<u8>> {
    let linkage = crate::vhdx_bat::vhdx_data_write_guid(parent)?;
    let parent_abs = parent
        .canonicalize()
        .map_err(|e| bad(format!("parent {}: {e}", parent.display())))?;
    let child_dir = match child.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let child_dir = child_dir
        .canonicalize()
        .map_err(|e| bad(format!("folder of {}: {e}", child.display())))?;

    let relative = relative_windows_path(&child_dir, &parent_abs);
    #[cfg(target_os = "windows")]
    let absolute = parent_abs.to_str().map(str::to_string);
    #[cfg(not(target_os = "windows"))]
    let absolute: Option<String> = None;
    if relative.is_none() && absolute.is_none() {
        return Err(bad(format!(
            "cannot express the path of {} from {} in a VHDX parent locator",
            parent.display(),
            child.display()
        )));
    }

    let mut pairs: Vec<(&str, String)> = vec![("parent_linkage", format!("{{{linkage}}}"))];
    if let Some(relative) = relative {
        pairs.push(("relative_path", relative));
    }
    if let Some(absolute) = absolute {
        pairs.push(("absolute_win32_path", absolute));
    }
    encode_parent_locator(&pairs)
}

/// `to` relative to the directory `from_dir`, both canonical, in Windows form
/// (`.\a\b`, `..\a\b`). `None` when there is no relative way (another drive)
/// or a name cannot be written in a Windows path.
fn relative_windows_path(from_dir: &Path, to: &Path) -> Option<String> {
    use std::path::Component;
    let from: Vec<Component> = from_dir.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    // The anchor (drive prefix, root) must be shared.
    let anchor = |c: &[Component]| {
        c.iter()
            .take_while(|c| !matches!(c, Component::Normal(_)))
            .count()
    };
    if common == 0 || common < anchor(&from) || common < anchor(&to) {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for c in &from[common..] {
        match c {
            Component::Normal(_) => parts.push("..".into()),
            _ => return None,
        }
    }
    for c in &to[common..] {
        match c {
            Component::Normal(name) => {
                let name = name.to_str()?;
                if name.contains('\\') {
                    return None;
                }
                parts.push(name.to_string());
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\\");
    Some(if parts[0] == ".." {
        joined
    } else {
        format!(".\\{joined}")
    })
}

/// Serialize a parent locator: 20-byte header, 12-byte entries, then the
/// UTF-16LE strings; offsets are relative to the locator's start.
fn encode_parent_locator(pairs: &[(&str, String)]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; 20 + 12 * pairs.len()];
    out[0..16].copy_from_slice(&guid_le(PARENT_LOCATOR_TYPE_VHDX));
    // 16..18 reserved.
    out[18..20].copy_from_slice(&(pairs.len() as u16).to_le_bytes());
    for (i, (key, value)) in pairs.iter().enumerate() {
        let k: Vec<u8> = key.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let v: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let (Ok(k_len), Ok(v_len)) = (u16::try_from(k.len()), u16::try_from(v.len())) else {
            return Err(bad(format!("parent locator entry {key} is too long")));
        };
        let k_off = out.len() as u32;
        out.extend_from_slice(&k);
        let v_off = out.len() as u32;
        out.extend_from_slice(&v);
        let e = 20 + i * 12;
        out[e..e + 4].copy_from_slice(&k_off.to_le_bytes());
        out[e + 4..e + 8].copy_from_slice(&v_off.to_le_bytes());
        out[e + 8..e + 10].copy_from_slice(&k_len.to_le_bytes());
        out[e + 10..e + 12].copy_from_slice(&v_len.to_le_bytes());
    }
    Ok(out)
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
        let region = crate::vhdx_bat::read_region_table(&mut file)?;
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

    // --- Differencing children made in userspace -------------------------

    use crate::vhdx_bat::{
        allocated_payload_blocks, vhdx_geometry, vhdx_parent_linkage_matches, vhdx_parent_path,
        VhdxGuestReader,
    };

    /// A parent the way the Linux engine makes one: 2 MiB blocks, a recognisable
    /// byte pattern in blocks 0 and 2 (of four), the rest never written.
    fn pattern_parent(path: &Path, sector: u32) -> u64 {
        let vsize = 8 * MB;
        let mut w = VhdxWriter::create(path, vsize, 2 * MB, sector).unwrap();
        let fill: Vec<u8> = (0..2 * MB).map(|i| (i % 251) as u8 + 1).collect();
        w.write_at(0, &fill).unwrap();
        w.write_at(4 * MB, &fill).unwrap();
        w.finish().unwrap();
        vsize
    }

    fn read_all(path: &Path) -> Vec<u8> {
        let mut r = VhdxGuestReader::open(path).unwrap();
        let mut buf = vec![0u8; r.virtual_size() as usize];
        r.read_at(0, &mut buf).unwrap();
        buf
    }

    /// The BAT of a file we created, raw.
    fn raw_bat(path: &Path) -> Vec<u64> {
        let mut f = File::open(path).unwrap();
        let mut raw = vec![0u8; MB as usize];
        f.seek(SeekFrom::Start(BAT_OFF)).unwrap();
        f.read_exact(&mut raw).unwrap();
        raw.chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn a_new_child_reads_exactly_as_its_parent() {
        for sector in [512u32, 4096] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("p.vhdx");
            let child = dir.path().join("c.avhdx");
            let vsize = pattern_parent(&parent, sector);

            let w = VhdxWriter::create_differencing(&child, &parent).unwrap();
            // Readable before the writer is even finished...
            assert_eq!(read_all(&child), read_all(&parent));
            w.finish().unwrap();
            // ...and after.
            assert_eq!(read_all(&child), read_all(&parent));

            let g = vhdx_geometry(&child).unwrap();
            let pg = vhdx_geometry(&parent).unwrap();
            assert!(g.has_parent && !pg.has_parent);
            assert_eq!(g.virtual_size, vsize);
            assert_eq!(g.block_size, CHILD_BLOCK_SIZE);
            assert_eq!(g.logical_sector_size, pg.logical_sector_size);
            assert_eq!(g.physical_sector_size, pg.physical_sector_size);
            assert!(allocated_payload_blocks(&child).unwrap().blocks.is_empty());
        }
    }

    /// Zeros written into a child must be ALLOCATED: skipping them would let the
    /// parent's bytes show through — a region emptied since the last backup
    /// would come back on restore.
    #[test]
    fn a_zero_write_to_a_child_is_allocated_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        pattern_parent(&parent, 512);

        let mut w = VhdxWriter::create_differencing(&child, &parent).unwrap();
        w.write_at(MB, &vec![0u8; MB as usize]).unwrap();
        w.finish().unwrap();

        assert_eq!(allocated_payload_blocks(&child).unwrap().blocks, vec![1]);
        let all = read_all(&child);
        assert!(all[MB as usize..2 * MB as usize].iter().all(|&b| b == 0));
        // The rest still comes from the parent.
        let parent_all = read_all(&parent);
        assert_eq!(all[..MB as usize], parent_all[..MB as usize]);
        assert_eq!(all[2 * MB as usize..], parent_all[2 * MB as usize..]);
    }

    /// A partial write fills the rest of its block from the parent, and the
    /// block is written whole: we never leave a PARTIALLY_PRESENT entry behind.
    #[test]
    fn a_partial_write_to_a_child_is_seeded_from_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        pattern_parent(&parent, 512);

        let mut w = VhdxWriter::create_differencing(&child, &parent).unwrap();
        w.write_at(4 * MB + 1000, &[0xEEu8; 3000]).unwrap();
        w.write_at(7 * MB - 10, &[0xDDu8; 10]).unwrap(); // over an unwritten parent block
        w.finish().unwrap();

        let mut expected = read_all(&parent);
        expected[(4 * MB + 1000) as usize..(4 * MB + 4000) as usize].fill(0xEE);
        expected[(7 * MB - 10) as usize..(7 * MB) as usize].fill(0xDD);
        assert_eq!(read_all(&child), expected);

        assert_eq!(allocated_payload_blocks(&child).unwrap().blocks, vec![4, 6]);
        for entry in raw_bat(&child) {
            let state = entry & 7;
            assert!(
                state == 0 || state == 6,
                "entry {entry:#x} has state {state}"
            );
        }
    }

    #[test]
    fn a_child_is_linked_to_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        pattern_parent(&parent, 512);
        VhdxWriter::create_differencing(&child, &parent)
            .unwrap()
            .finish()
            .unwrap();

        assert!(vhdx_parent_linkage_matches(&child, &parent).unwrap());
        let found = vhdx_parent_path(&child).unwrap().expect("a parent");
        assert_eq!(
            found.canonicalize().unwrap(),
            parent.canonicalize().unwrap()
        );
        // A grandchild links to the child, not to the base.
        let grandchild = dir.path().join("g.avhdx");
        VhdxWriter::create_differencing(&grandchild, &child)
            .unwrap()
            .finish()
            .unwrap();
        assert!(vhdx_parent_linkage_matches(&grandchild, &child).unwrap());
        assert_eq!(read_all(&grandchild), read_all(&parent));
    }

    /// The parent in another folder: the locator carries a `..\` path that
    /// still resolves on this system.
    #[test]
    fn a_parent_in_another_folder_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("full")).unwrap();
        std::fs::create_dir_all(dir.path().join("inc/deeper")).unwrap();
        let parent = dir.path().join("full/p.vhdx");
        let child = dir.path().join("inc/deeper/c.avhdx");
        pattern_parent(&parent, 512);
        VhdxWriter::create_differencing(&child, &parent)
            .unwrap()
            .finish()
            .unwrap();
        let found = vhdx_parent_path(&child).unwrap().expect("a parent");
        assert_eq!(
            found.canonicalize().unwrap(),
            parent.canonicalize().unwrap()
        );
        assert_eq!(read_all(&child), read_all(&parent));
    }

    #[test]
    fn relative_windows_paths() {
        let p = |s: &str| std::path::PathBuf::from(s);
        assert_eq!(
            relative_windows_path(&p("/b/x"), &p("/b/x/p.vhdx")).as_deref(),
            Some(r".\p.vhdx")
        );
        assert_eq!(
            relative_windows_path(&p("/b/x/y"), &p("/b/z/p.vhdx")).as_deref(),
            Some(r"..\..\z\p.vhdx")
        );
        assert_eq!(
            relative_windows_path(&p("/b"), &p("/b/z/p.vhdx")).as_deref(),
            Some(r".\z\p.vhdx")
        );
        #[cfg(unix)]
        assert_eq!(relative_windows_path(&p("/b"), &p(r"/b/a\b.vhdx")), None);
    }

    /// A child is a new file, as `CreateVirtualDisk` makes it: an existing one
    /// is never truncated.
    #[test]
    fn create_differencing_refuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        pattern_parent(&parent, 512);
        std::fs::write(&child, b"somebody else's").unwrap();
        assert!(VhdxWriter::create_differencing(&child, &parent).is_err());
        assert_eq!(std::fs::read(&child).unwrap(), b"somebody else's");
    }

    /// A differencing BAT has a sector-bitmap entry for every chunk, the last
    /// one included (MS-VHDX §2.5) — sized like a dynamic BAT it is one entry
    /// per chunk short.
    #[test]
    fn a_child_bat_has_a_bitmap_entry_per_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        // 4096 one-MiB blocks = exactly one chunk at 512-byte sectors.
        let w = VhdxWriter::create(&parent, 4096 * MB, 2 * MB, 512).unwrap();
        w.finish().unwrap();
        let w = VhdxWriter::create_differencing(&child, &parent).unwrap();
        assert_eq!(w.bat.len(), 4097);
        w.finish().unwrap();
    }

    /// Entries the writer did not allocate are written back as they were read.
    /// A PARTIALLY_PRESENT block (state 7, written by Windows) used to come
    /// back from `finish` as FULLY_PRESENT — the parent's sectors in it then
    /// read as whatever the child file held — and a ZERO entry as a block at
    /// file offset 0.
    #[test]
    fn finish_keeps_entries_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("p.vhdx");
        let child = dir.path().join("c.avhdx");
        pattern_parent(&parent, 512);
        VhdxWriter::create_differencing(&child, &parent)
            .unwrap()
            .finish()
            .unwrap();

        // Hand-made state: block 0 partially present (sectors 3..5 ours), its
        // chunk's bitmap present, block 2 ZERO.
        let len = std::fs::metadata(&child).unwrap().len();
        let (data_off, bitmap_off) = (len, len + MB);
        let mut f = OpenOptions::new().write(true).open(&child).unwrap();
        f.set_len(len + 2 * MB).unwrap();
        f.seek(SeekFrom::Start(data_off + 3 * 512)).unwrap();
        f.write_all(&[0x77u8; 3 * 512]).unwrap();
        f.seek(SeekFrom::Start(bitmap_off)).unwrap();
        f.write_all(&[0b0011_1000u8]).unwrap();
        let chunk = chunk_ratio(MB, 512);
        for (index, entry) in [(0u64, data_off | 7), (chunk, bitmap_off | 6), (2, 2)] {
            f.seek(SeekFrom::Start(BAT_OFF + index * 8)).unwrap();
            f.write_all(&entry.to_le_bytes()).unwrap();
        }
        drop(f);
        let before = raw_bat(&child);
        let reads_before = read_all(&child);

        let mut w = VhdxWriter::open_existing(&child).unwrap();
        w.write_at(5 * MB, &[0x55u8; 4096]).unwrap();
        w.finish().unwrap();

        let after = raw_bat(&child);
        for index in [0usize, 2, chunk as usize] {
            assert_eq!(after[index], before[index], "BAT entry {index} changed");
        }
        let mut expected = reads_before;
        expected[5 * MB as usize..5 * MB as usize + 4096].fill(0x55);
        assert_eq!(read_all(&child), expected);
    }
}
