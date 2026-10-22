//! Writing the Linux boot medium onto a USB stick (PLAN §6, stage 6): which
//! disks qualify, and the write itself. The decisions are in [`crate::media_stick`].
//!
//! Only a USB, card-reader or removable disk is offered and accepted: the
//! write erases the whole device, and the one rule that keeps an internal
//! disk out of reach without asking anyone to read device names is "not on
//! this bus". On the MMC bus only an SD card counts: eMMC is the same bus
//! soldered onto the board, the system disk of many small machines. The
//! restore guard (`guard_target_disk_device`) is asked as well, by the
//! service, before this runs.
//!
//! A name like `/dev/sdb` is not a stick: unplug one and plug another, and
//! the name moves. The list gives each stick's model, serial, size and device
//! number; the write request carries them back, and the device the write
//! actually opened is compared with them before its first byte is written.
//!
//! The device is held the way a restore holds its target
//! ([`crate::disk_session_linux`]): `O_EXCL` (a mounted stick is refused, not
//! unmounted behind the person's back), the udev lock and a udev hold, so the
//! desktop does not mount half an ISO while it is being written. Every byte is
//! read back and compared after the write — a counterfeit stick that reports
//! more capacity than it has fails here, not at the next boot.

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use own_backup_common::cancel::CancelToken;
use own_backup_common::engine_dto::{MediaStickDto, MediaStickIdentityDto};
use own_backup_common::linux_block as lb;
use own_backup_common::{tr, OwnBackupError, Result};
use tracing::{info, warn};

use crate::disk_session_linux::{DiskIo, KernelPartition, UDEV_TIMEOUT};
use crate::media_stick::{
    inspect_iso, plan_log_partition, LogPartitionPlan, ISO_HEAD_BYTES, LOG_LABEL,
};
use crate::restore_disk_linux::{open_disk, partition_node};
use crate::udev_hold_linux::UdevHold;

/// Copy and compare in pieces of this size.
const CHUNK: usize = 4 << 20;

/// Where sysfs lists whole disks (partitions are not here).
const SYS_BLOCK: &str = "/sys/block";

/// Where sysfs lists block devices by device number.
const SYS_DEV_BLOCK: &str = "/sys/dev/block";

/// The stages a write reports, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickStage {
    Write,
    Verify,
    Logs,
}

impl StickStage {
    pub const ALL: [StickStage; 3] = [StickStage::Write, StickStage::Verify, StickStage::Logs];

    pub fn id(self) -> &'static str {
        match self {
            StickStage::Write => "write",
            StickStage::Verify => "verify",
            StickStage::Logs => "logs",
        }
    }

    pub fn title(self) -> String {
        match self {
            StickStage::Write => tr!("media_stick.stage_write"),
            StickStage::Verify => tr!("media_stick.stage_verify"),
            StickStage::Logs => tr!("media_stick.stage_logs"),
        }
        .into_owned()
    }
}

/// What the write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickReport {
    pub iso_bytes: u64,
    pub volume_id: String,
    pub log_partition: Option<String>,
    pub log_skipped: Option<String>,
}

/// How a disk is attached, from where its sysfs directory really lives:
/// `…/usb2/2-1/…/block/sdb` is on USB, `…/mmc_host/mmc0/…` a card reader.
fn transport_of(real: &Path, removable: bool) -> Option<&'static str> {
    let parts: Vec<String> = real
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.iter().any(|p| {
        p.strip_prefix("usb")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    }) {
        Some("usb")
    } else if parts.iter().any(|p| p == "mmc_host") {
        Some("mmc")
    } else if removable {
        Some("removable")
    } else {
        None
    }
}

/// The disks under `sys_block` (`/sys/block`) a medium may go onto.
/// `mounted(name)` lists what is mounted from the disk or its partitions.
pub fn sticks_in(sys_block: &Path, mounted: impl Fn(&str) -> Vec<String>) -> Vec<MediaStickDto> {
    let Ok(entries) = std::fs::read_dir(sys_block) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if ["loop", "ram", "zram", "nbd", "dm-", "md", "sr", "fd"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let dir = sys_block.join(&name);
        let read = |f: &str| {
            std::fs::read_to_string(dir.join(f))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        // A write-protected card, an empty card reader.
        let sectors: u64 = read("size").parse().unwrap_or(0);
        if read("ro") == "1" || sectors == 0 {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&dir) else {
            continue;
        };
        let Some(transport) = transport_of(&real, read("removable") == "1") else {
            continue;
        };
        // An SD card, not eMMC (`MMC`, the board's own storage) nor an SDIO
        // function.
        if transport == "mmc" && read("device/type") != "SD" {
            continue;
        }
        let id = identity_at(&real, sectors * 512, read("dev"));
        out.push(MediaStickDto {
            device: format!("/dev/{name}"),
            model: id.model,
            size_bytes: id.size_bytes,
            transport: transport.into(),
            mounted: mounted(&name),
            serial: id.serial,
            dev: id.dev,
        });
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// Who the disk whose sysfs directory really is `real` is: vendor and model,
/// and a serial number — the device's own (`device/serial`: an SD card, an
/// NVMe disk) or that of the nearest parent device that has one (the USB
/// device a USB disk sits on).
fn identity_at(real: &Path, size_bytes: u64, dev: String) -> MediaStickIdentityDto {
    let read = |p: &Path| {
        std::fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let model = [
        read(&real.join("device/vendor")),
        read(&real.join("device/model")),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    let mut serial = read(&real.join("device/serial"));
    let mut dir = real.parent();
    while serial.is_empty() {
        let Some(d) = dir else { break };
        if d.file_name().is_none_or(|n| n == "devices") {
            break;
        }
        serial = read(&d.join("serial"));
        dir = d.parent();
    }
    MediaStickIdentityDto {
        model,
        serial,
        size_bytes,
        dev,
    }
}

/// Who the disk with device number `major:minor` is, through
/// `sys_dev_block` (`/sys/dev/block`); `size_bytes` as the open device says.
fn identity_of_number(
    sys_dev_block: &Path,
    (major, minor): (u32, u32),
    size_bytes: u64,
) -> MediaStickIdentityDto {
    let dev = format!("{major}:{minor}");
    let real = std::fs::canonicalize(sys_dev_block.join(&dev)).unwrap_or_default();
    identity_at(&real, size_bytes, dev)
}

fn describe(id: &MediaStickIdentityDto) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !id.model.is_empty() {
        parts.push(id.model.clone());
    }
    if !id.serial.is_empty() {
        parts.push(id.serial.clone());
    }
    parts.push(format!("{} MiB", id.size_bytes >> 20));
    parts.push(id.dev.clone());
    parts.join(", ")
}

/// Refuse unless `found` is the stick `expected` names. Every field counts:
/// two sticks of one model and size differ in their serial or their number.
fn same_stick(
    device: &str,
    expected: &MediaStickIdentityDto,
    found: &MediaStickIdentityDto,
) -> Result<()> {
    if expected == found {
        return Ok(());
    }
    warn!(device, ?expected, ?found, "not the stick that was listed");
    Err(OwnBackupError::RestoreIncompatible(
        tr!(
            "media_stick.changed",
            device = device,
            expected = describe(expected),
            found = describe(found)
        )
        .into_owned(),
    ))
}

/// Mount points of disk `name` and of its partitions, now.
fn mounted_from(name: &str) -> Vec<String> {
    let sys = Path::new(lb::SYS_CLASS_BLOCK);
    let mut devices = vec![name.to_string()];
    devices.extend(lb::partitions_of(sys, name));
    let mut out = Vec::new();
    for d in devices {
        if let Ok(Some(u)) = lb::device_usage(Path::new(&format!("/dev/{d}"))) {
            out.extend(u.mounted_rw.into_iter().chain(u.mounted_ro));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The disks of this machine a medium may go onto.
pub fn list_sticks() -> Vec<MediaStickDto> {
    sticks_in(Path::new(SYS_BLOCK), mounted_from)
}

/// `device` as one of [`list_sticks`], or the reason it is not.
pub fn stick(device: &str) -> Result<MediaStickDto> {
    let name = lb::kernel_name(Path::new(device)).ok_or_else(|| {
        OwnBackupError::Device(tr!("media_stick.not_a_device", device = device).into_owned())
    })?;
    list_sticks()
        .into_iter()
        .find(|s| s.device == format!("/dev/{name}"))
        .ok_or_else(|| {
            OwnBackupError::RestoreIncompatible(
                tr!("media_stick.not_a_stick", device = device).into_owned(),
            )
        })
}

/// `device` as a stick that may be written now: one of [`list_sticks`] with
/// nothing of it mounted.
pub fn check_stick(device: &str) -> Result<MediaStickDto> {
    let stick = stick(device)?;
    if !stick.mounted.is_empty() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "media_stick.mounted",
                device = stick.device,
                mounts = stick.mounted.join(", ")
            )
            .into_owned(),
        ));
    }
    Ok(stick)
}

/// Write the ISO at `iso` onto the stick at `device`, which must still be
/// the stick `expect` describes (from [`list_sticks`]). `progress` gets the
/// stage and its percentage.
pub fn write_stick(
    iso: &Path,
    device: &str,
    expect: &MediaStickIdentityDto,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(StickStage, u64),
) -> Result<StickReport> {
    let stick = check_stick(device)?;
    same_stick(device, expect, &stick.identity())?;
    write_image(iso, &stick.device, Some(expect), cancel, progress)
}

/// [`write_stick`] from an ISO already open (`label` names it in the log):
/// what the build checked is what gets written, whatever its path is now.
pub fn write_stick_file(
    iso: &std::fs::File,
    label: &Path,
    device: &str,
    expect: &MediaStickIdentityDto,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(StickStage, u64),
) -> Result<StickReport> {
    let stick = check_stick(device)?;
    same_stick(device, expect, &stick.identity())?;
    write_image_file(iso, label, &stick.device, Some(expect), cancel, progress)
}

fn mkfs_vfat() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join("mkfs.vfat"))
        .find(|p| p.is_file())
}

/// The write itself, on any whole disk: [`write_stick`] decides which disks
/// may get here (tests use a loop device). With `expect`, the device opened
/// must be that stick (compared by what the open descriptor is, not by its
/// name) before anything is written.
pub fn write_image(
    iso: &Path,
    device: &str,
    expect: Option<&MediaStickIdentityDto>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(StickStage, u64),
) -> Result<StickReport> {
    let file = std::fs::File::open(iso).map_err(|e| {
        OwnBackupError::ImageFormat(
            tr!(
                "media_stick.iso_unreadable",
                path = iso.display(),
                error = e
            )
            .into_owned(),
        )
    })?;
    write_image_file(&file, iso, device, expect, cancel, progress)
}

/// [`write_image`] from an ISO already open; `iso` names it in the log.
fn write_image_file(
    file: &std::fs::File,
    iso: &Path,
    device: &str,
    expect: Option<&MediaStickIdentityDto>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(StickStage, u64),
) -> Result<StickReport> {
    let len = file.metadata()?.len();
    let mut head = vec![0u8; ISO_HEAD_BYTES];
    if len < ISO_HEAD_BYTES as u64 {
        return Err(OwnBackupError::ImageFormat(
            tr!("media_stick.not_an_iso").into_owned(),
        ));
    }
    file.read_exact_at(&mut head, 0)?;
    let iso_info = inspect_iso(&head, len)?;

    let name = lb::kernel_name(Path::new(device)).ok_or_else(|| {
        OwnBackupError::Device(tr!("media_stick.not_a_device", device = device).into_owned())
    })?;
    // Before the open: udisks must not mount the first partition it
    // recognises while the rest of the image is still being written.
    let _hold = UdevHold::install(&[name.as_str()])
        .map_err(|e| warn!(error = %e, "udev hold not installed"))
        .ok();
    let mut session = open_disk(device)?;
    if let Some(expect) = expect {
        let number = session.device_mut().device_number()?;
        let found = identity_of_number(Path::new(SYS_DEV_BLOCK), number, session.length());
        same_stick(device, expect, &found)?;
    }
    if session.length() < len {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "media_stick.too_small",
                device = device,
                need_mib = len >> 20,
                have_mib = session.length() >> 20
            )
            .into_owned(),
        ));
    }
    info!(iso = %iso.display(), bytes = len, volume = %iso_info.volume_id, device, "writing the medium onto a stick");

    // Half an ISO with the full one's MBR and volume descriptor would look
    // bootable and fail somewhere in the middle of the boot; an unverified one
    // is not known to be better. Whatever stops the copy or its check —
    // cancel, a write or read error, a mismatch — the stick's start is wiped.
    if let Err(e) = copy_and_verify(file, &mut session, len, device, cancel, progress) {
        let wipe = vec![0u8; (1 << 20).min(len as usize)];
        let wiped = session
            .device_mut()
            .write_at(0, &wipe)
            .and_then(|()| session.device_mut().flush());
        match &wiped {
            Ok(()) => warn!(device, error = %e, "write abandoned; the stick's start was wiped"),
            Err(w) => {
                warn!(device, error = %e, wipe_error = %w, "write abandoned; wiping the stick's start failed too")
            }
        }
        let _ = session.finish(None, UDEV_TIMEOUT, std::thread::sleep);
        return Err(match (e, wiped) {
            (OwnBackupError::Cancelled, Ok(())) => OwnBackupError::Cancelled,
            (e, Ok(())) => OwnBackupError::Device(
                tr!("media_stick.failed_wiped", device = device, error = e).into_owned(),
            ),
            (e, Err(w)) => OwnBackupError::Device(
                tr!(
                    "media_stick.failed_not_wiped",
                    device = device,
                    error = e,
                    wipe_error = w
                )
                .into_owned(),
            ),
        });
    }
    info!(
        device,
        bytes = len,
        "the stick reads back exactly the image"
    );

    progress(StickStage::Logs, 0);
    let plan = plan_log_partition(&head[..512], len, session.length(), session.sector_size());
    let (log_partition, log_skipped) = match (plan, mkfs_vfat()) {
        (LogPartitionPlan::Add { .. }, None) => {
            let why = tr!("media_stick.log_no_mkfs").into_owned();
            reread_only(&mut session, device);
            session.finish(None, UDEV_TIMEOUT, std::thread::sleep)?;
            (None, Some(why))
        }
        (
            LogPartitionPlan::Add {
                slot,
                first_lba,
                sectors,
                lba0,
                partitions,
            },
            Some(mkfs),
        ) => {
            let expected: Vec<KernelPartition> = partitions
                .iter()
                .map(|&(number, start, size)| KernelPartition {
                    number,
                    start,
                    size,
                })
                .collect();
            session.write_raw_table(&[(0, &lba0)], &expected)?;
            session.finish(Some(&expected), UDEV_TIMEOUT, std::thread::sleep)?;
            let number = slot as u32 + 1;
            let node = partition_node(device, number).ok_or_else(|| {
                OwnBackupError::Device(
                    tr!("media_stick.log_no_node", device = device, number = number).into_owned(),
                )
            })?;
            info!(
                node,
                first_lba, sectors, "log partition added behind the image"
            );
            format_logs(&mkfs, &node)?;
            (Some(node), None)
        }
        (LogPartitionPlan::Skip(why), _) => {
            warn!(device, reason = %why, "no log partition on the stick");
            reread_only(&mut session, device);
            session.finish(None, UDEV_TIMEOUT, std::thread::sleep)?;
            (None, Some(why))
        }
    };
    progress(StickStage::Logs, 100);
    Ok(StickReport {
        iso_bytes: len,
        volume_id: iso_info.volume_id,
        log_partition,
        log_skipped,
    })
}

/// Copy `len` bytes of the image onto the stick, flush, and read every byte
/// back against the image.
fn copy_and_verify(
    file: &std::fs::File,
    session: &mut crate::restore_disk_linux::DiskSession,
    len: u64,
    device: &str,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(StickStage, u64),
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut back = vec![0u8; CHUNK];
    for stage in [StickStage::Write, StickStage::Verify] {
        let mut done = 0u64;
        let mut last = u64::MAX;
        while done < len {
            if cancel.is_cancelled() {
                return Err(OwnBackupError::Cancelled);
            }
            let n = CHUNK.min((len - done) as usize);
            file.read_exact_at(&mut buf[..n], done)?;
            if stage == StickStage::Write {
                session.device_mut().write_at(done, &buf[..n])?;
            } else {
                session.device_mut().read_at(done, &mut back[..n])?;
                if let Some(at) = (0..n).find(|&i| buf[i] != back[i]) {
                    return Err(OwnBackupError::Device(
                        tr!(
                            "media_stick.verify_mismatch",
                            device = device,
                            offset = done + at as u64
                        )
                        .into_owned(),
                    ));
                }
            }
            done += n as u64;
            let pct = done * 100 / len.max(1);
            if pct != last {
                last = pct;
                progress(stage, pct);
            }
        }
        if stage == StickStage::Write {
            session.device_mut().flush()?;
            session.device_mut().drop_cache()?;
        }
    }
    Ok(())
}

/// The image's own table, for the kernel: a stick that still shows its old
/// partitions until it is unplugged would be mounted by its old names.
fn reread_only<D: DiskIo>(
    session: &mut crate::disk_session_linux::LinuxDiskSession<D>,
    device: &str,
) {
    if let Err(e) = session.device_mut().reread_partitions() {
        warn!(device, error = %e, "the kernel did not re-read the stick's table; unplug it once");
    }
}

fn format_logs(mkfs: &Path, node: &str) -> Result<()> {
    let out = std::process::Command::new(mkfs)
        .args(["-F", "32", "-n", LOG_LABEL, node])
        .output()
        .map_err(|e| {
            OwnBackupError::Device(
                tr!("media_stick.log_mkfs_failed", node = node, error = e).into_owned(),
            )
        })?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(OwnBackupError::Device(
            tr!("media_stick.log_mkfs_failed", node = node, error = text).into_owned(),
        ));
    }
    // mkfs.vfat writes through the page cache of the node; the stick may be
    // pulled the moment the window says "done".
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(node) {
        let _ = f.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A `/sys/block` of symlinks into a `devices` tree, as the kernel lays it.
    fn fake_sys(root: &Path, disks: &[(&str, &str, &str, &str, u64)]) -> PathBuf {
        let block = root.join("block");
        std::fs::create_dir_all(&block).unwrap();
        for (name, path, removable, ro, size) in disks {
            let real = root.join("devices").join(path).join("block").join(name);
            std::fs::create_dir_all(real.join("device")).unwrap();
            std::fs::write(real.join("removable"), removable).unwrap();
            std::fs::write(real.join("ro"), ro).unwrap();
            std::fs::write(real.join("size"), size.to_string()).unwrap();
            std::fs::write(real.join("device/vendor"), "SanDisk ").unwrap();
            std::fs::write(real.join("device/model"), "Ultra\n").unwrap();
            std::fs::write(real.join("dev"), "8:16\n").unwrap();
            symlink(&real, block.join(name)).unwrap();
        }
        block
    }

    /// The real sysfs directory of `name` in a [`fake_sys`] tree.
    fn real_dir(block: &Path, name: &str) -> PathBuf {
        std::fs::canonicalize(block.join(name)).unwrap()
    }

    #[test]
    fn only_usb_card_reader_and_removable_disks_are_offered() {
        let dir = tempfile::tempdir().unwrap();
        let usb = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0";
        let block = fake_sys(
            dir.path(),
            &[
                ("sdb", usb, "1", "0", 30_000_000),
                // A USB SSD: not "removable", still USB.
                ("sdc", usb, "0", "0", 1_000_000_000),
                (
                    "mmcblk0",
                    "platform/mmc_host/mmc0/mmc0:0001",
                    "0",
                    "0",
                    60_000_000,
                ),
                (
                    "sdd",
                    "pci0000:00/0000:00:1f.2/ata1/host0",
                    "1",
                    "0",
                    8_000_000,
                ),
                // Internal disks, a write-protected card, an empty reader.
                (
                    "nvme0n1",
                    "pci0000:00/0000:00:1d.0/nvme/nvme0",
                    "0",
                    "0",
                    900_000_000,
                ),
                (
                    "sda",
                    "pci0000:00/0000:00:17.0/ata2/host1",
                    "0",
                    "0",
                    900_000_000,
                ),
                ("sde", usb, "1", "1", 30_000_000),
                ("sdf", usb, "1", "0", 0),
                ("loop0", "virtual", "0", "0", 1000),
            ],
        );
        std::fs::write(real_dir(&block, "mmcblk0").join("device/type"), "SD\n").unwrap();
        let sticks = sticks_in(&block, |n| {
            if n == "sdb" {
                vec!["/media/u/STICK".into()]
            } else {
                vec![]
            }
        });
        let got: Vec<(&str, &str)> = sticks
            .iter()
            .map(|s| (s.device.as_str(), s.transport.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/dev/mmcblk0", "mmc"),
                ("/dev/sdb", "usb"),
                ("/dev/sdc", "usb"),
                ("/dev/sdd", "removable"),
            ]
        );
        assert_eq!(sticks[1].model, "SanDisk Ultra");
        assert_eq!(sticks[1].size_bytes, 30_000_000 * 512);
        assert_eq!(sticks[1].mounted, vec!["/media/u/STICK".to_string()]);
    }

    /// eMMC sits on the same bus as an SD card reader but is the board's own
    /// storage; an SDIO function or a card of unknown type is not a card
    /// either.
    #[test]
    fn on_the_mmc_bus_only_an_sd_card_is_offered() {
        let dir = tempfile::tempdir().unwrap();
        let host = |n: u32| format!("platform/mmc_host/mmc{n}/mmc{n}:0001");
        let block = fake_sys(
            dir.path(),
            &[
                ("mmcblk0", &host(0), "0", "0", 60_000_000),
                ("mmcblk1", &host(1), "0", "0", 60_000_000),
                ("mmcblk2", &host(2), "0", "0", 60_000_000),
                ("mmcblk3", &host(3), "1", "0", 60_000_000),
            ],
        );
        for (name, kind) in [("mmcblk0", "MMC"), ("mmcblk1", "SD"), ("mmcblk2", "SDIO")] {
            std::fs::write(real_dir(&block, name).join("device/type"), kind).unwrap();
        }
        let got: Vec<String> = sticks_in(&block, |_| vec![])
            .into_iter()
            .map(|s| s.device)
            .collect();
        assert_eq!(got, vec!["/dev/mmcblk1".to_string()]);
    }

    /// The serial is the device's own, or the USB device's above it; the
    /// device number comes along; and a write is refused when the device it
    /// opened is not the listed one in any of them.
    #[test]
    fn a_stick_is_known_by_model_serial_size_and_number() {
        let dir = tempfile::tempdir().unwrap();
        let usb = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0";
        let block = fake_sys(
            dir.path(),
            &[
                ("sdb", usb, "1", "0", 30_000_000),
                (
                    "mmcblk0",
                    "platform/mmc_host/mmc0/mmc0:0001",
                    "1",
                    "0",
                    1000,
                ),
            ],
        );
        let usb_dev = dir.path().join("devices/pci0000:00/0000:00:14.0/usb2/2-1");
        std::fs::write(usb_dev.join("serial"), "4C530001\n").unwrap();
        // The root hub has a serial too; the nearer device wins.
        std::fs::write(usb_dev.parent().unwrap().join("serial"), "0000:00:14.0").unwrap();
        let card = real_dir(&block, "mmcblk0");
        std::fs::write(card.join("device/type"), "SD").unwrap();
        std::fs::write(card.join("device/serial"), "0x1234abcd").unwrap();

        let sticks = sticks_in(&block, |_| vec![]);
        assert_eq!(sticks[0].serial, "0x1234abcd");
        assert_eq!(sticks[1].serial, "4C530001");
        assert_eq!(sticks[1].dev, "8:16");
        let listed = sticks[1].identity();
        assert_eq!(
            listed,
            MediaStickIdentityDto {
                model: "SanDisk Ultra".into(),
                serial: "4C530001".into(),
                size_bytes: 30_000_000 * 512,
                dev: "8:16".into(),
            }
        );

        // Opened by number, as the write does.
        let by_number = dir.path().join("dev-block");
        std::fs::create_dir_all(&by_number).unwrap();
        symlink(real_dir(&block, "sdb"), by_number.join("8:16")).unwrap();
        let opened = identity_of_number(&by_number, (8, 16), 30_000_000 * 512);
        assert!(same_stick("/dev/sdb", &listed, &opened).is_ok());

        let other = |f: &dyn Fn(&mut MediaStickIdentityDto)| {
            let mut o = listed.clone();
            f(&mut o);
            same_stick("/dev/sdb", &listed, &o)
        };
        assert!(other(&|o| o.serial = "4C530002".into()).is_err());
        assert!(other(&|o| o.size_bytes += 512).is_err());
        assert!(other(&|o| o.dev = "8:32".into()).is_err());
        assert!(other(&|o| o.model = "Kingston".into()).is_err());
        let gone = identity_of_number(&by_number, (8, 32), 30_000_000 * 512);
        assert!(same_stick("/dev/sdb", &listed, &gone).is_err());
    }

    #[test]
    fn a_usb_word_inside_a_name_is_not_the_usb_bus() {
        let removable = false;
        assert_eq!(
            transport_of(Path::new("/sys/devices/pci0/usbmisc/block/sda"), removable),
            None
        );
        assert_eq!(
            transport_of(Path::new("/sys/devices/pci0/usb3/3-2/block/sda"), removable),
            Some("usb")
        );
    }

    /// An isohybrid image in the live-build layout, made by xorriso:
    /// isohybrid MBR, image partition from LBA 64, the EFI image appended as
    /// partition 2.
    fn medium_iso(dir: &Path) -> PathBuf {
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("isolinux")).unwrap();
        std::fs::create_dir_all(tree.join("live")).unwrap();
        std::fs::copy(
            "/usr/lib/ISOLINUX/isolinux.bin",
            tree.join("isolinux/isolinux.bin"),
        )
        .unwrap();
        std::fs::write(tree.join("live/filesystem.squashfs"), vec![0xA5u8; 3 << 20]).unwrap();
        let efi = dir.join("efi.img");
        std::fs::write(&efi, vec![0u8; 4 << 20]).unwrap();
        assert!(std::process::Command::new("mkfs.vfat")
            .arg(&efi)
            .output()
            .unwrap()
            .status
            .success());
        let iso = dir.join("medium.iso");
        let st = std::process::Command::new("xorriso")
            .args(["-as", "mkisofs", "-R", "-J", "-l", "-V", "OWNBACKUP"])
            .args(["-isohybrid-mbr", "/usr/lib/ISOLINUX/isohdpfx.bin"])
            .args(["-partition_offset", "16", "-b", "isolinux/isolinux.bin"])
            .args(["-c", "isolinux/boot.cat", "-no-emul-boot"])
            .args([
                "-boot-load-size",
                "4",
                "-boot-info-table",
                "-eltorito-alt-boot",
            ])
            .args([
                "-e",
                "--interval:appended_partition_2:all::",
                "-no-emul-boot",
            ])
            .arg("-append_partition")
            .arg("2")
            .arg("0xef")
            .arg(&efi)
            .arg("-o")
            .arg(&iso)
            .arg(&tree)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
        iso
    }

    /// A loop device over a 256 MiB file that was a FAT "superfloppy" stick
    /// before — the write must replace that, not live beside it.
    struct LoopStick(String);

    impl LoopStick {
        fn new(dir: &Path) -> Self {
            let disk = dir.join("stick.img");
            std::fs::File::create(&disk)
                .unwrap()
                .set_len(256 << 20)
                .unwrap();
            assert!(std::process::Command::new("mkfs.vfat")
                .args(["-F", "32", "-n", "OLDSTICK"])
                .arg(&disk)
                .output()
                .unwrap()
                .status
                .success());
            let out = std::process::Command::new("losetup")
                .args(["--show", "-f", "-P"])
                .arg(&disk)
                .output()
                .unwrap();
            let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert!(dev.starts_with("/dev/loop"), "{out:?}");
            Self(dev)
        }
    }

    impl Drop for LoopStick {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup")
                .args(["-d", &self.0])
                .status();
        }
    }

    /// The whole write on a loop device: written, read back, the log
    /// partition added and formatted, the boot sectors of the image
    /// untouched. Needs root, `losetup`, `xorriso`, isolinux, dosfstools and
    /// mtools: `sudo -E cargo test -p own-backup-backup media_stick_linux --
    /// --ignored`.
    #[test]
    #[ignore = "needs root, losetup, xorriso, isolinux, dosfstools and mtools"]
    fn real_loop_stick() {
        let dir = tempfile::tempdir().unwrap();
        let iso = medium_iso(dir.path());
        let stick = LoopStick::new(dir.path());
        let dev = stick.0.clone();

        let mut stages = Vec::new();
        let report = write_image(&iso, &dev, None, &CancelToken::default(), &mut |s, p| {
            stages.push((s, p))
        })
        .unwrap();
        assert_eq!(report.volume_id, "OWNBACKUP");
        assert_eq!(report.log_skipped, None);
        let node = report.log_partition.clone().unwrap();
        assert_eq!(node, format!("{dev}p3"));
        assert!(stages.contains(&(StickStage::Write, 100)));
        assert!(stages.contains(&(StickStage::Verify, 100)));
        assert_eq!(stages.last(), Some(&(StickStage::Logs, 100)));

        // Everything but slot 3 of LBA 0 is the image, byte for byte.
        let image = std::fs::read(&iso).unwrap();
        let mut written = vec![0u8; image.len()];
        std::fs::File::open(&dev)
            .unwrap()
            .read_exact_at(&mut written, 0)
            .unwrap();
        let differ: Vec<usize> = (0..image.len())
            .filter(|&i| image[i] != written[i])
            .collect();
        assert!(
            differ.iter().all(|&i| (478..494).contains(&i)),
            "{differ:?}"
        );
        let blkid = std::process::Command::new("blkid")
            .args(["-o", "value", "-s", "LABEL", &node])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&blkid.stdout).trim(), LOG_LABEL);
        // A clean FAT32 a file can be written to: the medium puts its log
        // there. Checked in userspace (dosfstools, mtools), which also works
        // where the kernel has no vfat.
        let fsck = std::process::Command::new("fsck.fat")
            .args(["-n", &node])
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stdout)
        );
        let log = dir.path().join("own-backup-service.log");
        std::fs::write(&log, b"line\n").unwrap();
        let copy = std::process::Command::new("mcopy")
            .args(["-i", &node])
            .arg(&log)
            .arg("::/own-backup-service.log")
            .output()
            .unwrap();
        assert!(
            copy.status.success(),
            "{}",
            String::from_utf8_lossy(&copy.stderr)
        );
    }

    /// Cancelled before the first byte: the write says so, and the stick's
    /// start is wiped rather than left looking like something it is not.
    #[test]
    #[ignore = "needs root, losetup, xorriso, isolinux and dosfstools"]
    fn real_loop_stick_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let iso = medium_iso(dir.path());
        let stick = LoopStick::new(dir.path());
        let cancel = CancelToken::default();
        cancel.cancel();
        let r = write_image(&iso, &stick.0, None, &cancel, &mut |_, _| {});
        assert!(matches!(r, Err(OwnBackupError::Cancelled)), "{r:?}");
        let mut head = vec![0xFFu8; 1 << 20];
        std::fs::File::open(&stick.0)
            .unwrap()
            .read_exact_at(&mut head, 0)
            .unwrap();
        assert!(head.iter().all(|&b| b == 0), "the old start is still there");
    }
}
