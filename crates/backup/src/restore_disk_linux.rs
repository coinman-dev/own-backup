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
use zeroize::Zeroizing;

use own_backup_common::gpt::DiskLayout;
use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionKind};
use own_backup_common::restore_plan::{
    PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec,
};
use own_backup_common::{tr, OwnBackupError, Result};

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
    pub passphrases: HashMap<u32, Zeroizing<String>>,
    /// Keys an earlier check of the same passphrases recovered (the
    /// service's), by source partition: taken when they belong to the
    /// container's header, instead of deriving them again.
    pub known_keys: HashMap<u32, crate::luks::LuksParams>,
}

pub use crate::luks_rebuild::{
    check_passphrases, containers_to_rebuild, describe_needs, passphrase_needs, Container,
};

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
    let checked = crate::luks_rebuild::check_passphrases_with(
        &containers,
        &req.passphrases,
        &req.known_keys,
    )?;
    if !checked.needs.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(describe_needs(
            &checked.needs,
        )));
    }
    let fits = crate::luks_rebuild::fit_containers(&sidecar, &containers, &checked.keys)?;
    // The image opens — before the disk is held.
    drop(ImageReadSource::open_with(
        &req.image_path,
        &sidecar.info.container,
        chain_key.as_ref(),
    )?);

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
    let target = req.target_device.clone();
    let written = crate::luks_rebuild::write_disk_data(
        &crate::luks_rebuild::DiskData {
            image_path: &req.image_path,
            keys: req.encryption.as_ref(),
            chain_key: chain_key.as_ref(),
            verify_checksums: req.verify_checksums,
            passphrases: &req.passphrases,
            label: &req.target_device,
            target_size,
        },
        &sidecar,
        &containers,
        fits,
        &checked.keys,
        &mut DiskWindow::new(session.device_mut(), 0, target_size),
        &mut |offset, size| {
            ReadWindow::open(&target, offset, size).map(|w| Box::new(w) as Box<dyn RawVolume>)
        },
        &mut |t| {
            progress(&DiskRestoreProgress {
                phase: "partition".into(),
                partition_index: t.partition_index,
                partition_count: count,
                part_label: t.part_label.clone(),
                blocks_done: t.blocks_done,
                blocks_total: t.blocks_total,
                bytes_written: t.bytes_written,
            })
        },
    )?;
    let (bytes, total) = (written.bytes, written.blocks_total);

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
    use own_backup_common::engine_dto::LuksPassphraseNeedDto;
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
        assert_eq!(crate::luks_rebuild::outside(0, 50, &skip), vec![(0, 50)]);
        assert_eq!(crate::luks_rebuild::outside(50, 100, &skip), vec![(50, 50)]);
        assert_eq!(crate::luks_rebuild::outside(150, 30, &skip), vec![]);
        assert_eq!(
            crate::luks_rebuild::outside(150, 200, &skip),
            vec![(200, 100)]
        );
        assert_eq!(
            crate::luks_rebuild::outside(0, 500, &skip),
            vec![(0, 100), (200, 100), (400, 100)]
        );
        assert_eq!(crate::luks_rebuild::outside(0, 500, &[]), vec![(0, 500)]);
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
            passphrases: pass
                .map(|p| (2u32, Zeroizing::new(p.to_string())))
                .into_iter()
                .collect(),
            known_keys: HashMap::new(),
        }
    }

    /// Restore `image` onto a zeroed disk of `size` bytes; with `known`, the
    /// keys are taken as handed on by an earlier check (no derivation).
    fn restore_onto(image: &std::path::Path, size: u64, pass: &str, known: bool) -> Vec<u8> {
        restore_sealed_onto(image, None, size, pass, known)
    }

    /// [`restore_onto`] for a copy sealed under `sealed` (an `.obk`).
    fn restore_sealed_onto(
        image: &std::path::Path,
        sealed: Option<&crate::seal::context::ArchiveKeys>,
        size: u64,
        pass: &str,
        known: bool,
    ) -> Vec<u8> {
        let sidecar = crate::sidecar::read_sidecar_with_keys(image, sealed).unwrap();
        let chain_key = sealed.map(|k| k.open_chain_of(image).unwrap().key);
        let containers = containers_to_rebuild(&sidecar).unwrap();
        let req = request(image, Some(pass));
        let checked = check_passphrases(&containers, &req.passphrases)
            .unwrap()
            .keys;
        let keys = if known {
            // As the service hands them on: taken for belonging to the
            // header, with no passphrase at hand to derive them again.
            let again =
                crate::luks_rebuild::check_passphrases_with(&containers, &HashMap::new(), &checked)
                    .unwrap();
            assert!(again.needs.is_empty());
            again.keys
        } else {
            checked
        };
        let fits = crate::luks_rebuild::fit_containers(&sidecar, &containers, &keys).unwrap();
        let disk = std::rc::Rc::new(std::cell::RefCell::new(vec![0u8; size as usize]));
        let mut io = Shared(disk.clone());
        let reader = disk.clone();
        let mut seen: Vec<(u64, u64)> = Vec::new();
        crate::luks_rebuild::write_disk_data(
            &crate::luks_rebuild::DiskData {
                image_path: &req.image_path,
                keys: sealed,
                chain_key: chain_key.as_ref(),
                verify_checksums: true,
                passphrases: &req.passphrases,
                label: "/dev/mem",
                target_size: size,
            },
            &sidecar,
            &containers,
            fits,
            &keys,
            &mut DiskWindow::new(&mut io, 0, size),
            &mut |offset, size| {
                let b = reader.borrow();
                Ok(Box::new(Bytes(
                    b[offset as usize..(offset + size) as usize].to_vec(),
                )))
            },
            &mut |p| {
                seen.push((p.blocks_done, p.blocks_total));
                Ok(())
            },
        )
        .unwrap();
        // One count across the plain blocks and the containers'.
        assert!(seen
            .windows(2)
            .all(|w| w[0].0 <= w[1].0 && w[0].1 == w[1].1));
        let (done, total) = *seen.last().unwrap();
        assert!(done <= total && done + 1 >= total, "{done} of {total}");
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

        // Unlocked again on the target, or with the key the check recovered:
        // the same disk either way.
        let out = restore_onto(&image, disk.layout.size_bytes, PASS, true);
        assert!(out == restore_onto(&image, disk.layout.size_bytes, PASS, false));
        // The same copy sealed in an `.obk`: the same disk.
        let keys = crate::engine_core::testkit::test_keys();
        let sealed = dir.path().join("disk.obk");
        disk.backup_sealed(&sealed, Some(keys.clone()));
        assert!(
            out == restore_sealed_onto(&sealed, Some(&keys), disk.layout.size_bytes, PASS, true),
            "the .obk copy restores the same disk"
        );
        assert!(crate::engine_core::disk_luks::fixture::private_leftovers(dir.path()).is_empty());
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

        // 4096-byte encryption sectors on a partition that starts between
        // them: refused before anything is written.
        let mut odd = load();
        odd.info.luks_headers[0].sector_size = 4096;
        assert!(containers_to_rebuild(&odd).is_ok());
        odd.disk_manifest.as_mut().unwrap().layout.partitions[1].first_lba += 1;
        let e = containers_to_rebuild(&odd).unwrap_err();
        assert!(e.to_string().contains("4096"), "{e}");
        // Settled before the disk is opened: a container without its key,
        // or with a key that is not its header's.
        let sidecar = load();
        let containers = containers_to_rebuild(&sidecar).unwrap();
        assert!(
            crate::luks_rebuild::fit_containers(&sidecar, &containers, &HashMap::new()).is_err()
        );
        let mut keys = check_passphrases(&containers, &request(&image, Some(PASS)).passphrases)
            .unwrap()
            .keys;
        assert!(crate::luks_rebuild::fit_containers(&sidecar, &containers, &keys).is_ok());
        keys.get_mut(&2).unwrap().master_key[0] ^= 1;
        assert!(crate::luks_rebuild::fit_containers(&sidecar, &containers, &keys).is_err());
        // And the Windows question: which partitions are LUKS in the clear.
        assert_eq!(crate::luks_rebuild::luks_in_clear(&load()), vec![2]);
        assert_eq!(crate::luks_rebuild::luks_in_clear(&headless), vec![2]);
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
