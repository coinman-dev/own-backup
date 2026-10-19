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

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::info;

use own_backup_common::gpt::DiskLayout;
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
pub struct ReadWindow {
    dev: LinuxRawDevice,
    offset: u64,
    size: u64,
}

impl ReadWindow {
    pub fn open(disk: &str, offset: u64, size: u64) -> Result<Self> {
        Ok(Self {
            dev: LinuxRawDevice::open_read(disk)?,
            offset,
            size,
        })
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
    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;

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
        "whole-disk restore: copying data"
    );

    let mut image =
        ImageReadSource::open_with(&req.image_path, &sidecar.info.container, chain_key.as_ref())?;
    let count = dm.layout.partitions.len();
    let total = sidecar.checksums.len() as u64;
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
        session.device_mut().write_at(offset, data)?;
        bytes += len as u64;
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
    session.finish(Some(&written.expected), UDEV_TIMEOUT, std::thread::sleep)?;
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
    })
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
