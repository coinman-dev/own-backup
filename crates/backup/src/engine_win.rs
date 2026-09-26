//! Windows volume backup/restore engine (plan v2, phase 1).
//!
//! Ties the building blocks together:
//! [`WinBlockReader`] → [`NtfsUsedSectorMap`] → [`copy_used_blocks`] →
//! attached VHDX ([`crate::vhdx`]) + GPT ([`crate::gpt_writer`]) + sidecar.
//!
//! v1 scope: **volume** backups (`C:`, `\\.\D:`, VSS snapshot paths). The
//! image is a GPT disk with a single Basic Data partition holding the raw
//! volume — directly mountable by Windows. Whole-disk backups and VSS
//! integration come in later phases.

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use own_backup_common::disk::DiskInfo;
use own_backup_common::gpt::DiskBackupManifest;
use own_backup_common::image::ImageManifest;
use own_backup_common::progress::{BackupProgress, RestoreProgress};
use own_backup_common::types::{
    BackupMode, BackupStatus, BackupType, BootMode, CompressionAlgo, ImageFormat, PartitionTable,
};
use own_backup_common::{tr, OwnBackupError, Result};
use own_backup_snapshot::VssSnapshotSession;

use crate::block_reader::BlockReader;
use crate::block_reader_win::WinBlockReader;
use crate::checksum::compute_block_checksum;
use crate::exclusions::{ByteRanges, ZeroingBlockReader};
use crate::exclusions_win::collect_excluded_ranges;
use crate::gpt_writer::{disk_gpt_write_ops, gpt_write_ops, GptLayout, DEFAULT_PARTITION_OFFSET};
use crate::raw_copy::copy_used_blocks;
use crate::sidecar::{
    read_embedded_sidecar, read_sidecar, sidecar_path_for,
    write_sidecar, ChainRole, Sidecar, SidecarInfo,
};
use crate::used_sectors::{AllUsedMap, UsedSectorMap};
use crate::used_sectors_win::NtfsUsedSectorMap;
use crate::vhdx::{
    attach_vhdx, create_differencing_vhdx, create_dynamic_vhdx, detach_vhdx, merge_into_parent, AttachMode,
    VhdxCreateOptions,
};
use crate::block_source::BlockSource;
use crate::vhdx_bat::allocated_payload_blocks;
use crate::win_device::{normalize_device_path, WinRawDevice};

/// Default backup block size: 1 MiB (multiple of every real sector size).
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;

const MIB: u64 = 1024 * 1024;

/// Why there is no by-type breakdown behind the most ordinary copy of all: the
/// file walk is the exclusion walk, and with no rules to apply nobody walks.
fn no_patterns() -> String {
    tr!("engine_win.no_patterns").into_owned()
}

/// Parameters for a volume backup.
pub struct VolumeBackupRequest {
    /// Source volume: `C:`, `\\.\C:` or a VSS snapshot device path.
    pub source: String,
    /// Output image path (`.vhdx`/`.avhdx`). Must not exist yet.
    pub image_path: PathBuf,
    /// Backup block size in bytes; use [`DEFAULT_BLOCK_SIZE`].
    pub block_size: u32,
    /// Copy every block even when an NTFS bitmap is available.
    pub all_sectors: bool,
    /// Take a VSS shadow copy of the volume and read from it (consistent
    /// backup of a live volume). Falls back to reading the live volume with a
    /// warning if VSS fails. Ignored for non-volume sources.
    pub use_vss: bool,
    /// Full / Incremental / Differential (phase 5). Non-full kinds need
    /// `parent_image`; if the chain turns out unusable (parent missing,
    /// volume resized, block size changed) the engine falls back to a full
    /// backup — the summary reports what was actually done.
    pub kind: BackupType,
    /// Parent image for incremental (previous version) or differential (the
    /// chain's full backup).
    pub parent_image: Option<PathBuf>,
    /// File-exclusion glob patterns (phase 3). Applied only in smart NTFS
    /// mode; excluded clusters are dropped from the used map and zeroed
    /// inside blocks that are still copied.
    pub exclusion_patterns: Vec<String>,
    /// GUI job id, stamped into the sidecar so retention can find this
    /// job's chains later.
    pub job_id: Option<String>,
    /// Passphrase for an encrypted (LUKS) source. When present and the source
    /// is a LUKS container, the volume is unlocked and its *decrypted* data is
    /// backed up (used blocks only); on restore, encryption must be re-applied
    /// manually. Ignored for non-encrypted sources.
    pub passphrase: Option<Zeroizing<String>>,
    /// Image container format (decision #13): VHDX — kernel-written, Windows-
    /// native mounting, NTFS/ReFS true removal, AVHDX chains; qcow2 — written
    /// in userspace with per-cluster zstd compression, no attach at all.
    /// qcow2 limits for now: full backups only, exclusions are zeroing-only.
    pub image_format: ImageFormat,
    /// Full source-disk layout snapshot (the parent physical disk of this
    /// volume, from the GUI's live inventory) to embed in the sidecar, so a
    /// restore can show the whole disk and flag the backed-up partition without
    /// jobs.json. `None` for CLI runs (the sidecar then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
    /// Present for an encrypted job: the keys the service opened before this
    /// run started. Nothing inside the run asks a human anything (§7.4), and
    /// the image written is `.obk` whatever `image_format` says.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// zstd level for qcow2 and `.obk` (1–19; anything else is the default,
    /// see [`crate::qcow2::zstd_level_or_default`]). VHDX is not compressed.
    pub zstd_level: i32,
}

/// Outcome of a completed volume backup.
pub struct VolumeBackupSummary {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub source_bytes: u64,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub blocks_copied: u64,
    pub bytes_copied: u64,
    pub duration: Duration,
    /// True when the NTFS used-sector bitmap was applied; false = every
    /// block was copied (explicit sector-by-sector or non-NTFS fallback).
    pub smart_mode: bool,
    /// True when the data was read from a VSS shadow copy.
    pub vss_used: bool,
    /// What was actually produced — may differ from the requested kind when
    /// the engine fell back to a full backup.
    pub backup_kind: BackupType,
    /// Used blocks skipped because they match the parent version.
    pub blocks_unchanged: u64,
    /// Files whose extents were excluded (phase 3).
    pub excluded_files: u64,
    /// Bytes excluded from the image by exclusion patterns.
    pub excluded_bytes: u64,
    /// Per-category byte totals of the files in the archive; None when no
    /// exclusion walk ran (exclusions disabled / non-NTFS / sector mode).
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Why the file walk did not run, in words for the result screen. `None`
    /// when it did run.
    ///
    /// One walk produces both the exclusions and the by-type breakdown, so one
    /// sentence explains both absences. It exists because a copy that quietly
    /// ignored twelve exclusion rules and then reported "Исключено файлов: 0"
    /// tells the user the rules matched nothing — when in fact they were never
    /// applied. That difference is the whole point of having the rules.
    pub file_scan_skipped: Option<String>,
}

/// Write target behind the format switch. VHDX goes through the kernel
/// (attach → raw writes to the PhysicalDrive node); qcow2 is written entirely
/// in userspace with per-cluster zstd compression — no attach, no SAN-policy
/// juggling, no flush pacing (the OS cache never holds a whole image).
enum ImageTarget {
    Vhdx {
        writer: crate::vhdx_format::VhdxWriter,
    },
    Qcow2 {
        writer: crate::qcow2::Qcow2StagedWriter<std::io::BufWriter<std::fs::File>>,
    },
    /// The encrypted container: the qcow2 path with our incompatible
    /// extension. Its sidecar is sealed inside instead of living in a guest
    /// meta partition, so `finalize` is where it goes in.
    Obk {
        writer: Box<crate::seal::obk::ObkStagedWriter<std::io::BufWriter<std::fs::File>>>,
    },
}

impl ImageTarget {
    /// Virtual disk size of the image being written.
    fn length(&self) -> u64 {
        match self {
            ImageTarget::Vhdx { writer } => writer.length(),
            ImageTarget::Qcow2 { writer } => writer.length(),
            ImageTarget::Obk { writer } => writer.length(),
        }
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match self {
            ImageTarget::Vhdx { writer } => writer.write_at(offset, data),
            ImageTarget::Qcow2 { writer } => writer.write_at(offset, data),
            ImageTarget::Obk { writer } => writer.write_at(offset, data),
        }
    }

    /// Finalize the image and log per-step timings.
    ///
    /// `sealed` is the sidecar of an encrypted image, with `reserve` bytes of
    /// room so the final one — the exclusion figures and the checksums the
    /// deletion pass touched are known only later — can replace it in place.
    /// The other two containers keep their sidecar in a guest meta partition
    /// and ignore both.
    fn finalize(self, sealed: &[u8], reserve: usize) -> Result<()> {
        match self {
            ImageTarget::Obk { writer } => {
                let t = Instant::now();
                // Records, index, sealed sidecar, tables, header — the header
                // last, which is this container's commit point.
                let buf = writer.finish_with_reserve(sealed, reserve)?;
                let file = buf
                    .into_inner()
                    .map_err(|e| OwnBackupError::Io(e.into_error()))?;
                file.sync_all()?;
                info!(finish_ms = t.elapsed().as_millis(), "obk finalize timings");
            }
            ImageTarget::Vhdx { writer } => {
                let t = Instant::now();
                writer.finish()?;
                info!(finish_ms = t.elapsed().as_millis(), "VHDX finalize timings");
            }
            ImageTarget::Qcow2 { writer } => {
                let t = Instant::now();
                // Write L2/L1/refcounts + header, then make the file durable.
                let buf = writer.finish()?;
                let file = buf
                    .into_inner()
                    .map_err(|e| OwnBackupError::Io(e.into_error()))?;
                file.sync_all()?;
                info!(finish_ms = t.elapsed().as_millis(), "qcow2 finalize timings");
            }
        }
        Ok(())
    }
}

/// Read source behind the same format switch, picked by the sidecar's
/// `container` field. BOTH formats are read in userspace now — no attach.
///
/// Reading a VHDX used to attach it read-only, and that attach was the last
/// place an image could still be mounted by Windows. `NO_DRIVE_LETTER`
/// withholds the letter, not the mount: the volumes surfaced anyway, and the
/// detach afterwards waited out the ~180 s PnP removal veto (measured
/// 2026-08-21: `verify: image OK` at 15:43:20, the backup "finished" at
/// 15:46:20). The write path can dismiss those volumes because it can take the
/// disk offline; a read attach has no writable handle to do that with, so the
/// volume comes straight back and the veto is unavoidable.
///
/// So the read path stopped attaching. `vhdx_bat::VhdxGuestReader` resolves the
/// differencing chain from the files themselves — the same reader the embedded
/// sidecar has always been read with — which makes VHDX reads work exactly like
/// qcow2 reads: plain file I/O, no device, no volume, nothing to detach, and
/// nothing that depends on a SAN policy taking effect. It is also what the
/// measurements said to expect: ~260 s for 3.4 GiB either way (CLAUDE.md).
pub(crate) enum ImageReadSource {
    Vhdx(Box<crate::vhdx_bat::VhdxGuestReader>),
    Qcow2(crate::qcow2::Qcow2RandomReader),
    Obk(Box<crate::seal::obk::ObkRandomReader>),
}

impl ImageReadSource {
    pub(crate) fn open(image_path: &Path, container: &str) -> Result<Self> {
        Self::open_with(image_path, container, None)
    }

    /// `key` opens an encrypted image and its whole chain. Without it an `.obk`
    /// is refused by name rather than half-read.
    pub(crate) fn open_with(
        image_path: &Path,
        container: &str,
        key: Option<&crate::seal::ChainKey>,
    ) -> Result<Self> {
        match (container, key) {
            ("qcow2", _) => Ok(Self::Qcow2(crate::qcow2::Qcow2RandomReader::open(image_path)?)),
            ("vhdx", _) => Ok(Self::Vhdx(Box::new(crate::vhdx_bat::VhdxGuestReader::open(image_path)?))),
            ("obk", Some(key)) => Ok(Self::Obk(Box::new(
                crate::seal::obk::ObkRandomReader::open(image_path, key)?,
            ))),
            ("obk", None) => Err(OwnBackupError::Encryption(
                tr!("engine_win.encrypted_needs_key", image = image_path.display()).into_owned(),
            )),
            (other, _) => Err(OwnBackupError::ImageFormat(format!(
                "unknown image container '{other}' in sidecar"
            ))),
        }
    }

    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Vhdx(r) => r.read_at(offset, buf),
            Self::Qcow2(r) => r.read_at(offset, buf),
            Self::Obk(r) => r.read_at(offset, buf),
        }
    }
}

/// Open a freshly-attached virtual disk for writing; the PhysicalDrive device
/// node can take a moment to appear after AttachVirtualDisk.
fn open_write_with_retry(path: &str) -> Result<WinRawDevice> {
    let mut last_err: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| OwnBackupError::Device(format!("cannot open {path}"))))
}



/// Back up a volume into a freshly created, directly mountable VHDX.
/// Данные исключённых файлов в копию не попадают в любом случае — они
/// вычитаются из карты ДО копирования. Записи из каталога образа убирает
/// драйвер файловой системы, и чтобы он их увидел, образ надо смонтировать:
/// VHDX монтирует сама Windows, qcow2 — наша iSCSI-цель ([`crate::attach_win`]).
/// Ставить с этим никаких условий больше не нужно: с 2026-09-18 стороннего
/// драйвера нет, а инициатор есть в любой Windows.
#[cfg(target_os = "windows")]
/// Отправить в интерфейс разбивку по типам файлов, как только она посчитана.
///
/// Одним сообщением и один раз: гонять её в каждом отчёте о ходе незачем —
/// она не меняется до конца копирования.
#[cfg(target_os = "windows")]
fn report_categories(
    progress: &mut impl FnMut(&BackupProgress) -> Result<()>,
    categories: Option<&crate::exclusions::CategorySizes>,
    excluded_files: u64,
    excluded_bytes: u64,
) {
    let Some(c) = categories else { return };
    let _ = progress(&BackupProgress {
        backup_id: "prepare".into(),
        status: BackupStatus::Running,
        categories: Some(c.as_pairs()),
        excluded: Some((excluded_files, excluded_bytes)),
        ..Default::default()
    });
}

/// What an encrypted run may be asked to write, checked before anything is
/// created. Encryption is the qcow2 path with our incompatible extension, and
/// listings and retention find those images by their `.obk` name.
fn check_encrypted_target(
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    format: ImageFormat,
    image_path: &Path,
) -> Result<()> {
    let Some(_) = encryption else { return Ok(()) };
    if format != ImageFormat::Qcow2 {
        return Err(OwnBackupError::Config(
            tr!("engine_win.encryption_needs_qcow2").into_owned(),
        ));
    }
    if image_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
        != Some(crate::seal::obk::OBK_EXTENSION)
    {
        return Err(OwnBackupError::Config(
            tr!(
                "engine_win.encrypted_image_extension",
                ext = crate::seal::obk::OBK_EXTENSION,
                image = image_path.display()
            )
            .into_owned(),
        ));
    }
    Ok(())
}

pub fn backup_volume(
    req: &VolumeBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    check_encrypted_target(req.encryption.as_ref(), req.image_format, &req.image_path)?;
    if req.image_path.exists() {
        return Err(OwnBackupError::Config(format!(
            "output image already exists: {}",
            req.image_path.display()
        )));
    }
    if let Some(parent) = req.image_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::dest_folder::create(parent)?;
        }
    }

    // Enable SeBackupPrivilege so the exclusion scan can read/list ACL-protected
    // paths (System Volume Information, protected app data). Best-effort.
    crate::privileges_win::enable_backup_privileges();

    let result = backup_volume_inner(req, progress);
    if result.is_err() {
        // Best-effort cleanup of a partial image.
        let _ = std::fs::remove_file(&req.image_path);
    }
    result
}

/// The volume-root form VSS expects; None for anything that is not a volume.
/// See [`crate::source_identity::volume_root_for_snapshot`] — it lives there
/// because it is pure string work and can be tested without Windows.
fn volume_root_of(device_path: &str) -> Option<String> {
    crate::source_identity::volume_root_for_snapshot(device_path)
}

/// Check that `parent_image` can serve as the parent of a new child image for
/// the volume being backed up.
fn validate_parent(
    parent_image: &Path,
    block_size: u32,
    volume_len: u64,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent image not found: {}",
            parent_image.display()
        )));
    }
    let sidecar = load_sidecar_with_keys(parent_image, keys)?;
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    if sidecar.manifest.source_disk.size_bytes != volume_len {
        return Err(OwnBackupError::Config(format!(
            "volume size changed since the parent backup ({} -> {volume_len} bytes)",
            sidecar.manifest.source_disk.size_bytes
        )));
    }
    Ok(sidecar)
}

/// Should we try for a shadow copy at all?
///
/// On the boot medium the answer is no, and not because VSS is missing (it is —
/// WinPE carries no Volume Shadow Copy Service) but because there is nothing for
/// it to do: the system on that disk is not running, no process holds a file
/// open, nothing is written while we read. A direct read of an offline volume is
/// already a consistent copy — that is the whole point of rescue media. Asking
/// anyway would cost a COM round trip and log a failure that means nothing.
fn snapshot_wanted(requested: bool) -> bool {
    if !requested {
        return false;
    }
    if own_backup_common::media_mode::is_media_mode() {
        info!("boot medium: the system is offline, reading the volume directly (cold copy)");
        return false;
    }
    true
}

/// A BitLocker source: the key, and the window onto the disk its ciphertext
/// lies in.
///
/// The volume device is not usable — locked, it refuses every read — so each
/// reader is opened straight onto `\\.\PhysicalDriveN`, bounded to the
/// partition.
#[cfg(target_os = "windows")]
struct BitLockerSource {
    params: crate::bitlocker::BitLockerParams,
    disk: String,
    offset: u64,
    length: u64,
}

/// A reader bounded to one partition of a disk, aligned for a device opened
/// without buffering (the FVE dataset starts 0x40 into its block, which is not
/// a sector boundary).
/// Размер сектора диска — по геометрии устройства, а не по догадке.
///
/// Нужен, когда BitLocker не заполнил поле в BPB: внутри exFAT или ReFS там
/// ноль, и без этого числа контейнер не открыть (см. `bitlocker::
/// resolve_sector_size`). Стоит одного открытия устройства.
#[cfg(target_os = "windows")]
fn disk_sector_size(disk: &str) -> Option<u32> {
    WinRawDevice::open_read(disk).ok().map(|d| d.sector_size())
}

#[cfg(target_os = "windows")]
fn disk_window(
    disk: &str,
    offset: u64,
    length: u64,
) -> Result<crate::aligned_volume::AlignedVolume<crate::volume_window::VolumeWindow>> {
    Ok(crate::aligned_volume::AlignedVolume::new(
        crate::volume_window::VolumeWindow::new(
            Box::new(WinRawDevice::open_read(disk)?),
            offset,
            length,
        )?,
    ))
}

#[cfg(target_os = "windows")]
impl BitLockerSource {
    fn window(
        &self,
    ) -> Result<crate::aligned_volume::AlignedVolume<crate::volume_window::VolumeWindow>> {
        disk_window(&self.disk, self.offset, self.length)
    }
}

/// Is this source a BitLocker volume, and does the passphrase open it?
///
/// `None` when it is not one (a LUKS container, or a plain volume); an error
/// when it is one and the passphrase does not fit, because by then the user
/// has been asked for it and silently copying the ciphertext would be worse
/// than stopping.
#[cfg(target_os = "windows")]
fn open_bitlocker_source(read_path: &str, passphrase: &str) -> Result<Option<BitLockerSource>> {
    let locked = match WinRawDevice::open_read(read_path) {
        Ok(mut dev) => {
            let mut header = vec![0u8; 4096];
            match dev.read_at(0, &mut header) {
                // Readable and BitLocker-shaped: a locked volume on a disk
                // moved here from another machine.
                Ok(()) => crate::bitlocker::is_bitlocker(&header),
                Err(e) if is_bitlocker_locked(&e) => true,
                Err(e) => return Err(e),
            }
        }
        Err(_) => false,
    };
    if !locked {
        return Ok(None);
    }

    let extents = crate::volume_win::volume_extents(read_path);
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.bitlocker_multi_extent",
                volume = read_path,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let disk = format!(r"\\.\PhysicalDrive{}", extent.disk_number);
    let mut probe = disk_window(&disk, extent.offset, extent.length)?;
    let params =
        crate::bitlocker::open_bitlocker_with(&mut probe, passphrase, disk_sector_size(&disk))?;
    info!(
        algo = format!("{:#x}", params.algorithm),
        disk = extent.disk_number,
        offset = extent.offset,
        "BitLocker source unlocked; backing up decrypted data"
    );
    Ok(Some(BitLockerSource { params, disk, offset: extent.offset, length: extent.length }))
}

/// Открыть заблокированный том силами Windows и закрыть его диск на запись.
///
/// Возвращает стража: пока он жив, диск нельзя изменить. Держать его надо до
/// конца копирования — том остаётся смонтированным, и Windows пишет в него,
/// когда ей вздумается (журнал, метки времени, System Volume Information).
#[cfg(target_os = "windows")]
/// Лежит ли внутри расшифрованного контейнера ReFS.
///
/// Признак тот же, что у инвентаризации (`fs_probe`): имя «ReFS» на смещении 3
/// И сигнатура структуры «FSRS» на 0x10 — одного имени мало, оно встречается и
/// в мусоре. Ошибка чтения значит «не знаем», а не «ReFS».
#[cfg(target_os = "windows")]
fn inner_fs_is_refs(src: &BitLockerSource) -> bool {
    let Ok(window) = src.window() else { return false };
    let mut vol = crate::bitlocker::BitLockerVolume::new(Box::new(window), src.params.clone());
    let mut boot = vec![0u8; 4096];
    if crate::fat_map::RawVolume::read_at(&mut vol, 0, &mut boot).is_err() {
        return false;
    }
    &boot[3..7] == b"ReFS" && &boot[0x10..0x14] == b"FSRS"
}

fn unlock_source_with_windows(
    read_path: &str,
    passphrase: &str,
) -> Result<crate::windows_unlock_win::ReadOnlyDisk> {
    use crate::windows_unlock_win as wu;

    let extents = crate::volume_win::volume_extents(read_path);
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.windows_unlock_multi_extent",
                volume = read_path,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let guard = wu::ReadOnlyDisk::arm(extent.disk_number)?;

    // 48 цифр — это ключ восстановления, что бы ни было между ними; всё
    // остальное считаем парольной фразой.
    let digits: String = passphrase.chars().filter(char::is_ascii_digit).collect();
    let is_key = digits.len() == 48
        && passphrase.chars().all(|c| c.is_ascii_digit() || c == '-' || c == ' ');
    let dashed;
    let secret = if is_key {
        dashed = digits
            .as_bytes()
            .chunks(6)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join("-");
        wu::Secret::RecoveryPassword(&dashed)
    } else {
        wu::Secret::Passphrase(passphrase)
    };
    wu::unlock_with_windows(read_path, secret)?;

    // Разблокировка и монтирование — разные события. FVE отдаёт ключ сразу, а
    // том появляется на своём устройстве через мгновение, и читать его в этот
    // промежуток бессмысленно: устройство ещё не отвечает. Ждём, пока с него
    // прочтётся хотя бы загрузочный сектор — не дольше пятнадцати секунд.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut last_err = None;
    loop {
        match WinRawDevice::open_read(read_path).and_then(|mut d| {
            let mut boot = vec![0u8; 512];
            crate::fat_map::RawVolume::read_at(&mut d, 0, &mut boot)?;
            // Нули значат «устройство есть, а тома на нём ещё нет».
            if boot.iter().all(|&b| b == 0) {
                return Err(OwnBackupError::Device(
                    tr!("engine_win.volume_not_mounted_yet").into_owned(),
                ));
            }
            Ok(())
        }) {
            Ok(()) => break,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    warn!(error = %e, "том открыт, но за 15 с так и не стал читаемым");
                    last_err = Some(e);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    info!(volume = read_path, "том открыт силами Windows — дальше он обычный");
    Ok(guard)
}

fn backup_volume_inner(
    req: &VolumeBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<VolumeBackupSummary> {
    let started = Instant::now();
    let source = normalize_device_path(&req.source);

    // --- VSS shadow copy (live lettered volumes only) ---
    let mut vss: Option<VssSnapshotSession> = None;
    let read_path = if snapshot_wanted(req.use_vss) {
        match volume_root_of(&source) {
            Some(root) => match VssSnapshotSession::create(&root) {
                Ok(session) => {
                    let path = session.device_path().to_string();
                    vss = Some(session);
                    path
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "VSS snapshot failed — reading the live volume (crash-consistent copy)"
                    );
                    source.clone()
                }
            },
            None => {
                info!("Source is not a lettered volume; skipping VSS");
                source.clone()
            }
        }
    } else {
        source.clone()
    };
    let vss_used = vss.is_some();

    // --- Optional LUKS unlock ---
    // When a passphrase is supplied and the source is a LUKS container, we back
    // up its DECRYPTED data area (used blocks only). The inner filesystem is
    // Linux (btrfs/ext/xfs), so the NTFS-specific steps below are skipped.
    // A BitLocker source is read through a window onto its DISK: locked, the
    // volume device answers FVE_E_LOCKED_VOLUME to everything, and the FVE
    // filter does not sit above the disk. Confirmed on a real volume
    // 2026-08-05 (Windows 10, XTS-AES 128, password protector).
    // Запрет записи на диск-источник, если том пришлось открывать силами
    // Windows: она его СМОНТИРУЕТ, а смонтированная ФС пишет сама. Страж живёт
    // до конца копирования — не до конца разблокировки.
    let mut _source_ro_guard: Option<crate::windows_unlock_win::ReadOnlyDisk> = None;
    let bitlocker = match &req.passphrase {
        Some(pass) => match open_bitlocker_source(&read_path, pass) {
            Ok(src) => src,
            // Своё чтение FVE знает только AES-XTS 128/256. Шифр вне этого
            // списка (AES-CBC, Elephant diffuser) — не повод сдаваться: то же
            // самое умеет сама Windows, и через неё том станет обычным
            // смонтированным, со всем быстрым путём (план B5).
            Err(e) => {
                warn!(error = %e, "свой разбор BitLocker не справился — прошу открыть Windows");
                match unlock_source_with_windows(&read_path, pass) {
                    Ok(guard) => {
                        _source_ro_guard = Some(guard);
                        None // том теперь обычный: читаем его как все
                    }
                    Err(e2) => {
                        warn!(error = %e2, "Windows тоже не открыла том");
                        return Err(e); // наружу — первая ошибка, она по делу
                    }
                }
            }
        },
        None => None,
    };

    // ReFS внутри контейнера: расшифровать её мы умеем, а перечислить в ней
    // файлы — нет, спецификации ReFS не существует. Хуже того, без разбора нет
    // и карты занятого: том пришлось бы копировать посекторно, целиком. Если
    // Windows готова открыть его сама, том станет обычным смонтированным — и
    // заработает и карта (FSCTL_GET_VOLUME_BITMAP на ReFS отвечает), и
    // исключения обходом дерева. Это и есть смысл этапа B5.
    //
    // **На практике сюда не доходит, и это выяснилось живой проверкой
    // 2026-08-20.** Наш разбор FVE отказывается от ReFS-контейнера ещё раньше:
    // размер сектора он берёт из BPB загрузочного сектора (`bitlocker.rs`), а у
    // ReFS этого поля нет — приходит ноль, и открытие отклоняется как
    // «implausible sector size 0». Срабатывает ветка выше, общая: «парсер не
    // справился — просим Windows», и результат тот же.
    //
    // Проверка всё же оставлена. Она стоит одного чтения загрузочного сектора,
    // а покрывает случай, которого мы не видели: контейнер, который парсер
    // ОТКРЫЛ, а внутри ReFS. Убрать её значит поставить на то, что такого не
    // бывает — а расплатой был бы тихий откат к посекторной копии тома целиком,
    // без единого предупреждения.
    let bitlocker = match (&bitlocker, &req.passphrase) {
        (Some(b), Some(pass)) if inner_fs_is_refs(b) => {
            info!("внутри контейнера ReFS — прошу Windows открыть том, иначе её не прочесть");
            match unlock_source_with_windows(&read_path, pass) {
                Ok(guard) => {
                    _source_ro_guard = Some(guard);
                    None
                }
                Err(e) => {
                    // Не беда: копия снимется нашим чтением, просто посекторно
                    // и без исключений — и об этом будет сказано ниже.
                    warn!(error = %e, "Windows не открыла ReFS-том — копирую своим чтением");
                    bitlocker
                }
            }
        }
        _ => bitlocker,
    };

    let mut src_probe = WinRawDevice::open_read(&read_path)?;
    let luks: Option<crate::luks::LuksParams> = match (&req.passphrase, &bitlocker) {
        (Some(pass), None) => {
            let mut header = vec![0u8; 4096];
            src_probe.read_at(0, &mut header)?;
            if crate::luks::is_luks(&header) {
                let params = crate::luks::open_luks(&mut src_probe, pass.as_bytes())?;
                info!(kind = ?params.kind, "LUKS source unlocked; backing up decrypted data");
                Some(params)
            } else {
                None
            }
        }
        _ => None,
    };
    let device_len = src_probe.length();
    let sector_size = src_probe.sector_size();
    drop(src_probe);

    // For an encrypted source the backed-up "volume" is the decrypted data area.
    let volume_len = match (&luks, &bitlocker) {
        (Some(p), _) => device_len.saturating_sub(p.data_offset),
        (_, Some(b)) => b.params.encrypted_size.min(b.length),
        _ => device_len,
    };

    // Opens a fresh read source for the used-map builders and the block reader:
    // the device itself, or a decrypting view over it — over the volume for
    // LUKS, over a window onto the disk for BitLocker.
    let open_source = |luks: &Option<crate::luks::LuksParams>| -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        if let Some(b) = &bitlocker {
            return Ok(Box::new(crate::bitlocker::BitLockerVolume::new(
                Box::new(b.window()?),
                b.params.clone(),
            )));
        }
        let dev = WinRawDevice::open_read(&read_path)?;
        Ok(match luks {
            Some(p) => Box::new(crate::decrypt::DecryptingVolume::new(Box::new(dev), p.clone())),
            None => Box::new(dev),
        })
    };

    if req.block_size == 0 || req.block_size % sector_size != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the source sector size {sector_size}",
            req.block_size
        )));
    }
    let total_blocks = volume_len.div_ceil(req.block_size as u64);
    // The container's header is stored in the sidecar (D19), and it is measured
    // in megabytes — the meta partition has to be reserved with that in mind or
    // the metadata write at the very end has nowhere to go.
    let luks_reserve =
        crate::sidecar::luks_reserve_for(luks.as_ref().map(|p| p.data_offset));
    let meta_size = crate::sidecar::calculate_meta_partition_size(total_blocks, luks_reserve);

    // --- Parent chain (incremental / differential) ---
    // A broken chain must not fail the run: fall back to a full backup and
    // report the fallback in the summary.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(parent_path) => {
                match validate_parent(
                    parent_path,
                    req.block_size,
                    volume_len,
                    req.encryption.as_ref(),
                ) {
                    Ok(sidecar) => parent = Some((parent_path.clone(), sidecar)),
                    Err(e) => {
                        warn!(
                            parent = %parent_path.display(),
                            error = %e,
                            "parent chain unusable — creating a full backup instead"
                        );
                        kind = BackupType::Full;
                    }
                }
            }
        }
    }
    // A chain must never cross container formats (decision 2026-07-08): the
    // format of an existing archive cannot be changed — that is a hard error,
    // not a fallback. The job runners refuse such runs before reaching the
    // engine; this guards direct callers.
    let want_container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION
    } else {
        req.image_format.extension()
    };
    if let Some((parent_path, parent_sidecar)) = &parent {
        if parent_sidecar.info.container != want_container {
            return Err(OwnBackupError::Config(format!(
                "parent image {} is '{}' but the requested format is '{}' — \
                 changing the format of an existing backup chain is not allowed; \
                 create a new job (or remove the old backups)",
                parent_path.display(),
                parent_sidecar.info.container,
                want_container
            )));
        }
    }

    // --- Used-sector map: NTFS → FAT32/exFAT → ext → btrfs → xfs → full copy ---
    let mut ntfs_map: Option<NtfsUsedSectorMap> = None;
    let mut fat_map: Option<crate::fat_map::FatUsedMap> = None;
    let mut ext_map: Option<crate::ext_map::ExtUsedMap> = None;
    let mut btrfs_map: Option<crate::btrfs_map::BtrfsUsedMap> = None;
    let mut xfs_map: Option<crate::xfs_map::XfsUsedMap> = None;
    // For an encrypted source the map is built over the decrypting view and
    // returned boxed directly (no exclusions apply to a Linux volume).
    let mut luks_used: Option<Box<dyn UsedSectorMap>> = None;
    // NTFS внутри LUKS/BitLocker: карта держится типизированной, пока не
    // решится вопрос исключений — их вычитание перестраивает её.
    let mut enc_ntfs_map: Option<crate::ntfs_map::NtfsUsedMap> = None;
    // FAT/exFAT внутри контейнера — то же самое (BitLocker To Go на флешке
    // обычно именно exFAT, и таких носителей больше, чем ReFS-томов).
    let mut enc_fat_map: Option<crate::fat_map::FatUsedMap> = None;
    // The filesystem actually found inside the volume. For a LUKS/BitLocker
    // source this is what is INSIDE the container, because that is what the
    // image holds — the partition table outside still says "unknown", and a
    // restore screen reading only that would show a padlock over a plain
    // btrfs (D14, R8).
    let mut detected_fs: Option<String> = None;
    let smart_mode = if req.all_sectors {
        info!("Sector-by-sector mode requested; copying all blocks");
        false
    } else if luks.is_some() || bitlocker.is_some() {
        // A decrypted volume: only the RawVolume-based parsers apply. The NTFS
        // one among them — a BitLocker volume holds NTFS, and its $Bitmap is
        // inside the ciphertext, so Windows' own FSCTL cannot reach it while
        // the volume is locked. The reader below can.
        let mut src = open_source(&luks)?;
        let map: Option<Box<dyn UsedSectorMap>> =
            if let Ok(m) = crate::ntfs_map::build_ntfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "NTFS", "Using NTFS map over decrypted volume");
                detected_fs = Some("NTFS".into());
                // НЕ боксим: NTFS внутри контейнера умеет исключения ДО копии
                // (план exclusions-before-copy, B3), а для этого карту надо
                // будет перестроить без исключённых кластеров.
                enc_ntfs_map = Some(m);
                None
            } else if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), req.block_size) {
                info!(fs = m.filesystem, "Using FAT map over decrypted volume");
                detected_fs = Some(m.filesystem.to_string());
                // Как и NTFS, держим типизированной: исключения перестроят её
                // без исключённых кластеров (этап B4).
                enc_fat_map = Some(m);
                None
            } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ext4", "Using ext map over decrypted volume");
                detected_fs = Some("ext4".into());
                Some(Box::new(m))
            } else if let Ok(m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "btrfs", "Using btrfs map over decrypted volume");
                detected_fs = Some("btrfs".into());
                Some(Box::new(m))
            } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "xfs", "Using xfs map over decrypted volume");
                detected_fs = Some("xfs".into());
                Some(Box::new(m))
            } else {
                warn!("no used-block map for the decrypted volume — sector-by-sector");
                None
            };
        let smart = map.is_some() || enc_ntfs_map.is_some() || enc_fat_map.is_some();
        luks_used = map;
        smart
    } else {
        match NtfsUsedSectorMap::open(&read_path, req.block_size) {
            Ok(map) => {
                ntfs_map = Some(map);
                detected_fs = Some("NTFS".into());
                true
            }
            Err(ntfs_err) => {
                // Not NTFS — try parsing FAT32/exFAT, then ext2/3/4, structures
                // directly (each on its own device handle).
                let fat_attempt = WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                    crate::fat_map::build_fat_used_map(&mut dev, req.block_size)
                });
                match fat_attempt {
                    Ok(map) => {
                        info!(fs = map.filesystem, "Using FAT used-cluster map");
                        detected_fs = Some(map.filesystem.to_string());
                        fat_map = Some(map);
                        true
                    }
                    Err(fat_err) => {
                        let ext_attempt = WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                            crate::ext_map::build_ext_used_map(&mut dev, req.block_size)
                        });
                        match ext_attempt {
                            Ok(map) => {
                                info!(fs = "ext4", "Using ext used-block map");
                                detected_fs = Some("ext4".into());
                                ext_map = Some(map);
                                true
                            }
                            Err(ext_err) => {
                                let btrfs_attempt =
                                    WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                                        crate::btrfs_map::build_btrfs_used_map(&mut dev, req.block_size)
                                    });
                                match btrfs_attempt {
                                    Ok(map) => {
                                        info!(fs = "btrfs", "Using btrfs used-block map");
                                        detected_fs = Some("btrfs".into());
                                        btrfs_map = Some(map);
                                        true
                                    }
                                    Err(btrfs_err) => {
                                        let xfs_attempt =
                                            WinRawDevice::open_read(&read_path).and_then(|mut dev| {
                                                crate::xfs_map::build_xfs_used_map(&mut dev, req.block_size)
                                            });
                                        match xfs_attempt {
                                            Ok(map) => {
                                                info!(fs = "xfs", "Using xfs used-block map");
                                                detected_fs = Some("xfs".into());
                                                xfs_map = Some(map);
                                                true
                                            }
                                            Err(xfs_err) => {
                                                warn!(
                                                    ntfs = %ntfs_err,
                                                    fat = %fat_err,
                                                    ext = %ext_err,
                                                    btrfs = %btrfs_err,
                                                    xfs = %xfs_err,
                                                    "no used-block map available, falling back to sector-by-sector"
                                                );
                                                false
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // --- File exclusions (phase 3) ---
    // Both families achieve TRUE removal (like Acronis) — a restored/mounted
    // volume shows no excluded file and reclaims its space — by different,
    // filesystem-appropriate means:
    //
    // * FAT32/exFAT — metadata patches: the raw directory tree is walked
    //   ourselves and the matched files' metadata is patched out of the image
    //   (directory entry deleted, allocation table / bitmap freed). The
    //   simple, fully-specified on-disk structures make this safe and testable.
    //
    // * NTFS/ReFS — two steps. First, CONTENT ZEROING (CLAUDE.md: "exclusions
    //   via FSCTL_GET_RETRIEVAL_POINTERS (clusters zeroed)"): the matched
    //   clusters are dropped from the used map and zeroed, so the file's DATA
    //   never leaves the machine. Then, after the copy, the mount-&-delete
    //   pass (`true_removal_win`, decision 2026-07-07) lets the FS DRIVER
    //   delete the catalog entries inside the image copy — we never parse or
    //   write NTFS/ReFS structures ourselves ("no NTFS parser", CLAUDE.md,
    //   still holds); the driver updates $MFT/$Bitmap/directory indexes (or
    //   the ReFS equivalents) correctly by construction. If that pass cannot
    //   run, the excluded files remain as zero-filled ghosts — a valid backup,
    //   just without the space reclaim.
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut zero_ranges: Option<ByteRanges> = None;
    // Byte-level metadata edits that make excluded FAT/exFAT files truly
    // disappear (delete the directory entry, free the allocation table). Empty
    // for NTFS/ReFS, which still use zeroing for now.
    let mut fat_patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    // "prepare"-phase progress tick. `total` is the number of MFT records the
    // scan will look at, known up front; 0 when whatever is scanning cannot say
    // (a directory walk meets its entries as it goes).
    let mut prepare_tick = |entries: u64, total: u64| {
        progress(&BackupProgress {
            backup_id: "prepare".into(),
            status: BackupStatus::Running,
            blocks_done: entries,
            blocks_total: total,
            bytes_written: 0,
            speed_bps: 0,
                categories: None,
                excluded: None,
        })
    };
    // Why the file walk did not happen — filled in below by whichever branch
    // skips it, and carried all the way to the result screen. The walk is what
    // produces BOTH the exclusions and the by-type breakdown, so one sentence
    // accounts for both being missing.
    let mut file_scan_skipped: Option<String> = None;
    let fat_exclusions = fat_map
        .as_ref()
        .is_some_and(|f| f.filesystem == "FAT32" || f.filesystem == "exFAT")
        && !req.exclusion_patterns.is_empty();
    // Matched paths for the post-copy true-removal pass (NTFS/ReFS): the FS
    // driver deletes them inside the finished image, so they vanish from the
    // catalog and their space is reclaimed — see `true_removal_win`.
    let mut removal_paths: Vec<crate::exclusions_win::ExcludedPath> = Vec::new();
    // Исключения решены ДО копии по $MFT — проход внутри образа не нужен:
    // исключённых данных в образе нет, осталось убрать записи из каталога.
    let mut pre_copy_exclusions = false;
    if let Some(map) = ntfs_map.as_mut() {
        if !req.exclusion_patterns.is_empty() {
            // Сначала по $MFT: последовательное чтение вместо обхода дерева.
            // На живом C: обход занимает 45 с, внутри образа на SMR — 16 минут;
            // чтение MFT того же образа — 15 с (замеры B0/B1, 2026-08-19). Плюс
            // ему не нужен дескриптор: обход не смог открыть pagefile.sys,
            // swapfile.sys и DumpStack.log.tmp («файл занят другим процессом»)
            // и оставил в копии 400 МиБ ровно тех файлов, ради которых
            // исключения и существуют.
            //
            // Разбирать метаданные тома, который прямо сейчас пишут, нельзя:
            // берёмся только за замороженный источник — снимок VSS или офлайн
            // на носителе. В остальных случаях (VSS не дали) остаётся обход:
            // он идёт через файловую систему и переживает движение под собой.
            let frozen = vss_used || own_backup_common::media_mode::is_media_mode();
            let fast = if frozen {
                scan_ntfs_exclusions_by_mft(&read_path, &req.exclusion_patterns, &mut prepare_tick)
            } else {
                Err(OwnBackupError::Device(tr!("engine_win.source_not_frozen").into_owned()))
            };
            match fast {
                Ok(mft) => {
                    excluded_files = mft.files_excluded;
                    excluded_bytes = mft.bytes_excluded;
                    categories = Some(mft.categories);
                    removal_paths = mft
                        .paths
                        .iter()
                        .map(|p| crate::exclusions_win::ExcludedPath {
                            rel: p.rel.clone(),
                            is_dir: p.is_dir,
                            is_reparse: p.is_reparse,
                        })
                        .collect();
                    info!(
                        files = mft.files_excluded,
                        bytes = mft.bytes_excluded,
                        kept = mft.kept.len(),
                        paths = removal_paths.len(),
                        records = mft.records_scanned,
                        "exclusions resolved from the MFT"
                    );
                    // Сводка по типам файлов известна уже сейчас — сканер видел
                    // каждый файл с размером. Отправляем её сразу, а не в конце:
                    // человеку, который смотрит на полосу двадцать минут, есть
                    // что почитать, и это ответ на вопрос «а что вообще в копии».
                    report_categories(&mut progress, categories.as_ref(), excluded_files, excluded_bytes);
                    if !mft.ranges.is_empty() {
                        map.apply_excluded_ranges(&mft.ranges);
                        zero_ranges = Some(mft.ranges);
                    }
                }
                Err(e) => {
                    // ReFS (у неё нет MFT), грязный том, неразобранная запись —
                    // всё это законные причины вернуться к обходу дерева.
                    warn!(error = %e, "MFT scan unavailable — falling back to the directory walk");
                    let mut walk_tick = |n: u64| prepare_tick(n, 0);
                    let mut scan = collect_excluded_ranges(
                        &read_path,
                        map.cluster_size(),
                        &req.exclusion_patterns,
                        &mut walk_tick,
                    )?;
                    excluded_files = scan.files_excluded;
                    excluded_bytes = scan.bytes_excluded;
                    categories = Some(scan.categories);
                    removal_paths = std::mem::take(&mut scan.paths);
                    if !scan.ranges.is_empty() {
                        map.apply_excluded_ranges(&scan.ranges);
                        zero_ranges = Some(scan.ranges);
                    }
                }
            }
        } else {
            file_scan_skipped = Some(no_patterns());
        }
        map.release_bitmap();
    } else if let Some(map) = enc_ntfs_map.as_mut() {
        // NTFS внутри контейнера, который Windows не смонтировала. Файлы в нём
        // всё равно перечислимы — по $MFT, прямо через расшифрованное окно
        // (`ntfs_scan`), и это решает исключения ДО копирования: экстенты
        // вычитаются из карты, исключённые данные не читаются и не пишутся.
        // Раньше здесь был отказ, а правила применялись потом, внутри готового
        // образа: скан 16 минут, зануление, merge и пересчёт сумм — прогон
        // 2026-08-19 пользователь отменил.
        if req.exclusion_patterns.is_empty() {
            file_scan_skipped = Some(no_patterns());
        } else {
            let mut src = open_source(&luks)?;
            let scan_result = {
                let cur = crate::ntfs_scan::RawVolumeCursor::new(src.as_mut());
                // Крейт читает запись MFT по килобайту; без кэша это миллион
                // обращений к носителю (замер: 313 с против 74 с).
                let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
                // Тот же канал «Подготовка: поиск исключаемых файлов», что у
                // обхода дерева: на 128 ГиБ проход идёт больше двух минут, и
                // молчащая полоса читается как зависание.
                let mut report = |n: u64, total: u64| {
                    let _ = progress(&BackupProgress {
                        backup_id: "prepare".into(),
                        status: BackupStatus::Running,
                        blocks_done: n,
                        blocks_total: total,
                        bytes_written: 0,
                        speed_bps: 0,
                categories: None,
                excluded: None,
                    });
                };
                crate::ntfs_scan::scan_mft_with(&mut buffered, &req.exclusion_patterns, &mut report)
            };
            match scan_result {
                Ok(scan) => {
                    excluded_files = scan.files_excluded;
                    excluded_bytes = scan.bytes_excluded;
                    categories = Some(scan.categories);
                    report_categories(&mut progress, categories.as_ref(), excluded_files, excluded_bytes);
                    for (path, reason) in &scan.kept {
                        debug!(file = %path, reason = reason.as_str(), "данные остаются в копии");
                    }
                    removal_paths = scan
                        .paths
                        .iter()
                        .map(|p| crate::exclusions_win::ExcludedPath {
                            rel: p.rel.clone(),
                            is_dir: p.is_dir,
                            is_reparse: p.is_reparse,
                        })
                        .collect();
                    info!(
                        files = scan.files_excluded,
                        bytes = scan.bytes_excluded,
                        kept = scan.kept.len(),
                        paths = removal_paths.len(),
                        records = scan.records_scanned,
                        "exclusions resolved from the MFT BEFORE the copy"
                    );
                    if !scan.ranges.is_empty() {
                        // Карта строится заново, без исключённых кластеров:
                        // блок, целиком занятый исключённым, не копируется
                        // вовсе; в общих блоках лишнее занулит ZeroingBlockReader.
                        let mut src2 = open_source(&luks)?;
                        let rebuilt = crate::ntfs_map::build_ntfs_used_map_excl(
                            src2.as_mut(),
                            req.block_size,
                            Some(&scan.ranges),
                        )?;
                        *map = rebuilt;
                        zero_ranges = Some(scan.ranges);
                    }
                    pre_copy_exclusions = true;
                }
                Err(e) => {
                    // Грязный том, битая запись, не NTFS внутри — откат на
                    // проход внутри готового образа (медленный, но рабочий).
                    warn!(error = %e, "MFT scan failed — falling back to the in-image pass");
                }
            }
        }
    } else if let Some(map) = enc_fat_map.as_mut() {
        // FAT/exFAT внутри контейнера, который Windows не смонтировала. Свой
        // разбор FAT читает каталоги прямо из расшифрованного окна — ему, как и
        // NTFS-сканеру, не нужен ни смонтированный том, ни дескриптор файла.
        // Раньше здесь был отказ: ветка исключений знала только NTFS.
        if req.exclusion_patterns.is_empty() {
            file_scan_skipped = Some(no_patterns());
        } else {
            let mut src = open_source(&luks)?;
            let mut walk_tick = |n: u64| prepare_tick(n, 0);
            match crate::fat_exclusions::scan_fat_exclusions(
                src.as_mut(),
                &req.exclusion_patterns,
                &mut walk_tick,
            ) {
                Ok(scan) => {
                    excluded_files = scan.files_excluded;
                    excluded_bytes = scan.bytes_excluded;
                    categories = Some(scan.categories);
                    info!(
                        files = scan.files_excluded,
                        bytes = scan.bytes_excluded,
                        entries = scan.entries_scanned,
                        patches = scan.patches.len(),
                        "exclusions resolved from FAT BEFORE the copy"
                    );
                    if !scan.ranges.is_empty() {
                        // Карта строится заново, без исключённых кластеров.
                        let mut src2 = open_source(&luks)?;
                        *map = crate::fat_map::build_fat_used_map_excl(
                            src2.as_mut(),
                            req.block_size,
                            Some(&scan.ranges),
                        )?;
                        zero_ranges = Some(scan.ranges);
                        // Правки метаданных: запись каталога помечается
                        // удалённой, цепочка в таблице освобождается — файл
                        // исчезает, а не остаётся нулевым призраком.
                        fat_patches = scan.patches;
                    }
                    pre_copy_exclusions = true;
                }
                Err(e) => {
                    warn!(error = %e, "FAT exclusion scan failed — keeping all files");
                    file_scan_skipped =
                        Some(tr!("engine_win.skip_fat_failed", error = e).into_owned());
                }
            }
        }
    } else if fat_exclusions {
        let mut dev = WinRawDevice::open_read(&read_path)?;
        let scan =
            crate::fat_exclusions::scan_fat_exclusions(&mut dev, &req.exclusion_patterns, |n| prepare_tick(n, 0))?;
        excluded_files = scan.files_excluded;
        excluded_bytes = scan.bytes_excluded;
        categories = Some(scan.categories);
        if !scan.ranges.is_empty() {
            // Rebuild the map so excluded clusters drop out (cluster-level).
            let mut dev2 = WinRawDevice::open_read(&read_path)?;
            let rebuilt =
                crate::fat_map::build_fat_used_map_excl(&mut dev2, req.block_size, Some(&scan.ranges))?;
            fat_map = Some(rebuilt);
            zero_ranges = Some(scan.ranges);
            // Metadata patches: delete the directory entries + free the FAT
            // chains so the files vanish instead of remaining zero-filled ghosts.
            fat_patches = scan.patches;
        }
    } else {
        // Nothing walked the files here. Say which of the reasons it was, in
        // the words the result screen will repeat back to the user.
        let reason = if req.all_sectors {
            tr!("engine_win.skip_sector_mode").into_owned()
        } else if bitlocker.is_some() {
            // `FSCTL_GET_RETRIEVAL_POINTERS` needs a mounted filesystem, and a
            // locked volume has none. Copying the data and deleting only the
            // catalog entries afterwards would leave those bytes in the image's
            // free space — exactly what "true removal" promises not to do.
            tr!("engine_win.skip_bitlocker_locked").into_owned()
        } else if luks.is_some() {
            tr!("engine_win.skip_luks").into_owned()
        } else if req.exclusion_patterns.is_empty() {
            no_patterns()
        } else {
            let fs = detected_fs
                .clone()
                .unwrap_or_else(|| tr!("engine_win.other_filesystem").into_owned());
            tr!("engine_win.skip_fs_unsupported", fs = fs).into_owned()
        };
        if !req.exclusion_patterns.is_empty() {
            warn!(
                patterns = req.exclusion_patterns.len(),
                reason = %reason,
                "file exclusions were NOT applied on the source"
            );
        }
        file_scan_skipped = Some(reason);
    }

    // …but a container's files can still be reached — after the copy, inside
    // the image, where their filesystem lies in the clear and Windows will
    // mount it. NTFS only: the pass walks the mounted volume and then reads
    // `$Bitmap` to know what it may zero, and neither exists on ReFS or on a
    // Linux filesystem Windows cannot mount at all.
    let encrypted_source = bitlocker.is_some() || luks.is_some();
    let in_image_exclusions = encrypted_source
        && !pre_copy_exclusions
        && !req.exclusion_patterns.is_empty()
        && !req.all_sectors
        && detected_fs.as_deref() == Some("NTFS");

    if in_image_exclusions && req.encryption.is_some() {
        // The fallback pass mounts the finished image and deletes inside it,
        // through a plaintext scratch layer. For an encrypted image that layer
        // must be encrypted too (§7.4), and it is not yet. Skipping the pass
        // instead would ship a copy that quietly kept the excluded files.
        return Err(OwnBackupError::Encryption(
            tr!("engine_win.encrypted_in_image_exclusions").into_owned(),
        ));
    }

    let used: Box<dyn UsedSectorMap> = if let Some(map) = enc_ntfs_map {
        Box::new(map)
    } else if let Some(map) = enc_fat_map {
        Box::new(map)
    } else if let Some(map) = luks_used {
        map
    } else {
        match (ntfs_map, fat_map, ext_map, btrfs_map, xfs_map) {
            (Some(map), ..) => Box::new(map),
            (None, Some(map), ..) => Box::new(map),
            (None, None, Some(map), ..) => Box::new(map),
            (None, None, None, Some(map), _) => Box::new(map),
            (None, None, None, None, Some(map)) => Box::new(map),
            (None, None, None, None, None) => Box::new(AllUsedMap::new(total_blocks)),
        }
    };
    let used_blocks = used.used_block_count();

    // --- Create the image target (and attach it, for VHDX) ---
    // Disk = 1 MiB GPT head + volume + meta partition + tail for the backup GPT.
    let disk_size = DEFAULT_PARTITION_OFFSET + volume_len.div_ceil(MIB) * MIB + meta_size + MIB;
    // The image's identity is settled before it is created: an encrypted image
    // seals it into its own root record, and the sidecar repeats it.
    let backup_id = Uuid::new_v4();

    // An encrypted run writes `.obk`: the same userspace path as qcow2, with
    // every cluster sealed. The sidecar goes inside the container, so the guest
    // meta partition is neither written nor needed (§7.3) — but its size is
    // still what the reservation for the final sidecar is measured by.
    let (mut target, data_offset) = if let Some(keys) = &req.encryption {
        let (backing_file, data_offset, chain) = match &parent {
            None => (None, DEFAULT_PARTITION_OFFSET, keys.start_chain()?),
            Some((parent_path, parent_sidecar)) => {
                let backing = if parent_path.parent() == req.image_path.parent() {
                    parent_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .ok_or_else(|| {
                            OwnBackupError::Config(format!(
                                "parent image path has no file name: {}",
                                parent_path.display()
                            ))
                        })?
                } else {
                    parent_path.to_string_lossy().into_owned()
                };
                // The chain key travels inside the parent image, so continuing
                // a chain needs the parent and this job's key, nothing else.
                let chain = keys.open_chain_of(parent_path)?;
                (
                    Some(backing),
                    parent_sidecar.info.volume_data_offset,
                    chain,
                )
            }
        };
        let role = match (&parent, kind) {
            (None, _) => ChainRole::Full,
            (Some(_), BackupType::Differential) => ChainRole::Differential,
            (Some(_), _) => ChainRole::Incremental,
        };
        let parent_backup_id = parent.as_ref().map(|(_, s)| s.manifest.backup_id);
        let plan = crate::seal::context::ImagePlan {
            backup_id,
            role,
            parent: match (&backing_file, parent_backup_id) {
                (Some(name), Some(id)) => Some((id, name.as_str())),
                _ => None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            virtual_size: disk_size,
            source_id: crate::source_identity::stable_source_id(&req.source),
            consolidated_increments: 0,
            chain_started_at: None,
            compression: crate::seal::obk::ObkCompression::Zstd {
                level: crate::qcow2::zstd_level_or_default(req.zstd_level),
            },
        };
        let writer = Box::new(crate::seal::obk::ObkStagedWriter::create(
            &req.image_path,
            &chain.key,
            keys.writer_options(&chain, plan),
        )?);
        (ImageTarget::Obk { writer }, data_offset)
    } else {
        match req.image_format {
        ImageFormat::Qcow2 => {
            // Userspace-written compressed image: no attach, no kernel writer.
            // A child references its parent by FILE NAME (qemu resolves
            // relative backing paths against the image's directory), so the
            // backup folder stays relocatable — same convention as the sidecar.
            let (backing_file, data_offset) = match &parent {
                None => (None, DEFAULT_PARTITION_OFFSET),
                Some((parent_path, parent_sidecar)) => {
                    // Same folder (the normal case) → bare file name; a parent
                    // elsewhere falls back to its absolute path.
                    let backing = if parent_path.parent() == req.image_path.parent() {
                        parent_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .ok_or_else(|| {
                                OwnBackupError::Config(format!(
                                    "parent image path has no file name: {}",
                                    parent_path.display()
                                ))
                            })?
                    } else {
                        parent_path.to_string_lossy().into_owned()
                    };
                    (Some(backing), parent_sidecar.info.volume_data_offset)
                }
            };
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                &req.image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: disk_size,
                    backing_file,
                    compression: crate::qcow2::Qcow2Compression::Zstd {
                        level: crate::qcow2::zstd_level_or_default(req.zstd_level),
                    },
                    ..Default::default()
                },
            )?;
            (ImageTarget::Qcow2 { writer }, data_offset)
        }
        ImageFormat::Vhdx => {
            let (writer, data_offset) = match &parent {
                None => {
                    create_dynamic_vhdx(
                        &req.image_path,
                        &VhdxCreateOptions {
                            max_size: disk_size,
                            logical_sector_size: sector_size,
                            physical_sector_size: std::cmp::max(sector_size, 4096),
                        },
                    )?;
                    let writer = crate::vhdx_format::VhdxWriter::open_existing(&req.image_path)?;
                    (writer, DEFAULT_PARTITION_OFFSET)
                }
                Some((parent_path, parent_sidecar)) => {
                    create_differencing_vhdx(&req.image_path, parent_path)?;
                    let writer = crate::vhdx_format::VhdxWriter::open_existing(&req.image_path)?;
                    (writer, parent_sidecar.info.volume_data_offset)
                }
            };
            (ImageTarget::Vhdx { writer }, data_offset)
        }
        }
    };

    let disk_guid = match &parent {
        None => {
            // --- GPT with Basic Data partition + OwnBackupMeta partition ---
            let meta_partition_guid = Uuid::new_v4();
            let layout = GptLayout {
                disk_size: target.length(),
                sector_size,
                partition_offset: DEFAULT_PARTITION_OFFSET,
                partition_size: volume_len,
                disk_guid: Uuid::new_v4(),
                partition_guid: Uuid::new_v4(),
                partition_name: "OwnBackup".into(),
                // An encrypted image keeps its sidecar sealed inside the
                // container, so there is no partition to describe.
                meta_partition_size: (req.encryption.is_none()).then_some(meta_size),
                meta_partition_guid: (req.encryption.is_none()).then_some(meta_partition_guid),
            };
            for op in gpt_write_ops(&layout)? {
                target.write_at(op.offset, &op.bytes)?;
            }
            // VHDX only: let partmgr re-read the fresh GPT (the disk stays
            // offline, so no volumes surface), then persist the offline
            // attribute keyed to the new disk GUID — future attaches of this
            // image and of every differencing child arrive offline, closing
            // the auto-mount race for the whole chain. Best-effort: a failure
            // only means the next attach may transiently mount. (qcow2 images
            // are never attached, so there is nothing to persist.)
            layout.disk_guid.to_string()
        }
        Some((_, parent_sidecar)) => parent_sidecar.manifest.source_disk.disk_guid.clone(),
    };

    // --- Copy used blocks into the partition ---
    // For child images only blocks that differ from the parent version are
    // written; everything else resolves through the VHDX chain.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sidecar)| {
        sidecar
            .checksums
            .iter()
            .map(|c| (c.block_number, c.checksum))
            .collect()
    });

    let mut reader: Box<dyn BlockReader> = if luks.is_some() || bitlocker.is_some() {
        // Read decrypted blocks through the LUKS or BitLocker view.
        Box::new(crate::block_reader::RawVolumeBlockReader::new(
            open_source(&luks)?,
            req.block_size,
        )?)
    } else {
        Box::new(WinBlockReader::open(&read_path, req.block_size)?)
    };
    if let Some(ranges) = zero_ranges {
        reader = Box::new(ZeroingBlockReader::new(reader, ranges));
    }
    // FAT/exFAT true-removal metadata patches (directory entries + allocation
    // table). Disjoint from the zeroed data clusters and from the NTFS boot-
    // sector patch below (which only fires for NTFS sources), so ordering is
    // immaterial — the patches win on their own byte ranges either way.
    if !fat_patches.is_empty() {
        info!(patches = fat_patches.len(), "Applying FAT/exFAT file-removal metadata patches");
        reader = Box::new(crate::block_reader::MultiPatchReader::new(
            reader,
            std::mem::take(&mut fat_patches),
        ));
    }

    // The NTFS backup boot sector lives at TotalSectors × BytesPerSector —
    // the first sector past the filesystem end (the partition is a few
    // sectors longer than the filesystem). That region cannot be read through
    // a volume handle (short read) and would be stored as zeros; NTFS then
    // "self-heals" the zeroed copy when Windows transiently auto-mounts a
    // differencing chain at attach time, writing into the child image and
    // failing verify on the volume's last block (confirmed by VHDX forensics:
    // exactly one healed sector, at TotalSectors, byte-equal to sector 0).
    // Store the correct copy ourselves: NTFS finds a valid backup boot sector
    // and writes nothing — and even if it writes, the bytes are identical.
    // (Skipped for a decrypted LUKS source — its inner FS is never NTFS — and
    // for a BitLocker one, whose volume device cannot be read at all while it
    // is locked; its backup boot sector comes through the decrypting view with
    // the rest of the volume.)
    if luks.is_none() && bitlocker.is_none() && volume_len > sector_size as u64 {
        let mut sector0 = vec![0u8; sector_size as usize];
        let mut probe = WinRawDevice::open_read(&read_path)?;
        probe.read_at(0, &mut sector0)?;
        drop(probe);
        if &sector0[3..7] == b"NTFS" {
            let bps = u16::from_le_bytes(sector0[0x0B..0x0D].try_into().unwrap()) as u64;
            let total_sectors =
                u64::from_le_bytes(sector0[0x28..0x30].try_into().unwrap());
            let backup_boot_offset = total_sectors.saturating_mul(bps);
            // Sanity: same sector size as the device, and the location must
            // fall within the volume's last MiB — a corrupt BPB must never
            // graft a boot sector into the middle of the data.
            if bps == sector_size as u64
                && backup_boot_offset + bps <= volume_len
                && backup_boot_offset >= volume_len.saturating_sub(MIB)
            {
                info!(
                    backup_boot_offset,
                    "Patching NTFS backup boot sector into the image tail"
                );
                reader = Box::new(crate::block_reader::PatchReader::new(
                    reader,
                    backup_boot_offset,
                    sector0,
                ));
            } else {
                warn!(
                    total_sectors,
                    bps, "NTFS BPB values out of range — tail patch skipped"
                );
            }
        }
    }

    let outcome = copy_used_blocks(
        &backup_id.to_string(),
        reader.as_mut(),
        used.as_ref(),
        parent_hashes.as_ref(),
        |block_number, data| {
            target.write_at(data_offset + block_number * req.block_size as u64, data)
        },
        &mut progress,
    )?;

    // Pre-allocate the metadata partition blocks in the VHDX/qcow2 image
    // by writing the initial sidecar blob before finalizing (prevents zero-write optimization).
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;

    // Construct a temporary initial sidecar to allocate the block
    let temp_manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id: parent.as_ref().map(|(_, s)| s.manifest.backup_id),
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: source.clone(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: disk_guid.clone(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
        partition_table: PartitionTable::Gpt,
        compression_algo: CompressionAlgo::None,
        compression_level: 0,
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: if smart_mode {
            BackupMode::UsedSectors
        } else {
            BackupMode::SectorBySector
        },
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    };
    let temp_image_file_name = req.image_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut temp_info = match &parent {
        None => SidecarInfo::new_full(temp_image_file_name, data_offset),
        Some((parent_path, _)) => {
            let parent_name = parent_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let role = match kind {
                BackupType::Incremental => ChainRole::Incremental,
                _ => ChainRole::Differential,
            };
            SidecarInfo::new_child(temp_image_file_name, role, parent_name, data_offset)
        }
    };
    temp_info.container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION.into()
    } else {
        req.image_format.extension().into()
    };
    temp_info.job_id = req.job_id.clone();
    temp_info.source_id = crate::source_identity::stable_source_id(&req.source);
    temp_info.source_disk_layout = req.source_disk.clone();
    let mut temp_blob = Vec::new();
    crate::sidecar::write_sidecar_v2_to(&mut temp_blob, &temp_info, &temp_manifest, &outcome.checksums)?;

    // Ensure the placeholder is at least meta_size to cover the entire partition space.
    if req.encryption.is_none() {
        let mut placeholder = vec![0u8; meta_size as usize];
        let placeholder_len = placeholder.len();
        let temp_blob_len = temp_blob.len();
        if temp_blob_len <= placeholder_len {
            placeholder[..temp_blob_len].copy_from_slice(&temp_blob);
        } else {
            placeholder.copy_from_slice(&temp_blob[..placeholder_len]);
        }
        // Set a non-zero byte at the end of the placeholder to ensure it spans the whole allocation
        if placeholder_len > 0 {
            placeholder[placeholder_len - 1] = 1;
        }
        target.write_at(meta_offset, &placeholder)?;
    }

    // Flushing a dynamic/differencing VHDX and detaching it writes all the
    // still-cached image data back to the destination drive — many seconds to
    // minutes for a large image. Signal a "finalize" phase (blocks_total == 0,
    // backup_id "finalize") so the UI shows progress instead of a frozen 100%.
    // Cancellation here is ignored: the data is already copied.
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: outcome.bytes_copied,
        speed_bps: 0,
                categories: None,
                excluded: None,
    });
    // Timed step by step inside finalize(): the stall lives in ONE of the
    // steps (flush of remaining cache vs handle close vs detach for VHDX;
    // metadata write + sync for qcow2), and the log says which.
    // An encrypted image seals this sidecar now and keeps `meta_size` bytes of
    // room, so the final one — with the exclusion figures and the checksums the
    // deletion pass touched — can take its place in the finished file.
    target.finalize(&temp_blob, meta_size as usize)?;

    // Notify VSS writers of success and release the shadow copy.
    let t = Instant::now();
    if let Some(session) = vss.take() {
        if let Err(e) = session.complete() {
            warn!(error = %e, "VSS BackupComplete failed (backup data is already safe)");
        }
    }
    info!(vss_release_ms = t.elapsed().as_millis(), "VSS released");

    // --- True removal of excluded files (NTFS/ReFS) ---
    // The FS driver deletes the recorded paths inside the image copy (the
    // source is never touched): catalog entries disappear and the space is
    // reclaimed on a restored volume. Failures before the merge fall back to
    // the zero-filled ghosts — still a valid backup; failures after the merge
    // fail the run (image/checksum mismatch must never be shipped). Checksums
    // of the driver-touched blocks are updated in place, so the sidecar below
    // describes the final image.
    let mut checksums = outcome.checksums;
    let mut used_blocks = used_blocks;
    let bytes_copied_so_far = outcome.bytes_copied;
    // The exclusion passes below run for MINUTES on a big volume (this run:
    // scan 34 s, delete 10 s, zero ~40 s, merge ~1.5 min, re-checksum ~4 min on
    // an HDD) — under the "сброс кэша" label that read as a hang. They get
    // their own phase id, so the interface can say what is actually happening.
    let mut phase_tick = move |id: &'static str, entries: u64, total: u64| {
        let _ = progress(&BackupProgress {
            backup_id: id.into(),
            status: BackupStatus::Running,
            blocks_done: entries,
            blocks_total: total,
            bytes_written: bytes_copied_so_far,
            speed_bps: 0,
                categories: None,
                excluded: None,
        });
    };

    // The source could not be walked, but the IMAGE can be: it holds the
    // volume's NTFS in the clear, so the exclusions happen there instead —
    // matched, deleted by the FS driver, and their clusters zeroed. This is the
    // only path on which a locked BitLocker volume honours the rules at all,
    // and it is where the figures for the result screen come from too.
    if in_image_exclusions {
        let tick = &mut |n, total| phase_tick("exclusions", n, total);
        let done = match req.image_format {
            ImageFormat::Vhdx => crate::true_removal_win::exclude_inside_image(
                &req.image_path,
                &req.exclusion_patterns,
                data_offset,
                volume_len,
                req.block_size,
                &mut checksums,
                tick,
            )?,
            // Same pass over a copy-on-write overlay attached over iSCSI: the driver
            // that owns NTFS does the deleting either way, and what it sits on
            // is the image format's business, not the pass's.
            ImageFormat::Qcow2 => crate::qcow2_removal_win::exclude_inside_qcow2(
                &req.image_path,
                &req.exclusion_patterns,
                data_offset,
                volume_len,
                req.block_size,
                Some(crate::qcow2::zstd_level_or_default(req.zstd_level)),
                &mut checksums,
                tick,
            )?,
        };
        match done {
            Some(done) => {
                info!(
                    files = done.files_excluded,
                    deleted = done.outcome.entries_deleted,
                    failed = done.outcome.entries_failed,
                    bytes_zeroed = done.bytes_zeroed,
                    "exclusions applied inside the image of an encrypted volume"
                );
                used_blocks = checksums.len() as u64;
                excluded_files = done.files_excluded;
                excluded_bytes = done.bytes_excluded;
                categories = Some(done.categories);
                file_scan_skipped = None;
            }
            None => {
                // Already logged in detail; the result screen must still say it.
                file_scan_skipped = Some(tr!("engine_win.skip_in_image_failed").into_owned());
            }
        }
    }

    if !removal_paths.is_empty() {
        let tick = &mut |n, total| phase_tick("exclusions", n, total);
        match req.image_format {
            ImageFormat::Vhdx => {
                // Differencing child + MergeVirtualDisk via the kernel VHDX driver.
                if crate::true_removal_win::remove_excluded_from_image(
                    &req.image_path,
                    &removal_paths,
                    data_offset,
                    volume_len,
                    req.block_size,
                    &mut checksums,
                    tick,
                )?
                .is_some()
                {
                    used_blocks = checksums.len() as u64;
                }
            }
            // An encrypted image gets the same pass over a SEALED scratch, and
            // the changed clusters go back by rebuilding the container: it is
            // written once, so they cannot be folded in place.
            ImageFormat::Qcow2 if req.encryption.is_some() => {
                let keys = req.encryption.as_ref().expect("checked above");
                let chain = keys.open_chain_of(&req.image_path)?;
                if crate::obk_removal_win::remove_excluded_from_obk(
                    &req.image_path,
                    &chain.key,
                    &removal_paths,
                    data_offset,
                    volume_len,
                    req.block_size,
                    crate::qcow2::zstd_level_or_default(req.zstd_level),
                    &mut checksums,
                    tick,
                )?
                .is_some()
                {
                    used_blocks = checksums.len() as u64;
                }
            }
            ImageFormat::Qcow2 => {
                // RW-mount over a CoW overlay, delete, fold the touched
                // clusters back with Qcow2InPlaceMerge (Q-R3).
                if crate::qcow2_removal_win::remove_excluded_from_qcow2(
                    &req.image_path,
                    &removal_paths,
                    data_offset,
                    volume_len,
                    req.block_size,
                    Some(crate::qcow2::zstd_level_or_default(req.zstd_level)),
                    &mut checksums,
                    tick,
                )?
                .is_some()
                {
                    used_blocks = checksums.len() as u64;
                }
            }
        }
    }

    // --- Sidecar ---
    // Child sidecars carry the FULL state checksums (every used block, written
    // or inherited), so verify/restore/mount treat the leaf exactly like a
    // full image — the VHDX chain resolves inherited blocks.
    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let manifest = ImageManifest {
        backup_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id: parent.as_ref().map(|(_, s)| s.manifest.backup_id),
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: source.clone(),
            model: String::new(),
            serial: String::new(),
            size_bytes: volume_len,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Volume".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid,
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi, // populated properly with inventory in phase 4
        partition_table: PartitionTable::Gpt,
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::zstd_level_or_default(req.zstd_level),
        },
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: if smart_mode {
            BackupMode::UsedSectors
        } else {
            BackupMode::SectorBySector
        },
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        // Encrypted source captured in the clear (used blocks only): record the
        // kind and that encryption must be re-applied after restore. The LUKS
        // header is intentionally not stored — a plaintext capture restores to
        // an unencrypted volume, so the original header is not needed.
        volume_encryption: {
            use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
            // The restore screen reads this: without it a decrypted BitLocker
            // volume would be shown as plain NTFS with no hint that the
            // original was encrypted and the restored copy will not be (D14).
            let kind = luks
                .as_ref()
                .map(|p| p.kind.clone())
                .or_else(|| bitlocker.as_ref().map(|b| b.params.kind.clone()));
            kind.map(|kind| VolumeEncryptionState {
                kind,
                capture_mode: VolumeCaptureMode::Plaintext,
                luks_header_backup_b64: None,
                note: Some("Decrypted at backup; re-enable encryption after restore".into()),
            })
        },
    };

    let mut sidecar_info = match &parent {
        None => SidecarInfo::new_full(image_file_name, data_offset),
        Some((parent_path, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_file_name = parent_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_file_name, data_offset)
        }
    };
    sidecar_info.container = if req.encryption.is_some() {
        crate::seal::obk::OBK_EXTENSION.into()
    } else {
        req.image_format.extension().into()
    };
    // The job id is what ties images into this job's chains: list_job_chains
    // filters on it, so without it every next run plans a FULL backup.
    sidecar_info.job_id = req.job_id.clone();
    // Who this volume IS, not what it is called: a label or a drive letter is
    // for display, and two volumes can share both (audit AUD-10).
    sidecar_info.source_id = crate::source_identity::stable_source_id(&req.source);
    sidecar_info.source_disk_layout = req.source_disk.clone();
    sidecar_info.categories = categories;
    sidecar_info.excluded_files = excluded_files;
    sidecar_info.excluded_bytes = excluded_bytes;
    sidecar_info.filesystem = detected_fs;
    // The container's own header, kept so a restore can put the encryption
    // back exactly as it was (D19). Failing to read it is not fatal — the
    // copy is still a good copy — but it does mean this image can only be
    // restored decrypted, so it is logged loudly.
    let luks_headers: Vec<own_backup_common::image::LuksHeaderBackup> = match &luks {
        Some(p) => {
            match WinRawDevice::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, p))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(bytes = bytes.len(), "LUKS header captured — restore can re-encrypt");
                    vec![own_backup_common::image::LuksHeaderBackup {
                        part_index: 0,
                        data_offset: p.data_offset,
                        sector_size: p.sector_size,
                        kind: p.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }]
                }
                Err(e) => {
                    warn!(error = %e, "could not read the LUKS header — this copy will only restore decrypted");
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    };
    sidecar_info.luks_headers = luks_headers;
    let sidecar_path = sidecar_path_for(&req.image_path);


    // M0: Write embedded sidecar v2 to the meta partition in the finished image file
    let sector = sector_size as u64;
    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
    let meta_offset = (last_lba + 1) * sector;
    let mut blob_bytes = Vec::new();
    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar_info, &manifest, &checksums)?;
    match (&req.encryption, req.image_format) {
        (Some(keys), _) => {
            // No guest meta partition to write into: the sidecar replaces the
            // reserved one inside the container, sealed under the same chain
            // key and with a fresh nonce.
            let chain = keys.open_chain_of(&req.image_path)?;
            crate::seal::obk::update_sealed_metadata(&req.image_path, &chain.key, &blob_bytes)?;
        }
        (None, ImageFormat::Vhdx) => {
            crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob_bytes)?;
        }
        (None, ImageFormat::Qcow2) => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob_bytes)?;
        }
    }

    info!(
        image = %req.image_path.display(),
        kind = %kind,
        blocks_written = outcome.blocks_copied,
        blocks_unchanged = outcome.blocks_unchanged,
        bytes = outcome.bytes_copied,
        excluded_files,
        secs = started.elapsed().as_secs(),
        "Volume backup complete"
    );

    Ok(VolumeBackupSummary {
        image_path: req.image_path.clone(),
        sidecar_path,
        source_bytes: volume_len,
        total_blocks,
        used_blocks,
        blocks_copied: outcome.blocks_copied,
        bytes_copied: outcome.bytes_copied,
        duration: started.elapsed(),
        smart_mode,
        vss_used,
        backup_kind: kind,
        blocks_unchanged: outcome.blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
        file_scan_skipped,
    })
}

// ===========================================================================
// Whole-disk backup: one image per disk (plan D1, .ai/whole-disk-single-image)
// ===========================================================================

/// One partition to include in a whole-disk backup.
pub struct DiskPartitionBackup {
    /// Source volume device (`\\.\Volume{…}`, a drive letter, or `part:…`).
    pub device: String,
    /// GPT slot (matches [`own_backup_common::gpt::GptPartitionEntry::index`]).
    pub part_index: u32,
    /// Passphrase for a LUKS partition (its decrypted data is captured).
    pub passphrase: Option<Zeroizing<String>>,
}

/// Parameters for a whole-disk (bootable) backup into a single image.
pub struct DiskBackupRequest {
    pub disk_number: u32,
    pub disk_model: String,
    pub partitions: Vec<DiskPartitionBackup>,
    pub image_path: PathBuf,
    pub block_size: u32,
    pub image_format: ImageFormat,
    pub job_id: Option<String>,
    /// Present for an encrypted job: the whole-disk image is written as `.obk`,
    /// with its sidecar — including the restore manifest — sealed inside the
    /// container instead of in a guest meta partition (§7.3).
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// File-exclusion glob patterns for Windows-family filesystems (NTFS/ReFS/
    /// FAT), applied per partition (plan D2): NTFS/ReFS drop + zero the matched
    /// clusters (data never leaves the machine; the catalog entry stays as a
    /// zero-filled ghost); FAT32/exFAT also patch the metadata so the files
    /// fully disappear.
    pub exclusion_patterns_windows: Vec<String>,
    /// File-exclusion masks for Linux filesystems (ext/btrfs/xfs). Selected per
    /// partition by filesystem; enforcement on ext/btrfs/xfs is a follow-up
    /// (stored/passed now, applied once the Linux-FS exclusion scanner lands).
    pub exclusion_patterns_linux: Vec<String>,
    /// Full / Incremental / Differential (plan D5). A non-full kind needs
    /// `parent_image` to be the previous whole-disk image of the SAME disk; if
    /// the chain turns out unusable (missing parent, geometry changed, format
    /// mismatch) the engine falls back to a full backup and reports it in the
    /// summary.
    pub kind: BackupType,
    /// Parent whole-disk image for an incremental (the previous version) or
    /// differential (the chain's full). A differencing child is written over it.
    pub parent_image: Option<PathBuf>,
    /// Full source-disk layout snapshot (from the GUI's live inventory) to embed
    /// in the disk manifest, so a bare-metal restore can identify the archived
    /// disk without jobs.json. `None` for CLI runs (the manifest then omits it).
    pub source_disk: Option<own_backup_common::disk::DiskInfo>,
    /// The user ticked "Загрузочная копия диска" for this disk.
    ///
    /// It decides one thing here: whether the machine's UEFI boot entries go
    /// into the image. They are the firmware's, not the disk's, and a copy of a
    /// data disk has no business carrying the boot menu.
    pub bootable: bool,
    /// zstd level for qcow2 and `.obk` (see [`VolumeBackupRequest::zstd_level`]).
    pub zstd_level: i32,
}

/// Read the machine's UEFI boot entries, to be embedded in the image.
///
/// Why this is worth anything after a restore: [`crate::gpt_writer`] recreates
/// the disk with every partition's ORIGINAL unique GUID, offset and size, and a
/// boot entry names its partition by exactly those three things. So an entry
/// saved here still points at the right partition on the restored disk — there
/// is nothing to translate, only something to put back.
///
/// Only for a copy the user marked BOOTABLE. On any other disk these entries
/// describe a boot menu that copy has nothing to do with: a data disk's backup
/// carrying the machine's firmware configuration is metadata nobody asked for,
/// and a restore of it onto another machine would be actively wrong.
///
/// `None` is not an error. A machine on a plain BIOS has no such entries, and
/// an unelevated run cannot read them; neither is a reason to spoil a backup
/// that succeeded.
fn boot_entries_for_image(bootable: bool) -> Option<crate::efivars::BootBackup> {
    if !bootable {
        info!("not a bootable copy — the machine's boot entries are not part of it");
        return None;
    }
    match crate::efivars_win::export() {
        Ok(backup) => {
            info!(entries = backup.readable.len(), "boot entries read into the image");
            Some(backup)
        }
        Err(e) => {
            info!(error = %e, "boot entries not read — the copy will carry none");
            None
        }
    }
}

/// Outcome of a completed whole-disk backup.
pub struct DiskBackupSummary {
    pub image_path: PathBuf,
    pub bytes_copied: u64,
    pub blocks_copied: u64,
    pub partitions: u32,
    pub duration: Duration,
    /// What was actually produced — may differ from the requested kind when a
    /// broken/incompatible parent forced a fallback to a full backup.
    pub backup_kind: BackupType,
    /// Used image blocks skipped because they matched the parent version.
    pub blocks_unchanged: u64,
    /// Files excluded across all partitions (plan D2).
    pub excluded_files: u64,
    /// Bytes their extents freed from the image.
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of kept files across all partitions (for the
    /// dashboard); `None` when no exclusion walk ran on any partition.
    pub categories: Option<crate::exclusions::CategorySizes>,
    /// Partitions whose files nobody walked, and why — named, because on a
    /// whole-disk copy "исключения не применялись" without saying to WHICH
    /// partition is barely better than silence. `None` when every partition
    /// was walked.
    pub file_scan_skipped: Option<String>,
}

/// One prepared partition: its VSS snapshot, decrypt params, used-map and the
/// GPT entry it fills. Snapshots for all partitions are taken up front so the
/// image is cross-volume consistent, then copied in one pass.
struct PreparedPartition {
    part_index: u32,
    device: String,
    read_path: String,
    luks: Option<crate::luks::LuksParams>,
    /// Ключ и окно на диск для запертого BitLocker-тома. Читать такой том через
    /// его собственное устройство нельзя — оно отвечает 0x80310000 на всё.
    bitlocker: Option<BitLockerSource>,
    /// Тот же том, но пароля нет: копируется шифротекстом через окно на диск.
    locked_window: Option<(String, u64, u64)>,
    /// Запрет записи на диск-источник: держится, пока раздел, открытый силами
    /// Windows, остаётся смонтированным — то есть до конца копирования.
    #[allow(dead_code)]
    ro_guard: Option<crate::windows_unlock_win::ReadOnlyDisk>,
    /// The container's own header, captured so a restore can put the
    /// encryption back (D19). `None` when the partition was not encrypted or
    /// the header could not be read.
    luks_header: Option<own_backup_common::image::LuksHeaderBackup>,
    used: Box<dyn UsedSectorMap>,
    volume_len: u64,
    part_offset: u64,
    filesystem: String,
    /// Excluded byte ranges (VOLUME-relative) and FAT/exFAT metadata patches for
    /// this partition (plan D2); empty when no exclusions apply.
    zero_ranges: ByteRanges,
    patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2).
    removal_paths: Vec<crate::exclusions_win::ExcludedPath>,
    /// The encrypted container this partition holds, if any — recorded whether
    /// or not it was opened, so the manifest can say how it was captured.
    container: Option<own_backup_common::image::VolumeEncryptionKind>,
    #[allow(dead_code)]
    vss: Option<VssSnapshotSession>, // kept alive until the copy is done
}

/// Which encrypted container a volume's first sectors say it is.
///
/// Costs nothing and needs no password — LUKS puts its magic and version at
/// offset 0, BitLocker its signature in the boot sector — and it is the only
/// way to know what a container we could NOT open was. Without it a locked
/// partition copied as ciphertext and an unlocked one copied in the clear look
/// the same in the manifest, and the restore screen has to guess which of two
/// opposite things to tell the user.
fn container_kind(header: &[u8]) -> Option<own_backup_common::image::VolumeEncryptionKind> {
    use own_backup_common::image::VolumeEncryptionKind as K;
    if crate::bitlocker::is_bitlocker(header) {
        return Some(K::BitLocker);
    }
    if crate::luks::is_luks(header) {
        // Version is the two big-endian bytes right after the magic.
        return Some(match u16::from_be_bytes([header[6], header[7]]) {
            1 => K::Luks1,
            2 => K::Luks2,
            _ => K::Other,
        });
    }
    None
}

/// Short display token for a source device (mirrors core's source_token):
/// a drive letter, `V`+8 hex of a volume GUID, or `P<disk><offsetHex>`.
fn disk_source_token(device: &str) -> String {
    let d = device.trim_start_matches(r"\\.\").trim_start_matches(r"\\?\");
    if let Some(rest) = d.strip_prefix("part:") {
        let mut it = rest.split(':');
        if let (Some(disk), Some(off)) = (it.next(), it.next()) {
            if let (Ok(dn), Ok(o)) = (disk.parse::<u32>(), off.parse::<u64>()) {
                return format!("P{dn}{:X}", o);
            }
        }
    }
    let bytes = d.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes[0].to_ascii_uppercase().to_string();
    }
    if let Some(pos) = d.find("Volume{") {
        let hex: String = d[pos + 7..].chars().take(8).collect();
        return format!("V{}", hex.to_uppercase());
    }
    "X".into()
}

/// A prepared partition used-map plus its file-exclusion outcome (plan D2).
struct PreparedMap {
    /// Used-sector map (`None` → sector-by-sector). For NTFS/FAT with exclusions
    /// the excluded clusters are already dropped from this map.
    map: Option<Box<dyn UsedSectorMap>>,
    /// Detected filesystem name for the manifest.
    filesystem: String,
    /// Excluded byte ranges (VOLUME-relative, finalized): zeroed in the image
    /// copy so excluded file DATA never leaves the machine.
    zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): deleted
    /// directory entries + freed allocation-table chains, so excluded FAT files
    /// truly disappear instead of remaining zero-filled ghosts.
    patches: Vec<(u64, Vec<u8>)>,
    /// NTFS/ReFS matched paths for the post-copy true-removal pass (plan D2):
    /// the FS driver deletes them inside the finished image so the catalog
    /// entry disappears too (not just a zero-filled ghost). Empty for FAT
    /// (patched directly) and Linux filesystems.
    removal_paths: Vec<crate::exclusions_win::ExcludedPath>,
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Per-category byte breakdown of the KEPT files on this partition (powers
    /// the dashboard's data breakdown); `None` when no exclusion walk ran.
    categories: Option<crate::exclusions::CategorySizes>,
    /// Why no walk ran over this partition, for the result screen. `None` when
    /// one did.
    skipped: Option<String>,
}

/// Build a partition's used-map and apply file exclusions (plan D2). The
/// filesystem is determined from the map that parses (over the DECRYPTED view
/// for a LUKS volume, so an unlocked Linux root reports "btrfs", not "unknown").
///
/// The exclusion mask set is chosen by filesystem family: `patterns_windows`
/// for NTFS/ReFS/FAT, `patterns_linux` for ext/btrfs/xfs. NTFS/ReFS drop + zero
/// the excluded clusters (the catalog entry survives as a zero-filled ghost —
/// whole-disk NTFS catalog true-removal is a later step); FAT32/exFAT also patch
/// the metadata so the files vanish. Enforcement on ext/btrfs/xfs is a follow-up
/// (no Linux-FS exclusion scanner yet), so Linux patterns are selected but not
/// yet acted on; LUKS-decrypted volumes are captured whole.
///
/// `tick` reports "prepare"-phase progress (and can cancel by returning `Err`).
/// Карта и исключения для раздела, который Windows не монтирует: NTFS внутри
/// BitLocker, прочитанная через расшифрованное окно.
///
/// Исключения решаются ДО копии, по $MFT (план exclusions-before-copy, B3):
/// перечислить файлы обходом дерева тут нельзя — тома для Windows не
/// существует, — а по MFT можно, и тогда исключённые кластеры просто не
/// попадают в карту.
/// Разобрать исключения по $MFT тома, который Windows смонтировала.
///
/// Том открывается как устройство и читается насквозь — тем же сканером, что
/// работает над расшифрованным окном BitLocker. Ничего не пишет.
#[cfg(target_os = "windows")]
/// `tick(records_done, records_total)`. The total is the whole point: the MFT
/// has a known record count (`$MFT` size / record size), so the wait can be a
/// bar with an end and an estimate instead of a number running up. Only this
/// scanner can say it — a directory walk does not know how many entries it is
/// about to meet, and passes 0.
fn scan_ntfs_exclusions_by_mft(
    read_path: &str,
    patterns: &[String],
    tick: &mut impl FnMut(u64, u64) -> Result<()>,
) -> Result<crate::ntfs_scan::MftScan> {
    let mut dev = WinRawDevice::open_read(read_path)?;
    let cur = crate::ntfs_scan::RawVolumeCursor::new(&mut dev);
    let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
    crate::ntfs_scan::scan_mft_with(&mut buffered, patterns, |done, total| {
        let _ = tick(done, total);
    })
}

fn prepare_encrypted_partition_map(
    mut src: Box<dyn crate::fat_map::RawVolume + Send>,
    block_size: u32,
    patterns: &[String],
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> PreparedMap {
    let mut out = PreparedMap {
        map: None,
        filesystem: "NTFS".into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
        skipped: None,
    };

    let scan = if patterns.is_empty() {
        out.skipped = Some(no_patterns());
        None
    } else {
        let cur = crate::ntfs_scan::RawVolumeCursor::new(src.as_mut());
        let mut buffered = crate::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
        match crate::ntfs_scan::scan_mft_with(&mut buffered, patterns, |n, total| {
            let _ = tick(n, total);
        }) {
            Ok(scan) => {
                info!(
                    files = scan.files_excluded,
                    bytes = scan.bytes_excluded,
                    kept = scan.kept.len(),
                    paths = scan.paths.len(),
                    records = scan.records_scanned,
                    "exclusions resolved from the MFT BEFORE the copy"
                );
                Some(scan)
            }
            Err(e) => {
                // Грязный том, не NTFS внутри, битая запись — копия делается
                // целиком, и об этом сказано вслух.
                warn!(error = %e, "MFT scan failed — this partition keeps every file");
                out.skipped = Some(tr!("engine_win.skip_mft_failed", error = e).into_owned());
                None
            }
        }
    };

    let excluded = scan.as_ref().map(|s| &s.ranges).filter(|r| !r.is_empty());
    match crate::ntfs_map::build_ntfs_used_map_excl(src.as_mut(), block_size, excluded) {
        Ok(map) => out.map = Some(Box::new(map)),
        Err(e) => {
            warn!(error = %e, "no NTFS map over the decrypted partition — copying every block");
            out.skipped
                .get_or_insert_with(|| tr!("engine_win.skip_ntfs_map_failed", error = e).into_owned());
            return out;
        }
    }
    if let Some(scan) = scan {
        out.excluded_files = scan.files_excluded;
        out.excluded_bytes = scan.bytes_excluded;
        out.categories = Some(scan.categories);
        out.removal_paths = scan
            .paths
            .iter()
            .map(|p| crate::exclusions_win::ExcludedPath {
                rel: p.rel.clone(),
                is_dir: p.is_dir,
                is_reparse: p.is_reparse,
            })
            .collect();
        out.zero_ranges = scan.ranges;
    }
    out
}

fn prepare_partition_map(
    read_path: &str,
    block_size: u32,
    luks: &Option<crate::luks::LuksParams>,
    patterns_windows: &[String],
    patterns_linux: &[String],
    // Источник заморожен (снимок VSS или офлайн на носителе) — только тогда
    // можно разбирать сырые метаданные: том под записью для этого не годится.
    frozen: bool,
    // `tick(entries_done, entries_total)`; total 0 = «сколько всего, неизвестно»
    // (обход каталога этого не знает, чтение $MFT — знает).
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> PreparedMap {
    // NTFS/ReFS/FAT use the Windows masks; ext/btrfs/xfs use the Linux masks.
    let patterns = patterns_windows;
    let none = |fs: &str, map: Option<Box<dyn UsedSectorMap>>| PreparedMap {
        map,
        filesystem: fs.into(),
        zero_ranges: ByteRanges::new(),
        patches: Vec::new(),
        removal_paths: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
        skipped: None,
    };
    // The same, with the reason nobody walked this partition's files.
    let none_because =
        |fs: &str, map: Option<Box<dyn UsedSectorMap>>, why: &str| PreparedMap {
            skipped: Some(why.into()),
            ..none(fs, map)
        };
    // Every exclusion scanner reads through this. The alignment wrapper is not
    // a nicety: a raw Windows device refuses reads that do not start on a
    // sector boundary, and ext inodes are 256 bytes — the root inode among the
    // unaligned ones. Without it the ext scan failed on its first read, skipped
    // the whole tree, and reported "excluded_files=0" as if it had worked.
    let open = || -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        let dev = WinRawDevice::open_read(read_path)?;
        Ok(match luks {
            Some(p) => Box::new(crate::aligned_volume::AlignedVolume::new(
                crate::decrypt::DecryptingVolume::new(Box::new(dev), p.clone()),
            )),
            None => Box::new(crate::aligned_volume::AlignedVolume::new(dev)),
        })
    };
    // NTFS first (only on a plain, non-decrypted volume), then FAT/ext/btrfs/xfs.
    if luks.is_none() {
        if let Ok(mut m) = NtfsUsedSectorMap::open(read_path, block_size) {
            let mut out = none("ntfs", None);
            if !patterns.is_empty() {
                // Сначала по $MFT — как и в копии одного тома (B1): вдвое
                // быстрее и не теряет pagefile.sys/swapfile.sys, которые обходу
                // не открыть. Обход остаётся запасным: ReFS (у неё нет MFT),
                // грязный том, неразобранная запись.
                let mut mft_tick = |n: u64, total: u64| tick(n, total);
                let fast = if frozen {
                    scan_ntfs_exclusions_by_mft(read_path, patterns, &mut mft_tick)
                } else {
                    Err(OwnBackupError::Device(tr!("engine_win.source_not_frozen").into_owned()))
                };
                match fast {
                    Ok(mft) => {
                        out.excluded_files = mft.files_excluded;
                        out.excluded_bytes = mft.bytes_excluded;
                        out.categories = Some(mft.categories);
                        out.removal_paths = mft
                            .paths
                            .iter()
                            .map(|p| crate::exclusions_win::ExcludedPath {
                                rel: p.rel.clone(),
                                is_dir: p.is_dir,
                                is_reparse: p.is_reparse,
                            })
                            .collect();
                        info!(
                            part = %read_path,
                            files = mft.files_excluded,
                            bytes = mft.bytes_excluded,
                            kept = mft.kept.len(),
                            "whole-disk: exclusions resolved from the MFT"
                        );
                        if !mft.ranges.is_empty() {
                            m.apply_excluded_ranges(&mft.ranges);
                            out.zero_ranges = mft.ranges;
                        }
                    }
                    Err(e) => {
                        warn!(part = %read_path, error = %e,
                              "whole-disk: MFT scan unavailable — falling back to the directory walk");
                        if let Ok(mut scan) =
                            collect_excluded_ranges(read_path, m.cluster_size(), patterns, |n| tick(n, 0))
                        {
                            out.excluded_files = scan.files_excluded;
                            out.excluded_bytes = scan.bytes_excluded;
                            out.categories = Some(scan.categories);
                            out.removal_paths = std::mem::take(&mut scan.paths);
                            if !scan.ranges.is_empty() {
                                m.apply_excluded_ranges(&scan.ranges);
                                out.zero_ranges = std::mem::take(&mut scan.ranges);
                            }
                        } else {
                            warn!(part = %read_path, "whole-disk: NTFS exclusion scan failed — keeping all files");
                            out.skipped =
                                Some(tr!("engine_win.skip_list_failed", fs = "NTFS").into_owned());
                        }
                    }
                }
            } else {
                out.skipped = Some(no_patterns());
            }
            m.release_bitmap();
            out.map = Some(Box::new(m));
            return out;
        }
    }
    let Ok(mut src) = open() else {
        return none_because("unknown", None, &tr!("engine_win.skip_partition_unreadable"));
    };
    if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), block_size) {
        let fs = m.filesystem.to_lowercase();
        let is_fat = m.filesystem == "FAT32" || m.filesystem == "exFAT";
        drop(src);
        if is_fat && !patterns.is_empty() {
            let mut out = none(&fs, None);
            if let Ok(mut dev) = WinRawDevice::open_read(read_path) {
                if let Ok(scan) = crate::fat_exclusions::scan_fat_exclusions(&mut dev, patterns, |n| tick(n, 0)) {
                    out.excluded_files = scan.files_excluded;
                    out.excluded_bytes = scan.bytes_excluded;
                    out.categories = Some(scan.categories);
                    if !scan.ranges.is_empty() {
                        // Rebuild the map so excluded clusters drop out.
                        if let Ok(mut dev2) = WinRawDevice::open_read(read_path) {
                            if let Ok(rebuilt) = crate::fat_map::build_fat_used_map_excl(&mut dev2, block_size, Some(&scan.ranges)) {
                                out.map = Some(Box::new(rebuilt));
                            }
                        }
                        out.zero_ranges = scan.ranges;
                        out.patches = scan.patches;
                    }
                }
            }
            if out.map.is_none() {
                // Exclusion rebuild failed → fall back to the full FAT map.
                if let Ok(mut src2) = open() {
                    if let Ok(m2) = crate::fat_map::build_fat_used_map(src2.as_mut(), block_size) {
                        out.map = Some(Box::new(m2));
                    }
                }
            }
            return out;
        }
        return none_because(
            &fs,
            Some(Box::new(m)),
            &if patterns.is_empty() {
                no_patterns()
            } else {
                tr!("engine_win.skip_fat_only").into_owned()
            },
        );
    }
    if let Ok(mut m) = crate::ext_map::build_ext_used_map(src.as_mut(), block_size) {
        // ext2/3/4: full file-level exclusions (Stage 1 drop+zero, Stage 2
        // metadata true-removal) — ext data blocks are exclusively owned, so
        // this is safe (unlike btrfs). Scanned over the DECRYPTED view for LUKS.
        drop(src);
        let mut out = none("ext4", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                match crate::ext_exclusions::scan_ext_exclusions(src2.as_mut(), patterns_linux, |n| tick(n, 0)) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        if !scan.ranges.is_empty() {
                            // Only what Stage 2 actually freed leaves the image.
                            // A zero-filled ghost keeps its blocks: the
                            // filesystem still lists the file, so those sectors
                            // must be restored as the zeros they became.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "ext exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "ext").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return out;
    }
    if let Ok(mut m) = crate::btrfs_map::build_btrfs_used_map(src.as_mut(), block_size) {
        // btrfs: CoW-safe file exclusions. Only extents PROVEN exclusively
        // owned by matched files are zeroed + dropped; anything shared with a
        // snapshot or a reflink is kept. Files whose extents are all private
        // are removed from the FS tree outright (Stage 2).
        //
        // Stage 2 destroyed a copy once (2026-07-28): excluding dense subtrees
        // (/proc, /sys, /dev, /run, /var/cache) emptied whole FS-tree leaves,
        // which stayed in the tree with no items while their parents still
        // pointed at them. The restored system aborted its first mount with
        // `tree first key mismatch … has=(0,0,0)`; later, stale csum entries
        // over freed extents corrupted the tree at first write. Three guards
        // now stand in the way (FS-, extent- and csum-tree leaves: files whose
        // removal would empty or overflow a leaf are spared to Stage 1), freed
        // extents LOSE their csum entries (`btrfs_del_csums` contract), and if
        // a leaf still breaks, the whole Stage 2 is discarded. A failed scan
        // keeps every file — a bigger image is always better than a corrupt
        // one.
        drop(src);
        let mut out = none("btrfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                // `true` = Stage 2: excluded files are REALLY removed from the
                // catalog (product requirement 2026-07-28, CLAUDE.md). The
                // scanner spares anything it cannot remove safely (shared
                // extents, hardlinks, leaf-breaking edits) down to Stage 1,
                // and any Stage-2 failure keeps the whole volume on Stage 1 —
                // see `scan_btrfs_exclusions`.
                match crate::btrfs_exclusions::scan_btrfs_exclusions(src2.as_mut(), patterns_linux, true, |n| tick(n, 0)) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        if !scan.ranges.is_empty() {
                            // Stage-2 frees may be dropped; Stage-1 ghosts keep
                            // their blocks — btrfs rewrote their checksums to
                            // the checksum of zeros, so a restore that leaves
                            // the target's old bytes there fails every read
                            // (EIO) instead of returning an empty file.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "btrfs exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "btrfs").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return out;
    }
    if let Ok(mut m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), block_size) {
        // xfs: reflink-aware Stage-1 zeroing (data is not checksummed on xfs, so
        // no metadata edits) — only extents proven NOT shared (refcount==1) are
        // zeroed + dropped. Catalog true-removal (Stage 2) is a follow-up.
        drop(src);
        let mut out = none("xfs", None);
        if !patterns_linux.is_empty() {
            if let Ok(mut src2) = open() {
                // `true` — Stage 2: записи каталогов, иноды и блоки. План
                // собирается целиком и применяется только если все три шага
                // согласились; иначе файл остаётся нулевым призраком.
                match crate::xfs_exclusions::scan_xfs_exclusions(src2.as_mut(), patterns_linux, true, |n| tick(n, 0)) {
                    Ok(scan) => {
                        out.excluded_files = scan.files_excluded;
                        out.excluded_bytes = scan.bytes_excluded;
                        out.categories = Some(scan.categories);
                        out.patches = scan.patches;
                        if !scan.ranges.is_empty() {
                            // Из образа уходит только то, что Stage 2 успел
                            // вернуть в свободное место. Призрак держит свои
                            // блоки: ФС всё ещё их читает.
                            m.apply_excluded_ranges(&scan.droppable, block_size);
                            out.zero_ranges = scan.ranges;
                        }
                    }
                    Err(e) => {
                        warn!(part = %read_path, error = %e, "xfs exclusions failed — keeping all files");
                        out.skipped =
                            Some(tr!("engine_win.skip_list_failed", fs = "xfs").into_owned());
                    }
                }
            }
        } else {
            out.skipped = Some(no_patterns());
        }
        out.map = Some(Box::new(m));
        return out;
    }
    none_because("unknown", None, &tr!("engine_win.skip_fs_unknown"))
}

/// One partition region inside the whole-disk image: its byte offset on the
/// disk, its decrypted length, and a reader for its (decrypted) volume data.
struct DiskRegion {
    part_offset: u64,
    volume_len: u64,
    reader: Box<dyn crate::fat_map::RawVolume + Send>,
    /// Excluded byte ranges (VOLUME-relative): zeroed after reading so excluded
    /// file data never enters the image (plan D2).
    zero_ranges: ByteRanges,
    /// FAT/exFAT metadata patches (VOLUME-relative offset → bytes): applied over
    /// the read data so excluded FAT files vanish from the directory/allocation.
    patches: Vec<(u64, Vec<u8>)>,
}

/// A [`BlockReader`] over the WHOLE disk image: block `N` (bytes `N*bs..`) is
/// assembled from the GPT write-ops and the partition regions at their real
/// offsets; bytes belonging to nothing read as zeros. This lets partitions sit
/// at any sector offset while the checksum grid stays the image's own blocks.
struct DiskCompositeReader {
    block_size: u32,
    image_size: u64,
    gpt_ops: Vec<(u64, Vec<u8>)>,
    regions: Vec<DiskRegion>,
}

impl BlockReader for DiskCompositeReader {
    fn read_block(&mut self, n: u64) -> Result<Vec<u8>> {
        let bs = self.block_size as u64;
        let start = n * bs;
        let len = bs.min(self.image_size - start) as usize;
        let end = start + len as u64;
        let mut buf = vec![0u8; len];
        // GPT bytes (protective MBR, primary/backup headers + entry arrays).
        for (off, bytes) in &self.gpt_ops {
            let g_end = off + bytes.len() as u64;
            let from = start.max(*off);
            let to = end.min(g_end);
            if from < to {
                buf[(from - start) as usize..(to - start) as usize]
                    .copy_from_slice(&bytes[(from - off) as usize..(to - off) as usize]);
            }
        }
        // Partition data (overwrites any overlap — partitions never overlap the
        // GPT head/tail in a valid layout, so order is immaterial).
        for r in &mut self.regions {
            let p_end = r.part_offset + r.volume_len;
            let from = start.max(r.part_offset);
            let to = end.min(p_end);
            if from < to {
                let vol_off = from - r.part_offset;
                let dst = &mut buf[(from - start) as usize..(to - start) as usize];
                r.reader.read_at(vol_off, dst)?;
                // Plan D2 — content zeroing: excluded file clusters read as zero
                // so their data never enters the image.
                r.zero_ranges.zero_overlaps(vol_off, dst);
                // FAT/exFAT metadata patches (deleted dir entries / freed
                // chains), applied over the same volume-relative window.
                let dst_end = vol_off + dst.len() as u64;
                for (poff, bytes) in &r.patches {
                    let p_start = *poff;
                    let p_end2 = poff + bytes.len() as u64;
                    let ov_start = vol_off.max(p_start);
                    let ov_end = dst_end.min(p_end2);
                    if ov_start < ov_end {
                        let d0 = (ov_start - vol_off) as usize;
                        let d1 = (ov_end - vol_off) as usize;
                        let b0 = (ov_start - p_start) as usize;
                        let b1 = b0 + (d1 - d0);
                        dst[d0..d1].copy_from_slice(&bytes[b0..b1]);
                    }
                }
            }
        }
        Ok(buf)
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn total_blocks(&self) -> u64 {
        self.image_size.div_ceil(self.block_size as u64)
    }
}

/// Whole-disk used-map: a disk-image block is "used" if it overlaps a used
/// block of any partition (straddling blocks count once).
struct DiskUsedMap {
    used: Vec<bool>,
    count: u64,
}

impl DiskUsedMap {
    fn build(prepared: &[PreparedPartition], bs: u64, total_blocks: u64) -> Self {
        let mut used = vec![false; total_blocks as usize];
        for prep in prepared {
            let part_end = prep.part_offset + prep.volume_len;
            let vblocks = prep.volume_len.div_ceil(bs);
            for vb in 0..vblocks {
                if prep.used.is_block_used(vb) {
                    let d0 = prep.part_offset + vb * bs;
                    let d1 = part_end.min(d0 + bs); // this volume block's disk byte span
                    for db in d0 / bs..=(d1 - 1) / bs {
                        if let Some(slot) = used.get_mut(db as usize) {
                            *slot = true;
                        }
                    }
                }
            }
        }
        let count = used.iter().filter(|&&u| u).count() as u64;
        Self { used, count }
    }
}

impl UsedSectorMap for DiskUsedMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.used.get(block_number as usize).copied().unwrap_or(false)
    }
    fn used_block_count(&self) -> u64 {
        self.count
    }
}

/// Back up a whole disk (several partitions of one physical drive) into ONE
/// bootable image with the real GPT + an embedded restore manifest. Used
/// sectors only (never a raw disk dump); each partition's data lands at its
/// real offset, so mounting the image shows every volume with a drive letter.
///
/// Scope: full + incremental/differential chains (plan D5); NTFS/FAT/ext/btrfs/
/// xfs + LUKS. File exclusions (plan D2) drop + zero excluded clusters per
/// partition, and patch FAT/exFAT metadata; NTFS/ReFS catalog true-removal
/// inside the whole-disk image (mount-&-delete across volumes) is not yet done —
/// excluded NTFS files remain as zero-filled ghosts (data still never leaves).
pub fn backup_disk(
    req: &DiskBackupRequest,
    progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    check_encrypted_target(req.encryption.as_ref(), req.image_format, &req.image_path)?;
    if req.image_path.exists() {
        return Err(OwnBackupError::Config(format!(
            "output image already exists: {}",
            req.image_path.display()
        )));
    }
    if let Some(parent) = req.image_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::dest_folder::create(parent)?;
        }
    }
    crate::privileges_win::enable_backup_privileges();
    let result = backup_disk_inner(req, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(&req.image_path);
    }
    result
}

fn backup_disk_inner(
    req: &DiskBackupRequest,
    mut progress: impl FnMut(&BackupProgress) -> Result<()>,
) -> Result<DiskBackupSummary> {
    let started = Instant::now();
    let bs = req.block_size as u64;

    // 1. Real GPT, filtered to the selected partitions (variant B keeps MSR
    // when the copy carries Windows — `DiskLayout::copy_carries_windows`).
    let mut layout = crate::gpt_reader::read_disk_layout(req.disk_number)?;
    let kept: std::collections::HashSet<u32> =
        req.partitions.iter().map(|p| p.part_index).collect();
    // What this copy is about to leave behind, named before it is dropped.
    // `retain_for_backup` removes an unselected partition from the layout
    // ENTIRELY — on restore its space comes back free — and that is too large a
    // decision to record as a count. On 2026-08-19 a bootable copy of a 954 GiB
    // disk ran with the EFI partition alone, because a locked BitLocker volume
    // cannot report `has_os` and the wizard therefore never selected it; the
    // whole trace in the log was `parts=1` beside a disk that has three, and the
    // run reported success in 0 s. The selection bug is fixed in the wizard —
    // this line is so the next one of its kind is legible from the log alone.
    let left_out: Vec<String> = layout
        .partitions
        .iter()
        .filter(|p| {
            !kept.contains(&p.index)
                && !p.type_guid.eq_ignore_ascii_case(own_backup_common::gpt::GPT_TYPE_MSR)
        })
        .map(|p| {
            let mib = p.size_bytes(layout.sector_size) / (1024 * 1024);
            if p.name.is_empty() {
                format!("#{} ({mib} МиБ)", p.index)
            } else {
                format!("#{} «{}» ({mib} МиБ)", p.index, p.name)
            }
        })
        .collect();
    if !left_out.is_empty() {
        warn!(
            disk = req.disk_number,
            kept = req.partitions.len(),
            left_out = %left_out.join(", "),
            "whole-disk backup: partitions are NOT in the copy — their space is free on restore"
        );
    }
    layout.retain_for_backup(&kept, req.source_disk.as_ref());
    let sector = layout.sector_size as u64;
    if bs == 0 || bs % sector != 0 {
        return Err(OwnBackupError::Config(format!(
            "block size {} is not a multiple of the sector size {sector}",
            req.block_size
        )));
    }

    // 2. Snapshot + map every partition up front (cross-volume consistency).
    info!(disk = req.disk_number, parts = req.partitions.len(), "whole-disk backup: preparing partitions");
    let mut prepared: Vec<PreparedPartition> = Vec::new();
    let mut excluded_files = 0u64;
    let mut excluded_bytes = 0u64;
    let mut categories: Option<crate::exclusions::CategorySizes> = None;
    // Partitions whose files nobody walked, each with its reason (see
    // `PreparedMap::skipped`).
    let mut scan_skips: Vec<String> = Vec::new();
    for p in &req.partitions {
        let entry = layout
            .partitions
            .iter()
            .find(|e| e.index == p.part_index)
            .ok_or_else(|| {
                OwnBackupError::Config(format!("partition {} not found in the disk GPT", p.part_index))
            })?;
        // Partitions sit at their REAL disk offset (sector-aligned, not
        // necessarily block-aligned — a BOOT partition may start at LBA 34). The
        // disk-composite reader below copies whole-disk blocks, so any offset is
        // fine; the checksum block grid is the image itself.
        let part_offset = entry.first_lba * sector;

        let source = normalize_device_path(&p.device);
        // VSS for a live lettered volume; else read the source directly.
        let mut vss: Option<VssSnapshotSession> = None;
        let read_path = match volume_root_of(&source).filter(|_| snapshot_wanted(true)) {
            Some(root) => match VssSnapshotSession::create(&root) {
                Ok(session) => {
                    let path = session.device_path().to_string();
                    vss = Some(session);
                    path
                }
                Err(e) => {
                    warn!(error = %e, device = %source, "VSS failed — reading the live volume");
                    source.clone()
                }
            },
            None => source.clone(),
        };

        // What sort of container this partition is, asked of its first sectors
        // and of nobody's memory. Read whether or not a passphrase came with
        // it: a container we cannot open is still copied — as ciphertext — and
        // the restore screen has to be able to tell that apart from a container
        // we opened and captured in the clear.
        let mut probe = WinRawDevice::open_read(&read_path)?;
        let mut header = vec![0u8; 4096];
        // Заблокированный BitLocker-том отвечает отказом на ЛЮБОЕ чтение через
        // своё устройство (0x80310000) — включая это. Раньше здесь стоял `?`, и
        // копия целого диска падала на первом же таком разделе: до 2026-08-19
        // мастер их просто не выбирал, и ветка ни разу не встречалась вживую.
        // Отказ — не ошибка, а ответ: том зашифрован и заперт, читать его надо
        // через окно на диск, как это давно делает копия одного тома.
        let mut locked = match probe.read_at(0, &mut header) {
            Ok(()) => false,
            Err(e) if is_bitlocker_locked(&e) => {
                info!(part = p.part_index, "partition is a locked BitLocker volume");
                true
            }
            Err(e) => return Err(e),
        };
        let container = if locked {
            Some(own_backup_common::image::VolumeEncryptionKind::BitLocker)
        } else {
            container_kind(&header)
        };
        // LUKS unlock (decrypted data area is captured).
        let luks = match &p.passphrase {
            Some(pass) if !locked && crate::luks::is_luks(&header) => {
                Some(crate::luks::open_luks(&mut probe, pass.as_bytes())?)
            }
            _ => None,
        };
        // BitLocker: ключ и окно на диск, за которым лежит шифротекст. Пароля
        // нет — раздел всё равно копируется, но как есть, шифротекстом.
        // Запрет записи на диск, если раздел придётся открывать силами Windows.
        // Живёт до конца копирования: том остаётся смонтированным, и Windows
        // пишет в него, когда ей вздумается.
        let mut part_ro_guard: Option<crate::windows_unlock_win::ReadOnlyDisk> = None;
        let bitlocker = match &p.passphrase {
            Some(pass) if locked || crate::bitlocker::is_bitlocker(&header) => {
                match open_bitlocker_source(&read_path, pass) {
                    Ok(src) => src,
                    // Свой разбор FVE знает только AES-XTS, а размер сектора
                    // берёт из BPB — которого у ReFS нет вовсе («implausible
                    // sector size 0», замер 2026-08-20). И то и другое не повод
                    // ронять задание: то же самое умеет сама Windows, и после
                    // неё раздел становится обычным смонтированным.
                    Err(e) => {
                        warn!(part = p.part_index, error = %e,
                              "свой разбор BitLocker не справился — прошу открыть Windows");
                        match unlock_source_with_windows(&read_path, pass) {
                            Ok(guard) => {
                                part_ro_guard = Some(guard);
                                locked = false; // том теперь читается как все
                                // Перечитать заголовок: под ним уже открытая ФС,
                                // и по ней дальше определяется всё остальное.
                                drop(probe);
                                probe = WinRawDevice::open_read(&read_path)?;
                                let _ = probe.read_at(0, &mut header);
                                None
                            }
                            Err(e2) => {
                                warn!(part = p.part_index, error = %e2,
                                      "Windows тоже не открыла раздел");
                                return Err(e); // наружу — первая ошибка, она по делу
                            }
                        }
                    }
                }
            }
            _ => None,
        };
        // Экстент тома нужен и без ключа: у запертого тома нельзя спросить даже
        // длину через его устройство.
        let locked_extent = if locked && bitlocker.is_none() {
            let extents = crate::volume_win::volume_extents(&read_path);
            match extents.as_slice() {
                [e] => Some((format!(r"\\.\PhysicalDrive{}", e.disk_number), e.offset, e.length)),
                _ => {
                    return Err(OwnBackupError::Device(
                        tr!(
                            "engine_win.locked_multi_extent",
                            volume = read_path,
                            count = extents.len()
                        )
                        .into_owned(),
                    ))
                }
            }
        } else {
            None
        };
        let device_len = match (&bitlocker, &locked_extent) {
            (Some(b), _) => b.length,
            (None, Some((_, _, len))) => *len,
            _ => probe.length(),
        };
        drop(probe);
        // The container's own header, kept verbatim so a restore can rebuild
        // it byte-for-byte (D19). A failure here costs the ability to restore
        // this partition encrypted, not the backup itself, so it only warns.
        let luks_header = luks.as_ref().and_then(|l| {
            match WinRawDevice::open_read(&read_path)
                .and_then(|mut d| crate::luks::read_header_area(&mut d, l))
            {
                Ok(bytes) => {
                    use base64::Engine;
                    info!(part = p.part_index, bytes = bytes.len(), "LUKS header captured");
                    Some(own_backup_common::image::LuksHeaderBackup {
                        part_index: p.part_index,
                        data_offset: l.data_offset,
                        sector_size: l.sector_size,
                        kind: l.kind.clone(),
                        header_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    })
                }
                Err(e) => {
                    warn!(part = p.part_index, error = %e,
                          "could not read the LUKS header — this partition will only restore decrypted");
                    None
                }
            }
        });
        let volume_len = match &luks {
            Some(l) => device_len.saturating_sub(l.data_offset),
            None => device_len,
        };
        // The volume must fit the GPT slot it fills.
        let slot_len = (entry.last_lba + 1 - entry.first_lba) * sector;
        if volume_len > slot_len {
            return Err(OwnBackupError::Config(format!(
                "partition {} volume ({volume_len} B) exceeds its GPT slot ({slot_len} B)",
                p.part_index
            )));
        }

        let prepare_tick = |entries: u64, total: u64| {
            progress(&BackupProgress {
                backup_id: "prepare".into(),
                status: BackupStatus::Running,
                blocks_done: entries,
                blocks_total: total,
                bytes_written: 0,
                speed_bps: 0,
                categories: None,
                excluded: None,
            })
        };
        let pm = match &bitlocker {
            // Запертый том, ключ у нас есть: карта и исключения снимаются с
            // расшифрованного окна, ровно как в копии одного тома.
            Some(b) => prepare_encrypted_partition_map(
                Box::new(crate::bitlocker::BitLockerVolume::new(
                    Box::new(b.window()?),
                    b.params.clone(),
                )),
                req.block_size,
                &req.exclusion_patterns_windows,
                // Своя обёртка: скану есть что сказать про общее число записей,
                // и полоса «N из M» честнее бегущего счётчика.
                |done, total| {
                    progress(&BackupProgress {
                        backup_id: "prepare".into(),
                        status: BackupStatus::Running,
                        blocks_done: done,
                        blocks_total: total,
                        bytes_written: 0,
                        speed_bps: 0,
                categories: None,
                excluded: None,
                    })
                },
            ),
            None if locked => PreparedMap {
                map: None, // ключа нет — копируем шифротекст целиком
                // Уходит в манифест образа (`DiskImageRef::filesystem`) — это
                // данные копии, не текст окна: не переводится.
                filesystem: "BitLocker (заперт)".into(),
                zero_ranges: ByteRanges::new(),
                patches: Vec::new(),
                removal_paths: Vec::new(),
                excluded_files: 0,
                excluded_bytes: 0,
                categories: None,
                skipped: Some(tr!("engine_win.skip_bitlocker_no_password").into_owned()),
            },
            None => prepare_partition_map(
                &read_path,
                req.block_size,
                &luks,
                &req.exclusion_patterns_windows,
                &req.exclusion_patterns_linux,
                // Снимок этого раздела взят, или мы на носителе и система
                // офлайн — только тогда сырые метаданные не движутся.
                vss.is_some() || own_backup_common::media_mode::is_media_mode(),
                prepare_tick,
            ),
        };
        let used = pm.map.unwrap_or_else(|| Box::new(AllUsedMap::new(volume_len.div_ceil(bs))));
        excluded_files += pm.excluded_files;
        excluded_bytes += pm.excluded_bytes;
        if let Some(c) = &pm.categories {
            categories.get_or_insert_with(Default::default).merge(c);
        }
        if let Some(why) = &pm.skipped {
            scan_skips.push(
                tr!(
                    "engine_win.skip_partition_line",
                    index = p.part_index,
                    fs = pm.filesystem,
                    why = why
                )
                .into_owned(),
            );
        }
        info!(
            part = p.part_index,
            fs = %pm.filesystem,
            used_blocks = used.used_block_count(),
            excluded_files = pm.excluded_files,
            offset = part_offset,
            "whole-disk backup: partition prepared"
        );
        prepared.push(PreparedPartition {
            part_index: p.part_index,
            device: source,
            read_path,
            luks,
            bitlocker,
            locked_window: locked_extent,
            ro_guard: part_ro_guard,
            volume_len,
            part_offset,
            filesystem: pm.filesystem,
            luks_header,
            zero_ranges: pm.zero_ranges,
            patches: pm.patches,
            removal_paths: pm.removal_paths,
            container,
            used,
            vss,
        });
    }

    // Разделы подготовлены — значит сводка по типам файлов посчитана по всем
    // сразу. Отправляем её одним сообщением, до первого скопированного блока.
    report_categories(&mut progress, categories.as_ref(), excluded_files, excluded_bytes);

    // 3a. Parent chain (plan D5). A non-full kind writes a differencing CHILD
    // over the previous whole-disk image; a broken/incompatible parent falls
    // back to a full backup (reported in the summary) rather than failing.
    let mut kind = req.kind;
    let mut parent: Option<(PathBuf, Sidecar)> = None;
    if kind != BackupType::Full {
        match &req.parent_image {
            None => {
                warn!("no parent image supplied for a {kind} whole-disk backup — creating a full backup");
                kind = BackupType::Full;
            }
            Some(pp) => match validate_disk_parent(pp, req.block_size, req.image_format, req.encryption.as_ref()) {
                Ok(sc) => parent = Some((pp.clone(), sc)),
                Err(e) => {
                    warn!(parent = %pp.display(), error = %e,
                        "whole-disk parent chain unusable — creating a full backup instead");
                    kind = BackupType::Full;
                }
            },
        }
    }

    // 3b. Size the image from the used-block total, plan the meta partition.
    // Build the whole-disk used-map FIRST (over the partitions region) so the
    // meta partition is sized from the actual disk-block count: a partition not
    // aligned to the block size straddles two disk-blocks per volume block, so
    // the count can exceed the sum of per-volume used counts.
    let part_end = prepared.iter().map(|p| p.part_offset + p.volume_len).max().unwrap_or(0);
    let part_end_blocks = part_end.div_ceil(bs);
    let disk_used = DiskUsedMap::build(&prepared, bs, part_end_blocks);
    // A child inherits the parent's virtual size and GPT (the differencing image
    // resolves them through the chain), so its geometry MUST match the parent
    // exactly. Size the meta partition from the geometry the parent committed to
    // (its full-state used count) — not this run's count, which for an
    // incremental differs. If the resulting plan no longer matches the parent's
    // size (the disk was repartitioned), fall back to a full backup.
    // Stored container headers (D19) are megabytes each, so the reserve has to
    // include them. A child takes the PARENT's reserve, for the same reason it
    // takes the parent's block count: its geometry must match exactly.
    // Read now, not at the end: the size has to be known before the metadata
    // partition is sized, and reading NVRAM is instant and independent of
    // everything the copy does.
    let boot_entries = boot_entries_for_image(req.bootable);
    let own_reserve = crate::sidecar::luks_reserve_for(
        prepared.iter().filter_map(|p| p.luks_header.as_ref().map(|h| h.data_offset)),
    ) + crate::sidecar::boot_entries_reserve(boot_entries.as_ref());
    let full_used = |used: u64, reserve: u64| {
        crate::sidecar::calculate_meta_partition_size(used.max(1), reserve)
    };
    // What a parent ACTUALLY reserved, measured the way `own_reserve` is: its
    // container headers AND its boot entries.
    //
    // Counting only the first made every bootable chain start over from a full
    // copy. `own_reserve` includes the boot-entries room — around 18 KiB for a
    // machine with four UEFI entries — so on a disk without LUKS the test below
    // read "18730 > 0" every single time, and the run silently wrote a full
    // backup into a file named `_inc_`. Nothing said so except the warning,
    // which named two numbers that were never comparable.
    let reserved_by = |sc: &Sidecar| {
        crate::sidecar::luks_headers_reserve(&sc.info.luks_headers)
            + crate::sidecar::boot_entries_reserve(sc.info.boot_entries.as_ref())
    };
    let mut meta_size = match &parent {
        Some((_, sc)) => full_used(sc.manifest.used_blocks, reserved_by(sc)),
        None => full_used(disk_used.count, own_reserve),
    };
    // Does this run's metadata fit the room the parent laid down? The child
    // cannot grow the image — its geometry is the parent's — so if it does not,
    // the only honest move is to start a new full backup rather than run for an
    // hour and fail at the metadata write.
    //
    // The comparison is between the two SIZES, not between the reserves they
    // are computed from, and that is the whole point. The reserve carries
    // meaningless jitter: it is measured from the serialized boot entries,
    // whose JSON holds a timestamp whose textual length wobbles by a byte or
    // three from run to run. Comparing reserves therefore refused a perfectly
    // good parent over `18730 > 18727` — three bytes, inside a slack of 16
    // kilobytes — and every copy of the chain came out full. The size rounds to
    // whole megabytes, where three bytes cannot be seen.
    //
    // Sizes also ask the wider question the reserve alone could not: the child's
    // checksum table grows with the volume, and a source that has gained enough
    // used blocks genuinely will not fit either.
    if let Some((pp, sc)) = &parent {
        let have = full_used(sc.manifest.used_blocks, reserved_by(sc));
        let want = full_used(disk_used.count, own_reserve);
        if want > have {
            warn!(
                parent = %pp.display(),
                parent_meta = have,
                needed_meta = want,
                parent_reserve = reserved_by(sc),
                own_reserve,
                parent_blocks = sc.manifest.used_blocks,
                own_blocks = disk_used.count,
                "this copy's metadata does not fit the room the parent reserved \
                 — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
            meta_size = want;
        }
    }
    // An encrypted image has no guest meta partition: its sidecar is sealed
    // inside the container (§7.3). `meta_size` stays the measure of how much
    // room that sidecar needs — it becomes the container's reservation instead
    // of a partition's size — but the image itself is planned without one.
    let encrypted = req.encryption.is_some();
    let planned_meta = |bytes: u64| if encrypted { 0 } else { bytes };
    let mut plan = own_backup_common::gpt::plan_whole_disk_image(&layout, planned_meta(meta_size));
    if let Some((pp, sc)) = &parent {
        if plan.image_size != sc.manifest.source_disk.size_bytes {
            warn!(
                parent = %pp.display(),
                planned = plan.image_size,
                parent_size = sc.manifest.source_disk.size_bytes,
                "whole-disk parent geometry changed — creating a full backup instead"
            );
            kind = BackupType::Full;
            parent = None;
            meta_size = full_used(disk_used.count, own_reserve);
            plan = own_backup_common::gpt::plan_whole_disk_image(&layout, planned_meta(meta_size));
        }
    }
    let meta_offset = plan.meta_first_lba * sector;
    let meta_guid = (!encrypted).then(Uuid::new_v4);

    // 4. Create the image and (for a full) write its GPT. The image GPT gets
    //    FRESH random disk + partition GUIDs, NEVER the source disk's — a
    //    byte-clone of the live disk's GUIDs collides with it (Windows offlines
    //    one of the two, and if the destination lives on the source disk the
    //    whole disk, incl. the backup drive, vanishes: the 2026-07-11 E:
    //    incident). The REAL GUIDs are kept in the manifest below; restore
    //    writes them back (bootable).
    let mut safe_layout = layout.clone();
    safe_layout.disk_guid = Uuid::new_v4().to_string().to_uppercase();
    for p in &mut safe_layout.partitions {
        p.part_guid = Uuid::new_v4().to_string().to_uppercase();
    }
    // The GPT write-op grid (offsets + lengths) is deterministic from the plan
    // and partition count — the GUID VALUES only change the bytes. A child
    // reuses the PARENT's exact GPT bytes (read below) so any used block that
    // overlaps the GPT head (a partition starting at LBA 34) matches the parent
    // and is not needlessly rewritten; a full writes its own fresh GPT.
    let mut gpt_ops: Vec<(u64, Vec<u8>)> =
        crate::gpt_writer::whole_disk_gpt_ops(&safe_layout, &plan, meta_guid)?
            .into_iter()
            .map(|op| (op.offset, op.bytes))
            .collect();

    // Identity of this image, needed before it is created: the container seals
    // it into its own root record when encrypted.
    let backup_id = Uuid::new_v4();
    let (mut target, sector_size) = match &parent {
        None => create_disk_image_target(
            &req.image_path,
            plan.image_size,
            req.image_format,
            req.encryption.as_ref(),
            &DiskImagePlan {
                backup_id,
                role: ChainRole::Full,
                parent: None,
                disk_number: req.disk_number,
                zstd_level: crate::qcow2::zstd_level_or_default(req.zstd_level),
            },
        )?,
        Some((pp, sc)) => {
            // Child inherits the parent's GPT through the chain — overwrite the
            // op bytes with the parent's actual GPT so the composite reader
            // reproduces them byte-for-byte. Drop the parent read handle before
            // creating the differencing child (VHDX CreateVirtualDisk opens the
            // parent itself).
            {
                // The chain key comes from the parent itself, which is where
                // the wrapped one travels.
                let parent_key = match &req.encryption {
                    Some(keys) => Some(keys.open_chain_of(pp)?.key),
                    None => None,
                };
                let mut pimg = ImageReadSource::open_with(
                    pp,
                    &container_ext(req.image_format, req.encryption.as_ref()),
                    parent_key.as_ref(),
                )?;
                for (off, bytes) in &mut gpt_ops {
                    pimg.read_at(*off, bytes)?;
                }
            }
            let parent_name = pp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            create_disk_child_target(
                &req.image_path,
                pp,
                plan.image_size,
                req.image_format,
                req.encryption.as_ref(),
                &DiskImagePlan {
                    backup_id,
                    role: match kind {
                        BackupType::Differential => ChainRole::Differential,
                        _ => ChainRole::Incremental,
                    },
                    parent: Some((sc.manifest.backup_id, parent_name)),
                    disk_number: req.disk_number,
                    zstd_level: crate::qcow2::zstd_level_or_default(req.zstd_level),
                },
            )?
        }
    };
    if parent.is_none() {
        // Full backup: lay down the fresh GPT — but ONLY into blocks the copy
        // pass will NOT write. A partition that begins inside the first block
        // (LBA 34, offset 17408) makes block 0 "used"; the composite copy then
        // writes that block with the GPT bytes already baked in, so writing the
        // GPT op there too would hit the same host cluster twice — fatal for the
        // write-once qcow2 writer ("cluster 0 written twice") and merely
        // redundant for VHDX. Unused GPT regions (an aligned primary GPT, the
        // backup GPT at the image tail) still need this direct write.
        for (off, bytes) in &gpt_ops {
            let mut p = 0usize;
            while p < bytes.len() {
                let abs = off + p as u64;
                let block = abs / bs;
                let take = (((block + 1) * bs - abs) as usize).min(bytes.len() - p);
                if !disk_used.is_block_used(block) {
                    target.write_at(abs, &bytes[p..p + take])?;
                }
                p += take;
            }
        }
    }

    // 5. Copy the disk as whole-image blocks (block N = image offset N*bs). Each
    //    block is assembled from the GPT and the partitions at their real
    //    offsets, so partitions need no block alignment and the checksum grid is
    //    the image itself (block_number * bs = disk offset — verifiable).
    //    `disk_used` (built above) returns false past the partitions region, so
    //    the meta/tail blocks (written separately) are not copied.
    let mut regions: Vec<DiskRegion> = Vec::new();
    for prep in &prepared {
        let reader: Box<dyn crate::fat_map::RawVolume + Send> = match (&prep.bitlocker, &prep.locked_window) {
            // Расшифрованное окно на диск: том заперт, его устройство молчит.
            (Some(b), _) => Box::new(crate::bitlocker::BitLockerVolume::new(
                Box::new(b.window()?),
                b.params.clone(),
            )),
            // Заперт и без ключа — тот же путь, но без расшифровки.
            (None, Some((disk, off, len))) => Box::new(disk_window(disk, *off, *len)?),
            (None, None) => {
                let dev = WinRawDevice::open_read(&prep.read_path)?;
                match &prep.luks {
                    Some(l) => Box::new(crate::decrypt::DecryptingVolume::new(Box::new(dev), l.clone())),
                    None => Box::new(dev),
                }
            }
        };
        regions.push(DiskRegion {
            part_offset: prep.part_offset,
            volume_len: prep.volume_len,
            reader,
            zero_ranges: prep.zero_ranges.clone(),
            patches: prep.patches.clone(),
        });
    }
    let mut composite = DiskCompositeReader {
        block_size: req.block_size,
        image_size: plan.image_size,
        gpt_ops,
        regions,
    };
    // For a child only blocks that differ from the parent version are written;
    // the rest resolve through the differencing chain. `outcome.checksums` still
    // covers EVERY used block, so the child's sidecar describes the full image.
    let parent_hashes: Option<HashMap<u64, u64>> = parent.as_ref().map(|(_, sc)| {
        sc.checksums.iter().map(|c| (c.block_number, c.checksum)).collect()
    });
    let outcome = copy_used_blocks(
        &format!("disk{}", req.disk_number),
        &mut composite,
        &disk_used,
        parent_hashes.as_ref(),
        |block, data| target.write_at(block * bs, data),
        &mut progress,
    )?;
    let mut checksums = outcome.checksums;
    let bytes_copied = outcome.bytes_copied;
    let blocks_copied = outcome.blocks_copied;
    let blocks_unchanged = outcome.blocks_unchanged;

    // 6. Reserve the meta partition region while the image is open so the VHDX
    //    BAT physically allocates EVERY block of it (an all-zero write is
    //    optimised away → the block stays ZERO/unallocated and the later
    //    guest-write of the real blob fails with "block state 2"). A non-zero
    //    fill across the whole region forces allocation; the guest-write below
    //    then overwrites it with the actual sidecar. Then finalize + release.
    //    An encrypted image has no such partition: the sidecar goes inside the
    //    container at finalize, with `meta_size` bytes of room reserved so the
    //    final one — the exclusion figures and the re-checksummed blocks are
    //    known only after the pass below — can replace it in place.
    if !encrypted {
        let placeholder = vec![0x01u8; meta_size as usize];
        target.write_at(meta_offset, &placeholder)?;
    }
    let _ = progress(&BackupProgress {
        backup_id: "finalize".into(),
        status: BackupStatus::Running,
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: bytes_copied,
        speed_bps: 0,
                categories: None,
                excluded: None,
    });
    // A provisional sidecar, so the container has something of the right shape
    // to reserve around. Nothing reads it: the run either replaces it below or
    // fails, and a failed run deletes its image.
    let provisional = if encrypted {
        let mut blob = Vec::new();
        let info = SidecarInfo::new_full("provisional", 0);
        crate::sidecar::write_sidecar_v2_to(
            &mut blob,
            &info,
            &whole_disk_image_manifest(
                &layout,
                req,
                sector_size,
                plan.image_size,
                plan.image_size.div_ceil(bs),
                0,
                kind,
                None,
            ),
            &[],
        )?;
        blob
    } else {
        Vec::new()
    };
    target.finalize(&provisional, meta_size as usize)?;
    for prep in &mut prepared {
        if let Some(session) = prep.vss.take() {
            if let Err(e) = session.complete() {
                warn!(error = %e, "VSS release failed (data is already safe)");
            }
        }
    }

    // 6b. True removal of excluded NTFS/ReFS files inside the finished image
    //     (plan D2, both formats, full AND incremental). Mount the whole-disk
    //     image once — an incremental is a differencing child, so the temp child
    //     the removal pass creates over it resolves the WHOLE chain and every
    //     partition's files are visible — let the FS driver delete each
    //     partition's recorded paths on its own volume, then merge into THIS
    //     image's leaf + re-checksum the touched blocks. So excluded files vanish
    //     from the catalog instead of remaining zero-filled ghosts. Same graceful
    //     contract as the per-volume path: a pre-merge failure keeps the ghosts
    //     (valid backup), only a post-merge failure fails the run — cannot break
    //     a working backup. The content is already zeroed either way, so no
    //     excluded data leaks even when this pass is skipped.
    let removal: Vec<(u64, Vec<crate::exclusions_win::ExcludedPath>)> = prepared
        .iter_mut()
        .filter(|p| !p.removal_paths.is_empty())
        .map(|p| (p.part_offset, std::mem::take(&mut p.removal_paths)))
        .collect();
    if !removal.is_empty() {
        // Каждый проход удаления называет свой итог сам: удаление — пути,
        // зануление — куски, пере-хеширование — блоки. Ноль значит «итог
        // неизвестен», и полоса тогда остаётся бегущей, как раньше.
        let mut tick = |entries: u64, total: u64| {
            let _ = progress(&BackupProgress {
                backup_id: "exclusions".into(),
                status: BackupStatus::Running,
                blocks_done: entries,
                blocks_total: total,
                bytes_written: bytes_copied,
                speed_bps: 0,
                categories: None,
                excluded: None,
            });
        };
        match (&req.encryption, req.image_format) {
            // Encrypted: the same mount-and-delete, over a sealed scratch, with
            // the container rebuilt around whatever the driver changed. A
            // plaintext scratch beside an encrypted copy would hold exactly the
            // filesystem metadata naming the files being removed (§7.4).
            (Some(keys), _) => {
                let chain = keys.open_chain_of(&req.image_path)?;
                crate::obk_removal_win::remove_excluded_from_obk_disk_image(
                    &req.image_path,
                    &chain.key,
                    &removal,
                    plan.image_size,
                    req.block_size,
                    crate::qcow2::zstd_level_or_default(req.zstd_level),
                    &mut checksums,
                    &mut tick,
                )?;
            }
            (None, ImageFormat::Vhdx) => {
                crate::true_removal_win::remove_excluded_from_disk_image(
                    &req.image_path,
                    &removal,
                    plan.image_size,
                    req.block_size,
                    &mut checksums,
                    &mut tick,
                )?;
            }
            (None, ImageFormat::Qcow2) => {
                crate::qcow2_removal_win::remove_excluded_from_disk_image_qcow2(
                    &req.image_path,
                    &removal,
                    plan.image_size,
                    req.block_size,
                    Some(crate::qcow2::zstd_level_or_default(req.zstd_level)),
                    &mut checksums,
                    &mut tick,
                )?;
            }
        }
    }

    // 7. Build the embedded manifest + sidecar and write it into the meta region.
    let image_file_name = req
        .image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let images: Vec<own_backup_common::gpt::DiskImageRef> = prepared
        .iter()
        .map(|p| own_backup_common::gpt::DiskImageRef {
            part_index: p.part_index,
            token: disk_source_token(&p.device),
            image_file_name: image_file_name.clone(),
            filesystem: p.filesystem.clone(),
            // Opened → the image holds the volume in the clear. Not opened →
            // the container's own bytes went in untouched, and the restore
            // brings it back encrypted, password and all.
            encryption: p.container.clone().map(|kind| {
                use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionState};
                let opened = p.luks.is_some();
                VolumeEncryptionState {
                    kind,
                    capture_mode: if opened {
                        VolumeCaptureMode::Plaintext
                    } else {
                        VolumeCaptureMode::SectorBySector
                    },
                    luks_header_backup_b64: None, // the header itself lives in `info.luks_headers`
                    note: None,
                }
            }),
        })
        .collect();
    let disk_manifest = own_backup_common::gpt::DiskBackupManifest {
        format_version: own_backup_common::gpt::DiskBackupManifest::FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        job_id: req.job_id.clone(),
        disk_number: req.disk_number,
        disk_model: req.disk_model.clone(),
        layout: layout.clone(),
        images,
        source_disk: req.source_disk.clone(),
    };

    let total_blocks = plan.image_size.div_ceil(bs);
    // `used_blocks` is the FULL image state (every checksummed block), not just
    // this run's writes — the next child sizes its meta partition from it, so it
    // must stay stable across the chain (checksums.len() == disk_used.count).
    let mut manifest = whole_disk_image_manifest(
        &layout,
        req,
        sector_size,
        plan.image_size,
        total_blocks,
        checksums.len() as u64,
        kind,
        parent.as_ref().map(|(_, sc)| sc.manifest.backup_id),
    );
    // The container sealed this identity into its own root record before a
    // single cluster was written; the sidecar has to repeat it, or the two
    // halves of the image would name different backups.
    manifest.backup_id = backup_id;
    // A whole-disk image is disk-block absolute → volume_data_offset = 0.
    let mut info = match &parent {
        None => SidecarInfo::new_full(image_file_name, 0),
        Some((pp, _)) => {
            let role = match kind {
                BackupType::Differential => ChainRole::Differential,
                _ => ChainRole::Incremental,
            };
            let parent_name = pp
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            SidecarInfo::new_child(image_file_name, role, parent_name, 0)
        }
    };
    info.container = container_ext(req.image_format, req.encryption.as_ref());
    info.job_id = req.job_id.clone();
    info.source_disk_layout = req.source_disk.clone();
    info.categories = categories;
    info.excluded_files = excluded_files;
    info.excluded_bytes = excluded_bytes;
    // One captured container header per encrypted partition (D19): with them a
    // restore rebuilds the container byte-for-byte, so the restored system's
    // crypttab and initramfs keep working untouched.
    info.luks_headers = prepared.iter().filter_map(|p| p.luks_header.clone()).collect();
    // The machine's boot entries — INSIDE the image, with everything else that
    // describes it. They used to be written to a `.boot.json` beside it: the
    // last piece of a backup that was not in the backup, and the one a user
    // would carry to another disk without noticing it had stayed behind.
    //
    // On 2026-09-18 this machine lost the entries for Acronis and R-Drive Image
    // while their loaders sat untouched on the ESP; no copy taken before that
    // would have brought them back. That is what they are for.
    info.boot_entries = boot_entries;

    let mut blob = Vec::new();
    crate::sidecar::write_sidecar_v2_full_to(&mut blob, &info, &manifest, &checksums, Some(&disk_manifest))?;
    // The metadata partition was sized before any of this existed as bytes. If
    // the blob will not fit, the boot entries are the one part of it that can
    // be given up: they are the machine's, identical from run to run, and the
    // chain's other copies have them. Everything else in here describes THIS
    // image and has nowhere else to live.
    if blob.len() as u64 > meta_size && info.boot_entries.is_some() {
        warn!(
            blob = blob.len(),
            meta_size,
            "no room for the boot entries in this image's metadata — writing without them"
        );
        info.boot_entries = None;
        blob.clear();
        crate::sidecar::write_sidecar_v2_full_to(&mut blob, &info, &manifest, &checksums, Some(&disk_manifest))?;
    }
    match (&req.encryption, req.image_format) {
        (Some(keys), _) => {
            // No guest meta partition to write into: the sidecar replaces the
            // reserved one inside the container, sealed under the same chain
            // key and with a fresh nonce.
            let chain = keys.open_chain_of(&req.image_path)?;
            crate::seal::obk::update_sealed_metadata(&req.image_path, &chain.key, &blob)?;
        }
        (None, ImageFormat::Vhdx) => {
            crate::vhdx_bat::write_vhdx_guest_at(&req.image_path, meta_offset, &blob)?
        }
        (None, ImageFormat::Qcow2) => {
            crate::qcow2::merge::write_qcow2_guest_blob(&req.image_path, meta_offset, &blob)?
        }
    }

    info!(
        image = %req.image_path.display(),
        disk = req.disk_number,
        kind = %kind,
        partitions = prepared.len(),
        blocks_written = blocks_copied,
        blocks_unchanged,
        bytes = bytes_copied,
        secs = started.elapsed().as_secs(),
        "Whole-disk backup complete"
    );
    Ok(DiskBackupSummary {
        image_path: req.image_path.clone(),
        bytes_copied,
        blocks_copied,
        partitions: prepared.len() as u32,
        duration: started.elapsed(),
        backup_kind: kind,
        blocks_unchanged,
        excluded_files,
        excluded_bytes,
        categories,
        file_scan_skipped: if scan_skips.is_empty() {
            None
        } else {
            Some(scan_skips.join("; "))
        },
    })
}

/// Validate that `parent_image` can serve as the parent of a new whole-disk
/// child: it exists, is itself a whole-disk image (carries an embedded disk
/// manifest), uses the same block size and container format. The image-size
/// (geometry) match is checked by the caller once the child's plan is known.
/// Returns the parent's full sidecar (its checksums seed the incremental skip).
fn validate_disk_parent(
    parent_image: &Path,
    block_size: u32,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    if !parent_image.exists() {
        return Err(OwnBackupError::Config(format!(
            "parent whole-disk image not found: {}",
            parent_image.display()
        )));
    }
    // An encrypted parent keeps its sidecar sealed inside the container, so
    // continuing that chain needs the job's key — the same key the new image
    // will be written with.
    let sidecar = match encryption {
        Some(keys) => {
            let chain = keys.open_chain_of(parent_image)?;
            crate::sidecar::read_sealed_sidecar(parent_image, &chain.key)?
        }
        None => {
            let ext = parent_image
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            read_embedded_sidecar(parent_image, &ext)?
        }
    };
    if sidecar.disk_manifest.is_none() {
        return Err(OwnBackupError::Config(format!(
            "parent {} is not a whole-disk image (no embedded disk manifest)",
            parent_image.display()
        )));
    }
    if sidecar.manifest.block_size != block_size {
        return Err(OwnBackupError::Config(format!(
            "parent block size {} != requested {block_size}",
            sidecar.manifest.block_size
        )));
    }
    let want = container_ext(format, encryption);
    if sidecar.info.container != want {
        return Err(OwnBackupError::Config(format!(
            "parent image is '{}' but the requested format is '{want}' — a backup \
             chain cannot change container format",
            sidecar.info.container,
        )));
    }
    Ok(sidecar)
}

/// Create a differencing CHILD target over `parent_path` for an incremental /
/// differential whole-disk backup. The child inherits the parent's virtual size
/// and its GPT (resolved through the chain), so no GPT is written here.
/// `image_size` is the parent's virtual size (used only to size the qcow2 child).
fn create_disk_child_target(
    image_path: &Path,
    parent_path: &Path,
    image_size: u64,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    plan: &DiskImagePlan,
) -> Result<(ImageTarget, u32)> {
    if let Some(keys) = encryption {
        return create_disk_obk_target(image_path, Some(parent_path), image_size, keys, plan);
    }
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            // Reference the parent by bare file name when it sits in the same
            // folder (qemu resolves relative backing paths against the image's
            // directory) — same convention as the per-volume path.
            let backing = if parent_path.parent() == image_path.parent() {
                parent_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        OwnBackupError::Config(format!(
                            "parent image path has no file name: {}",
                            parent_path.display()
                        ))
                    })?
            } else {
                parent_path.to_string_lossy().into_owned()
            };
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: image_size,
                    backing_file: Some(backing),
                    compression: crate::qcow2::Qcow2Compression::Zstd { level: plan.zstd_level },
                    ..Default::default()
                },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            create_differencing_vhdx(image_path, parent_path)?;
            let writer = crate::vhdx_format::VhdxWriter::open_existing(image_path)?;
            Ok((ImageTarget::Vhdx { writer }, sector_size))
        }
    }
}

/// Create an image target of `disk_size` bytes with no parent (full backup).
/// Returns the target and its logical sector size.
/// What distinguishes one whole-disk image of a chain from the next. The
/// per-volume path carries the same thing in [`crate::seal::context::ImagePlan`];
/// this is the disk-shaped half of it, so the two constructors below can build
/// an encrypted target without the volume path's arguments.
struct DiskImagePlan {
    backup_id: Uuid,
    role: ChainRole,
    /// Identity and file name of the parent image, for a child.
    parent: Option<(Uuid, String)>,
    disk_number: u32,
    /// Already resolved: 1–19.
    zstd_level: i32,
}

/// The extension a run's images carry: `.obk` when encrypted, the job's format
/// otherwise. One place, because listings, chains and the format-change guard
/// all have to agree with the writer.
fn container_ext(
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
) -> String {
    match encryption {
        Some(_) => crate::seal::obk::OBK_EXTENSION.into(),
        None => format.extension().into(),
    }
}

/// The encrypted target for a whole-disk image, full or child.
fn create_disk_obk_target(
    image_path: &Path,
    parent_path: Option<&Path>,
    disk_size: u64,
    keys: &crate::seal::context::ArchiveKeys,
    plan: &DiskImagePlan,
) -> Result<(ImageTarget, u32)> {
    let chain = match parent_path {
        // The chain key travels inside the parent image, so continuing a chain
        // needs the parent and this job's key, nothing else.
        Some(parent) => keys.open_chain_of(parent)?,
        None => keys.start_chain()?,
    };
    let image_plan = crate::seal::context::ImagePlan {
        backup_id: plan.backup_id,
        role: plan.role,
        parent: plan.parent.as_ref().map(|(id, name)| (*id, name.as_str())),
        created_at: chrono::Utc::now().to_rfc3339(),
        virtual_size: disk_size,
        // A whole-disk copy is identified by the disk it came from, the way a
        // volume copy is by its volume.
        source_id: Some(format!("disk{}", plan.disk_number)),
        consolidated_increments: 0,
        chain_started_at: None,
        compression: crate::seal::obk::ObkCompression::Zstd { level: plan.zstd_level },
    };
    let writer = Box::new(crate::seal::obk::ObkStagedWriter::create(
        image_path,
        &chain.key,
        keys.writer_options(&chain, image_plan),
    )?);
    Ok((ImageTarget::Obk { writer }, 512))
}

fn create_disk_image_target(
    image_path: &Path,
    disk_size: u64,
    format: ImageFormat,
    encryption: Option<&crate::seal::context::ArchiveKeys>,
    plan: &DiskImagePlan,
) -> Result<(ImageTarget, u32)> {
    if let Some(keys) = encryption {
        return create_disk_obk_target(image_path, None, disk_size, keys, plan);
    }
    let sector_size = 512u32;
    match format {
        ImageFormat::Qcow2 => {
            let writer = crate::qcow2::Qcow2StagedWriter::create(
                image_path,
                crate::qcow2::Qcow2WriterOptions {
                    virtual_size: disk_size,
                    backing_file: None,
                    compression: crate::qcow2::Qcow2Compression::Zstd { level: plan.zstd_level },
                    ..Default::default()
                },
            )?;
            Ok((ImageTarget::Qcow2 { writer }, sector_size))
        }
        ImageFormat::Vhdx => {
            create_dynamic_vhdx(
                image_path,
                &VhdxCreateOptions {
                    max_size: disk_size,
                    logical_sector_size: sector_size,
                    physical_sector_size: std::cmp::max(sector_size, 4096),
                },
            )?;
            let writer = crate::vhdx_format::VhdxWriter::open_existing(image_path)?;
            Ok((ImageTarget::Vhdx { writer }, sector_size))
        }
    }
}

/// Build the embedded [`ImageManifest`] describing the whole-disk image.
#[allow(clippy::too_many_arguments)]
fn whole_disk_image_manifest(
    layout: &own_backup_common::gpt::DiskLayout,
    req: &DiskBackupRequest,
    sector_size: u32,
    image_size: u64,
    total_blocks: u64,
    used_blocks: u64,
    kind: BackupType,
    parent_backup_id: Option<Uuid>,
) -> ImageManifest {
    ImageManifest {
        backup_id: Uuid::new_v4(),
        created_at: chrono::Utc::now().to_rfc3339(),
        backup_type: kind,
        parent_backup_id,
        source_os: "windows".into(),
        source_disk: DiskInfo {
            device_id: format!(r"\\.\PhysicalDrive{}", req.disk_number),
            model: req.disk_model.clone(),
            serial: String::new(),
            size_bytes: image_size,
            sector_size_logical: sector_size,
            sector_size_physical: std::cmp::max(sector_size, 4096),
            media_type: "Disk".into(),
            partition_table: PartitionTable::Gpt,
            disk_guid: layout.disk_guid.clone(),
            partitions: vec![],
        },
        boot_mode: BootMode::Uefi,
        partition_table: PartitionTable::Gpt,
        compression_algo: match req.image_format {
            ImageFormat::Vhdx => CompressionAlgo::None,
            ImageFormat::Qcow2 => CompressionAlgo::Zstd,
        },
        compression_level: match req.image_format {
            ImageFormat::Vhdx => 0,
            ImageFormat::Qcow2 => crate::qcow2::zstd_level_or_default(req.zstd_level),
        },
        encryption_algo: None,
        encryption_salt: None,
        block_size: req.block_size,
        total_blocks,
        used_blocks,
        backup_mode: BackupMode::UsedSectors,
        checksum_algorithm: "xxhash64".into(),
        preflight_report: None,
        volume_encryption: None,
    }
}

/// Parameters for a volume restore.
pub struct VolumeRestoreRequest {
    /// Backup image (`.vhdx`) with its `.obkm` sidecar next to it.
    pub image_path: PathBuf,
    /// Target volume/device to overwrite (`D:`, `\\.\D:`). DESTRUCTIVE.
    pub target: String,
    /// Verify each block's checksum while reading from the image.
    pub verify_checksums: bool,
    /// Keys for an encrypted image: a password's or the recovery key's when a
    /// person restores by hand, the service's own for an automatic check.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
}

/// Outcome of a completed restore.
pub struct VolumeRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub duration: Duration,
}

/// Turn a restore target into the device path that can actually take the whole
/// volume, plus whatever had to be taken down to allow it.
///
/// Returns a `part:<disk>:<offset>:<len>` window when the target is a volume
/// sitting on exactly one partition, and the target unchanged otherwise — a
/// raw device path, a `part:` spec the caller already built, or a volume
/// spanning several extents (a spanned or mirrored set, which is not a
/// partition and must not be guessed at).
///
/// The returned guard holds the dismount. Dropping it early lets Windows mount
/// the volume back on top of a restore still in progress, so it is bound to the
/// write, not to this function.
#[cfg(target_os = "windows")]
fn partition_window_for(target: &str) -> (String, Option<TakenVolume>) {
    let plain = normalize_device_path(target);
    if target.starts_with("part:") {
        return (plain, None);
    }
    let Some(root) = crate::volume_win::volume_root_for_path(target)
        .or_else(|| crate::volume_win::volume_root_for_path(&format!("{target}\\")))
    else {
        return (plain, None);
    };
    let extents = crate::volume_win::volume_extents(&root);
    let [e] = extents.as_slice() else {
        warn!(
            target,
            extents = extents.len(),
            "restore: the target is not one partition — writing through the volume device"
        );
        return (plain, None);
    };
    let guard =
        crate::volume_win::VolumeDismountGuard::for_restore_range(e.disk_number, e.offset, e.length);
    if !guard.failed.is_empty() {
        warn!(volumes = ?guard.failed, "restore: some volumes would not dismount");
    }
    info!(
        target,
        disk = e.disk_number,
        offset = e.offset,
        length = e.length,
        "restore: writing through the partition rather than the volume device"
    );
    let taken = TakenVolume { guard: Some(guard), give_back: drive_letter_of(target).map(|l| (root, l)) };
    (format!("part:{}:{}:{}", e.disk_number, e.offset, e.length), Some(taken))
}

/// `H`, from `H:`, `H:\` or `h:`. Nothing for a device path or a mount folder.
#[cfg(target_os = "windows")]
fn drive_letter_of(target: &str) -> Option<char> {
    let mut chars = target.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// A volume taken down for a restore, and the letter it is owed back.
///
/// The dismount guard strips every letter it takes down — right for a
/// whole-disk restore, whose plan assigns letters afresh afterwards, and wrong
/// here: restoring onto `H:` left a volume with the right data and NO letter.
/// Measured on the stand: `Get-Volume -DriveLetter H` empty straight after, the
/// restored file still absent from `H:\` sixty seconds later, and all thirty
/// files present when the same partition was read from outside. The person
/// would have been looking at a missing drive after a restore that worked.
///
/// On drop, in this order: release the lock, then give the letter back. The
/// order matters — a volume still locked by us cannot be mounted under it. And
/// it happens on drop so a failed restore returns the letter too: the volume
/// was theirs before we touched it.
#[cfg(target_os = "windows")]
struct TakenVolume {
    guard: Option<crate::volume_win::VolumeDismountGuard>,
    give_back: Option<(String, char)>,
}

#[cfg(target_os = "windows")]
impl Drop for TakenVolume {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some((root, letter)) = self.give_back.take() {
            match crate::letters_win::assign_letter(&root, letter) {
                Ok(()) => info!(volume = %root, %letter, "restore: the target has its letter back"),
                Err(e) => warn!(
                    volume = %root, %letter, error = %e,
                    "restore: the target's letter could not be given back — the data is \
                     written; assign it in Disk Management"
                ),
            }
        }
    }
}

/// Restore a volume image onto a target device. Overwrites the target!
pub fn restore_volume(
    req: &VolumeRestoreRequest,
    mut progress: impl FnMut(&RestoreProgress) -> Result<()>,
) -> Result<VolumeRestoreSummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_with_keys(&req.image_path, req.encryption.as_ref())?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    // A volume target is written through its PARTITION, not through the volume
    // device. Measured, not assumed: writing `\\.\H:` at the last block of a
    // 1 GiB volume got `wanted 1048576 wrote 1044480`, with the device itself
    // reporting exactly 1073741824 bytes and the write ending exactly there.
    // Windows caps a raw write through a volume handle four kilobytes short of
    // the length it advertises, so the tail — NTFS's backup boot sector — can
    // never be written that way, and every restore of a full volume ended in an
    // error message about work that had in fact succeeded.
    //
    // The whole-disk road never had this: it writes through the disk at the
    // partition's offset. This takes the same road, and takes the volume down
    // first so Windows stops guarding those sectors on its behalf.
    let (target, _taken) = partition_window_for(&req.target);
    let mut target_dev = WinRawDevice::open_write(&target)?;
    info!(
        target = %target,
        target_bytes = target_dev.length(),
        source_bytes = volume_len,
        block_size,
        "restore: target opened"
    );
    // Nothing may hold the target's sectors while they are overwritten. Windows
    // refuses a raw write into a MOUNTED volume outright — `WriteFile(\\.\H:)
    // ... 0x80070005` at offset 0, before a single block — so a restore onto an
    // ordinary target failed for the ordinary reason that it was mounted. The
    // whole-disk road has always taken its volumes down (`TargetWriter`); this
    // one, the simplest operation there is, never did.
    //
    // After the open and on the SAME handle: the lock belongs to the handle
    // that holds it, and taking it on another one only locks this writer out.
    //
    // A no-op for the partition window above — that handle is the disk, whose
    // volumes the guard already took down — and the fallback for a target that
    // could not be resolved to one.
    target_dev.lock_and_dismount();
    if target_dev.length() < volume_len {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "target {} is {} bytes, smaller than the backed-up volume ({} bytes)",
            target,
            target_dev.length(),
            volume_len
        )));
    }
    if block_size % target_dev.sector_size() as u64 != 0 {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "image block size {block_size} does not match target sector size {}",
            target_dev.sector_size()
        )));
    }

    let chain_key = match (&req.encryption, sidecar.info.container.as_str()) {
        (Some(keys), "obk") => Some(keys.open_chain_of(&req.image_path)?.key),
        _ => None,
    };
    let mut image_dev = ImageReadSource::open_with(
        &req.image_path,
        &sidecar.info.container,
        chain_key.as_ref(),
    )?;

    let restore_id = Uuid::new_v4().to_string();
    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;
    let mut buf = vec![0u8; block_size as usize];

    for entry in &sidecar.checksums {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let data = &mut buf[..len];
        image_dev.read_at(data_offset + offset, data)?;

        if req.verify_checksums {
            let actual = compute_block_checksum(data);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        target_dev.write_at(offset, data)?;
        blocks_restored += 1;
        bytes_restored += len as u64;

        let elapsed = started.elapsed().as_secs_f64();
        progress(&RestoreProgress {
            restore_id: restore_id.clone(),
            status: BackupStatus::Running,
            blocks_done: blocks_restored,
            blocks_total: total,
            bytes_written: bytes_restored,
            speed_bps: if elapsed > 0.0 {
                (bytes_restored as f64 / elapsed) as u64
            } else {
                0
            },
        })?;
    }

    target_dev.flush()?;
    drop(image_dev);

    info!(
        image = %req.image_path.display(),
        target = %target,
        blocks = blocks_restored,
        "Volume restore complete"
    );

    Ok(VolumeRestoreSummary {
        blocks_restored,
        bytes_restored,
        duration: started.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// Whole-disk (bootable) restore
// ---------------------------------------------------------------------------

/// Restore a whole disk from a `.obdm` manifest onto a target physical drive.
/// DESTRUCTIVE — the target is repartitioned and every partition overwritten.
pub struct DiskRestoreRequest {
    /// Path to the `.obdm` disk-backup manifest (its images live alongside).
    pub manifest_path: PathBuf,
    /// Target physical drive number (`\\.\PhysicalDrive{target_disk}`).
    pub target_disk: u32,
    pub verify_checksums: bool,
    /// Keys for an encrypted image. Opened from a password or a recovery key
    /// by whoever asked for the restore — never from the service's own store,
    /// which exists so scheduled COPIES need no one present (§7.4).
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
}

/// Progress for a whole-disk restore.
pub struct DiskRestoreProgress {
    /// "gpt" while writing the partition table, "partition" while copying data.
    pub phase: String,
    /// 0-based index of the partition being restored, and the total count.
    pub partition_index: usize,
    pub partition_count: usize,
    pub part_label: String,
    pub blocks_done: u64,
    pub blocks_total: u64,
    /// Bytes written across the whole disk so far.
    pub bytes_written: u64,
}

pub struct DiskRestoreSummary {
    pub partitions: usize,
    pub bytes_written: u64,
    pub duration: Duration,
}

/// Recreate a disk from its `.obdm`: write the captured GPT, then restore each
/// partition's image into its original offset. The target is taken offline for
/// the duration so Windows does not auto-mount (and lock) partitions mid-write,
/// then brought back online. The GUID-preserving GPT keeps a UEFI system
/// bootable without touching the BCD.
///
/// The caller is responsible for the destructive confirmation and the
/// [`crate`]-external system-disk guard (`own_backup_restore::guard`).
pub fn restore_disk(
    req: &DiskRestoreRequest,
    progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    // A single whole-disk image (plan D1) carries its GPT + data + manifest
    // inside one file; the legacy path takes a `.obdm` + per-volume images.
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2")
        || ext == crate::seal::obk::OBK_EXTENSION
    {
        return restore_disk_from_image(req, progress);
    }
    restore_disk_legacy(req, progress)
}

/// Restore a whole disk from a single embedded-manifest image (plan D3): copy
/// the image's used blocks to the target at their real offsets, then overlay
/// the REAL GPT (real disk + partition GUIDs from the manifest — the image
/// itself carries random GUIDs to avoid colliding with the live source disk).
/// The real GUIDs keep a UEFI system bootable without touching the BCD.
fn restore_disk_from_image(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();
    let ext = req
        .manifest_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    // An encrypted image keeps everything below inside the container, sealed:
    // the disk layout, the partition list and the checksums this restore reads
    // block by block. Without the key there is nothing here to act on, and
    // `load_sidecar_with_keys` says so by name.
    let chain_key = match (&req.encryption, ext.as_str()) {
        (Some(keys), _) => Some(keys.open_chain_of(&req.manifest_path)?.key),
        (None, _) => None,
    };
    let sidecar = match &chain_key {
        Some(key) => crate::sidecar::read_sealed_sidecar(&req.manifest_path, key)?,
        None => read_embedded_sidecar(&req.manifest_path, &ext)?,
    };
    let dm = sidecar.disk_manifest.clone().ok_or_else(|| {
        OwnBackupError::ImageFormat(
            "image has no embedded whole-disk manifest — not a bootable disk image".into(),
        )
    })?;
    let layout = &dm.layout; // real GUIDs
    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    // Validates every partition fits the target (and rejects a smaller disk).
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %dm.disk_model,
        partitions = dm.images.len(),
        "Whole-disk restore (embedded manifest): copying data"
    );

    let mut image = ImageReadSource::open_with(
        &req.manifest_path,
        &sidecar.info.container,
        chain_key.as_ref(),
    )?;
    let mut td = WinRawDevice::open_write(&target_path)?;
    td.set_offline_writable(false)?;

    // Copy every stored image block to the same offset on the target. A block
    // straddling the GPT head carries the image's random GPT bytes — harmless,
    // the real GPT below overwrites the GPT sectors while the partition data
    // stays. (data_offset is 0 for a whole-disk image → block N is at N*bs.)
    let total = sidecar.checksums.len() as u64;
    let mut done = 0u64;
    let mut bytes = 0u64;
    let mut buf = vec![0u8; bs as usize];
    for entry in &sidecar.checksums {
        let offset = entry.block_number * bs;
        let len = bs.min(image_size - offset) as usize;
        if offset + len as u64 > target_size {
            return Err(OwnBackupError::RestoreIncompatible(format!(
                "image block at {offset} exceeds the target disk size {target_size}"
            )));
        }
        let data = &mut buf[..len];
        image.read_at(offset, data)?;
        if req.verify_checksums {
            let actual = compute_block_checksum(data);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }
        td.write_at(offset, data)?;
        done += 1;
        bytes += len as u64;
        progress(&DiskRestoreProgress {
            phase: "partition".into(),
            partition_index: 0,
            partition_count: dm.images.len(),
            part_label: tr!("engine_win.label_partition_data").into_owned(),
            blocks_done: done,
            blocks_total: total,
            bytes_written: bytes,
        })?;
    }

    // Overlay the REAL GPT last, so the restored disk carries the source's disk
    // and partition GUIDs (bootable) rather than the image's random ones.
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: dm.images.len(),
        partition_count: dm.images.len(),
        part_label: tr!("engine_win.label_partition_table").into_owned(),
        blocks_done: total,
        blocks_total: total,
        bytes_written: bytes,
    })?;
    for op in &ops {
        td.write_at(op.offset, &op.bytes)?;
    }
    td.flush()?;
    let _ = td.update_properties();
    drop(td);
    drop(image);

    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }
    info!(
        target = %target_path,
        partitions = dm.images.len(),
        bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore (embedded manifest) complete"
    );
    Ok(DiskRestoreSummary {
        partitions: dm.images.len(),
        bytes_written: bytes,
        duration: started.elapsed(),
    })
}

/// Legacy whole-disk restore from a `.obdm` manifest + per-volume images.
fn restore_disk_legacy(
    req: &DiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();

    let raw = std::fs::read(&req.manifest_path).map_err(|e| {
        OwnBackupError::ImageFormat(format!(
            "cannot read disk manifest {}: {e}",
            req.manifest_path.display()
        ))
    })?;
    let manifest: DiskBackupManifest = serde_json::from_slice(&raw)
        .map_err(|e| OwnBackupError::ImageFormat(format!("bad .obdm manifest: {e}")))?;
    let folder = req
        .manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let layout = &manifest.layout;
    let sector = layout.sector_size as u64;

    let target_path = format!(r"\\.\PhysicalDrive{}", req.target_disk);

    // Probe the target size and build the GPT (validates every partition fits).
    let target_size = WinRawDevice::open_read(&target_path)?.length();
    let ops = disk_gpt_write_ops(layout, target_size)?;

    info!(
        target = %target_path,
        model = %manifest.disk_model,
        partitions = manifest.images.len(),
        "Whole-disk restore: writing GPT"
    );
    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: 0,
        partition_count: manifest.images.len(),
        part_label: tr!("engine_win.label_partition_table").into_owned(),
        blocks_done: 0,
        blocks_total: 0,
        bytes_written: 0,
    })?;

    // Take the target offline (session-only), write the protective MBR + GPT,
    // then refresh so the OS re-reads the (offline) table.
    {
        let mut td = WinRawDevice::open_write(&target_path)?;
        td.set_offline_writable(false)?;
        for op in &ops {
            td.write_at(op.offset, &op.bytes)?;
        }
        td.flush()?;
        let _ = td.update_properties();
    }

    // Restore each partition image into its window on the (offline) target.
    let count = manifest.images.len();
    let mut total_bytes = 0u64;
    for (i, img) in manifest.images.iter().enumerate() {
        let entry = layout
            .partitions
            .iter()
            .find(|p| p.index == img.part_index)
            .ok_or_else(|| {
                OwnBackupError::RestoreIncompatible(format!(
                    "manifest references partition {} which is not in the GPT layout",
                    img.part_index
                ))
            })?;
        let off = entry.first_lba * sector;
        let len = (entry.last_lba + 1 - entry.first_lba) * sector;
        let target = format!("part:{}:{off}:{len}", req.target_disk);
        let label = if entry.name.is_empty() {
            tr!("engine_win.label_partition", index = img.part_index, fs = img.filesystem)
                .into_owned()
        } else {
            format!("{} ({})", entry.name, img.filesystem)
        };
        info!(
            image = %img.image_file_name,
            part = img.part_index,
            offset = off,
            "Whole-disk restore: restoring partition"
        );

        let vreq = VolumeRestoreRequest {
            image_path: folder.join(&img.image_file_name),
            target,
            verify_checksums: req.verify_checksums,
            // Whole-disk images are never encrypted (the backup path refuses
            // to make one), so their partitions need no key either.
            encryption: None,
        };
        let base = total_bytes;
        let summary = restore_volume(&vreq, |p| {
            progress(&DiskRestoreProgress {
                phase: "partition".into(),
                partition_index: i,
                partition_count: count,
                part_label: label.clone(),
                blocks_done: p.blocks_done,
                blocks_total: p.blocks_total,
                bytes_written: base + p.bytes_written,
            })
        })?;
        total_bytes += summary.bytes_restored;
    }

    // Bring the target back online and refresh so the restored volumes appear.
    if let Ok(td) = WinRawDevice::open_write(&target_path) {
        let _ = td.set_online_this_session();
        let _ = td.update_properties();
    }

    info!(
        target = %target_path,
        partitions = count,
        bytes = total_bytes,
        secs = started.elapsed().as_secs(),
        "Whole-disk restore complete"
    );
    Ok(DiskRestoreSummary {
        partitions: count,
        bytes_written: total_bytes,
        duration: started.elapsed(),
    })
}

/// What [`finalize_boot`] left behind, for the restore report.
#[derive(Debug, Default)]
pub struct BootFinalizeReport {
    /// The entry `bcdboot` left as the store's default — the one that boots the
    /// restored Windows. `None` when the store could not be read.
    pub default_id: Option<String>,
    /// Windows loader entries the store held before the menu was trimmed
    /// (recovery/ramdisk entries not counted — those never show in the menu).
    pub loaders_found: usize,
    /// The boot menu was reduced to that single entry.
    pub menu_trimmed: bool,
    /// `\Windows\bootstat.dat` was removed, so the first boot does not open the
    /// recovery screen.
    pub boot_status_cleared: bool,
    /// Why the menu was left as it was, when it was.
    pub note: Option<String>,
}

/// Recreate the boot files (and, on UEFI, the firmware NVRAM entry) for a
/// restored Windows via `bcdboot`, then leave exactly ONE entry in the boot
/// menu.
///
/// Needed only for a **fresh disk / new hardware** — a same-disk restore that
/// preserved the partition GUIDs boots as-is. `windows_volume` is the restored
/// OS volume ("C:"); `system_volume` is the EFI System Partition mount ("S:")
/// for UEFI (omit to let bcdboot pick the current one). Typically run from
/// WinPE.
///
/// The menu step is not cosmetic. The ESP is restored from the copy, so it
/// arrives carrying the SOURCE machine's BCD, whose loader entries name the
/// source disk's GPT GUIDs; the restored disk gets its own, so those entries
/// resolve to nothing. `bcdboot` adds a correct entry and makes it the default
/// but leaves the inherited ones in the display order — which is why a restored
/// Windows offered two identical "Windows 10" items, one booting and one dying
/// with `0xc000000e \Windows\system32\winload.efi` (2026-07-30). Trimming the
/// display order to the default hides them without deleting anything: the
/// entries stay in the store for anyone who wants to look.
pub fn finalize_boot(
    windows_volume: &str,
    system_volume: Option<&str>,
    uefi: bool,
) -> Result<BootFinalizeReport> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let win_dir = format!(r"{}\Windows", windows_volume.trim_end_matches(['\\', '/']));
    let mut cmd = std::process::Command::new("bcdboot");
    cmd.arg(&win_dir);
    if let Some(esp) = system_volume {
        cmd.args(["/s", esp]);
    }
    cmd.args(["/f", if uefi { "UEFI" } else { "BIOS" }]);
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = cmd
        .output()
        .map_err(|e| OwnBackupError::Device(format!("failed to run bcdboot: {e}")))?;
    if !out.status.success() {
        return Err(OwnBackupError::RestoreIncompatible(format!(
            "bcdboot failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    info!(win_dir, uefi, "bcdboot: boot files recreated");

    let boot_status_cleared = clear_boot_status(windows_volume);

    let mut report = match system_volume {
        Some(esp) => trim_boot_menu(esp, uefi, windows_volume),
        // Without /s we do not know which store bcdboot wrote, and the "current"
        // store belongs to whatever is running this (WinPE, or the live OS) —
        // never something to edit blind.
        None => BootFinalizeReport {
            note: Some(tr!("engine_win.note_no_esp").into_owned()),
            ..Default::default()
        },
    };
    report.boot_status_cleared = boot_status_cleared;
    Ok(report)
}

/// Remove the restored system's `\Windows\bootstat.dat`.
///
/// A copy is taken from a *running* Windows (VSS), so the shutdown it records
/// never happened: on the first boot the boot manager reads that file, decides
/// the machine "couldn't turn off properly" and stops at the recovery screen
/// with `0xc0000001`. Pressing Enter boots it and the state clears itself, but
/// a restore that ends in an error screen reads as a failed restore.
///
/// Windows recreates the file on the next boot, which is why this is preferred
/// over `bootstatuspolicy IgnoreShutdownFailures`: that setting would silence
/// genuine shutdown failures on the machine for good.
///
/// Best-effort — a copy whose Windows directory is spelled differently, or a
/// file that will not go, simply leaves the one-time recovery screen in place.
fn clear_boot_status(windows_volume: &str) -> bool {
    let path = format!(r"{}\Windows\bootstat.dat", windows_volume.trim_end_matches(['\\', '/']));
    if !std::path::Path::new(&path).exists() {
        debug!(path = %path, "boot status: nothing to clear");
        return false;
    }
    if let Err(e) = std::fs::remove_file(&path) {
        // The file is hidden + system; that alone does not block a delete, but
        // a read-only attribute does.
        let wide = crate::win_device::to_wide(&path);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let _ = unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            )
        };
        if let Err(again) = std::fs::remove_file(&path) {
            warn!(
                path = %path, error = %e, retry_error = %again,
                "boot status: could not remove bootstat.dat — the first boot may stop at the \
                 recovery screen once"
            );
            return false;
        }
    }
    info!(
        path = %path,
        "boot status: removed bootstat.dat so the first boot does not open the recovery screen"
    );
    true
}

/// Run `bcdedit` with the given arguments; stdout on success, a message on
/// failure. The output is read as lossy UTF-8 on purpose: everything this code
/// looks for (identifiers, `partition=X:`) is ASCII, while bcdedit's prose is
/// localised and in the console code page.
fn bcdedit(args: &[&str]) -> std::result::Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let out = std::process::Command::new("bcdedit")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| tr!("engine_win.bcdedit_start_failed", error = e).into_owned())?;
    if !out.status.success() {
        return Err(tr!(
            "engine_win.bcdedit_failed",
            args = args.join(" "),
            code = out.status.code().unwrap_or(-1),
            output = String::from_utf8_lossy(&out.stderr).trim()
        )
        .into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first `{guid}` in `text`, as bcdedit prints identifiers.
fn first_identifier(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text[start..].find('}')? + start;
    let id = &text[start..=end];
    // 32 hex digits + 4 dashes + 2 braces; aliases like {default} are shorter.
    (id.len() == 38).then(|| id.to_string())
}

/// Loader entries in a store, as `(identifier, is_ramdisk)` pairs. A ramdisk
/// loader is the Recovery Environment: it never appears in the boot menu and
/// must not be counted with the menu entries.
fn loader_entries(store: &str) -> Vec<(String, bool)> {
    let Ok(text) = bcdedit(&["/store", store, "/enum", "osloader", "/v"]) else {
        return Vec::new();
    };
    text.split("\r\n\r\n")
        .filter_map(|block| {
            let id = first_identifier(block)?;
            Some((id, block.contains("ramdisk=")))
        })
        .collect()
}

/// Leave the store's boot menu showing only the entry bcdboot made default.
///
/// Deliberately conservative: if the default entry cannot be read, or does not
/// point at the Windows volume that was just restored, nothing is changed and
/// the reason is reported — a wrong guess here is an unbootable machine, while
/// an untouched menu is merely the extra question the user already sees.
fn trim_boot_menu(esp_volume: &str, uefi: bool, windows_volume: &str) -> BootFinalizeReport {
    let esp = esp_volume.trim_end_matches(['\\', '/']);
    let store = if uefi {
        format!(r"{esp}\EFI\Microsoft\Boot\BCD")
    } else {
        format!(r"{esp}\Boot\BCD")
    };
    if !std::path::Path::new(&store).exists() {
        warn!(store = %store, "boot menu: store not found where bcdboot should have written it");
        return BootFinalizeReport {
            note: Some(tr!("engine_win.note_store_not_found", store = store).into_owned()),
            ..Default::default()
        };
    }

    let default_block = match bcdedit(&["/store", &store, "/enum", "{default}", "/v"]) {
        Ok(text) => text,
        Err(e) => {
            warn!(error = %e, "boot menu: cannot read the default entry");
            return BootFinalizeReport {
                note: Some(tr!("engine_win.note_default_unreadable", error = e).into_owned()),
                ..Default::default()
            };
        }
    };
    let Some(default_id) = first_identifier(&default_block) else {
        warn!("boot menu: the default entry has no readable identifier");
        return BootFinalizeReport {
            note: Some(tr!("engine_win.note_default_unknown").into_owned()),
            ..Default::default()
        };
    };

    // The default must be the entry bcdboot just wrote for THIS restore: its
    // device is the restored Windows volume, which is holding a drive letter for
    // exactly this reason.
    let letter = windows_volume.trim_end_matches(['\\', '/']).to_ascii_uppercase();
    let expected = format!("PARTITION={letter}"); // matched against an uppercased block
    let loaders = loader_entries(&store);
    let menu_loaders = loaders.iter().filter(|(_, ramdisk)| !ramdisk).count();
    if !default_block.to_ascii_uppercase().contains(&expected) {
        warn!(
            default = %default_id, expected = %expected,
            "boot menu: the default entry does not point at the restored volume — left as it is"
        );
        return BootFinalizeReport {
            default_id: Some(default_id),
            loaders_found: menu_loaders,
            note: Some(tr!("engine_win.note_default_elsewhere").into_owned()),
            menu_trimmed: false,
            ..Default::default()
        };
    }

    match bcdedit(&["/store", &store, "/displayorder", &default_id]) {
        Ok(_) => {
            info!(
                store = %store,
                default = %default_id,
                loaders = menu_loaders,
                "boot menu: display order reduced to the entry bcdboot created"
            );
            BootFinalizeReport {
                default_id: Some(default_id),
                loaders_found: menu_loaders,
                menu_trimmed: menu_loaders > 1,
                note: None,
                ..Default::default()
            }
        }
        Err(e) => {
            warn!(error = %e, "boot menu: could not set the display order");
            BootFinalizeReport {
                default_id: Some(default_id),
                loaders_found: menu_loaders,
                menu_trimmed: false,
                note: Some(tr!("engine_win.note_trim_failed", error = e).into_owned()),
                ..Default::default()
            }
        }
    }
}

/// Result of verifying an image against its sidecar checksums.
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub mismatched_blocks: Vec<u64>,
    pub duration: Duration,
}

impl VerifySummary {
    pub fn valid(&self) -> bool {
        self.mismatched_blocks.is_empty()
    }
}

/// Verify every block of an image against the sidecar checksums.
pub fn verify_image(
    image_path: &Path,
    progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    verify_image_with_keys(image_path, None, progress)
}

/// The same check for an encrypted image. With the key this is the full check
/// §7.4 asks for: the record tags and the authenticated index decide whether
/// the bytes are ours before any checksum is computed over them.
pub fn verify_image_with_keys(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    verify_image_scoped(image_path, keys, VerifyScope::Everything, progress)
}

/// How much of an image a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyScope {
    /// Every block the restore point is made of, read through the whole chain.
    /// Catches damage in ANY copy the point depends on — a bad block in the
    /// base breaks every increment above it, and only this reading notices.
    #[default]
    Everything,
    /// Only the blocks THIS file stores: what the copy just written brought.
    /// An increment of 2 GiB over a 40 GiB base is checked in 2 GiB of reads,
    /// not 42. The trade is said out loud in the interface — the older copies
    /// are not read, so damage in them is not found by this check.
    /// For a full copy, which has no parent, the two are the same.
    LatestCopyOnly,
}

/// Backup block numbers whose data lives in THIS file rather than a parent.
///
/// Asked of the container itself, per cluster, without falling through to the
/// backing file: qcow2 by its L2 descriptor, `.obk` by its authenticated index,
/// VHDX by its BAT. A cluster the file marks explicitly ZERO counts as stored
/// here too — in a child that is a write (it hides the parent's data), and a
/// check that skipped it would never notice a region wrongly emptied.
///
/// A backup block covers `[data_offset + n·bs, data_offset + (n+1)·bs)` in
/// guest bytes; a stored cluster marks every block it touches. Guest bytes
/// before `data_offset` (a whole-disk image's own metadata) belong to no block.
fn blocks_stored_in_this_file(
    image_path: &Path,
    container: &str,
    chain_key: Option<&crate::seal::ChainKey>,
    data_offset: u64,
    block_size: u64,
) -> Result<std::collections::HashSet<u64>> {
    let mut out = std::collections::HashSet::new();
    let mut mark = |guest_start: u64, len: u64| {
        let end = guest_start.saturating_add(len);
        if end <= data_offset || block_size == 0 {
            return;
        }
        let from = guest_start.max(data_offset) - data_offset;
        let to = end - 1 - data_offset;
        for n in (from / block_size)..=(to / block_size) {
            out.insert(n);
        }
    };
    match container {
        "obk" => {
            let key = chain_key.ok_or_else(|| {
                OwnBackupError::Encryption(tr!("engine_win.verify_latest_needs_key").into_owned())
            })?;
            let mut image = crate::seal::obk::ObkImage::open_path(image_path, key)?;
            let cs = image.cluster_size();
            for c in 0..image.total_clusters() {
                if image.cluster_state(c)? != crate::seal::obk::ClusterState::Absent {
                    mark(c * cs, cs);
                }
            }
        }
        "qcow2" => {
            let file = std::io::BufReader::new(std::fs::File::open(image_path)?);
            let mut image = crate::qcow2::reader::Qcow2Reader::open(file)?;
            let cs = image.cluster_size();
            for c in 0..image.total_clusters() {
                if !matches!(image.descriptor(c)?, crate::qcow2::ClusterDescriptor::Unallocated) {
                    mark(c * cs, cs);
                }
            }
        }
        "vhdx" | "avhdx" => {
            let alloc = crate::vhdx_bat::allocated_payload_blocks(image_path)?;
            for b in alloc.blocks {
                mark(b * alloc.block_size, alloc.block_size);
            }
        }
        other => {
            return Err(OwnBackupError::ImageFormat(
                tr!("engine_win.verify_latest_unknown_container", container = other).into_owned(),
            ))
        }
    }
    Ok(out)
}

/// The same check with a chosen scope — see [`VerifyScope`].
pub fn verify_image_scoped(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    scope: VerifyScope,
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<VerifySummary> {
    let started = Instant::now();
    let sidecar = load_sidecar_with_keys(image_path, keys)?;
    let block_size = sidecar.manifest.block_size as u64;
    let volume_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;

    let chain_key = match (keys, sidecar.info.container.as_str()) {
        (Some(keys), "obk") => Some(keys.open_chain_of(image_path)?.key),
        _ => None,
    };
    let mut image_dev =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;

    // The sidecar of an increment describes the WHOLE state, so reading all
    // of it walks every copy underneath. For "latest only" the file itself is
    // asked which blocks it holds, and only those are read.
    let only: Option<std::collections::HashSet<u64>> = match scope {
        VerifyScope::Everything => None,
        VerifyScope::LatestCopyOnly => Some(blocks_stored_in_this_file(
            image_path,
            &sidecar.info.container,
            chain_key.as_ref(),
            data_offset,
            block_size,
        )?),
    };
    let entries: Vec<_> = sidecar
        .checksums
        .iter()
        .filter(|e| only.as_ref().is_none_or(|set| set.contains(&e.block_number)))
        .collect();
    if let Some(set) = &only {
        info!(
            image = %image_path.display(),
            of_this_copy = entries.len(),
            of_the_state = sidecar.checksums.len(),
            stored_here = set.len(),
            "verify: only the blocks this copy wrote"
        );
    }

    let total = entries.len() as u64;
    let mut checked = 0u64;
    let mut mismatched = Vec::new();

    for entry in entries {
        let offset = entry.block_number * block_size;
        let len = std::cmp::min(block_size, volume_len - offset) as usize;

        let mut buf = vec![0u8; len];
        image_dev.read_at(data_offset + offset, &mut buf)?;
        let actual = compute_block_checksum(&buf);

        if actual != entry.checksum {
            // Re-read once before declaring corruption: a freshly detached and
            // re-attached image can hand back a transient bad read that a
            // second read clears. Only a checksum that stays wrong is a real
            // image defect — and we log exactly which block for diagnosis.
            let mut buf2 = vec![0u8; len];
            image_dev.read_at(data_offset + offset, &mut buf2)?;
            let actual2 = compute_block_checksum(&buf2);
            if actual2 != entry.checksum {
                // Structure hints: an all-zero block or a non-zero tail where
                // zeros were recorded each point at a specific failure mode
                // (unwritten region vs external write into the image).
                let all_zero = buf2.iter().all(|&b| b == 0);
                let tail_zero = buf2[len.saturating_sub(4096)..].iter().all(|&b| b == 0);
                warn!(
                    block = entry.block_number,
                    image_offset = data_offset + offset,
                    len,
                    expected = format!("{:016x}", entry.checksum),
                    actual = format!("{:016x}", actual2),
                    all_zero,
                    tail_4096_zero = tail_zero,
                    "verify: block checksum mismatch (image defect)"
                );
                mismatched.push(entry.block_number);
            } else {
                info!(
                    block = entry.block_number,
                    "verify: transient read mismatch cleared on re-read"
                );
            }
        }
        checked += 1;
        progress(checked, total)?;
    }

    if mismatched.is_empty() {
        info!(image = %image_path.display(), blocks = total, "verify: image OK");
    }

    Ok(VerifySummary {
        blocks_checked: checked,
        mismatched_blocks: mismatched,
        duration: started.elapsed(),
    })
}

/// Path of the temporary differencing child a read-write mount writes into.
/// Its existence is what says "this image is mounted RW right now" — the
/// consolidation guard reads it, because folding an increment into a base
/// somebody is writing through would corrupt both.
pub fn child_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.avhdx");
    image_path.with_file_name(name)
}

fn scratch_path_for(image_path: &Path) -> PathBuf {
    let mut name = image_path.file_name().unwrap_or_default().to_os_string();
    name.push(".mount_rw.scratch");
    image_path.with_file_name(name)
}

/// Mount an image; volumes inside get drive letters from Windows.
/// The attachment survives process exit — use [`unmount_image`] to detach.
pub fn mount_image(image_path: &Path, read_write: bool) -> Result<String> {
    mount_image_with_keys(image_path, read_write, None)
}

/// The same, able to mount an encrypted image. Manual mounting is where a
/// person types a password (§7.4); the service passes its own keys for the
/// maintenance mounts it does inside a job.
pub fn mount_image_with_keys(
    image_path: &Path,
    read_write: bool,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<String> {
    if crate::sidecar::detect_container_format(image_path).ok()
        == Some(crate::seal::obk::OBK_EXTENSION)
    {
        return mount_obk_via_iscsi(image_path, read_write, keys);
    }
    // Dispatch by the container's OWN magic, never by sidecar metadata (new
    // backups carry no external .obkm, and a silent fall-through once sent a
    // qcow2 into vhdmp, which reported it as "file corrupted", error 1392).
    // qcow2 cannot go through vhdmp; it is served over an iSCSI target of ours
    // to Windows' own initiator (see `mount_qcow2_via_iscsi`).
    let mut magic = [0u8; 8];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(image_path)
            .map_err(|e| OwnBackupError::Device(format!("open {}: {e}", image_path.display())))?;
        f.read_exact(&mut magic)
            .map_err(|e| OwnBackupError::Device(format!("read {}: {e}", image_path.display())))?;
    }
    if magic[..4] == [0x51, 0x46, 0x49, 0xFB] {
        // "QFI\xfb"
        return mount_qcow2_via_iscsi(image_path, read_write);
    }
    if &magic != b"vhdxfile" {
        return Err(OwnBackupError::ImageFormat(format!(
            "{} is neither a VHDX nor a qcow2 image (unrecognized signature)",
            image_path.display()
        )));
    }

    let attached = if read_write {
        let child_path = child_path_for(image_path);
        let _ = std::fs::remove_file(&child_path);
        create_differencing_vhdx(&child_path, image_path)?;
        attach_vhdx(&child_path, AttachMode::ReadWriteMounted)?
    } else {
        attach_vhdx(image_path, AttachMode::ReadOnlyMounted)?
    };

    // Backup images carry a persisted "offline" disk attribute (it keeps the
    // chain from auto-mounting during backups), so for browsing the disk must
    // be brought online explicitly — for this session only, the registry
    // attribute stays. Requires a read-write device handle (IOCTL
    // access rights), which vhdmp grants even on a read-only attach.
    match open_write_with_retry(attached.physical_path()) {
        Ok(dev) => {
            if let Err(e) = dev.set_online_this_session() {
                warn!(error = %e, "could not bring the mounted image online — it may show as offline in Disk Management");
            }
        }
        Err(e) => {
            warn!(error = %e, "could not open the mounted image to bring it online");
        }
    }
    letter_mounted_image(attached.physical_path());
    Ok(attached.physical_path().to_string())
}

/// Make a mounted image browsable: its disk online for this session, and its
/// data volumes lettered.
///
/// Neither happens by itself. A read-only iSCSI disk is left as the SAN policy
/// says, which on a server or the boot medium is offline; and Windows letters
/// only volumes it has never met, which a backup image rarely is — see
/// [`crate::letters_win::letter_data_volumes_on_disk`].
fn letter_mounted_image(physical: &str) {
    let Some(disk) = disk_number_from_physical(physical) else {
        return;
    };
    if let Ok(dev) = open_write_with_retry(physical) {
        if matches!(dev.is_offline_read_only(), Ok((true, _))) {
            match dev.set_online_this_session() {
                Ok(()) => info!(%physical, "mounted image: disk brought online for browsing"),
                Err(e) => warn!(%physical, error = %e, "mounted image: disk stays offline"),
            }
        }
    }
    let letters = crate::letters_win::letter_data_volumes_on_disk(disk);
    info!(%physical, ?letters, "mounted image: drive letters");
}

/// Detach a previously mounted image.
pub fn unmount_image(image_path: &Path) -> Result<()> {
    // qcow2 mounts are tracked in-process (the target dies with us).
    if qcow2_mounts().lock().unwrap().contains_key(image_path) {
        return unmount_qcow2_via_iscsi(image_path);
    }
    // A qcow2 we are not serving is simply not mounted — and must never reach
    // the VHDX road below, which hands it to `OpenVirtualDisk` and gets "the
    // file or directory is corrupted" (error 1392) for an image that is
    // perfectly fine (seen 2026-09-18, on a second press of "Отключить").
    if is_qcow2_file(image_path) {
        return Err(OwnBackupError::Config(
            tr!("engine_win.not_mounted", image = image_path.display()).into_owned(),
        ));
    }

    let child_path = child_path_for(image_path);
    if child_path.exists() {
        info!(image = %image_path.display(), "VHDX RW mount: unmounting child differencing VHDX");
        
        // 1. Detach child first
        detach_vhdx(&child_path)?;

        // 2. Read the touched blocks from BAT before we merge/delete it
        let alloc = match allocated_payload_blocks(&child_path) {
            Ok(a) if !a.blocks.is_empty() => Some(a),
            _ => None,
        };

        if let Some(alloc) = alloc {
            info!(image = %image_path.display(), count = alloc.blocks.len(), "VHDX RW mount: merging modified blocks back into base");
            
            // 3. Merge the child into the parent
            merge_into_parent(&child_path)?;
            let _ = std::fs::remove_file(&child_path);

            // 4. Update the sidecar with rehashed checksums (embedded or external)
            let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::true_removal_win::rehash_touched_blocks(
                    image_path,
                    &alloc,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                    &mut |_, _| {}, // interactive unmount: no progress channel
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    crate::vhdx_bat::write_vhdx_guest_at(image_path, meta_offset, &blob_bytes)?;
                    info!(image = %image_path.display(), "VHDX RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(&sidecar_path, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    info!(image = %image_path.display(), "VHDX RW mount: .obkm sidecar updated successfully");
                }
            }
        } else {
            // No blocks touched, just delete child
            let _ = std::fs::remove_file(&child_path);
        }
        Ok(())
    } else {
        detach_vhdx(image_path)
    }
}

// ---------------------------------------------------------------------------
// qcow2 mounting over our own iSCSI target
// ---------------------------------------------------------------------------

/// One qcow2 image currently shown to Windows as a disk, and the scratch
/// overlay behind it when the mount is writable.
struct MountedImage {
    attached: crate::attach_win::Attached,
    overlay: Option<std::sync::Arc<std::sync::Mutex<crate::overlay::WritableOverlaySource>>>,
}

/// qcow2 mounts served by THIS process: the target dies with the process, so
/// this only makes sense inside the long-running service (or a foreground CLI
/// command).
static QCOW2_MOUNTS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, MountedImage>>> =
    std::sync::OnceLock::new();

fn qcow2_mounts() -> &'static std::sync::Mutex<HashMap<PathBuf, MountedImage>> {
    QCOW2_MOUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Is this image (qcow2) currently served to Windows by a mount of ours?
/// Companion to [`child_path_for`] for the consolidation guard.
pub fn qcow2_mounted_here(image_path: &Path) -> bool {
    qcow2_mounts().lock().map(|m| m.contains_key(image_path)).unwrap_or(true)
}

/// Present a qcow2 backup (incl. its backing chain) to Windows as a real disk:
/// serve it over an iSCSI target of ours and have Windows' own initiator log
/// in. Windows mounts the filesystem inside and assigns a drive letter.
/// Returns the physical disk path (`\\.\PhysicalDriveN`).
fn mount_qcow2_via_iscsi(image_path: &Path, read_write: bool) -> Result<String> {
    if let Some(existing) = qcow2_mounts().lock().unwrap().get(image_path) {
        return Ok(existing.attached.physical.clone());
    }

    let (virtual_size, cluster_size) = {
        let r = crate::qcow2::Qcow2RandomReader::open(image_path)?;
        (r.virtual_size(), r.cluster_size())
    };
    let serial = crate::iscsi::serial_for_image(image_path);

    // Writable mounts never touch the image: everything lands in a scratch file
    // beside it, and only what the caller asks for is merged back on unmount.
    let (attached, overlay) = if read_write {
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
        let overlay_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch)
            .map_err(|e| OwnBackupError::Device(format!("create scratch file: {e}")))?;
        let base: Box<dyn BlockSource> = Box::new(crate::qcow2::Qcow2RandomReader::open(image_path)?);
        let overlay = std::sync::Arc::new(std::sync::Mutex::new(
            crate::overlay::WritableOverlaySource::new(base, cluster_size, overlay_file),
        ));
        let attached =
            crate::attach_win::attach_rw(Box::new(overlay.clone()), virtual_size, &serial)?;
        (attached, Some(overlay))
    } else {
        let reader = crate::qcow2::Qcow2RandomReader::open(image_path)?;
        let attached = crate::attach_win::attach_ro(Box::new(reader), virtual_size, &serial)?;
        (attached, None)
    };

    let physical = attached.physical.clone();
    info!(
        image = %image_path.display(),
        physical = %physical,
        read_write,
        "qcow2 mounted over iSCSI"
    );
    letter_mounted_image(&physical);
    qcow2_mounts()
        .lock()
        .unwrap()
        .insert(image_path.to_path_buf(), MountedImage { attached, overlay });
    Ok(physical)
}

/// Serve an encrypted image read-only to Windows' own iSCSI initiator.
///
/// Read-write is refused, and that refusal is the point: a writable mount puts
/// everything the filesystem driver writes into a scratch file beside the
/// image, and for an encrypted image that file would be plaintext — the one
/// thing §7.4 says must not happen. The encrypted scratch is still to come.
fn mount_obk_via_iscsi(
    image_path: &Path,
    read_write: bool,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<String> {
    let keys = keys.ok_or_else(|| {
        OwnBackupError::Encryption(
            tr!("engine_win.encrypted_needs_key", image = image_path.display()).into_owned(),
        )
    })?;
    if read_write {
        return Err(OwnBackupError::Encryption(
            tr!("engine_win.encrypted_read_only").into_owned(),
        ));
    }
    if let Some(existing) = qcow2_mounts().lock().unwrap().get(image_path) {
        return Ok(existing.attached.physical.clone());
    }

    let chain = keys.open_chain_of(image_path)?;
    let reader = crate::seal::obk::ObkRandomReader::open(image_path, &chain.key)?;
    let virtual_size = reader.virtual_size();
    let serial = crate::iscsi::serial_for_image(image_path);
    let attached = crate::attach_win::attach_ro(Box::new(reader), virtual_size, &serial)?;
    let physical = attached.physical.clone();
    info!(
        image = %image_path.display(),
        physical = %physical,
        "encrypted image mounted read-only over iSCSI"
    );
    letter_mounted_image(&physical);
    qcow2_mounts().lock().unwrap().insert(
        image_path.to_path_buf(),
        MountedImage {
            attached,
            overlay: None,
        },
    );
    Ok(physical)
}

fn unmount_qcow2_via_iscsi(image_path: &Path) -> Result<()> {
    let entry = qcow2_mounts().lock().unwrap().remove(image_path);
    let Some(entry) = entry else {
        return Err(OwnBackupError::Config(format!(
            "{} is not mounted by this service",
            image_path.display()
        )));
    };
    // Dismount, log out, stop serving — in that order, and before anything is
    // merged: the overlay must not move under a disk Windows still has open.
    entry.attached.detach();
    info!(image = %image_path.display(), "qcow2 unmounted (iSCSI session logged out)");

    // If it was mounted in RW mode, we have the overlay and need to merge + rehash + update sidecar!
    if let Some(overlay_arc) = entry.overlay {
        let touched = overlay_arc.lock().unwrap().touched_clusters();
        if !touched.is_empty() {
            info!(image = %image_path.display(), count = touched.len(), "QCOW2 RW mount: merging modified clusters back into base");
            
            let ext = image_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
            // At the level the copy was made with, from its own manifest, so
            // the merged clusters match the rest of the image. Unreadable →
            // the default (`Qcow2InPlaceMerge` falls back to it on `None`).
            let zstd_level = read_embedded_sidecar(image_path, &ext)
                .or_else(|_| read_sidecar(&sidecar_path_for(image_path)))
                .map(|s| crate::qcow2::zstd_level_or_default(s.manifest.compression_level))
                .ok();
            {
                let mut ov = overlay_arc.lock().unwrap();
                let mut merge = crate::qcow2::Qcow2InPlaceMerge::open(image_path, zstd_level)?;
                for &c in &touched {
                    let data = ov.read_cluster(c)?;
                    merge.write_cluster(c, &data)?;
                }
                merge.commit()?;
            }

            // Now rehash the touched regions and update the sidecar (embedded or external)!
            let mut read_from_embedded = false;
            let sidecar_attempt = match read_embedded_sidecar(image_path, &ext) {
                Ok(s) => {
                    read_from_embedded = true;
                    Ok(s)
                }
                Err(_) => {
                    let sidecar_path = sidecar_path_for(image_path);
                    read_sidecar(&sidecar_path)
                }
            };
            if let Ok(mut sidecar) = sidecar_attempt {
                let cluster_size = {
                    let ov = overlay_arc.lock().unwrap();
                    ov.cluster_size()
                };
                let data_offset = sidecar.info.volume_data_offset;
                let volume_len = sidecar.manifest.source_disk.size_bytes;
                let block_size = sidecar.manifest.block_size;
                let _blocks_rehashed = crate::qcow2_removal_win::rehash_touched(
                    image_path,
                    &touched,
                    cluster_size,
                    data_offset,
                    volume_len,
                    block_size,
                    &mut sidecar.checksums,
                    &mut |_, _| {}, // interactive unmount: no progress channel
                )?;
                if read_from_embedded {
                    let sector = 512u64;
                    let last_lba = (DEFAULT_PARTITION_OFFSET + volume_len).div_ceil(sector) - 1;
                    let meta_offset = (last_lba + 1) * sector;
                    let mut blob_bytes = Vec::new();
                    crate::sidecar::write_sidecar_v2_to(&mut blob_bytes, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    crate::qcow2::merge::write_qcow2_guest_blob(image_path, meta_offset, &blob_bytes)?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: embedded sidecar updated successfully");
                } else {
                    let sidecar_path = sidecar_path_for(image_path);
                    write_sidecar(&sidecar_path, &sidecar.info, &sidecar.manifest, &sidecar.checksums)?;
                    info!(image = %image_path.display(), "QCOW2 RW mount: .obkm sidecar updated successfully");
                }
            }
        }
        // Delete scratch file
        let scratch = scratch_path_for(image_path);
        let _ = std::fs::remove_file(&scratch);
    }

    info!(image = %image_path.display(), "qcow2 unmounted (AIM device removed)");
    Ok(())
}

/// Did this read fail because the volume is a locked BitLocker volume?
///
/// `FVE_E_LOCKED_VOLUME` (`0x80310000`) is what the filesystem stack returns for
/// every read of a locked volume through its volume handle. The code is carried
/// in the formatted Windows error — its text is localised, the hex is not, so
/// that is what is matched.
fn is_bitlocker_locked(e: &OwnBackupError) -> bool {
    e.to_string().contains("0x80310000")
}

/// Outcome of decrypting an encrypted source: whether the passphrase unlocked
/// it, and (on success) the inner filesystem's name / volume label / used bytes
/// so the UI can show them like any other volume.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DecryptedProbe {
    pub ok: bool,
    pub filesystem: String,
    pub label: String,
    pub used_bytes: u64,
    /// Where the filesystem INSIDE the container says it was last mounted
    /// (ext only). This is what turns "the password opened something" into an
    /// answer to the question the wizard actually asked: is the system in here,
    /// or is this a data volume the user merely wanted to look at?
    pub linux_last_mounted: String,
}

/// How an encrypted partition can be opened, asked of the volume itself.
///
/// `(what it accepts, whether we can do any of it)`. Costs a few sector reads
/// and no password: BitLocker's FVE metadata lists its protectors in the clear,
/// and a LUKS header its keyslots. The interface needs it to stop asking a
/// TPM-only volume for a passphrase it does not have — the answer to which is
/// not "wrong password" but "wrong question".
///
/// When the container is neither, or its metadata cannot be read, the answer is
/// "we do not know" — an empty list and `true`. Saying `false` there would put
/// "открыть нечем" under a volume nobody has actually asked, which is exactly
/// what a LUKS partition used to get: the wrong question in the other
/// direction. Letting the user try costs one rejected passphrase; refusing to
/// ask costs them the volume.
#[cfg(target_os = "windows")]
pub fn describe_encryption(disk: u32, offset: u64, length: u64) -> (Vec<String>, bool, bool) {
    let unknown = (Vec::new(), true, false);
    let path = format!(r"\\.\PhysicalDrive{disk}");
    let Ok(mut window) = disk_window(&path, offset, length) else {
        return unknown;
    };
    if let Ok(info) = crate::bitlocker::inspect_bitlocker_with(&mut window, disk_sector_size(&path)) {
        let names = info
            .protectors
            .iter()
            .map(|p| {
                if p.count > 1 {
                    format!("{} ×{}", p.name, p.count)
                } else {
                    p.name.clone()
                }
            })
            .collect();
        // Мастеру важно «есть ли что набрать», а не «справится ли наш разбор»:
        // чего не умеем мы, откроет Windows. Иначе поле пароля не показывается
        // там, где пароль отлично подошёл бы.
        return (names, info.typeable(), info.recovery_key_only());
    }
    match crate::luks::inspect_luks(&mut window) {
        Ok(info) => (info.methods, info.openable, false),
        Err(e) => {
            debug!(disk, offset, error = %e, "encrypted partition described neither as BitLocker nor as LUKS");
            unknown
        }
    }
}

/// Describe what is inside a container we have just unlocked: filesystem,
/// volume label, bytes in use.
///
/// The superblock alone answers this for ext/xfs/btrfs/FAT — their free-space
/// counters and labels live there. NTFS keeps neither: its label is in the MFT
/// and its free space in `$Bitmap`. Windows would know both, but only for a
/// volume it has mounted, and this one it cannot mount — so both are read here,
/// from the same structures the used-block map is built from. A failure there
/// costs the numbers, not the unlock.
#[cfg(target_os = "windows")]
fn probe_decrypted(view: &mut dyn crate::fat_map::RawVolume) -> Result<DecryptedProbe> {
    use own_backup_common::types::FileSystem;

    let want = (own_backup_inventory::fs_probe::PROBE_LEN as u64).min(view.len()) as usize;
    let mut buf = vec![0u8; want];
    view.read_at(0, &mut buf)?;
    let Some(probed) = own_backup_inventory::fs_probe::identify(&buf) else {
        return Ok(DecryptedProbe { ok: true, ..Default::default() });
    };
    let filesystem = format!("{:?}", probed.fs).to_lowercase();
    let mut label = probed.label.unwrap_or_default();
    let mut used_bytes = probed.used_bytes.unwrap_or(0);

    if probed.fs == FileSystem::Ntfs {
        match crate::ntfs_map::ntfs_used_bytes(view) {
            Ok(used) => used_bytes = used,
            Err(e) => warn!(error = %e, "could not count used bytes of the unlocked NTFS volume"),
        }
        if label.is_empty() {
            label = crate::ntfs_map::ntfs_volume_label(view).unwrap_or_default();
        }
    }
    Ok(DecryptedProbe {
        ok: true,
        filesystem,
        label,
        used_bytes,
        linux_last_mounted: probed.last_mounted.unwrap_or_default(),
    })
}

/// Open a BitLocker volume through the disk it lives on, and describe what is
/// inside it.
///
/// The volume device is useless here: locked, it answers `FVE_E_LOCKED_VOLUME`
/// to everything. The partition's bytes are on `\\.\PhysicalDriveN` all the
/// same — that is how the inventory recognises such a volume in the first
/// place — so the passphrase is tried against a window onto the disk.
///
/// Confirmed on a real volume (2026-08-05): Windows 10, XTS-AES 128, password
/// protector.
#[cfg(target_os = "windows")]
fn probe_bitlocker(volume_device: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let extents = crate::volume_win::volume_extents(volume_device);
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.bitlocker_multi_extent",
                volume = volume_device,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let disk = format!(r"\\.\PhysicalDrive{}", extent.disk_number);
    let mut probe = disk_window(&disk, extent.offset, extent.length)?;
    let params = match crate::bitlocker::open_bitlocker_with(&mut probe, passphrase, disk_sector_size(&disk))
    {
        Ok(p) => p,
        // Wrong passphrase — a fact for the interface, not an error. AES-CCM
        // verifies its own tag, so this really does mean "not this password".
        Err(e) if crate::bitlocker::is_wrong_passphrase(&e) => {
            info!(error = %e, "BitLocker passphrase rejected");
            return Ok(DecryptedProbe { ok: false, ..Default::default() });
        }
        // А это НЕ про пароль: шифр вне AES-XTS (панель Windows шифрует съёмные
        // диски в режиме совместимости по умолчанию), ReFS без BPB, нечитаемый
        // заголовок. Пароль при этом мог подойти — в журнале так и написано,
        // «VMK unwrapped», — и отвечать «пароль не подходит» значит отправить
        // человека перенабирать верное. Всё это открывает сама Windows.
        Err(e) => {
            warn!(error = %e, "свой разбор BitLocker не справился — прошу открыть Windows");
            return probe_by_windows(volume_device, passphrase);
        }
    };
    info!(
        algo = format!("{:#x}", params.algorithm),
        disk = extent.disk_number,
        offset = extent.offset,
        "BitLocker source unlocked; backing up decrypted data"
    );

    let mut view = crate::bitlocker::BitLockerVolume::new(
        Box::new(disk_window(&disk, extent.offset, extent.length)?),
        params,
    );
    probe_decrypted(&mut view)
}

/// Открыть том силами Windows и осмотреть то, что получилось.
///
/// После разблокировки том становится обычным смонтированным, и читается он
/// как любой другой — своей расшифровки уже не нужно. Страж держит диск
/// закрытым на запись только пока мы читаем метки: копия дальше пойдёт общей
/// дорогой, и держать диск дольше незачем.
#[cfg(target_os = "windows")]
fn probe_by_windows(volume_device: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let guard = match unlock_source_with_windows(volume_device, passphrase) {
        Ok(g) => g,
        Err(e) => {
            // Windows тоже не открыла. Это может быть и неверный пароль, и
            // отсутствие компонента BitLocker на носителе — точнее сказать
            // нечего, и «не подошёл» здесь честнее, чем молчание.
            info!(error = %e, "Windows не открыла том");
            return Ok(DecryptedProbe { ok: false, ..Default::default() });
        }
    };
    let probed = WinRawDevice::open_read(volume_device)
        .and_then(|mut dev| probe_decrypted(&mut dev));
    drop(guard);
    probed
}

/// Unlock the encrypted container at `device_path` with `passphrase` and probe
/// the decrypted filesystem — LUKS through the device itself, BitLocker through
/// the disk under it (see [`probe_bitlocker`]).
///
/// `ok = false` means the passphrase was rejected, and nothing else: an
/// unsupported cipher, an unreadable header or a KDF that could not get its
/// memory come back as errors, so the interface never answers "пароль не
/// подходит" to a question about something else. A source that is not an
/// encrypted container at all returns `ok = true` with empty info — there was
/// nothing to unlock.
pub fn unlock_and_probe(device_path: &str, passphrase: &str) -> Result<DecryptedProbe> {
    let src = normalize_device_path(device_path);
    let mut dev = WinRawDevice::open_read(&src)?;
    let mut header = vec![0u8; 4096];
    match dev.read_at(0, &mut header) {
        Ok(()) => {}
        // A locked BitLocker volume refuses every read through its own device.
        // Its ciphertext is on the DISK, which the FVE filter does not sit
        // above, so the passphrase is tried there instead.
        Err(e) if is_bitlocker_locked(&e) => return probe_bitlocker(&src, passphrase),
        Err(e) => return Err(e),
    }
    // An unlocked BitLocker volume reads as plain NTFS through its device —
    // Windows decrypts on the way — and needs nothing from us. A locked one
    // whose header we could still read (a disk moved here from elsewhere)
    // goes down the same path as above.
    if crate::bitlocker::is_bitlocker(&header) {
        return probe_bitlocker(&src, passphrase);
    }
    if !crate::luks::is_luks(&header) {
        return Ok(DecryptedProbe { ok: true, ..Default::default() });
    }
    let params = match crate::luks::open_luks(&mut dev, passphrase.as_bytes()) {
        Ok(p) => p,
        // No keyslot took it: a fact for the interface, not an error.
        Err(e) if crate::luks::is_wrong_passphrase(&e) => {
            info!(error = %e, "LUKS passphrase rejected");
            return Ok(DecryptedProbe { ok: false, ..Default::default() });
        }
        // Anything else — a cipher we do not implement, a header we could not
        // read, an Argon2 that could not get its memory — is NOT a wrong
        // password, and saying so would send the user off to re-type a password
        // that was right all along.
        Err(e) => return Err(e),
    };
    drop(dev);

    // Read the decrypted superblock region and identify the inner filesystem.
    let mut view =
        crate::decrypt::DecryptingVolume::new(Box::new(WinRawDevice::open_read(&src)?), params);
    probe_decrypted(&mut view)
}

/// Read an image's sidecar: embedded first, adjacent `.obkm` as a fallback
/// for images written before the sidecar moved inside the container.
pub fn load_sidecar_of(image_path: &Path) -> Result<Sidecar> {
    load_sidecar_with_keys(image_path, None)
}

/// The same, able to open an encrypted image. Its sidecar is sealed inside the
/// container, so there is nothing to read without the key — and saying which
/// key is missing beats failing with "unsupported container".
pub fn load_sidecar_with_keys(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    // One reader for the whole product; this name stays because the Windows
    // engine is full of callers of it.
    crate::sidecar::read_sidecar_with_keys(image_path, keys)
}

fn get_diskpart_virtual_disks() -> Vec<(u32, String)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut results = Vec::new();

    let mut cmd = match std::process::Command::new("cmd")
        .args(["/c", "chcp 65001 >nul && diskpart"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn diskpart command proxy");
            return results;
        }
    };

    if let Some(mut stdin) = cmd.stdin.take() {
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "list vdisk") {
            tracing::error!(error = %e, "failed to write to diskpart stdin");
        }
    }

    let mut byte_buf = Vec::new();
    if let Some(mut stdout) = cmd.stdout.take() {
        use std::io::Read;
        if let Err(e) = stdout.read_to_end(&mut byte_buf) {
            tracing::error!(error = %e, "failed to read from diskpart stdout");
        }
    }
    let output = String::from_utf8_lossy(&byte_buf).to_string();

    let mut stderr_output = String::new();
    if let Some(mut stderr) = cmd.stderr.take() {
        use std::io::Read;
        let _ = stderr.read_to_string(&mut stderr_output);
    }

    let _ = cmd.wait();

    for line in output.lines() {
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }

        let line_lower = line_trimmed.to_lowercase();
        if !line_lower.contains(".vhdx") && !line_lower.contains(".vhd") && !line_lower.contains(".avhdx") {
            continue;
        }

        let disk_num = (|| {
            let numeric_tokens: Vec<u32> = line_trimmed
                .split_whitespace()
                .filter_map(|t| t.parse::<u32>().ok())
                .collect();
            if numeric_tokens.len() >= 2 {
                Some(numeric_tokens[1])
            } else {
                None
            }
        })();

        let disk_num = match disk_num {
            Some(n) => n,
            None => continue,
        };

        let mut path_start = None;
        let chars: Vec<char> = line_trimmed.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == ':' && i > 0 && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i - 1);
                break;
            }
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                path_start = Some(i);
                break;
            }
        }

        if let Some(start) = path_start {
            let path = chars[start..].iter().collect::<String>().trim().to_string();
            results.push((disk_num, path));
        }
    }

    results
}

pub fn get_all_active_mounts(
    internal_mounts: &[own_backup_common::engine_dto::ActiveMountDto],
) -> Vec<own_backup_common::engine_dto::ActiveMountDto> {
    let mut result = internal_mounts.to_vec();

    for (disk_num, image_path) in get_diskpart_virtual_disks() {
        let device_path = format!(r"\\.\PhysicalDrive{disk_num}");

        let clean_item_path = image_path.replace('/', "\\");
        let exists = result.iter().any(|m| {
            let clean_m_path = m.image_path.replace('/', "\\");
            clean_m_path.eq_ignore_ascii_case(&clean_item_path)
        });

        if !exists {
            result.push(own_backup_common::engine_dto::ActiveMountDto {
                image_path,
                physical_path: device_path,
                read_write: false,
                volume_letters: Vec::new(),
            });
        }
    }

    // Resolve the drive letters Windows assigned to each mounted disk so the UI
    // can show "J:" instead of "PhysicalDrive5" / "Диск 5".
    for m in &mut result {
        if let Some(disk_num) = disk_number_from_physical(&m.physical_path) {
            m.volume_letters = crate::volume_win::drive_letters_on_disk(disk_num);
        }
    }

    result
}

/// Is this file a qcow2? By its magic, not by its extension: what a backup is
/// called is the user's business, what it is is the file's own.
fn is_qcow2_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == [0x51, 0x46, 0x49, 0xFB]
}

/// Parse `\\.\PhysicalDriveN` → `N`.
fn disk_number_from_physical(physical_path: &str) -> Option<u32> {
    physical_path
        .rsplit("PhysicalDrive")
        .next()
        .and_then(|n| n.trim().parse().ok())
}



