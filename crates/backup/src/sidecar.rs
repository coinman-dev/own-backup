//! Sidecar manifest (`.obkm`) written next to each VHDX backup image.
//!
//! VHDX has no per-block checksums and no room for our metadata, so every
//! image gets a sidecar file carrying:
//! - the JSON manifest (source disk layout, job parameters, chain links) —
//!   reuses [`ImageManifest`], which outlived the retired `.obk` container;
//! - xxHash64 checksums of every backed-up block, for `verify`.
//!
//! File layout (all integers little-endian):
//! ```text
//! magic "OBKMSC01" (8 bytes)
//! u32   JSON length
//! [..]  JSON: { "info": SidecarInfo, "manifest": ImageManifest }
//! u64   checksum entry count
//! [..]  entries: (u64 block_number, u64 xxhash64) × count
//! ```

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use own_backup_common::disk::DiskInfo;
use own_backup_common::image::ImageManifest;
use own_backup_common::{OwnBackupError, Result};

pub const SIDECAR_MAGIC: &[u8; 8] = b"OBKMSC01";
pub const SIDECAR_MAGIC_V2: &[u8; 8] = b"OBKMSC02";
pub const SIDECAR_EXTENSION: &str = "obkm";

/// Role of an image inside a backup chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainRole {
    /// Full backup — a self-contained dynamic VHDX.
    Full,
    /// Incremental — differencing VHDX whose parent is the previous version.
    Incremental,
    /// Differential — differencing VHDX whose parent is the full backup.
    Differential,
}

/// Container-level info that has no place in the (obk-era) [`ImageManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarInfo {
    /// Sidecar format version (bump on layout changes).
    pub format_version: u32,
    /// Version of Own Backup that wrote this image (`CARGO_PKG_VERSION`, e.g.
    /// "0.1.0").
    ///
    /// Not decoration. A copy outlives the build that made it, and when one
    /// turns out to be damaged the first question is *which build produced it*
    /// — as it was for the btrfs exclusions that zeroed live metadata: the
    /// difference between a good copy and a corrupt one was the build, and
    /// nothing in the file said so. Empty for images written before this field
    /// existed, which is itself the answer ("older than 0.1.1").
    #[serde(default)]
    pub app_version: String,
    /// Container kind; currently always `"vhdx"`.
    pub container: String,
    /// File name (not path) of the image this sidecar describes.
    pub image_file_name: String,
    /// The machine's UEFI boot entries as they were when the copy was taken.
    ///
    /// Inside the image, like everything else that describes it. They used to
    /// be written to a `<image>.boot.json` beside it — the last piece of a
    /// backup that was not in the backup, and the one a user would carry to
    /// another disk without noticing it had stayed behind.
    ///
    /// Why a copy holds them at all: a boot entry lives in the motherboard's
    /// NVRAM, not on the disk. A whole-disk copy captures every partition and
    /// none of the menu that starts them, so a restore onto a fresh machine
    /// comes up with the partitions in place and nothing offering to boot them.
    ///
    /// `None` when the copy is not a bootable one, when the firmware could not
    /// be read, and in every copy written before this moved inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_entries: Option<crate::efivars::BootBackup>,
    /// A stable identity for the volume this image was taken from.
    ///
    /// Everything that chose a parent used to go through the image's FILE NAME
    /// — a token built from the drive letter, or, for an encrypted `part:`
    /// source with no letter, from its cleaned-up label (audit AUD-10). Two
    /// volumes labelled the same are then the same volume as far as chain
    /// planning is concerned, and an increment can be written onto a parent
    /// that is a copy of a different disk.
    ///
    /// On Windows this is the volume's GUID path, which survives a letter
    /// change and a reboot, or `disk-guid:offset` for a partition with no
    /// mounted volume. `None` in copies written before this existed and where
    /// no stable identity could be worked out — and then the token is all
    /// there is, which is why it is still used as well.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Role of the image in its chain.
    pub chain_role: ChainRole,
    /// File name of the parent image for incremental/differential images.
    pub parent_image_file_name: Option<String>,
    /// Byte offset inside the virtual disk where the backed-up volume data
    /// begins (the GPT partition start; 0 for whole-disk images).
    #[serde(default)]
    pub volume_data_offset: u64,
    /// Id of the GUI job that produced this image. Retention only ever touches
    /// sidecars carrying a matching job id; images without one (older builds,
    /// CLI runs) are never auto-deleted.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Full snapshot of the physical source disk this image came from — the
    /// whole disk with its partition list, captured from the live inventory at
    /// backup time. Present for BOTH per-volume images (the parent disk; the
    /// backed-up partition is identified by the job's sources) and whole-disk
    /// images. Lets a restore UI / Boot-CD render the archived disk and
    /// highlight the backed-up partitions WITHOUT any external job database
    /// (`jobs.json`). `None` for CLI runs / backups taken before snapshots.
    #[serde(default)]
    pub source_disk_layout: Option<DiskInfo>,
    /// File-type breakdown (bytes per Изображения/Видео/Аудио/Документы/Система/
    /// Другое) of what this image holds, from the backup's file walk. Embedded
    /// so the restore card's "по типам файлов" renders from the image alone
    /// (Boot-CD) without jobs.json. `None` when the walk didn't run.
    #[serde(default)]
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Count of files dropped by exclusion rules for this image, embedded so the
    /// restore card's "Исключено" line works from the image alone (no jobs.json).
    #[serde(default)]
    pub excluded_files: u64,
    /// Total bytes of the excluded files (see `excluded_files`).
    #[serde(default)]
    pub excluded_bytes: u64,
    /// The filesystem actually found INSIDE the volume this image holds.
    ///
    /// For a LUKS or BitLocker source this is the *decrypted* filesystem,
    /// because that is what the image contains — the backup stores the volume
    /// in the clear. The partition table outside still reads `Unknown`, which
    /// is the only honest answer before decryption, so a restore screen that
    /// consulted the disk snapshot alone would draw a padlock over what is now
    /// a plain btrfs (D14). `None` for images written before this field
    /// existed, and for a sector-by-sector copy of an unrecognised filesystem.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// How many increments consolidation has folded into this image.
    ///
    /// Only ever non-zero on a chain's base. It exists because the run planner
    /// counts a chain's versions by the FILES in the folder, and the sliding
    /// window removes files: without this, "a full version after every N
    /// versions" would stop firing the moment a window was enabled, the chain
    /// would never roll over, and one base would be rewritten forever with no
    /// independent copy of anything. The planner adds this to the number of
    /// images it can see, so N keeps meaning "N increments".
    ///
    /// Lives in the sidecar rather than the job database on purpose — the same
    /// reason retention reads sidecars: a folder of images has to remain
    /// self-describing when the database is gone.
    #[serde(default)]
    pub consolidated_increments: u32,
    /// When this image's CHAIN was started, not when this image was written.
    ///
    /// Only meaningful on a base, and it exists because consolidation moves
    /// the base's `created_at` forward — the base stops being the copy of the
    /// day it was taken. Anything asking "how old is this chain" would then
    /// see a chain that never ages, and an age-based rule would never fire.
    ///
    /// `None` on every image written before this existed, and on a full that
    /// has never been consolidated — in both cases the base's own `created_at`
    /// is the honest answer, which is what [`crate::chains::Chain::started_at`]
    /// falls back to.
    #[serde(default)]
    pub chain_started_at: Option<String>,
    /// Headers of the encrypted containers this image holds, one per partition
    /// that was decrypted for the copy (D19).
    ///
    /// Without them a restore can only produce a bare filesystem where the
    /// container used to be — readable, but not bootable. With them the
    /// container is put back exactly as it was. Empty for images written before
    /// this existed, and for sources that were never encrypted.
    #[serde(default)]
    pub luks_headers: Vec<own_backup_common::image::LuksHeaderBackup>,
}

impl SidecarInfo {
    pub fn new_full(image_file_name: impl Into<String>, volume_data_offset: u64) -> Self {
        Self {
            format_version: 1,
            app_version: env!("CARGO_PKG_VERSION").into(),
            container: "vhdx".into(),
            image_file_name: image_file_name.into(),
            boot_entries: None,
            source_id: None,
            chain_role: ChainRole::Full,
            parent_image_file_name: None,
            volume_data_offset,
            job_id: None,
            source_disk_layout: None,
            categories: None,
            excluded_files: 0,
            excluded_bytes: 0,
            filesystem: None,
            consolidated_increments: 0,
            chain_started_at: None,
            luks_headers: Vec::new(),
        }
    }

    pub fn new_child(
        image_file_name: impl Into<String>,
        role: ChainRole,
        parent: impl Into<String>,
        volume_data_offset: u64,
    ) -> Self {
        Self {
            format_version: 1,
            app_version: env!("CARGO_PKG_VERSION").into(),
            container: "vhdx".into(),
            image_file_name: image_file_name.into(),
            boot_entries: None,
            source_id: None,
            chain_role: role,
            parent_image_file_name: Some(parent.into()),
            volume_data_offset,
            job_id: None,
            source_disk_layout: None,
            categories: None,
            excluded_files: 0,
            excluded_bytes: 0,
            filesystem: None,
            consolidated_increments: 0,
            chain_started_at: None,
            luks_headers: Vec::new(),
        }
    }
}

/// One block checksum entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockChecksum {
    pub block_number: u64,
    pub checksum: u64,
}

/// A parsed sidecar file.
#[derive(Debug)]
pub struct Sidecar {
    pub info: SidecarInfo,
    pub manifest: ImageManifest,
    pub checksums: Vec<BlockChecksum>,
    /// Whole-disk restore manifest, present when this image is a single
    /// bootable disk image carrying several partitions (the embedded successor
    /// of the standalone `.obdm`). `None` for a plain per-volume image.
    pub disk_manifest: Option<own_backup_common::gpt::DiskBackupManifest>,
}

#[derive(Serialize, Deserialize)]
struct JsonHeader {
    info: SidecarInfo,
    manifest: ImageManifest,
    /// Embedded whole-disk manifest (see [`Sidecar::disk_manifest`]). Absent in
    /// per-volume images and every pre-2026-07-11 sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_manifest: Option<own_backup_common::gpt::DiskBackupManifest>,
}

/// The conventional sidecar path for an image: `<image>.obkm`
/// (e.g. `job_full_x.vhdx` → `job_full_x.vhdx.obkm`).
pub fn sidecar_path_for(image_path: &Path) -> std::path::PathBuf {
    let mut name = image_path.as_os_str().to_os_string();
    name.push(".");
    name.push(SIDECAR_EXTENSION);
    std::path::PathBuf::from(name)
}

/// Size the image's metadata partition.
///
/// It has to hold the whole sidecar blob, and it is reserved **before** that
/// blob exists — the checksums inside it are produced by the copy the region is
/// being reserved for. So this is an estimate, and it has to be an estimate
/// that is never too small: the write at the end fails outright if the blob
/// does not fit, after the copy has already run.
///
/// `extra_bytes` is room for anything beyond the checksum table and the small
/// JSON header — today that means stored LUKS headers, which are megabytes
/// rather than kilobytes (see [`luks_headers_reserve`]).
pub fn calculate_meta_partition_size(total_blocks: u64, extra_bytes: u64) -> u64 {
    let bytes = total_blocks * 16 + extra_bytes + 65536;
    let mib = 1024 * 1024;
    bytes.div_ceil(mib) * mib
}

/// Room to reserve for stored container headers (D19).
///
/// They travel base64-encoded inside the sidecar's JSON, which inflates them by
/// a third; zstd then squeezes most of that back out, but the reserve is
/// deliberately computed on the *un*compressed size. Betting on a compression
/// ratio here would mean a backup that runs for an hour and then cannot write
/// its own metadata — the estimate is allowed to be generous, and is not
/// allowed to be optimistic.
pub fn luks_headers_reserve(headers: &[own_backup_common::image::LuksHeaderBackup]) -> u64 {
    headers.iter().map(|h| h.data_offset.div_ceil(3) * 4 + 65536).sum()
}

/// Room to reserve for the machine's embedded boot entries.
///
/// Measured rather than guessed — they are read before the metadata partition
/// is sized, so their real serialized length is available. Computed on the
/// UNcompressed JSON for the same reason as the container headers: a backup
/// that runs for an hour and then cannot write its own metadata is not a
/// trade worth making for a few kilobytes of image.
pub fn boot_entries_reserve(backup: Option<&crate::efivars::BootBackup>) -> u64 {
    match backup {
        None => 0,
        // The variables are already base64 inside the structure, so its own
        // JSON length is the honest figure; the slack covers the framing.
        Some(b) => b.to_json().map(|j| j.len() as u64).unwrap_or(0) + 16384,
    }
}

/// The same reserve computed straight from the container parameters, for the
/// point in a backup where the headers have not been captured yet but their
/// size is already known.
pub fn luks_reserve_for(data_offsets: impl IntoIterator<Item = u64>) -> u64 {
    data_offsets.into_iter().map(|d| d.div_ceil(3) * 4 + 65536).sum()
}

#[cfg(test)]
mod meta_size_tests {
    use super::*;
    use own_backup_common::image::{LuksHeaderBackup, VolumeEncryptionKind};

    const MIB: u64 = 1024 * 1024;

    fn header(data_offset: u64) -> LuksHeaderBackup {
        LuksHeaderBackup {
            part_index: 1,
            data_offset,
            sector_size: 512,
            kind: VolumeEncryptionKind::Luks2,
            header_b64: String::new(),
        }
    }

    #[test]
    fn a_plain_image_needs_room_for_its_checksums() {
        // 14 219 blocks × 16 bytes + slack, rounded up to a whole mebibyte.
        assert_eq!(calculate_meta_partition_size(14_219, 0), MIB);
    }

    #[test]
    fn a_stored_luks2_header_dominates_the_reserve() {
        // The failure this guards: a 16 MiB header inside a 1 MiB region.
        // 14 219 blocks alone fit in 1 MiB; with the header they need 23.
        let reserve = luks_headers_reserve(&[header(16 * MIB)]);
        let size = calculate_meta_partition_size(14_219, reserve);
        assert!(size >= 22 * MIB, "reserved only {size} bytes for a 16 MiB header");
    }

    #[test]
    fn the_reserve_covers_base64_without_counting_on_compression() {
        // base64 inflates by 4/3; zstd usually wins it back, but a reserve that
        // assumes it would fail after the copy has already run.
        let raw = 16 * MIB;
        assert!(luks_headers_reserve(&[header(raw)]) >= raw * 4 / 3);
    }

    #[test]
    fn several_encrypted_partitions_each_get_room() {
        let one = luks_headers_reserve(&[header(16 * MIB)]);
        let three = luks_headers_reserve(&[header(16 * MIB), header(16 * MIB), header(16 * MIB)]);
        assert_eq!(three, one * 3);
    }

    #[test]
    fn the_two_ways_of_computing_the_reserve_agree() {
        // One is used after the headers are captured, the other before — they
        // must not drift, or a child's geometry stops matching its parent's.
        assert_eq!(
            luks_headers_reserve(&[header(2 * MIB), header(16 * MIB)]),
            luks_reserve_for([2 * MIB, 16 * MIB])
        );
    }

    #[test]
    fn no_encryption_reserves_nothing() {
        assert_eq!(luks_headers_reserve(&[]), 0);
        assert_eq!(luks_reserve_for([]), 0);
    }
}

/// Write a sidecar to `path` (atomically via a `.tmp` rename).
pub fn write_sidecar(
    path: &Path,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    let tmp_path = path.with_extension(format!("{SIDECAR_EXTENSION}.tmp"));
    {
        let file = File::create(&tmp_path)?;
        let mut w = BufWriter::new(file);
        write_sidecar_to(&mut w, info, manifest, checksums)?;
        w.flush()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn write_varint<W: Write>(w: &mut W, mut val: u64) -> std::io::Result<usize> {
    let mut bytes_written = 0;
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        w.write_all(&[byte])?;
        bytes_written += 1;
        if val == 0 {
            break;
        }
    }
    Ok(bytes_written)
}

fn read_varint<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut val = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "varint overflow"));
        }
    }
    Ok(val)
}

/// Write a sidecar to any writer.
pub fn write_sidecar_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    let json = serde_json::to_vec(&JsonHeader {
        info: info.clone(),
        manifest: manifest.clone(),
        disk_manifest: None, // legacy v1 sidecar never carries a whole-disk manifest
    })?;
    if json.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("sidecar JSON too large".into()));
    }

    w.write_all(SIDECAR_MAGIC)?;
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(&json)?;
    w.write_all(&(checksums.len() as u64).to_le_bytes())?;
    for entry in checksums {
        w.write_all(&entry.block_number.to_le_bytes())?;
        w.write_all(&entry.checksum.to_le_bytes())?;
    }
    Ok(())
}

/// Write a sidecar format v2 blob (with compression and integrity check) to any
/// writer. Per-volume images carry no whole-disk manifest.
pub fn write_sidecar_v2_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
) -> Result<()> {
    write_sidecar_v2_full_to(w, info, manifest, checksums, None)
}

/// Like [`write_sidecar_v2_to`] but also embeds a whole-disk restore manifest —
/// used by a single bootable disk image so its `.obdm` lives inside the file.
pub fn write_sidecar_v2_full_to<W: Write>(
    w: &mut W,
    info: &SidecarInfo,
    manifest: &ImageManifest,
    checksums: &[BlockChecksum],
    disk_manifest: Option<&own_backup_common::gpt::DiskBackupManifest>,
) -> Result<()> {
    let mut info = info.clone();
    info.format_version = 2;

    let json = serde_json::to_vec(&JsonHeader {
        info,
        manifest: manifest.clone(),
        disk_manifest: disk_manifest.cloned(),
    })?;
    let json_zstd = zstd::bulk::compress(&json, 3)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    if json_zstd.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("compressed sidecar JSON too large".into()));
    }

    let mut checksums = checksums.to_vec();
    checksums.sort_by_key(|c| c.block_number);

    let mut table_buf = Vec::new();
    let mut prev = 0u64;
    for entry in &checksums {
        let delta = entry.block_number - prev;
        write_varint(&mut table_buf, delta)?;
        table_buf.write_all(&entry.checksum.to_le_bytes())?;
        prev = entry.block_number;
    }

    let table_zstd = if !table_buf.is_empty() {
        zstd::bulk::compress(&table_buf, 3)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?
    } else {
        Vec::new()
    };
    if table_zstd.len() > u32::MAX as usize {
        return Err(OwnBackupError::ImageFormat("compressed sidecar checksum table too large".into()));
    }

    let mut blob = Vec::new();
    blob.write_all(SIDECAR_MAGIC_V2)?;
    blob.write_all(&(json_zstd.len() as u32).to_le_bytes())?;
    blob.write_all(&json_zstd)?;
    blob.write_all(&(checksums.len() as u64).to_le_bytes())?;
    blob.write_all(&(table_zstd.len() as u32).to_le_bytes())?;
    blob.write_all(&table_zstd)?;

    let hash = xxhash_rust::xxh64::xxh64(&blob, 0);
    blob.write_all(&hash.to_le_bytes())?;

    w.write_all(&blob)?;
    Ok(())
}

/// Maximum accepted JSON header size (defensive cap against a corrupt length).
const MAX_JSON_LEN: u32 = 100 * 1024 * 1024;

/// Hard ceiling on the number of checksum entries, whatever a header claims.
/// Reached only when the header does not say how big the source was; the
/// limit derived below is normally far tighter.
const MAX_CHECKSUMS: u64 = 1 << 32;

/// How many entries are reserved before a single one has been read.
///
/// The point of the cap is that the count field is UNTRUSTED input. Reserving
/// `count` up front — which both readers used to do — let a forty-byte corrupt
/// file ask for 64 GiB of memory before anything checked whether the table was
/// even there (audit AUD-15). Everything past this comes from the vector's own
/// growth, driven by entries that actually arrived, so a bogus count now costs
/// one small allocation and an end-of-file error.
const PREALLOC_CHECKSUMS: u64 = 1 << 16;

/// Largest block size we will believe. Our writers use 1 MiB at most; the
/// margin is for formats we have not written yet, not for arbitrary numbers.
const MAX_BLOCK_SIZE: u64 = 64 * 1024 * 1024;

/// The most checksum entries this sidecar's own header allows.
///
/// A checksum describes one block of the source volume, so the source's size
/// is the ceiling: `ceil(size / block_size)`, plus one for a trailing partial
/// block. When the header does not say (block size or size missing — some old
/// sidecars), fall back to the absolute cap rather than refusing the file:
/// supporting readable older formats matters more than a tight bound on a
/// file that has already parsed as JSON.
fn checksum_limit(manifest: &ImageManifest) -> u64 {
    let block = manifest.block_size as u64;
    let size = manifest.source_disk.size_bytes;
    if block == 0 || block > MAX_BLOCK_SIZE || size == 0 {
        return MAX_CHECKSUMS;
    }
    size.div_ceil(block).saturating_add(1).min(MAX_CHECKSUMS)
}

/// Check a count field against that limit before it is used for anything.
fn checked_checksum_count(count: u64, manifest: &ImageManifest, what: &str) -> Result<u64> {
    let limit = checksum_limit(manifest);
    if count > limit {
        return Err(OwnBackupError::ImageFormat(format!(
            "{what}: таблица заявляет {count} контрольных сумм, а образ такого \
             размера вмещает не больше {limit} — файл повреждён"
        )));
    }
    Ok(count)
}

/// A vector for `count` entries whose up-front allocation does not depend on
/// `count` being honest. `try_reserve`, so even that bounded ask reports a
/// failure instead of aborting the process.
fn reserve_checksums(count: u64) -> Result<Vec<BlockChecksum>> {
    let mut v: Vec<BlockChecksum> = Vec::new();
    let want = count.min(PREALLOC_CHECKSUMS) as usize;
    v.try_reserve(want).map_err(|_| {
        OwnBackupError::ImageFormat(format!(
            "не удалось выделить память под {want} контрольных сумм"
        ))
    })?;
    Ok(v)
}

/// Read a sidecar file from `path`.
pub fn read_sidecar(path: &Path) -> Result<Sidecar> {
    let file = File::open(path)?;
    read_sidecar_from(&mut BufReader::new(file))
}

/// Format-agnostic guest-byte reader for the embedded sidecar blob.
enum GuestBlob {
    Vhdx(std::path::PathBuf),
    Qcow2(Box<crate::qcow2::Qcow2RandomReader>),
}

impl GuestBlob {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            GuestBlob::Vhdx(path) => crate::vhdx_bat::read_vhdx_guest_at(path, offset, buf),
            GuestBlob::Qcow2(reader) => reader.read_at(offset, buf),
        }
    }
}

/// Which container an image really is, decided by its first eight bytes.
///
/// The file name is a guess; the magic is ground truth. They can disagree in
/// ordinary use: `--output` and `--format` are independent on the command line,
/// so `--output monday.img --format vhdx` writes a perfectly good VHDX, and a
/// user is free to rename `backup.vhdx` to anything afterwards. While the
/// container was picked by extension, both cases produced an image that was
/// written successfully and then could not be read at all — `verify`, `mount`,
/// `restore` and the chain listing every one of them failed with "unsupported
/// container format: img". Found by the first root run of the `--ignored`
/// tests, 2026-08-20.
pub fn detect_container_format(image_path: &Path) -> Result<&'static str> {
    let mut head = [0u8; 8];
    let mut file = File::open(image_path)?;
    let mut got = 0;
    while got < head.len() {
        match file.read(&mut head[got..])? {
            0 => break,
            n => got += n,
        }
    }
    if got == head.len() && &head == b"vhdxfile" {
        return Ok("vhdx");
    }
    if got >= 4 && u32::from_be_bytes(head[..4].try_into().unwrap()) == crate::qcow2::QCOW2_MAGIC {
        // An encrypted image IS a qcow2 file, so the magic alone would call it
        // one and every reader after that would fail somewhere deeper. The
        // incompatible feature bit is what separates them.
        let mut features = [0u8; 8];
        if file.seek(std::io::SeekFrom::Start(72)).is_ok()
            && file.read_exact(&mut features).is_ok()
            && u64::from_be_bytes(features) & crate::seal::obk::INCOMPAT_OBK_SEAL != 0
        {
            return Ok(crate::seal::obk::OBK_EXTENSION);
        }
        return Ok("qcow2");
    }
    Err(OwnBackupError::ImageFormat(format!(
        "{}: not a VHDX, qcow2 or encrypted image (no known signature)",
        image_path.display()
    )))
}

/// Locate the OwnBackupMeta partition of an image: returns a guest reader plus
/// the partition's (offset, size) in guest bytes.
fn open_meta_partition(image_path: &Path, container_format: &str) -> Result<(GuestBlob, u64, u64)> {
    const SECTOR: usize = 512;
    // `container_format` is whatever the caller inferred from the name — kept
    // only for the error message when the file has no signature to read.
    let container_format = detect_container_format(image_path).unwrap_or(container_format);
    let (mut guest, vsize) = match container_format {
        "vhdx" | "avhdx" => {
            let alloc = crate::vhdx_bat::allocated_payload_blocks(image_path)?;
            (GuestBlob::Vhdx(image_path.to_path_buf()), alloc.virtual_size)
        }
        "qcow2" => {
            let reader = crate::qcow2::Qcow2RandomReader::open(image_path)?;
            let vsize = reader.virtual_size();
            (GuestBlob::Qcow2(Box::new(reader)), vsize)
        }
        // An encrypted image has no guest meta partition at all: its sidecar is
        // sealed inside the container. Say that, rather than "unsupported" —
        // the caller needs a key, not a different file.
        "obk" => {
            return Err(OwnBackupError::Encryption(format!(
                "{}: копия зашифрована — её сведения запечатаны внутри образа, \
нужен пароль или ключ восстановления",
                image_path.display()
            )))
        }
        other => {
            return Err(OwnBackupError::ImageFormat(format!(
                "unsupported container format: {other}"
            )))
        }
    };
    let mut gpt_buf = vec![0u8; 34 * SECTOR];
    guest.read_at(0, &mut gpt_buf)?;
    let layout = crate::gpt_reader::parse_disk_layout(&gpt_buf, SECTOR as u32, vsize)?;
    let (meta_offset, meta_size) = crate::gpt_reader::find_meta_partition(&layout).ok_or_else(
        || OwnBackupError::ImageFormat("OwnBackupMeta partition not found in GPT".into()),
    )?;
    Ok((guest, meta_offset, meta_size))
}

/// Read the embedded sidecar blob (exact size, derived from its own length
/// fields — never a fixed cap: the checksum table of a large disk runs to tens
/// of MiB). `header_only` stops after the JSON header.
fn read_meta_blob(image_path: &Path, container_format: &str, header_only: bool) -> Result<Vec<u8>> {
    let (mut guest, meta_offset, meta_size) = open_meta_partition(image_path, container_format)?;

    // magic (8) + JSON length (4) — shared by v1 and v2 layouts.
    if meta_size < 12 {
        return Err(OwnBackupError::ImageFormat("meta partition too small for a sidecar".into()));
    }
    let mut head = [0u8; 12];
    guest.read_at(meta_offset, &mut head)?;
    let json_len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as u64;
    if json_len == 0 || json_len > MAX_JSON_LEN as u64 || 12 + json_len > meta_size {
        return Err(OwnBackupError::ImageFormat(format!(
            "embedded sidecar JSON length {json_len} out of range"
        )));
    }

    let total = if header_only {
        12 + json_len
    } else if &head[..8] == SIDECAR_MAGIC_V2 {
        // ... + entry count (8) + compressed table length (4) + table + hash (8)
        let mut mid = [0u8; 12];
        guest.read_at(meta_offset + 12 + json_len, &mut mid)?;
        let table_len = u32::from_le_bytes(mid[8..12].try_into().unwrap()) as u64;
        12 + json_len + 8 + 4 + table_len + 8
    } else if &head[..8] == SIDECAR_MAGIC {
        // ... + entry count (8) + entries (16 each)
        let mut cnt = [0u8; 8];
        guest.read_at(meta_offset + 12 + json_len, &mut cnt)?;
        let count = u64::from_le_bytes(cnt);
        if count > 1u64 << 32 {
            return Err(OwnBackupError::ImageFormat(format!(
                "embedded sidecar checksum count {count} out of range"
            )));
        }
        12 + json_len + 8 + count * 16
    } else {
        return Err(OwnBackupError::ImageFormat("no sidecar magic in the meta partition".into()));
    };
    if total > meta_size {
        return Err(OwnBackupError::ImageFormat(format!(
            "embedded sidecar ({total} bytes) exceeds the meta partition ({meta_size} bytes)"
        )));
    }

    let mut blob = vec![0u8; total as usize];
    guest.read_at(meta_offset, &mut blob)?;
    Ok(blob)
}

/// Guest offset and size of the image's meta partition, read from the image's
/// own GPT.
///
/// This is the locator the readers use, and the only one anything writing the
/// blob back should use: the backup engine derives the same offset from the
/// volume length and sector size at write time, and a second copy of that
/// arithmetic is a second thing to get wrong on an image whose geometry came
/// from somewhere else.
pub fn meta_partition_of(image_path: &Path, container_format: &str) -> Result<(u64, u64)> {
    let (_, offset, size) = open_meta_partition(image_path, container_format)?;
    Ok((offset, size))
}

/// Replace an image's embedded sidecar in place, at the offset its own GPT
/// gives. Used by consolidation, which rewrites metadata without rewriting
/// data.
///
/// Takes the whole [`Sidecar`] rather than its parts on purpose: a whole-disk
/// image carries its restore manifest in the same blob, and writing the parts
/// through [`write_sidecar_v2_to`] would silently drop it — the image would
/// still verify and still mount, and only a bare-metal restore would find out.
pub fn write_embedded_sidecar(
    image_path: &Path,
    container_format: &str,
    sidecar: &Sidecar,
) -> Result<()> {
    let (meta_offset, meta_size) = meta_partition_of(image_path, container_format)?;
    let mut blob = Vec::new();
    write_sidecar_v2_full_to(
        &mut blob,
        &sidecar.info,
        &sidecar.manifest,
        &sidecar.checksums,
        sidecar.disk_manifest.as_ref(),
    )?;
    if blob.len() as u64 > meta_size {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar ({} bytes) does not fit the meta partition ({meta_size} bytes)",
            blob.len()
        )));
    }
    match container_format {
        "vhdx" | "avhdx" => crate::vhdx_bat::write_vhdx_guest_at(image_path, meta_offset, &blob),
        "qcow2" => crate::qcow2::merge::write_qcow2_guest_blob(image_path, meta_offset, &blob),
        other => Err(OwnBackupError::ImageFormat(format!(
            "unsupported container format: {other}"
        ))),
    }
}

/// Read the embedded sidecar v2 from the virtual disk image.
pub fn read_embedded_sidecar(image_path: &Path, container_format: &str) -> Result<Sidecar> {
    let blob = read_meta_blob(image_path, container_format, false)?;
    read_sidecar_from(&mut std::io::Cursor::new(blob))
}

/// Read only the SidecarInfo and ImageManifest from the embedded sidecar of the virtual disk image.
pub fn read_embedded_sidecar_header(
    image_path: &Path,
    container_format: &str,
) -> Result<(SidecarInfo, ImageManifest)> {
    let blob = read_meta_blob(image_path, container_format, true)?;
    read_sidecar_header_from(&mut std::io::Cursor::new(blob))
}

/// Read an image's full sidecar: embedded (inside the image) first, then the
/// adjacent `.obkm` file (backups from before the embedded-sidecar change).
/// The sidecar of an **encrypted** image: the same bytes a plaintext image
/// keeps in its meta partition, sealed inside the container instead.
///
/// §7.3 keeps the guest meta partition out of `.obk` on purpose — the disk
/// layout, machine name, volume labels, file categories, UEFI entries and the
/// plaintext checksums are exactly what the sealed half is for. So this is the
/// only way to read them, and it needs the chain key.
/// It opens **this image alone**, never its chain. A sidecar belongs to the
/// image that wrote it, so the parent has nothing to add — and the one caller
/// that needs it most, repointing an increment during consolidation, runs in
/// the window where the chain deliberately does not open (the base has been
/// rewritten and the link has not moved yet).
pub fn read_sealed_sidecar(image_path: &Path, key: &crate::seal::ChainKey) -> Result<Sidecar> {
    let mut image = crate::seal::obk::ObkImage::open_path(image_path, key)?;
    let blob = image.sealed_metadata()?;
    if blob.is_empty() {
        return Err(OwnBackupError::ImageFormat(format!(
            "{}: the image carries no sidecar",
            image_path.display()
        )));
    }
    read_sidecar_from(&mut &blob[..])
}

pub fn read_image_sidecar(image_path: &Path) -> Result<Sidecar> {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match read_embedded_sidecar(image_path, &ext) {
        Ok(s) => Ok(s),
        Err(embedded_err) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({embedded_err}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar(&sidecar_path)
        }
    }
}

/// Header-only counterpart of [`read_image_sidecar`] (info + manifest, no
/// checksum table) — cheap enough for folder scans and dispatch decisions.
/// The machine's boot entries as this copy holds them, wherever they are.
///
/// Inside the image is where they live now. A copy taken before they moved in
/// has them in a `<image>.boot.json` beside it, and those copies are still
/// perfectly good copies — so that file is read as a fallback rather than
/// forgotten. Nothing writes it any more.
///
/// `None` means this copy carries none: not a bootable copy, a BIOS machine,
/// or a run that could not read the firmware.
/// **Reads the copy WITHOUT a key**, so an ENCRYPTED copy answers that it has
/// no boot entries — its whole sidecar is sealed. Use [`boot_entries_of`]
/// wherever the sidecar has already been opened, which is everywhere that
/// matters; this one is for a caller that has nothing else.
pub fn boot_entries_of_image(image_path: &Path) -> Option<crate::efivars::BootBackup> {
    if let Ok((info, _)) = read_image_sidecar_header(image_path) {
        if info.boot_entries.is_some() {
            return info.boot_entries;
        }
    }
    boot_entries_beside(image_path)
}

/// The entries a copy carries, from a sidecar that is already open.
///
/// The menu lives in the motherboard's NVRAM, so a copy that holds every
/// partition holds none of what starts them — which is why a bootable copy
/// carries the entries too, sealed with the rest of its metadata. Asking the
/// FILE for them again (as [`boot_entries_of_image`] does) gets "none" from an
/// encrypted copy, and then the screen offers no question and the restore
/// reports there was nothing to put back.
pub fn boot_entries_of(
    sidecar: &Sidecar,
    image_path: &Path,
) -> Option<crate::efivars::BootBackup> {
    if sidecar.info.boot_entries.is_some() {
        return sidecar.info.boot_entries.clone();
    }
    // Copies written before the entries moved inside keep them in a file next
    // to the image.
    boot_entries_beside(image_path)
}

fn boot_entries_beside(image_path: &Path) -> Option<crate::efivars::BootBackup> {
    let beside = boot_entries_path_beside(image_path);
    let json = std::fs::read_to_string(&beside).ok()?;
    match crate::efivars::BootBackup::load(&json) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(path = %beside.display(), error = %e, "boot entries beside the image are unreadable");
            None
        }
    }
}

/// Where a copy taken before the entries moved inside keeps them.
pub fn boot_entries_path_beside(image_path: &Path) -> std::path::PathBuf {
    image_path.with_extension(format!(
        "{}.boot.json",
        image_path.extension().and_then(|e| e.to_str()).unwrap_or("img")
    ))
}

pub fn read_image_sidecar_header(image_path: &Path) -> Result<(SidecarInfo, ImageManifest)> {
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match read_embedded_sidecar_header(image_path, &ext) {
        Ok(h) => Ok(h),
        Err(embedded_err) => {
            let sidecar_path = sidecar_path_for(image_path);
            if !sidecar_path.exists() {
                return Err(OwnBackupError::ImageFormat(format!(
                    "embedded sidecar not found ({embedded_err}) and adjacent sidecar not found: {}",
                    sidecar_path.display()
                )));
            }
            read_sidecar_header(&sidecar_path)
        }
    }
}

/// Can `image_path` serve as a chain parent / be restored? True when its
/// sidecar metadata is readable — embedded (inside the image) or the adjacent
/// `.obkm` file. Chain planners must accept either: images written since the
/// embedded-sidecar change carry no external file at all.
pub fn image_sidecar_readable(image_path: &Path) -> bool {
    // An ENCRYPTED image keeps its sidecar sealed inside the container, so the
    // two checks below both say "no" for one that is perfectly usable. The
    // question being asked is whether this copy can be a chain parent, and for
    // that the open header is the right authority: it is what the listing and
    // the retention pass already group by, and the run that writes the child
    // holds the key the sealed half needs.
    //
    // Answering "no" here was not a cosmetic wrong answer: the planners take it
    // as "the parent is unusable" and fall back to a FULL backup — so every run
    // of an encrypted job wrote a new full copy and no chain ever formed.
    if detect_container_format(image_path).ok() == Some(crate::seal::obk::OBK_EXTENSION) {
        return crate::seal::obk::read_listing(image_path).is_ok();
    }
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    read_embedded_sidecar_header(image_path, &ext).is_ok()
        || sidecar_path_for(image_path).exists()
}

/// Read only the JSON header (info + manifest) of a sidecar, skipping the
/// checksum table. Cheap enough for scanning whole folders (retention, chain
/// discovery, image info).
pub fn read_sidecar_header(path: &Path) -> Result<(SidecarInfo, ImageManifest)> {
    let file = File::open(path)?;
    read_sidecar_header_from(&mut BufReader::new(file))
}

/// Read only the JSON header from any reader.
pub fn read_sidecar_header_from<R: Read>(r: &mut R) -> Result<(SidecarInfo, ImageManifest)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic == SIDECAR_MAGIC {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let json_len = u32::from_le_bytes(len_buf);
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(OwnBackupError::ImageFormat(format!(
                "sidecar JSON length {json_len} out of range"
            )));
        }
        let mut json = vec![0u8; json_len as usize];
        r.read_exact(&mut json)?;
        let header: JsonHeader = serde_json::from_slice(&json)?;
        Ok((header.info, header.manifest))
    } else if &magic == SIDECAR_MAGIC_V2 {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let json_len = u32::from_le_bytes(len_buf);
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(OwnBackupError::ImageFormat(format!(
                "sidecar v2 JSON length {json_len} out of range"
            )));
        }
        let mut json_zstd = vec![0u8; json_len as usize];
        r.read_exact(&mut json_zstd)?;
        let json_decompressed = zstd::bulk::decompress(&json_zstd, MAX_JSON_LEN as usize)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
        let header: JsonHeader = serde_json::from_slice(&json_decompressed)?;
        Ok((header.info, header.manifest))
    } else {
        Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ))
    }
}

fn read_sidecar_v2_from_after_magic<R: Read>(r: &mut R) -> Result<Sidecar> {
    let mut header_bytes = Vec::new();
    header_bytes.write_all(SIDECAR_MAGIC_V2)?;

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    header_bytes.write_all(&len_buf)?;
    let json_len = u32::from_le_bytes(len_buf);
    if json_len == 0 || json_len > MAX_JSON_LEN {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 JSON length {json_len} out of range"
        )));
    }

    let mut json_zstd = vec![0u8; json_len as usize];
    r.read_exact(&mut json_zstd)?;
    header_bytes.write_all(&json_zstd)?;

    let json_decompressed = zstd::bulk::decompress(&json_zstd, MAX_JSON_LEN as usize)
        .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
    let header: JsonHeader = serde_json::from_slice(&json_decompressed)?;

    let mut count_buf = [0u8; 8];
    r.read_exact(&mut count_buf)?;
    header_bytes.write_all(&count_buf)?;
    let count = u64::from_le_bytes(count_buf);
    let count = checked_checksum_count(count, &header.manifest, "sidecar v2")?;

    let mut table_len_buf = [0u8; 4];
    r.read_exact(&mut table_len_buf)?;
    header_bytes.write_all(&table_len_buf)?;
    let table_zstd_len = u32::from_le_bytes(table_len_buf);
    if table_zstd_len > 500 * 1024 * 1024 {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 table compressed length {table_zstd_len} too large"
        )));
    }

    let mut table_zstd = vec![0u8; table_zstd_len as usize];
    r.read_exact(&mut table_zstd)?;
    header_bytes.write_all(&table_zstd)?;

    let mut expected_hash_buf = [0u8; 8];
    r.read_exact(&mut expected_hash_buf)?;
    let expected_hash = u64::from_le_bytes(expected_hash_buf);

    let actual_hash = xxhash_rust::xxh64::xxh64(&header_bytes, 0);
    if actual_hash != expected_hash {
        return Err(OwnBackupError::ImageFormat(format!(
            "sidecar v2 integrity check failed (expected hash {expected_hash:x}, got {actual_hash:x})"
        )));
    }

    let mut checksums = reserve_checksums(count)?;
    if count > 0 {
        // An entry is a varint delta (1..=10 bytes) plus eight checksum bytes,
        // so the decompressed table cannot honestly exceed 18 bytes per entry.
        // The old bound was `count * 16 + 10 MiB` over an unchecked count —
        // a 64 GiB decompression bomb ceiling (audit AUD-15).
        let max_decompressed_table = count
            .checked_mul(18)
            .and_then(|n| n.checked_add(64))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                OwnBackupError::ImageFormat(format!(
                    "sidecar v2: таблица из {count} записей не помещается в память"
                ))
            })?;
        let table_decompressed = zstd::bulk::decompress(&table_zstd, max_decompressed_table)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?;

        let limit = checksum_limit(&header.manifest);
        let mut cursor = std::io::Cursor::new(table_decompressed);
        let mut prev = 0u64;
        for i in 0..count {
            let delta = read_varint(&mut cursor)?;
            // Deltas are cumulative, so a corrupt one used to wrap (release) or
            // panic (debug) instead of being reported. And a block number past
            // the end of the image is not merely odd: `verify_image` turns it
            // into `volume_len - offset`, which underflows.
            let block_number = prev.checked_add(delta).filter(|b| *b <= limit).ok_or_else(|| {
                OwnBackupError::ImageFormat(format!(
                    "sidecar v2: запись {i} указывает на блок за пределами образа \
                     (предыдущий {prev}, шаг {delta}, предел {limit})"
                ))
            })?;
            let mut cksum_buf = [0u8; 8];
            cursor.read_exact(&mut cksum_buf)?;
            let checksum = u64::from_le_bytes(cksum_buf);
            checksums.push(BlockChecksum {
                block_number,
                checksum,
            });
            prev = block_number;
        }
    }

    Ok(Sidecar {
        info: header.info,
        manifest: header.manifest,
        checksums,
        disk_manifest: header.disk_manifest,
    })
}

/// Read a sidecar from any reader.
pub fn read_sidecar_from<R: Read>(r: &mut R) -> Result<Sidecar> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic == SIDECAR_MAGIC {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let json_len = u32::from_le_bytes(len_buf);
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(OwnBackupError::ImageFormat(format!(
                "sidecar JSON length {json_len} out of range"
            )));
        }

        let mut json = vec![0u8; json_len as usize];
        r.read_exact(&mut json)?;
        let header: JsonHeader = serde_json::from_slice(&json)?;

        let mut count_buf = [0u8; 8];
        r.read_exact(&mut count_buf)?;
        let count = u64::from_le_bytes(count_buf);
        let count = checked_checksum_count(count, &header.manifest, "sidecar")?;

        let limit = checksum_limit(&header.manifest);
        let mut checksums = reserve_checksums(count)?;
        let mut entry = [0u8; 16];
        for i in 0..count {
            r.read_exact(&mut entry)?;
            let block_number = u64::from_le_bytes(entry[0..8].try_into().unwrap());
            if block_number > limit {
                return Err(OwnBackupError::ImageFormat(format!(
                    "sidecar: запись {i} указывает на блок {block_number} за пределами \
                     образа (предел {limit})"
                )));
            }
            checksums.push(BlockChecksum {
                block_number,
                checksum: u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            });
        }

        Ok(Sidecar {
            info: header.info,
            manifest: header.manifest,
            checksums,
            disk_manifest: header.disk_manifest,
        })
    } else if &magic == SIDECAR_MAGIC_V2 {
        read_sidecar_v2_from_after_magic(r)
    } else {
        Err(OwnBackupError::ImageFormat(
            "not a sidecar file (bad magic)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::types::*;
    use std::io::Cursor;
    use uuid::Uuid;

    fn make_manifest() -> ImageManifest {
        ImageManifest {
            backup_id: Uuid::nil(),
            created_at: "2026-07-01T00:00:00Z".into(),
            backup_type: BackupType::Full,
            parent_backup_id: None,
            source_os: "windows".into(),
            source_disk: DiskInfo {
                device_id: r"\\.\PhysicalDrive0".into(),
                model: "Test SSD".into(),
                serial: "S1".into(),
                size_bytes: 1024 * 1024,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                media_type: "SSD".into(),
                partition_table: PartitionTable::Gpt,
                disk_guid: Uuid::nil().to_string(),
                partitions: vec![],
            },
            boot_mode: BootMode::Uefi,
            partition_table: PartitionTable::Gpt,
            compression_algo: CompressionAlgo::None,
            compression_level: 0,
            encryption_algo: None,
            encryption_salt: None,
            block_size: 65536,
            total_blocks: 16,
            used_blocks: 3,
            backup_mode: BackupMode::UsedSectors,
            checksum_algorithm: "xxhash64".into(),
            preflight_report: None,
            volume_encryption: None,
        }
    }

    #[test]
    fn sidecar_roundtrip() {
        let info = SidecarInfo::new_full("job_full.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![
            BlockChecksum { block_number: 0, checksum: 0xDEAD },
            BlockChecksum { block_number: 5, checksum: 0xBEEF },
            BlockChecksum { block_number: 15, checksum: u64::MAX },
        ];

        let mut buf = Vec::new();
        write_sidecar_to(&mut buf, &info, &manifest, &checksums).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.container, "vhdx");
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
        assert_eq!(sidecar.info.image_file_name, "job_full.vhdx");
        assert_eq!(sidecar.info.volume_data_offset, 1024 * 1024);
        assert_eq!(sidecar.manifest.block_size, 65536);
        assert_eq!(sidecar.checksums, checksums);
        // Which build wrote the copy, carried inside it. The question comes up
        // exactly when the copy is suspect and the build that made it is long
        // gone from the machine.
        assert_eq!(sidecar.info.app_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_sidecar_written_before_the_version_field_still_reads() {
        // Copies made by 0.1.0 have no `app_version` at all. They must keep
        // opening — and say so by leaving the field empty, rather than claiming
        // the version of whatever is reading them now.
        let json = serde_json::json!({
            "info": {
                "format_version": 1,
                "container": "vhdx",
                "image_file_name": "old.vhdx",
                "chain_role": "full",
                "parent_image_file_name": null
            },
            "manifest": serde_json::to_value(make_manifest()).unwrap(),
        });
        #[derive(serde::Deserialize)]
        struct Doc {
            info: SidecarInfo,
        }
        let doc: Doc = serde_json::from_value(json).unwrap();
        assert_eq!(doc.info.app_version, "", "an old copy must not borrow our version");
        assert_eq!(doc.info.image_file_name, "old.vhdx");
    }

    #[test]
    fn sidecar_roundtrip_incremental_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("job_inc_1.avhdx");
        let path = sidecar_path_for(&image);
        assert!(path.to_string_lossy().ends_with("job_inc_1.avhdx.obkm"));

        let info = SidecarInfo::new_child(
            "job_inc_1.avhdx",
            ChainRole::Incremental,
            "job_full.vhdx",
            1024 * 1024,
        );
        write_sidecar(&path, &info, &make_manifest(), &[]).unwrap();

        let sidecar = read_sidecar(&path).unwrap();
        assert_eq!(sidecar.info.chain_role, ChainRole::Incremental);
        assert_eq!(
            sidecar.info.parent_image_file_name.as_deref(),
            Some("job_full.vhdx")
        );
        assert!(sidecar.checksums.is_empty());
    }

    #[test]
    fn sidecar_rejects_bad_magic() {
        let mut buf = Vec::new();
        write_sidecar_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        buf[0] = b'X';
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn sidecar_rejects_truncated_checksums() {
        let mut buf = Vec::new();
        let checksums = vec![BlockChecksum { block_number: 1, checksum: 2 }];
        write_sidecar_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &checksums)
            .unwrap();
        buf.truncate(buf.len() - 4);
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn sidecar_v2_roundtrip() {
        let info = SidecarInfo::new_full("job_full.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![
            BlockChecksum { block_number: 0, checksum: 0xDEAD },
            BlockChecksum { block_number: 5, checksum: 0xBEEF },
            BlockChecksum { block_number: 15, checksum: u64::MAX },
        ];

        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &info, &manifest, &checksums).unwrap();

        // 1. Read entire sidecar
        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.container, "vhdx");
        assert_eq!(sidecar.info.chain_role, ChainRole::Full);
        assert_eq!(sidecar.info.image_file_name, "job_full.vhdx");
        assert_eq!(sidecar.info.volume_data_offset, 1024 * 1024);
        assert_eq!(sidecar.info.format_version, 2);
        assert_eq!(sidecar.manifest.block_size, 65536);
        assert_eq!(sidecar.checksums, checksums);

        // 2. Read header only
        let (read_info, read_manifest) = read_sidecar_header_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read_info.format_version, 2);
        assert_eq!(read_info.image_file_name, "job_full.vhdx");
        assert_eq!(read_manifest.block_size, 65536);

        // Plain per-volume sidecar carries no whole-disk manifest.
        assert!(sidecar.disk_manifest.is_none());
    }

    #[test]
    fn sidecar_v2_embeds_disk_manifest() {
        use own_backup_common::gpt::{
            DiskBackupManifest, DiskImageRef, DiskLayout, GptPartitionEntry,
        };
        let info = SidecarInfo::new_full("modulator_disk3.vhdx", 1024 * 1024);
        let manifest = make_manifest();
        let checksums = vec![BlockChecksum { block_number: 0, checksum: 7 }];
        let disk = DiskBackupManifest {
            format_version: DiskBackupManifest::FORMAT_VERSION,
            created_at: "2026-07-11T00:00:00Z".into(),
            job_id: Some("job-1".into()),
            disk_number: 3,
            disk_model: "Samsung SSD 970 PRO 512GB".into(),
            layout: DiskLayout {
                disk_guid: "11111111-2222-3333-4444-555555555555".into(),
                sector_size: 512,
                size_bytes: 512 * 1024 * 1024 * 1024,
                partitions: vec![GptPartitionEntry {
                    index: 1,
                    type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                    part_guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    first_lba: 2048,
                    last_lba: 4095,
                    attributes: 0,
                    name: "EFI".into(),
                }],
            },
            images: vec![DiskImageRef {
                part_index: 1,
                token: "VE7F57C39".into(),
                image_file_name: "modulator_disk3.vhdx".into(),
                filesystem: "fat32".into(),
                encryption: None,
            }],
            source_disk: None,
        };

        let mut buf = Vec::new();
        write_sidecar_v2_full_to(&mut buf, &info, &manifest, &checksums, Some(&disk)).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        let got = sidecar.disk_manifest.expect("embedded disk manifest");
        assert_eq!(got.disk_number, 3);
        assert_eq!(got.disk_model, "Samsung SSD 970 PRO 512GB");
        assert_eq!(got.layout.partitions.len(), 1);
        assert_eq!(got.layout.partitions[0].type_guid, own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM);
        assert_eq!(got.images[0].token, "VE7F57C39");
        assert_eq!(sidecar.checksums, checksums);
    }

    #[test]
    fn sidecar_v2_roundtrip_empty_checksums() {
        let info = SidecarInfo::new_full("job_full.vhdx", 0);
        let manifest = make_manifest();

        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &info, &manifest, &[]).unwrap();

        let sidecar = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(sidecar.info.format_version, 2);
        assert!(sidecar.checksums.is_empty());
    }

    #[test]
    fn sidecar_v2_rejects_bad_magic() {
        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        buf[0] = b'X';
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn sidecar_v2_rejects_corrupted_hash() {
        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &SidecarInfo::new_full("x.vhdx", 0), &make_manifest(), &[])
            .unwrap();
        let len = buf.len();
        // Corrupt the xxhash64 at the end of the blob
        buf[len - 1] ^= 0xFF;
        assert!(read_sidecar_from(&mut Cursor::new(&buf)).is_err());
    }

    #[test]
    fn test_read_embedded_sidecar_qcow2() {
        use crate::gpt_writer::{gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
        use crate::qcow2::{Qcow2Writer, Qcow2WriterOptions, Qcow2Compression};

        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("test.qcow2");

        // 1. Setup layout and construct sidecar info
        let sector_size = 512;
        let volume_len = 8 * 1024 * 1024; // 8 MiB volume
        let block_size = 65536;
        let total_blocks = volume_len / block_size;
        let meta_size = calculate_meta_partition_size(total_blocks, 0);
        let disk_size = DEFAULT_PARTITION_OFFSET + volume_len + meta_size + 1024 * 1024; // 1 MiB tail

        let meta_partition_guid = uuid::Uuid::new_v4();
        let layout = GptLayout {
            disk_size,
            sector_size,
            partition_offset: DEFAULT_PARTITION_OFFSET,
            partition_size: volume_len,
            disk_guid: uuid::Uuid::new_v4(),
            partition_guid: uuid::Uuid::new_v4(),
            partition_name: "OwnBackup".into(),
            meta_partition_size: Some(meta_size),
            meta_partition_guid: Some(meta_partition_guid),
        };

        // Initialize mock disk memory buffer
        let mut disk_buf = vec![0u8; disk_size as usize];

        // Write the GPT layout
        for op in gpt_write_ops(&layout).unwrap() {
            disk_buf[op.offset as usize..op.offset as usize + op.bytes.len()].copy_from_slice(&op.bytes);
        }

        // Setup the sidecar blob
        let mut info = SidecarInfo::new_full("test.qcow2", DEFAULT_PARTITION_OFFSET);
        info.container = "qcow2".into();
        info.job_id = Some("test_job".into());
        let manifest = make_manifest();
        let checksums = vec![BlockChecksum { block_number: 2, checksum: 12345 }];

        let mut sidecar_bytes = Vec::new();
        write_sidecar_v2_to(&mut sidecar_bytes, &info, &manifest, &checksums).unwrap();

        // Write the sidecar blob to the meta partition guest offset in our disk buffer
        let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len) / sector_size as u64 - 1;
        let meta_offset = (last_lba + 1) * sector_size as u64;
        disk_buf[meta_offset as usize..meta_offset as usize + sidecar_bytes.len()].copy_from_slice(&sidecar_bytes);

        // 2. Build the qcow2 image file using Qcow2Writer
        let file = std::fs::File::create(&image_path).unwrap();
        let mut qcow_writer = Qcow2Writer::new(
            std::io::BufWriter::new(file),
            Qcow2WriterOptions {
                virtual_size: disk_size,
                compression: Qcow2Compression::None,
                ..Default::default()
            },
        ).unwrap();

        let cs = 64 * 1024;
        let mut cluster_index = 0;
        while cluster_index * cs < disk_size {
            let chunk_start = (cluster_index * cs) as usize;
            let chunk_end = std::cmp::min(disk_size as usize, chunk_start + cs as usize);
            let chunk = &disk_buf[chunk_start..chunk_end];
            if chunk.iter().any(|&b| b != 0) {
                let mut cluster_data = vec![0u8; cs as usize];
                cluster_data[..chunk.len()].copy_from_slice(chunk);
                qcow_writer.write_cluster(cluster_index, &cluster_data).unwrap();
            }
            cluster_index += 1;
        }
        qcow_writer.finish().unwrap();

        // 3. Read the embedded sidecar back and verify!
        let read_sidecar = read_embedded_sidecar(&image_path, "qcow2").unwrap();
        assert_eq!(read_sidecar.info.image_file_name, "test.qcow2");
        assert_eq!(read_sidecar.info.container, "qcow2");
        assert_eq!(read_sidecar.info.job_id.as_deref(), Some("test_job"));
        assert_eq!(read_sidecar.checksums, checksums);

        let (read_info, read_manifest) = read_embedded_sidecar_header(&image_path, "qcow2").unwrap();
        assert_eq!(read_info.image_file_name, "test.qcow2");
        assert_eq!(read_manifest.block_size, 65536);

        // Renaming a backup is an ordinary thing to do, and the extension used
        // to decide which container to open — so the same file under a name
        // with no extension had to keep reading. Before the magic-byte check it
        // failed with "unsupported container format: ".
        let renamed = image_path.parent().unwrap().join("monday-copy");
        std::fs::rename(&image_path, &renamed).unwrap();
        let after_rename = read_embedded_sidecar(&renamed, "").unwrap();
        assert_eq!(after_rename.info.image_file_name, "test.qcow2");
        assert_eq!(after_rename.checksums, checksums);
    }

    /// The container is decided by the file's first bytes, never by its name.
    #[test]
    fn container_format_comes_from_the_magic_not_the_name() {
        let dir = tempfile::tempdir().unwrap();

        // A VHDX called `.qcow2` is still a VHDX, and the other way round.
        let lying_vhdx = dir.path().join("actually-vhdx.qcow2");
        std::fs::write(&lying_vhdx, b"vhdxfile\x00\x00\x00\x00").unwrap();
        assert_eq!(detect_container_format(&lying_vhdx).unwrap(), "vhdx");

        let lying_qcow = dir.path().join("actually-qcow2.vhdx");
        std::fs::write(&lying_qcow, b"QFI\xfb\x00\x00\x00\x03").unwrap();
        assert_eq!(detect_container_format(&lying_qcow).unwrap(), "qcow2");

        // Discrimination: a file that is neither must say so, not guess. A
        // short file must not be read past its end either.
        let junk = dir.path().join("notes.vhdx");
        std::fs::write(&junk, b"hello there, not an image").unwrap();
        assert!(detect_container_format(&junk).is_err());

        let tiny = dir.path().join("tiny.vhdx");
        std::fs::write(&tiny, b"vhdx").unwrap();
        assert!(detect_container_format(&tiny).is_err());
    }

    // --- Пределы памяти при чтении таблицы (аудит AUD-15) -------------------
    //
    // Заголовок разбирается раньше счётчика, и в нём уже есть размер источника
    // и размер блока. Значит, сколько контрольных сумм в файле МОЖЕТ быть —
    // известно до того, как счётчику поверили. Раньше ему верили до 2^32 и
    // сразу звали `Vec::with_capacity`: сорок байт мусора просили 64 ГиБ.

    /// Счётчик, которого не может быть при таком образе, отвергается сразу —
    /// и, что важнее, ДО всякой большой аллокации.
    #[test]
    fn a_count_larger_than_the_image_is_refused_at_once() {
        let mut buf = Vec::new();
        buf.extend_from_slice(SIDECAR_MAGIC);
        let json = serde_json::to_vec(&JsonHeader {
            info: SidecarInfo::new_full("x.vhdx", 0),
            manifest: make_manifest(), // 1 MiB источника, блок 64 KiB => 16 блоков
            disk_manifest: None,
        })
        .unwrap();
        buf.extend_from_slice(&(json.len() as u32).to_le_bytes());
        buf.extend_from_slice(&json);
        // Столько записей этот образ вместить не может ни при каком раскладе.
        buf.extend_from_slice(&(u32::MAX as u64).to_le_bytes());
        // И ни одной записи за счётчиком нет — файл кончается здесь.

        let started = std::time::Instant::now();
        let err = read_sidecar_from(&mut Cursor::new(&buf)).unwrap_err();
        assert!(
            format!("{err}").contains("вмещает не больше"),
            "должна быть сказана причина, а не EOF: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "отказ должен быть мгновенным, без попытки выделить память под таблицу"
        );
    }

    /// То же для v2 — у него счётчик ещё и задаёт потолок распаковки таблицы.
    #[test]
    fn a_v2_count_larger_than_the_image_is_refused_at_once() {
        let info = SidecarInfo::new_full("x.vhdx", 0);
        let manifest = make_manifest();
        let mut buf = Vec::new();
        write_sidecar_v2_to(&mut buf, &info, &manifest, &[]).unwrap();

        // Подменяем счётчик на заведомо невозможный. Контрольная сумма шапки
        // после этого не сойдётся, поэтому проверку размера обязана пройти
        // ДО неё — иначе она бесполезна: сначала выделим память, потом узнаем.
        let json_len = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let count_at = 8 + 4 + json_len;
        buf[count_at..count_at + 8].copy_from_slice(&(u32::MAX as u64).to_le_bytes());

        let err = read_sidecar_from(&mut Cursor::new(&buf)).unwrap_err();
        assert!(
            format!("{err}").contains("вмещает не больше"),
            "размер должен проверяться раньше хеша шапки: {err}"
        );
    }

    /// Честный файл читается как читался — предел не должен задевать край.
    #[test]
    fn a_table_that_fills_the_image_exactly_still_reads() {
        let info = SidecarInfo::new_full("x.vhdx", 0);
        let manifest = make_manifest(); // 16 блоков
        let checksums: Vec<_> = (0..16)
            .map(|i| BlockChecksum { block_number: i, checksum: i * 7 })
            .collect();

        for v2 in [false, true] {
            let mut buf = Vec::new();
            if v2 {
                write_sidecar_v2_to(&mut buf, &info, &manifest, &checksums).unwrap();
            } else {
                write_sidecar_to(&mut buf, &info, &manifest, &checksums).unwrap();
            }
            let read = read_sidecar_from(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(read.checksums.len(), 16, "v2={v2}");
            assert_eq!(read.checksums[15].block_number, 15, "v2={v2}");
        }
    }

    /// Старый sidecar без размера источника читается по-прежнему: поддержку
    /// допустимых старых форматов предел ломать не должен.
    #[test]
    fn a_header_that_does_not_say_the_size_falls_back_to_the_absolute_cap() {
        let mut manifest = make_manifest();
        manifest.source_disk.size_bytes = 0;
        assert_eq!(checksum_limit(&manifest), MAX_CHECKSUMS);

        let mut manifest = make_manifest();
        manifest.block_size = 0;
        assert_eq!(checksum_limit(&manifest), MAX_CHECKSUMS);
    }

    /// Накопление дельт складывалось без проверки: подделанный шаг
    /// оборачивался (release) или ронял программу (debug). И номер блока за
    /// краем образа — не просто странность: `verify_image` считает по нему
    /// `volume_len - offset`, и это вычитание уходит в минус.
    #[test]
    fn a_block_number_past_the_end_of_the_image_is_an_error_not_a_panic() {
        let info = SidecarInfo::new_full("x.vhdx", 0);
        let manifest = make_manifest(); // 16 блоков
        let checksums = vec![BlockChecksum { block_number: u64::MAX - 1, checksum: 1 }];

        for v2 in [false, true] {
            let mut buf = Vec::new();
            if v2 {
                write_sidecar_v2_to(&mut buf, &info, &manifest, &checksums).unwrap();
            } else {
                write_sidecar_to(&mut buf, &info, &manifest, &checksums).unwrap();
            }
            let err = read_sidecar_from(&mut Cursor::new(&buf)).unwrap_err();
            assert!(
                format!("{err}").contains("за пределами"),
                "v2={v2}: {err}"
            );
        }
    }

    // --- Записи загрузки живут внутри образа (2026-09-20) -------------------
    //
    // Раньше они лежали рядом файлом `<образ>.boot.json` — единственный кусок
    // копии, который был не в копии, и именно тот, который пользователь унесёт
    // на другой диск, не заметив, что он остался.

    fn a_boot_backup(machine: &str) -> crate::efivars::BootBackup {
        crate::efivars::BootBackup {
            format: crate::efivars::BACKUP_FORMAT.into(),
            version: crate::efivars::BACKUP_VERSION,
            machine: machine.into(),
            taken_at: "2026-09-19T21:53:53Z".into(),
            variables: vec![crate::efivars::SavedVar::new("Boot0000", 7, b"whatever")],
            readable: vec![crate::efivars::SavedNote {
                name: "Boot0000".into(),
                description: "Windows 11".into(),
                file: Some(r"\EFI\MICROSOFT\BOOT\BOOTMGFW.EFI".into()),
                partition_guid: None,
            }],
        }
    }

    /// Положили внутрь — оттуда и читаются.
    #[test]
    fn boot_entries_written_into_the_image_are_read_back_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("copy.vhdx");
        std::fs::write(&image, b"not really an image").unwrap();

        let mut info = SidecarInfo::new_full("copy.vhdx", 0);
        info.boot_entries = Some(a_boot_backup("MODULATOR"));
        write_sidecar(&sidecar_path_for(&image), &info, &make_manifest(), &[]).unwrap();

        let read = boot_entries_of_image(&image).expect("the copy carries them");
        assert_eq!(read.machine, "MODULATOR");
        assert_eq!(read.readable[0].description, "Windows 11");
    }

    /// Уже открытый сайдкар отвечает сам, не перечитывая файл.
    ///
    /// Для зашифрованной копии это единственный работающий путь: её сайдкар
    /// запечатан, и чтение файла без ключа отвечает «записей нет» о копии, в
    /// которой их четыре. Запасной путь к файлу рядом при этом сохраняется —
    /// он для копий, снятых до переезда записей внутрь.
    #[test]
    fn an_open_sidecar_answers_about_its_own_boot_entries() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("copy.obk");

        let mut info = SidecarInfo::new_full("copy.obk", 0);
        info.boot_entries = Some(a_boot_backup("MODULATOR"));
        let sidecar = Sidecar {
            info,
            manifest: make_manifest(),
            checksums: Vec::new(),
            disk_manifest: None,
        };
        // Ничего не лежит ни рядом, ни в файле — ответ приходит из сайдкара.
        assert!(!image.exists());
        let read = boot_entries_of(&sidecar, &image).expect("из открытого сайдкара");
        assert_eq!(read.machine, "MODULATOR");

        // А сайдкар без записей всё так же смотрит в файл рядом.
        let plain = Sidecar {
            info: SidecarInfo::new_full("copy.obk", 0),
            manifest: make_manifest(),
            checksums: Vec::new(),
            disk_manifest: None,
        };
        assert!(boot_entries_of(&plain, &image).is_none());
        std::fs::write(
            boot_entries_path_beside(&image),
            serde_json::to_string(&a_boot_backup("OLDER")).unwrap(),
        )
        .unwrap();
        assert_eq!(boot_entries_of(&plain, &image).unwrap().machine, "OLDER");
    }

    /// Копия, снятая до переезда, хранит их рядом — и это по-прежнему годная
    /// копия, поэтому файл читается как запасной путь.
    #[test]
    fn an_older_copy_keeps_them_beside_it_and_that_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("copy.vhdx");
        std::fs::write(&image, b"not really an image").unwrap();
        // Sidecar без записей — как у всех копий до этого изменения.
        let info = SidecarInfo::new_full("copy.vhdx", 0);
        write_sidecar(&sidecar_path_for(&image), &info, &make_manifest(), &[]).unwrap();

        let beside = boot_entries_path_beside(&image);
        assert_eq!(beside.file_name().unwrap(), "copy.vhdx.boot.json");
        std::fs::write(&beside, a_boot_backup("OLDBOX").to_json().unwrap()).unwrap();

        let read = boot_entries_of_image(&image).expect("the file beside it");
        assert_eq!(read.machine, "OLDBOX");
    }

    /// Внутреннее старшинствует: если обе копии на месте, читается та, что в
    /// образе, — она точно принадлежит ему.
    #[test]
    fn what_is_inside_the_image_wins_over_what_lies_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("copy.vhdx");
        std::fs::write(&image, b"not really an image").unwrap();

        let mut info = SidecarInfo::new_full("copy.vhdx", 0);
        info.boot_entries = Some(a_boot_backup("INSIDE"));
        write_sidecar(&sidecar_path_for(&image), &info, &make_manifest(), &[]).unwrap();
        std::fs::write(
            boot_entries_path_beside(&image),
            a_boot_backup("BESIDE").to_json().unwrap(),
        )
        .unwrap();

        assert_eq!(boot_entries_of_image(&image).unwrap().machine, "INSIDE");
    }

    /// Копия без записей — обычное дело: не загрузочная, машина на BIOS, или
    /// прошивку не дали прочитать. Это не ошибка и не повод что-то искать.
    #[test]
    fn a_copy_without_boot_entries_simply_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("copy.vhdx");
        std::fs::write(&image, b"not really an image").unwrap();
        let info = SidecarInfo::new_full("copy.vhdx", 0);
        write_sidecar(&sidecar_path_for(&image), &info, &make_manifest(), &[]).unwrap();

        assert!(boot_entries_of_image(&image).is_none());
    }

    /// Место под них резервируется по измеренному размеру, а не на глаз: они
    /// читаются до того, как считается размер служебного раздела.
    #[test]
    fn the_room_reserved_for_them_is_measured_not_guessed() {
        assert_eq!(boot_entries_reserve(None), 0, "нечего — нечего и резервировать");
        let backup = a_boot_backup("MODULATOR");
        let want = boot_entries_reserve(Some(&backup));
        let json = backup.to_json().unwrap().len() as u64;
        assert!(want > json, "резерв должен покрывать сами данные и обрамление");
    }

    /// Несколько байт разницы в резерве — это дрожание, а не разница в
    /// потребности: резерв меряется по сериализованным загрузочным записям, а
    /// в их JSON лежит метка времени, длина которой гуляет от запуска к
    /// запуску. Размер округляется до мегабайта, и такое дрожание в нём не
    /// видно — потому сравнивать надо размеры, а не резервы.
    ///
    /// Числа взяты с живого прогона 2026-09-22, где сравнение резервов дало
    /// «18730 > 18727» и каждая копия цепочки диска выходила полной.
    #[test]
    fn a_few_bytes_of_jitter_do_not_change_the_meta_size() {
        let blocks = 41_690;
        assert_eq!(
            calculate_meta_partition_size(blocks, 18_727),
            calculate_meta_partition_size(blocks, 18_730),
        );
        // А настоящая разница — видна: том, подросший на десятки тысяч блоков,
        // требует больше места под таблицу сумм, и это уже причина начать
        // цепочку заново.
        assert!(
            calculate_meta_partition_size(blocks + 100_000, 18_727)
                > calculate_meta_partition_size(blocks, 18_730)
        );
    }

    /// Место, зарезервированное родителем, ребёнок считает ПО СОХРАНЁННОМУ
    /// сайдкару — и должен получить то же число, что родитель посчитал по
    /// живым записям. Иначе ребёнку «не хватает места» там, где его хватает.
    ///
    /// Так и случилось на живом прогоне 2026-09-22: резерв ребёнка считал
    /// загрузочные записи, а резерв родителя — нет, и каждая копия цепочки
    /// диска молча выходила полной, в файле с именем `_inc_`. Здесь закреплена
    /// та половина, которую можно проверить без Windows: запись и чтение
    /// сайдкара не меняют размер, по которому считается резерв.
    #[test]
    fn a_parents_boot_entries_reserve_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let entries = a_boot_backup("MODULATOR");
        let live = boot_entries_reserve(Some(&entries));
        assert!(live > 0, "записи есть — резерв не нулевой");

        let mut info = SidecarInfo::new_full("parent.qcow2", 0);
        info.boot_entries = Some(entries);
        let path = dir.path().join("parent.qcow2.obkm");
        write_sidecar(&path, &info, &make_manifest(), &[]).unwrap();

        let back = read_sidecar(&path).unwrap();
        assert_eq!(
            boot_entries_reserve(back.info.boot_entries.as_ref()),
            live,
            "ребёнок должен увидеть у родителя ровно тот резерв, который тот заложил"
        );
    }

    /// «Годится ли эта копия в родители» для зашифрованной копии отвечается
    /// БЕЗ ключа — по открытому заголовку, тому же, по которому её находят
    /// списки и ретеншн.
    ///
    /// Пока здесь отвечалось «нет», планировщик считал родителя непригодным и
    /// каждый запуск шифрованного задания делал новую полную копию: цепочка не
    /// складывалась никогда.
    #[test]
    fn an_encrypted_image_can_be_a_chain_parent_without_a_key() {
        use crate::seal::obk::container_tests::Fixture;
        use crate::seal::obk::ObkWriter;
        use uuid::Uuid;

        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("j_C_full_1.obk");
        let mut w = ObkWriter::create(
            &image,
            &f.chain,
            f.options(Uuid::new_v4(), ChainRole::Full, None),
        )
        .unwrap();
        w.write_cluster(0, &vec![7u8; 4096]).unwrap();
        w.finish(b"sealed sidecar").unwrap();

        assert!(!sidecar_path_for(&image).exists(), "рядом ничего не лежит");
        assert!(
            image_sidecar_readable(&image),
            "зашифрованная копия обязана годиться в родители"
        );

        // А вот файл, который лишь называется образом, — нет.
        let bogus = dir.path().join("j_C_full_2.obk");
        std::fs::write(&bogus, b"not an image at all").unwrap();
        assert!(!image_sidecar_readable(&bogus));
    }
}
