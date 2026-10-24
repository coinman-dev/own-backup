//! Writing one partition's data out of a copy, sector by sector, and reading
//! it back — the platform-independent half of a plan's `RestorePartition`.
//!
//! Which blocks, which slices and where they land is [`crate::restore_map`];
//! this is the loop around it: read each stored block, check it against its
//! checksum, write the slice that belongs to the partition, zero what no
//! block covered, and — for a container being put back — encrypt on the way.
//! The device is the caller's: Windows writes through a `part:` window with a
//! dismount guard that retries refused writes (`restore_win`), Linux through
//! the whole disk it holds exclusively (`restore_linux`).

use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use own_backup_common::cancel::CancelToken;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::engine_core::read::ImageReadSource;
use crate::fat_map::RawVolume;
use crate::restore_map::{check_fits, plan_block_copy, uncovered_ranges, SourceWindow};
use crate::sidecar::Sidecar;

/// Chunk used when zeroing the gaps between restored blocks.
const WIPE_CHUNK: usize = 4 * 1024 * 1024;

/// Where restored sectors go.
pub trait SectorTarget {
    /// Write `data` at `offset` from the partition's first byte.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    /// Logical sector size of the device.
    fn sector_size(&self) -> u32;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SectorRestoreOptions {
    /// Verify each block against the checksum stored in the sidecar.
    pub verify_checksums: bool,
    /// Zero the stretches no stored block covers (D16).
    pub wipe_free_space: bool,
    /// Windows: assume the caller has already taken the disk offline, so there
    /// is nothing to dismount. Skips the volume sweep, which is not free.
    /// Linux holds the whole disk exclusively and has nothing to dismount.
    pub disk_already_offline: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SectorRestoreProgress {
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone)]
pub struct SectorRestoreSummary {
    pub blocks_restored: u64,
    pub bytes_restored: u64,
    pub bytes_wiped: u64,
    pub duration: Duration,
    /// The user cancelled partway. The partition is left half-written and is
    /// not usable until restored again — there is no rollback.
    pub cancelled: bool,
}

/// Which stretch of the image belongs to source partition `src_index`.
///
/// For a whole-disk image the answer comes from the embedded disk manifest's
/// GPT. For a plain volume image the image *is* the volume, and `src_index` is
/// not consulted — there is only one thing it could mean.
pub fn source_window_for(sidecar: &Sidecar, src_index: u32) -> Result<SourceWindow> {
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return Ok(SourceWindow::whole(image_len));
    };
    let p = dm
        .layout
        .partitions
        .iter()
        .find(|p| p.index == src_index)
        .ok_or_else(|| {
            OwnBackupError::RestoreIncompatible(
                tr!(
                    "restore_win.no_such_partition",
                    index = src_index,
                    present = dm
                        .layout
                        .partitions
                        .iter()
                        .map(|p| p.index.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into_owned(),
            )
        })?;
    Ok(SourceWindow::partition(
        p.first_lba,
        p.last_lba,
        dm.layout.sector_size,
    ))
}

/// The chain key for an encrypted image, from the job/manual keys the caller
/// holds. `None` for every other container, which needs none.
///
/// Resolved per image because the wrapped chain key travels inside the image
/// itself — that is what lets a copy be opened on a machine that has never
/// seen the job.
pub(crate) fn chain_key_of(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Option<crate::seal::ChainKey>> {
    match keys {
        Some(k)
            if crate::sidecar::detect_container_format(image_path).ok()
                == Some(crate::seal::obk::OBK_EXTENSION) =>
        {
            Ok(Some(k.open_chain_of(image_path)?.key))
        }
        _ => Ok(None),
    }
}

/// The source window of `src_index`, refused when it does not fit a target
/// partition of `target_size` bytes. Asked before the device is opened.
pub fn window_fitting(sidecar: &Sidecar, src_index: u32, target_size: u64) -> Result<SourceWindow> {
    let window = source_window_for(sidecar, src_index)?;
    if target_size < window.len {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_win.target_too_small",
                target = target_size,
                source = window.len
            )
            .into_owned(),
        ));
    }
    Ok(window)
}

/// Copy one partition's stored blocks from `image_path` into `dev`, a target
/// partition of `target_size` bytes. `label` names the device in the log.
///
/// DESTRUCTIVE. The caller is responsible for the confirmation, for the
/// running-system guard and for holding the target.
// The image and its key, what to read out of it, where to put it, and how
// to report: bundling them into a struct would only move the same list
// somewhere the caller has to fill in by hand.
#[allow(clippy::too_many_arguments)]
pub fn write_partition_blocks(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    target_size: u64,
    dev: &mut dyn SectorTarget,
    label: &str,
    opts: SectorRestoreOptions,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let window = window_fitting(sidecar, src_index, target_size)?;

    if !block_size.is_multiple_of(dev.sector_size() as u64) {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_win.block_not_sector_multiple",
                block_size = block_size,
                sector = dev.sector_size()
            )
            .into_owned(),
        ));
    }
    let chain_key = chain_key_of(image_path, keys)?;
    let mut image =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;

    let total = sidecar.checksums.len() as u64;
    let mut blocks_restored = 0u64;
    let mut bytes_restored = 0u64;
    let mut covered: Vec<(u64, u64)> = Vec::new();
    let mut cancelled = false;

    for entry in &sidecar.checksums {
        // Checked at every block: the data copy is the one operation long
        // enough for cancellation to be worth anything (D18).
        if cancel.is_cancelled() {
            cancelled = true;
            warn!(
                blocks_restored,
                "restore cancelled — the partition is half-written and unusable until restored again"
            );
            break;
        }

        let Some(copy) = plan_block_copy(
            entry.block_number,
            block_size,
            image_len,
            data_offset,
            window,
        ) else {
            continue; // this block belongs to some other partition
        };
        check_fits(&copy, target_size)?;

        // The whole block is read even when only a slice is wanted: the stored
        // checksum was computed over the whole block, and half a block cannot
        // be checked against it.
        let mut buf = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut buf)?;

        if opts.verify_checksums {
            let actual = compute_block_checksum(&buf);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        dev.write_at(
            copy.dst_offset,
            &buf[copy.slice_start..copy.slice_start + copy.slice_len],
        )?;
        blocks_restored += 1;
        bytes_restored += copy.slice_len as u64;
        if opts.wipe_free_space {
            covered.push((copy.dst_offset, copy.slice_len as u64));
        }

        progress(&SectorRestoreProgress {
            blocks_done: blocks_restored,
            blocks_total: total,
            bytes_written: bytes_restored,
        })?;
    }

    let mut bytes_wiped = 0u64;
    if opts.wipe_free_space && !cancelled {
        bytes_wiped = wipe_gaps(dev, &mut covered, window.len, cancel)?;
    }

    dev.flush()?;
    drop(image);

    info!(
        image = %image_path.display(),
        target = %label,
        src_index,
        blocks = blocks_restored,
        bytes = bytes_restored,
        bytes_wiped,
        cancelled,
        secs = started.elapsed().as_secs(),
        "partition restored"
    );

    Ok(SectorRestoreSummary {
        blocks_restored,
        bytes_restored,
        bytes_wiped,
        duration: started.elapsed(),
        cancelled,
    })
}

/// Zero everything inside the volume that no stored block covered (D16).
///
/// Only the volume's own length is wiped, not the whole target partition: past
/// the volume's end lies space the filesystem does not know about yet, and on a
/// larger target that is exactly the space `ExtendFilesystem` is about to claim.
/// Writing zeroes there would be pointless work, sometimes a lot of it.
fn wipe_gaps(
    dev: &mut dyn SectorTarget,
    covered: &mut [(u64, u64)],
    volume_len: u64,
    cancel: &CancelToken,
) -> Result<u64> {
    let gaps = uncovered_ranges(covered, volume_len);
    let zeros = vec![0u8; WIPE_CHUNK];
    let mut wiped = 0u64;
    for (start, len) in gaps {
        let mut written = 0u64;
        while written < len {
            if cancel.is_cancelled() {
                debug!(wiped, "free-space wipe cancelled");
                return Ok(wiped);
            }
            let chunk = ((len - written) as usize).min(WIPE_CHUNK);
            dev.write_at(start + written, &zeros[..chunk])?;
            written += chunk as u64;
            wiped += chunk as u64;
        }
    }
    debug!(bytes = wiped, "free space between restored blocks zeroed");
    Ok(wiped)
}

/// How much of a slice of `slice_len` bytes at `payload_off` lies inside a
/// data area of `payload_len` bytes, in whole encryption sectors; `None` when
/// nothing does.
fn payload_slice(
    payload_off: u64,
    slice_len: usize,
    payload_len: u64,
    params: &crate::luks::LuksParams,
) -> Option<usize> {
    let room = payload_len.checked_sub(payload_off)?;
    let len = (slice_len as u64).min(room);
    let len = len - len % params.sector_size.max(1) as u64;
    (len > 0).then_some(len as usize)
}

/// [`wipe_gaps`] for a container being re-encrypted: the zeros have to be
/// *encrypted* zeros, or the plaintext there is noise.
///
/// This is not tidiness. A block whose every used cluster was excluded is
/// dropped from the used map and never stored, yet the filesystem it belongs to
/// still describes those sectors — on btrfs with the excluded file's checksums
/// rewritten to the checksum of zeros. Leave the target's old ciphertext in
/// place and the plaintext decrypts to noise, so the very first read of such a
/// file fails its checksum:
///
/// ```text
/// E: Ошибка чтения - read (5: Ошибка ввода/вывода)
/// ```
///
/// which is how a restored Kubuntu's apt met the .deb files it had excluded.
/// The plain sector path has wiped its gaps since D16; the encrypted path
/// simply never did, and only an encrypted target could show it.
fn wipe_gaps_encrypted(
    dev: &mut dyn SectorTarget,
    covered: &mut [(u64, u64)],
    payload_len: u64,
    params: &crate::luks::LuksParams,
    data_offset: u64,
    cancel: &CancelToken,
) -> Result<u64> {
    let gaps = uncovered_ranges(covered, payload_len);
    let mut wiped = 0u64;
    let mut buf = vec![0u8; WIPE_CHUNK];
    for (start, len) in gaps {
        let mut written = 0u64;
        while written < len {
            if cancel.is_cancelled() {
                debug!(wiped, "free-space wipe cancelled");
                return Ok(wiped);
            }
            let chunk = ((len - written) as usize).min(WIPE_CHUNK);
            let off = start + written;
            // The tweak depends on the offset, so every chunk is encrypted
            // where it lands — a cached ciphertext block would be wrong.
            buf[..chunk].fill(0);
            params.encrypt(off, &mut buf[..chunk])?;
            dev.write_at(data_offset + off, &buf[..chunk])?;
            written += chunk as u64;
            wiped += chunk as u64;
        }
    }
    debug!(
        bytes = wiped,
        "free space inside the container zeroed (encrypted)"
    );
    Ok(wiped)
}

// ---------------------------------------------------------------------------
// Verification after a sector restore (R9, D10)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct VerifySummary {
    pub blocks_checked: u64,
    pub bytes_checked: u64,
    /// Block numbers that did not match. Kept rather than counted: "block
    /// 41983 differs" is something a user can act on, "3 blocks differ" is not.
    pub mismatches: Vec<u64>,
    /// Blocks whose stored checksum did not match the image itself — the copy
    /// is damaged, and the restore faithfully wrote damaged data.
    pub image_corrupt: Vec<u64>,
    pub cancelled: bool,
}

impl VerifySummary {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty() && self.image_corrupt.is_empty() && !self.cancelled
    }
}

/// Read back what [`write_partition_blocks`] wrote into `dev` and check it.
///
/// Two different failures are worth telling apart, so both are checked:
///
/// - the block read from the *image* does not match its stored checksum → the
///   backup itself is damaged, and the restore dutifully wrote damaged data;
/// - the image block is fine but the *target* does not match it → the write did
///   not land (a failing disk, a driver that lied about flushing).
///
/// Comparing the target against the image directly, rather than recomputing a
/// checksum over it, is what makes a partial block checkable: only a slice of
/// the first and last block belongs to the partition, and a checksum over a
/// slice cannot be compared with one computed over the whole block.
///
/// `crypt` must be given for a container that was re-encrypted: the bytes on
/// the disk are then ciphertext sitting past the LUKS header, and comparing
/// them with the copy's plaintext reports every single block as wrong. That is
/// what it did — 12691 mismatches out of 12691 blocks on a partition whose
/// restored system booted perfectly.
#[allow(clippy::too_many_arguments)]
pub fn verify_partition_blocks(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    dev: &mut dyn RawVolume,
    label: &str,
    crypt: Option<&crate::luks::LuksParams>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<VerifySummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let window = source_window_for(sidecar, src_index)?;

    let chain_key = chain_key_of(image_path, keys)?;
    let mut image =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;

    let mut out = VerifySummary::default();
    let total = sidecar.checksums.len() as u64;

    for entry in &sidecar.checksums {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        let Some(copy) = plan_block_copy(
            entry.block_number,
            block_size,
            image_len,
            data_offset,
            window,
        ) else {
            continue;
        };

        let mut from_image = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut from_image)?;
        if compute_block_checksum(&from_image) != entry.checksum {
            out.image_corrupt.push(entry.block_number);
            continue; // no point comparing the target against a bad reference
        }

        let slice_len = match crypt {
            None => copy.slice_len,
            // Only what lies inside the data area was written
            // ([`write_container_blocks`]).
            Some(params) => {
                // The copy's data area, as `container_fitting` measured it
                // for the write — not the target's, which may be larger.
                let payload_len = crate::restore_map::payload_len(
                    window.len,
                    params.data_offset,
                    sidecar.disk_manifest.is_some(),
                )?;
                match payload_slice(copy.dst_offset, copy.slice_len, payload_len, params) {
                    Some(len) => len,
                    None => continue,
                }
            }
        };
        let mut from_disk = vec![0u8; slice_len];
        match crypt {
            None => dev.read_at(copy.dst_offset, &mut from_disk)?,
            Some(params) => {
                // Past the header, and encrypted: read where the restore wrote
                // and undo the encryption before comparing.
                let payload_off = copy.dst_offset;
                dev.read_at(params.data_offset + payload_off, &mut from_disk)?;
                params.decrypt(payload_off, &mut from_disk)?;
            }
        }
        if from_disk != from_image[copy.slice_start..copy.slice_start + slice_len] {
            out.mismatches.push(entry.block_number);
        }

        out.blocks_checked += 1;
        out.bytes_checked += slice_len as u64;
        progress(&SectorRestoreProgress {
            blocks_done: out.blocks_checked,
            blocks_total: total,
            bytes_written: out.bytes_checked,
        })?;
    }

    info!(
        target = %label,
        src_index,
        blocks = out.blocks_checked,
        mismatches = out.mismatches.len(),
        image_corrupt = out.image_corrupt.len(),
        cancelled = out.cancelled,
        secs = started.elapsed().as_secs(),
        "restore verified"
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// Putting the encrypted container back (D19)
// ---------------------------------------------------------------------------

/// The container to rebuild on a partition, and the passphrase that opens it.
pub struct ReencryptRequest<'a> {
    pub header: &'a own_backup_common::image::LuksHeaderBackup,
    /// Supplied by the user at restore time and never stored anywhere. The
    /// backup keeps the header — public data — not the key to it.
    pub passphrase: &'a str,
}

/// What a container restore needs to know before the device is opened: the
/// source window, the payload length, and the header bytes to write first.
pub struct ContainerFit {
    pub window: SourceWindow,
    pub payload_len: u64,
    pub header: Vec<u8>,
}

/// Refuse a container that does not fit `target_size`: the payload no longer
/// starts at the partition's first byte, the header takes the front.
pub fn container_fitting(
    sidecar: &Sidecar,
    src_index: u32,
    target_size: u64,
    req: &ReencryptRequest<'_>,
) -> Result<ContainerFit> {
    let window = source_window_for(sidecar, src_index)?;
    let header = crate::luks::decode_stored_header(req.header)?;
    let payload_len = crate::restore_map::payload_len(
        window.len,
        req.header.data_offset,
        sidecar.disk_manifest.is_some(),
    )?;
    let needed = req.header.data_offset + payload_len;
    if target_size < needed {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_win.target_too_small_for_container",
                target = target_size,
                header = req.header.data_offset,
                payload = payload_len
            )
            .into_owned(),
        ));
    }
    Ok(ContainerFit {
        window,
        payload_len,
        header,
    })
}

/// Restore a partition **with its encryption**: write the container's saved
/// header back, then re-encrypt the payload under the master key its own
/// keyslots yield (D19).
///
/// The result is byte-identical to the original container — same UUID, same
/// keyslots, same passphrase — so nothing on the restored system has to change.
/// That is the whole point: the alternative leaves a bare filesystem, and a
/// Linux install whose `crypttab` and initramfs wait forever for a device that
/// is not there.
///
/// The payload moves in two steps that must not be confused:
/// - the image holds the volume **decrypted**, at offsets relative to the
///   payload start;
/// - the target holds it **encrypted**, at those offsets plus `data_offset`,
///   with the XTS tweak derived from the payload-relative offset.
///
/// Mixing the two up produces a container that opens and decrypts to noise,
/// with no error raised anywhere — hence the offsets are named apart and the
/// tweak always comes from `payload_off`.
///
/// `read_back` opens the target for reading: the header is parsed off the
/// target, not off the blob just decoded, which proves it landed and is
/// readable where the system will look for it.
#[allow(clippy::too_many_arguments)]
pub fn write_container_blocks(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    fit: ContainerFit,
    dev: &mut dyn SectorTarget,
    read_back: &mut dyn FnMut() -> Result<Box<dyn RawVolume>>,
    label: &str,
    opts: SectorRestoreOptions,
    req: &ReencryptRequest<'_>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    write_container_blocks_with(
        image_path, keys, sidecar, src_index, fit, dev, read_back, label, opts, req, None, cancel,
        progress,
    )
}

/// [`write_container_blocks`] with the container's key already recovered
/// from the saved header (`known`, by [`crate::luks::open_luks_from_header`]
/// with the same passphrase): the header read back off the target is then
/// compared with what was written instead of being unlocked again — one key
/// derivation (seconds of Argon2) fewer, the same proof that it landed.
#[allow(clippy::too_many_arguments)]
pub fn write_container_blocks_with(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_index: u32,
    fit: ContainerFit,
    dev: &mut dyn SectorTarget,
    read_back: &mut dyn FnMut() -> Result<Box<dyn RawVolume>>,
    label: &str,
    opts: SectorRestoreOptions,
    req: &ReencryptRequest<'_>,
    known: Option<&crate::luks::LuksParams>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&SectorRestoreProgress) -> Result<()>,
) -> Result<SectorRestoreSummary> {
    let started = Instant::now();
    let block_size = sidecar.manifest.block_size as u64;
    let image_len = sidecar.manifest.source_disk.size_bytes;
    let data_offset = sidecar.info.volume_data_offset;
    let ContainerFit {
        window,
        payload_len,
        header,
    } = fit;
    // A key handed in must be this header's — its digest, data offset and
    // sector size — or the data would go under a key no keyslot here opens.
    if let Some(params) = known {
        if !crate::luks::key_belongs_to(&header, params)? {
            return Err(OwnBackupError::Device(
                tr!("sector_restore.key_not_this_header", target = label).into_owned(),
            ));
        }
    }

    dev.write_at(0, &header)?;
    dev.flush()?;
    info!(target = %label, bytes = header.len(), "LUKS header written back");

    let params = {
        let mut probe = read_back()?;
        match known {
            None => crate::luks::open_luks(probe.as_mut(), req.passphrase.as_bytes())?,
            Some(params) => {
                let mut landed = vec![0u8; header.len()];
                probe.read_at(0, &mut landed)?;
                if landed != header {
                    return Err(OwnBackupError::Device(
                        tr!("sector_restore.header_not_landed", target = label).into_owned(),
                    ));
                }
                params.clone()
            }
        }
    };
    if params.data_offset != req.header.data_offset {
        return Err(OwnBackupError::ImageFormat(
            tr!(
                "restore_win.header_offset_mismatch",
                actual = params.data_offset,
                expected = req.header.data_offset
            )
            .into_owned(),
        ));
    }

    let chain_key = chain_key_of(image_path, keys)?;
    let mut image =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;
    let total = sidecar.checksums.len() as u64;
    let mut covered: Vec<(u64, u64)> = Vec::new();
    let mut out = SectorRestoreSummary {
        blocks_restored: 0,
        bytes_restored: 0,
        bytes_wiped: 0,
        duration: Duration::ZERO,
        cancelled: false,
    };

    for entry in &sidecar.checksums {
        if cancel.is_cancelled() {
            out.cancelled = true;
            warn!(
                blocks = out.blocks_restored,
                "re-encrypting restore cancelled — the container is incomplete and will not open"
            );
            break;
        }
        let Some(copy) = plan_block_copy(
            entry.block_number,
            block_size,
            image_len,
            data_offset,
            window,
        ) else {
            continue;
        };
        // `dst_offset` is where the byte sits inside the *payload*; on disk it
        // lands that far past the header.
        let payload_off = copy.dst_offset;
        // A block that runs past the data area's end still carries its last
        // bytes: only the part beyond it is dropped (a whole-disk block grid
        // need not end where the payload does).
        let Some(slice_len) = payload_slice(payload_off, copy.slice_len, payload_len, &params)
        else {
            continue; // beyond the container's data area
        };

        let mut buf = vec![0u8; copy.block_len];
        image.read_at(copy.image_offset, &mut buf)?;
        if opts.verify_checksums {
            let actual = compute_block_checksum(&buf);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }

        let mut slice = buf[copy.slice_start..copy.slice_start + slice_len].to_vec();
        params.encrypt(payload_off, &mut slice)?;
        dev.write_at(req.header.data_offset + payload_off, &slice)?;

        out.blocks_restored += 1;
        out.bytes_restored += slice_len as u64;
        if opts.wipe_free_space {
            covered.push((payload_off, slice_len as u64));
        }
        progress(&SectorRestoreProgress {
            blocks_done: out.blocks_restored,
            blocks_total: total,
            bytes_written: out.bytes_restored,
        })?;
    }

    if opts.wipe_free_space && !out.cancelled {
        out.bytes_wiped = wipe_gaps_encrypted(
            dev,
            &mut covered,
            payload_len,
            &params,
            req.header.data_offset,
            cancel,
        )?;
    }

    dev.flush()?;
    drop(image);
    out.duration = started.elapsed();
    info!(
        target = %label,
        src_index,
        blocks = out.blocks_restored,
        bytes = out.bytes_restored,
        wiped = out.bytes_wiped,
        cancelled = out.cancelled,
        secs = out.duration.as_secs(),
        "partition restored with its encryption intact"
    );
    Ok(out)
}
