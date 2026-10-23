//! Writing a restore onto a whole disk under Linux (PLAN §6, stage 3).
//!
//! Every write of a Linux restore goes through the disk the session holds
//! (`disk_session_linux`: `O_EXCL` + the udev lock): a partition is a window
//! at its offset on that descriptor, not its own device node. That is what
//! keeps the kernel, udev and the desktop out from the first sector written to
//! the last one checked — opening `/dev/sdb2` by itself would need the disk to
//! be let go first, and a filesystem that is recognisable before it is fully
//! written gets mounted in that gap.
//!
//! Here: [`DiskWindow`] (one partition of the held disk, for the shared
//! `sector_restore` loops), [`ReadWindow`] (the same stretch through a second,
//! read-only descriptor, for re-reading a written LUKS header) and
//! [`restore_disk`], the whole-disk restore of a bootable copy.
//!
//! A LUKS partition the copy captured decrypted (its container opened — by
//! the engine with the passphrase, or by the kernel on the running system)
//! comes back as the container it was: its saved header is written at the
//! partition's start and the plaintext re-encrypted under the master key that
//! header's keyslots give for the container's own passphrase
//! (`sector_restore::write_container_blocks`, D19). Same UUID, same keyslots:
//! the restored system's crypttab and initramfs find it as before. Plaintext
//! is never written onto such a partition: without the passphrase — or
//! without a saved header, or for a container this program cannot rebuild —
//! the restore refuses before the first byte. The decrypted data stays
//! reachable by mounting the copy or by restoring the partition alone in the
//! plan designer, which says what it leaves.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::info;

use own_backup_common::engine_dto::LuksPassphraseNeedDto;
use own_backup_common::gpt::DiskLayout;
use own_backup_common::image::{LuksHeaderBackup, VolumeCaptureMode, VolumeEncryptionKind};
use own_backup_common::restore_plan::{
    PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec,
};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::disk_session_linux::{DiskIo, HostView, LinuxDiskSession, RealDisk, RealOpener};
use crate::engine_core::read::ImageReadSource;
use crate::fat_map::RawVolume;
use crate::linux_device::LinuxRawDevice;
use crate::sector_restore::SectorTarget;
use crate::sidecar::Sidecar;

pub use crate::disk_session_linux::UDEV_TIMEOUT;

/// A session on a real disk.
pub type DiskSession = LinuxDiskSession<RealDisk>;

/// Open the whole disk at `device` exclusively and take the udev lock.
pub fn open_disk(device: &str) -> Result<DiskSession> {
    LinuxDiskSession::open(&RealOpener, device, HostView::default())
}

/// One partition of a held disk: `offset`/`size` in bytes on the disk.
pub struct DiskWindow<'a, D: DiskIo> {
    io: &'a mut D,
    offset: u64,
    size: u64,
}

impl<'a, D: DiskIo> DiskWindow<'a, D> {
    pub fn new(io: &'a mut D, offset: u64, size: u64) -> Self {
        Self { io, offset, size }
    }

    fn check(&self, offset: u64, len: usize) -> Result<()> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(OwnBackupError::Device(
                tr!(
                    "restore_disk_linux.past_partition_end",
                    offset = offset,
                    len = len,
                    size = self.size
                )
                .into_owned(),
            )),
        }
    }
}

impl<D: DiskIo> SectorTarget for DiskWindow<'_, D> {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check(offset, data.len())?;
        self.io.write_at(self.offset + offset, data)
    }

    fn flush(&mut self) -> Result<()> {
        self.io.flush()
    }

    fn sector_size(&self) -> u32 {
        self.io.sector_size()
    }
}

impl<D: DiskIo> RawVolume for DiskWindow<'_, D> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        self.io.read_at(self.offset + offset, buf)
    }

    fn len(&self) -> u64 {
        self.size
    }
}

/// A partition read through its own read-only descriptor of the disk. Opening
/// without `O_EXCL` is allowed while the session holds the disk exclusively.
///
/// It exists to re-read what was just written, so opening it drops the disk's
/// cached pages first: the reads then come from the disk, not from the page
/// cache the writes filled.
pub struct ReadWindow {
    dev: LinuxRawDevice,
    offset: u64,
    size: u64,
}

impl ReadWindow {
    pub fn open(disk: &str, offset: u64, size: u64) -> Result<Self> {
        let dev = LinuxRawDevice::open_read(disk)?;
        dev.drop_cache()?;
        Ok(Self { dev, offset, size })
    }
}

impl RawVolume for ReadWindow {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(OwnBackupError::Device(
                tr!(
                    "restore_disk_linux.past_partition_end",
                    offset = offset,
                    len = buf.len(),
                    size = self.size
                )
                .into_owned(),
            ));
        }
        self.dev.read_at(self.offset + offset, buf)
    }

    fn len(&self) -> u64 {
        self.size
    }
}

/// A partition opened by itself (`O_EXCL` on its own node), for a disk the
/// running system stands on and so cannot be held whole.
impl SectorTarget for LinuxRawDevice {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        LinuxRawDevice::write_at(self, offset, data)
    }

    fn flush(&mut self) -> Result<()> {
        LinuxRawDevice::flush(self)
    }

    fn sector_size(&self) -> u32 {
        LinuxRawDevice::sector_size(self)
    }
}

/// The GPT a whole-disk copy captured, as a table to write: the source's own
/// disk and partition GUIDs, types, attributes and names, on a disk of
/// `disk_size` bytes.
pub fn spec_from_layout(layout: &DiskLayout, disk_size: u64) -> PartitionTableSpec {
    PartitionTableSpec {
        scheme: PartitionScheme::Gpt,
        sector_size: layout.sector_size,
        disk_size_bytes: disk_size,
        disk_id: layout.disk_guid.clone(),
        entries: layout
            .partitions
            .iter()
            .map(|p| PartitionSpec {
                index: p.index,
                first_lba: p.first_lba,
                last_lba: p.last_lba,
                kind: PartitionKind::Gpt {
                    type_guid: p.type_guid.clone(),
                    part_guid: p.part_guid.clone(),
                    attributes: p.attributes,
                    name: p.name.clone(),
                },
            })
            .collect(),
    }
}

/// Recreate a whole disk from a bootable copy. DESTRUCTIVE.
pub struct LinuxDiskRestoreRequest {
    /// The single whole-disk image (`.vhdx`, `.qcow2`, `.obk`).
    pub image_path: PathBuf,
    /// The target disk (`/dev/disk/by-id/…`, `/dev/sdb`).
    pub target_device: String,
    pub verify_checksums: bool,
    /// Keys of an encrypted copy, from what the person typed.
    pub encryption: Option<crate::seal::context::ArchiveKeys>,
    /// The passphrases of the LUKS containers the copy holds decrypted, by
    /// SOURCE partition index ([`containers_to_rebuild`]): each opens its
    /// container's saved header, whose master key re-encrypts the data. The
    /// containers' own secrets, typed for this restore and never stored — a
    /// different thing from the archive password of an encrypted copy.
    pub passphrases: HashMap<u32, String>,
}

/// A LUKS container the copy holds decrypted, which the restore puts back.
#[derive(Debug, Clone)]
pub struct Container {
    /// Its partition: the index in the copy's table, which the restore
    /// writes as it is — on the source and on the target alike.
    pub part_index: u32,
    /// Its slot on the disk, in bytes.
    pub offset: u64,
    pub size: u64,
    /// The header area the copy kept.
    pub header: LuksHeaderBackup,
}

/// The LUKS containers a whole-disk restore of `sidecar`'s copy has to put
/// back: every partition whose header the copy kept. Refused — before
/// anything is written — when a partition was captured decrypted from a
/// LUKS container whose header the copy does not have (plaintext would land
/// where the restored system waits for a container), or when the container is
/// one this program cannot rebuild (a cipher it does not implement,
/// authenticated encryption, a reencryption under way, no passphrase slot).
pub fn containers_to_rebuild(sidecar: &Sidecar) -> Result<Vec<Container>> {
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return Ok(Vec::new());
    };
    let sector = dm.layout.sector_size.max(1) as u64;
    let mut out = Vec::new();
    for p in &dm.layout.partitions {
        let header = sidecar
            .info
            .luks_headers
            .iter()
            .find(|h| h.part_index == p.index);
        let decrypted_luks = dm
            .images
            .iter()
            .find(|i| i.part_index == p.index)
            .and_then(|i| i.encryption.as_ref())
            .is_some_and(|e| {
                matches!(
                    e.kind,
                    VolumeEncryptionKind::Luks1 | VolumeEncryptionKind::Luks2
                ) && e.capture_mode == VolumeCaptureMode::Plaintext
            });
        match header {
            Some(h) => {
                let bytes = crate::luks::decode_stored_header(h)?;
                let why = crate::luks::restore_obstacles(&bytes)?;
                if !why.is_empty() {
                    return Err(OwnBackupError::RestoreIncompatible(
                        tr!(
                            "restore_disk_linux.luks_cannot_rebuild",
                            index = p.index,
                            why = why.join("; ")
                        )
                        .into_owned(),
                    ));
                }
                out.push(Container {
                    part_index: p.index,
                    offset: p.first_lba * sector,
                    size: (p.last_lba + 1 - p.first_lba) * sector,
                    header: h.clone(),
                });
            }
            None if decrypted_luks => {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("restore_disk_linux.luks_no_header", index = p.index).into_owned(),
                ))
            }
            None => {}
        }
    }
    Ok(out)
}

/// The containers whose passphrase is missing from `passphrases` or wrong.
/// A passphrase is wrong only when no keyslot accepted it (the digest check
/// failed); any other failure is the error it is.
pub fn passphrase_needs(
    containers: &[Container],
    passphrases: &HashMap<u32, String>,
) -> Result<Vec<LuksPassphraseNeedDto>> {
    let mut out = Vec::new();
    for c in containers {
        let wrong = match passphrases.get(&c.part_index) {
            None => false,
            Some(p) if crate::luks::check_stored_passphrase(&c.header, p)? => continue,
            Some(_) => true,
        };
        out.push(LuksPassphraseNeedDto {
            part_index: c.part_index,
            kind: c.header.kind.clone(),
            wrong,
        });
    }
    Ok(out)
}

/// What `needs` asks for, in words.
pub fn describe_needs(needs: &[LuksPassphraseNeedDto]) -> String {
    let list: Vec<String> = needs
        .iter()
        .map(|n| {
            if n.wrong {
                tr!(
                    "restore_disk_linux.luks_passphrase_wrong",
                    index = n.part_index
                )
            } else {
                tr!(
                    "restore_disk_linux.luks_passphrase_missing",
                    index = n.part_index
                )
            }
            .into_owned()
        })
        .collect();
    tr!(
        "restore_disk_linux.luks_passphrases_needed",
        list = list.join(", ")
    )
    .into_owned()
}

/// The parts of the stretch `[start, start + len)` outside every range
/// `[a, b)` of `skip`, as `(offset, len)` in order.
fn outside(start: u64, len: u64, skip: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut pieces = vec![(start, start + len)];
    for &(a, b) in skip {
        let mut next = Vec::with_capacity(pieces.len() + 1);
        for (s, e) in pieces {
            if b <= s || a >= e {
                next.push((s, e));
                continue;
            }
            if s < a {
                next.push((s, a));
            }
            if b < e {
                next.push((b, e));
            }
        }
        pieces = next;
    }
    pieces.sort_unstable();
    pieces.into_iter().map(|(s, e)| (s, e - s)).collect()
}

/// Progress of a whole-disk restore, in the shape the Windows engine reports.
pub struct DiskRestoreProgress {
    /// "partition" while copying data, "gpt" while writing the table.
    pub phase: String,
    pub partition_index: usize,
    pub partition_count: usize,
    pub part_label: String,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
}

pub struct DiskRestoreSummary {
    pub partitions: usize,
    pub bytes_written: u64,
    pub duration: Duration,
    /// Said, not raised: everything is written by the time these arise.
    pub warnings: Vec<String>,
    /// Partitions put back as the LUKS containers they were.
    pub containers: Vec<u32>,
}

/// Copy every stored block of the image to the same offset on the target,
/// then write the copy's REAL GPT (its own disk and partition GUIDs — the
/// image carries random ones) through the session, which tells the kernel and
/// waits for udev. The running-system guard and the identity check are the
/// caller's.
pub fn restore_disk(
    req: &LinuxDiskRestoreRequest,
    mut progress: impl FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<DiskRestoreSummary> {
    let started = Instant::now();
    let ext = req
        .image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") && ext != crate::seal::obk::OBK_EXTENSION
    {
        // The `.obdm` set of per-volume images predates the single image;
        // Linux never made one.
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_disk_linux.legacy_manifest").into_owned(),
        ));
    }
    let chain_key = match &req.encryption {
        Some(keys) => Some(keys.open_chain_of(&req.image_path)?.key),
        None => None,
    };
    let sidecar = match &chain_key {
        Some(key) => crate::sidecar::read_sealed_sidecar(&req.image_path, key)?,
        None => crate::sidecar::read_embedded_sidecar(&req.image_path, &ext)?,
    };
    let dm = sidecar.disk_manifest.clone().ok_or_else(|| {
        OwnBackupError::ImageFormat(tr!("restore_disk_linux.not_a_disk_image").into_owned())
    })?;
    // Every container and its passphrase are settled before the disk is
    // touched: a wrong one found at the container's turn would leave the
    // disk half written.
    let containers = containers_to_rebuild(&sidecar)?;
    let needs = passphrase_needs(&containers, &req.passphrases)?;
    if !needs.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(describe_needs(&needs)));
    }

    let mut session = open_disk(&req.target_device)?;
    let target_size = session.length();
    // Validates every partition fits the target, and refuses a smaller disk.
    crate::gpt_writer::disk_gpt_write_ops(&dm.layout, target_size)?;
    if session.sector_size() != dm.layout.sector_size {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "disk_session_linux.sector_mismatch",
                planned = dm.layout.sector_size,
                disk = req.target_device.as_str(),
                actual = session.sector_size()
            )
            .into_owned(),
        ));
    }
    info!(
        target = %req.target_device,
        model = %dm.disk_model,
        partitions = dm.layout.partitions.len(),
        containers = containers.len(),
        "whole-disk restore: copying data"
    );

    let count = dm.layout.partitions.len();
    let total = sidecar.checksums.len() as u64;
    let target = req.target_device.clone();
    let bytes = write_disk_data(
        req,
        &sidecar,
        chain_key.as_ref(),
        &containers,
        session.device_mut(),
        &mut |offset, size| {
            ReadWindow::open(&target, offset, size).map(|w| Box::new(w) as Box<dyn RawVolume>)
        },
        &mut progress,
    )?;

    progress(&DiskRestoreProgress {
        phase: "gpt".into(),
        partition_index: count,
        partition_count: count,
        part_label: tr!("engine_win.label_partition_table").into_owned(),
        blocks_done: total,
        blocks_total: total,
        bytes_written: bytes,
    })?;
    let written = session.write_partition_table(&spec_from_layout(&dm.layout, target_size))?;
    let mut warnings = decrypted_bitlocker(&sidecar)
        .into_iter()
        .map(|index| tr!("restore_disk_linux.bitlocker_decrypted", index = index).into_owned())
        .collect::<Vec<_>>();
    if let Err(e) = session.finish(Some(&written.expected), UDEV_TIMEOUT, std::thread::sleep) {
        tracing::warn!(error = %e, "the disk is written, but udev did not settle on it");
        warnings.push(tr!("restore_disk_linux.udev_not_settled", error = e).into_owned());
    }
    info!(
        target = %req.target_device,
        partitions = count,
        bytes,
        secs = started.elapsed().as_secs(),
        kernel = ?written.kernel,
        "whole-disk restore complete"
    );
    Ok(DiskRestoreSummary {
        partitions: count,
        bytes_written: bytes,
        duration: started.elapsed(),
        warnings,
        containers: containers.iter().map(|c| c.part_index).collect(),
    })
}

/// Partitions the copy holds decrypted from BitLocker: Linux has no way to
/// put that encryption back, and the restored volume is in the clear.
fn decrypted_bitlocker(sidecar: &Sidecar) -> Vec<u32> {
    sidecar
        .disk_manifest
        .iter()
        .flat_map(|dm| &dm.images)
        .filter(|i| {
            i.encryption.as_ref().is_some_and(|e| {
                e.kind == VolumeEncryptionKind::BitLocker
                    && e.capture_mode == VolumeCaptureMode::Plaintext
            })
        })
        .map(|i| i.part_index)
        .collect()
}

/// Write the copy's data onto the disk `io`: every stored block at its own
/// offset, except inside the slots of `containers`, which are put back as
/// the containers they were — header first, then the data re-encrypted
/// (`sector_restore::write_container_blocks`). `read_back(offset, size)`
/// reads a slot from the disk itself, past every cache. Bytes written.
#[allow(clippy::too_many_arguments)]
fn write_disk_data<D: DiskIo>(
    req: &LinuxDiskRestoreRequest,
    sidecar: &Sidecar,
    chain_key: Option<&crate::seal::ChainKey>,
    containers: &[Container],
    io: &mut D,
    read_back: &mut dyn FnMut(u64, u64) -> Result<Box<dyn RawVolume>>,
    progress: &mut dyn FnMut(&DiskRestoreProgress) -> Result<()>,
) -> Result<u64> {
    use crate::sector_restore::{
        container_fitting, write_container_blocks, ReencryptRequest, SectorRestoreOptions,
    };

    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;
    let target_size = io.length();
    let count = sidecar
        .disk_manifest
        .as_ref()
        .map_or(0, |dm| dm.layout.partitions.len());
    let total = sidecar.checksums.len() as u64;
    let skip: Vec<(u64, u64)> = containers
        .iter()
        .map(|c| (c.offset, c.offset + c.size))
        .collect();

    let mut image =
        ImageReadSource::open_with(&req.image_path, &sidecar.info.container, chain_key)?;
    let mut bytes = 0u64;
    let mut buf = vec![0u8; bs as usize];
    for (n, entry) in sidecar.checksums.iter().enumerate() {
        let offset = entry.block_number * bs;
        let len = bs.min(image_size - offset) as usize;
        if offset + len as u64 > target_size {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!(
                    "restore_disk_linux.block_past_disk",
                    offset = offset,
                    size = target_size
                )
                .into_owned(),
            ));
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
        // The decrypted data of a container never lands in the clear.
        for (at, piece) in outside(offset, len as u64, &skip) {
            let from = (at - offset) as usize;
            io.write_at(at, &data[from..from + piece as usize])?;
            bytes += piece;
        }
        progress(&DiskRestoreProgress {
            phase: "partition".into(),
            partition_index: 0,
            partition_count: count,
            part_label: tr!("engine_win.label_partition_data").into_owned(),
            blocks_done: n as u64 + 1,
            blocks_total: total,
            bytes_written: bytes,
        })?;
    }
    drop(image);

    // Never set: cancelling is the progress callback's, which every loop
    // asks after each block.
    let cancel = own_backup_common::cancel::CancelToken::new(uuid::Uuid::new_v4());
    for (n, c) in containers.iter().enumerate() {
        let passphrase = req.passphrases.get(&c.part_index).ok_or_else(|| {
            OwnBackupError::RestoreIncompatible(describe_needs(&[LuksPassphraseNeedDto {
                part_index: c.part_index,
                kind: c.header.kind.clone(),
                wrong: false,
            }]))
        })?;
        let creq = ReencryptRequest {
            header: &c.header,
            passphrase,
        };
        let fit = container_fitting(sidecar, c.part_index, c.size, &creq)?;
        let label = format!("{}:{}:{}", req.target_device, c.offset, c.size);
        let part_label =
            tr!("restore_disk_linux.label_container", index = c.part_index).into_owned();
        let (offset, size) = (c.offset, c.size);
        let base = bytes;
        let mut window = DiskWindow::new(io, offset, size);
        let summary = write_container_blocks(
            &req.image_path,
            req.encryption.as_ref(),
            sidecar,
            c.part_index,
            fit,
            &mut window,
            &mut || read_back(offset, size),
            &label,
            SectorRestoreOptions {
                verify_checksums: req.verify_checksums,
                wipe_free_space: false,
                disk_already_offline: true,
            },
            &creq,
            &cancel,
            &mut |pr| {
                progress(&DiskRestoreProgress {
                    phase: "partition".into(),
                    partition_index: n,
                    partition_count: count,
                    part_label: part_label.clone(),
                    blocks_done: pr.blocks_done,
                    blocks_total: pr.blocks_total,
                    bytes_written: base + pr.bytes_written,
                })
            },
        )?;
        bytes += summary.bytes_restored;
        info!(
            part = c.part_index,
            bytes = summary.bytes_restored,
            "LUKS container put back from its saved header"
        );
    }
    Ok(bytes)
}

/// The device node of partition `number` of the disk at `device`, from sysfs
/// (`/dev/sdb2`, `/dev/nvme0n1p2`). `None` when the kernel shows no such
/// partition.
pub fn partition_node(device: &str, number: u32) -> Option<String> {
    use own_backup_common::linux_block as lb;
    let sys = std::path::Path::new(lb::SYS_CLASS_BLOCK);
    let disk = lb::kernel_name(std::path::Path::new(device))?;
    lb::partitions_of(sys, &disk)
        .into_iter()
        .find(|p| {
            std::fs::read_to_string(sys.join(p).join("partition"))
                .ok()
                .and_then(|n| n.trim().parse::<u32>().ok())
                == Some(number)
        })
        .map(|p| format!("/dev/{p}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::gpt::GptPartitionEntry;

    struct Mem {
        data: Vec<u8>,
    }

    impl DiskIo for Mem {
        fn path(&self) -> &str {
            "/dev/mem"
        }
        fn length(&self) -> u64 {
            self.data.len() as u64
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let o = offset as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            let o = offset as usize;
            self.data[o..o + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn drop_cache(&mut self) -> Result<()> {
            Ok(())
        }
        fn reread_partitions(
            &mut self,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn delete_partition(
            &mut self,
            _: u32,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn add_partition(
            &mut self,
            _: &crate::disk_session_linux::KernelPartition,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn kernel_name(&self) -> &str {
            "mem"
        }
    }

    #[test]
    fn a_window_writes_at_its_offset_and_never_past_its_end() {
        let mut disk = Mem {
            data: vec![0u8; 8192],
        };
        {
            let mut w = DiskWindow::new(&mut disk, 1024, 2048);
            SectorTarget::write_at(&mut w, 0, &[7u8; 512]).unwrap();
            SectorTarget::write_at(&mut w, 1536, &[9u8; 512]).unwrap();
            assert!(SectorTarget::write_at(&mut w, 1537, &[1u8; 512]).is_err());
            let mut back = [0u8; 512];
            RawVolume::read_at(&mut w, 1536, &mut back).unwrap();
            assert_eq!(back, [9u8; 512]);
            assert!(RawVolume::read_at(&mut w, 2048, &mut back).is_err());
        }
        assert!(disk.data[..1024].iter().all(|b| *b == 0));
        assert!(disk.data[1024..1536].iter().all(|b| *b == 7));
        assert!(disk.data[2560..3072].iter().all(|b| *b == 9));
        assert!(disk.data[3072..].iter().all(|b| *b == 0));
    }

    #[test]
    fn only_what_lies_outside_the_containers_is_written_as_it_is() {
        let skip = [(100, 200), (300, 400)];
        assert_eq!(outside(0, 50, &skip), vec![(0, 50)]);
        assert_eq!(outside(50, 100, &skip), vec![(50, 50)]);
        assert_eq!(outside(150, 30, &skip), vec![]);
        assert_eq!(outside(150, 200, &skip), vec![(200, 100)]);
        assert_eq!(
            outside(0, 500, &skip),
            vec![(0, 100), (200, 100), (400, 100)]
        );
        assert_eq!(outside(0, 500, &[]), vec![(0, 500)]);
    }

    /// A disk the test writes to and the restore reads back from.
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl DiskIo for Shared {
        fn path(&self) -> &str {
            "/dev/mem"
        }
        fn length(&self) -> u64 {
            self.0.borrow().len() as u64
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let o = offset as usize;
            buf.copy_from_slice(&self.0.borrow()[o..o + buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            let o = offset as usize;
            self.0.borrow_mut()[o..o + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn drop_cache(&mut self) -> Result<()> {
            Ok(())
        }
        fn reread_partitions(
            &mut self,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn delete_partition(
            &mut self,
            _: u32,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn add_partition(
            &mut self,
            _: &crate::disk_session_linux::KernelPartition,
        ) -> std::result::Result<(), crate::disk_session_linux::IoctlError> {
            Ok(())
        }
        fn kernel_name(&self) -> &str {
            "mem"
        }
    }

    struct Bytes(Vec<u8>);

    impl RawVolume for Bytes {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let o = offset as usize;
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn request(image: &std::path::Path, pass: Option<&str>) -> LinuxDiskRestoreRequest {
        LinuxDiskRestoreRequest {
            image_path: image.to_path_buf(),
            target_device: "/dev/mem".into(),
            verify_checksums: true,
            encryption: None,
            passphrases: pass.map(|p| (2u32, p.to_string())).into_iter().collect(),
        }
    }

    /// Restore `image` onto a zeroed disk of `size` bytes.
    fn restore_onto(image: &std::path::Path, size: u64, pass: &str) -> Vec<u8> {
        let sidecar = crate::engine_core::read::load_sidecar_of(image).unwrap();
        let containers = containers_to_rebuild(&sidecar).unwrap();
        let disk = std::rc::Rc::new(std::cell::RefCell::new(vec![0u8; size as usize]));
        let mut io = Shared(disk.clone());
        let reader = disk.clone();
        write_disk_data(
            &request(image, Some(pass)),
            &sidecar,
            None,
            &containers,
            &mut io,
            &mut |offset, size| {
                let b = reader.borrow();
                Ok(Box::new(Bytes(
                    b[offset as usize..(offset + size) as usize].to_vec(),
                )))
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        let out = disk.borrow().clone();
        out
    }

    /// A container the copy holds decrypted comes back byte for byte — its
    /// header, and its data under the key that header gives — and its
    /// plaintext never lands on the disk; the passphrase is settled first.
    #[test]
    fn a_decrypted_container_is_put_back_as_it_was() {
        use crate::engine_core::disk_luks::fixture::*;
        let dir = tempfile::tempdir().unwrap();
        let disk = LuksDisk::new();
        let image = dir.path().join("disk.qcow2");
        disk.backup(&image);
        let sidecar = crate::engine_core::read::load_sidecar_of(&image).unwrap();

        let containers = containers_to_rebuild(&sidecar).unwrap();
        assert_eq!(containers.len(), 1);
        assert_eq!(
            (
                containers[0].part_index,
                containers[0].offset,
                containers[0].size
            ),
            (2, PART_AT, disk.container.len() as u64)
        );
        let none = passphrase_needs(&containers, &HashMap::new()).unwrap();
        assert_eq!(
            none,
            vec![LuksPassphraseNeedDto {
                part_index: 2,
                kind: VolumeEncryptionKind::Luks1,
                wrong: false
            }]
        );
        let wrong =
            passphrase_needs(&containers, &request(&image, Some("nope")).passphrases).unwrap();
        assert!(wrong[0].wrong);
        assert!(describe_needs(&wrong).contains('2'));
        assert!(
            passphrase_needs(&containers, &request(&image, Some(PASS)).passphrases)
                .unwrap()
                .is_empty()
        );

        let out = restore_onto(&image, disk.layout.size_bytes, PASS);
        let part = &out[PART_AT as usize..PART_AT as usize + disk.container.len()];
        assert!(
            part == disk.container.as_slice(),
            "the container, byte for byte"
        );
        let esp = &out[ESP_AT as usize..ESP_AT as usize + disk.esp_data.len()];
        assert!(esp == disk.esp_data.as_slice());
        // The data area opens with the passphrase and reads as the source's.
        let mut vol = Bytes(part.to_vec());
        let params = crate::luks::open_luks(&mut vol, PASS.as_bytes()).unwrap();
        let mut back = part[HEADER as usize..].to_vec();
        params.decrypt(0, &mut back).unwrap();
        assert!(back == disk.plain);
        // The read-back check of a plan restore agrees, the last block —
        // which runs past the data area — included.
        let cancel = own_backup_common::cancel::CancelToken::new(uuid::Uuid::new_v4());
        let v = crate::sector_restore::verify_partition_blocks(
            &image,
            None,
            &sidecar,
            2,
            &mut Bytes(part.to_vec()),
            "test",
            Some(&params),
            &cancel,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(v.ok() && v.blocks_checked > 0, "{:?}", v.mismatches);
    }

    /// Plaintext is never written where a container was: a partition
    /// captured decrypted without its header, or one this program cannot
    /// rebuild, stops the restore before the disk is touched.
    #[test]
    fn a_container_that_cannot_be_put_back_stops_the_restore() {
        use crate::engine_core::disk_luks::fixture::*;
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let disk = LuksDisk::new();
        let image = dir.path().join("disk.qcow2");
        disk.backup(&image);
        let load = || crate::engine_core::read::load_sidecar_of(&image).unwrap();

        let mut headless = load();
        headless.info.luks_headers.clear();
        let e = containers_to_rebuild(&headless).unwrap_err();
        assert!(e.to_string().contains('2'), "{e}");

        let mut serpent = load();
        let h = &mut serpent.info.luks_headers[0];
        let mut bytes = crate::luks::decode_stored_header(h).unwrap();
        bytes[8..15].copy_from_slice(b"serpent");
        h.header_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let e = containers_to_rebuild(&serpent).unwrap_err();
        assert!(e.to_string().contains("serpent"), "{e}");
    }

    #[test]
    fn the_captured_layout_becomes_a_table_with_the_source_guids() {
        let layout = DiskLayout {
            disk_guid: "11111111-2222-3333-4444-555555555555".into(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![GptPartitionEntry {
                index: 1,
                type_guid: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(),
                part_guid: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into(),
                first_lba: 2048,
                last_lba: 206847,
                attributes: 1,
                name: "EFI system partition".into(),
            }],
        };
        let spec = spec_from_layout(&layout, 2 << 30);
        assert_eq!(spec.disk_size_bytes, 2 << 30);
        assert_eq!(spec.disk_id, layout.disk_guid);
        assert_eq!(spec.entries.len(), 1);
        match &spec.entries[0].kind {
            PartitionKind::Gpt {
                part_guid,
                attributes,
                name,
                ..
            } => {
                assert_eq!(part_guid, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
                assert_eq!(*attributes, 1);
                assert_eq!(name, "EFI system partition");
            }
            other => panic!("not GPT: {other:?}"),
        }
    }
}
