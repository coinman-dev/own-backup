//! A restore session on one whole disk under Linux — the counterpart of
//! [`crate::disk_session_win`] (PLAN §6, stage 3).
//!
//! Windows isolates a disk by taking it offline. Linux has no such switch; the
//! same guarantees come from four separate mechanisms, and the session applies
//! them in a fixed order:
//!
//! 1. **`O_EXCL` on the whole disk.** The kernel refuses the open while any
//!    partition of the disk is mounted, used as swap, or held by device-mapper
//!    / md (`EBUSY`), and refuses those claims while we hold it. This is what
//!    stops a table from being rewritten under a live filesystem.
//! 2. **`flock(LOCK_EX)` on the same descriptor** — the block-device locking
//!    protocol of systemd-udevd (<https://systemd.io/BLOCK_DEVICE_LOCKING/>).
//!    udevd takes `LOCK_SH|LOCK_NB` before probing a disk and skips it while
//!    someone holds the exclusive lock, so it does not run blkid, LVM or md
//!    autoassembly against a half-written table. udevd's own shared lock is
//!    short-lived, so a busy lock is retried for a few seconds before giving up.
//! 3. **Telling the kernel.** After the table is written and synced,
//!    `BLKRRPART` makes the kernel drop and rescan every partition. It refuses
//!    (`EBUSY`) while any partition is open — something we do not control can
//!    hold one open without `O_EXCL` — and then the change is applied
//!    partition by partition with `BLKPG` (delete what changed or vanished,
//!    add what is new), which only needs the changed partitions to be closed.
//!    Unchanged partitions are not touched either way.
//! 4. **Waiting for udev.** Releasing the lock and closing the descriptor makes
//!    udevd re-probe the disk (it watches block devices for close-after-write).
//!    The session then waits until the kernel's partitions match the written
//!    table, every partition has its `/dev` node and its udev database record,
//!    and udevd's queue is empty — only then is it safe to open a new partition
//!    by name and write a filesystem into it.
//!
//! The kernel and udev side is behind [`DiskIo`] so the order, the fallback
//! and the waiting can be tested without root; [`HostView`] reads sysfs,
//! `/dev` and `/run/udev` under a configurable root for the same reason.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use own_backup_common::restore_plan::{PartitionKind, PartitionScheme, PartitionTableSpec};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::boot_code::BootCodeOutcome;
use crate::table_writer::partition_table_write_ops_keeping_boot_code;

/// Sysfs, `/dev` and udev always count in 512-byte units.
const KERNEL_SECTOR: u64 = 512;

/// How long a busy device lock (udevd probing) is retried before giving up.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long udev gets to bring up the partitions of a new table.
pub const UDEV_TIMEOUT: Duration = Duration::from_secs(30);

/// A partition as the kernel exposes it (512-byte units, like sysfs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KernelPartition {
    pub number: u32,
    pub start: u64,
    pub size: u64,
}

/// Why the kernel refused an ioctl: busy partitions get the `BLKPG`
/// fallback, anything else is an error.
#[derive(Debug)]
pub enum IoctlError {
    Busy,
    Other(std::io::Error),
}

impl From<std::io::Error> for IoctlError {
    fn from(e: std::io::Error) -> Self {
        if e.raw_os_error() == Some(libc::EBUSY) {
            IoctlError::Busy
        } else {
            IoctlError::Other(e)
        }
    }
}

impl std::fmt::Display for IoctlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoctlError::Busy => f.write_str("EBUSY"),
            IoctlError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// An opened, exclusively claimed and locked whole disk.
pub trait DiskIo {
    fn path(&self) -> &str;
    /// Length in bytes, from the device itself.
    fn length(&self) -> u64;
    /// Logical sector size.
    fn sector_size(&self) -> u32;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    /// `BLKRRPART`.
    fn reread_partitions(&mut self) -> std::result::Result<(), IoctlError>;
    /// `BLKPG_DEL_PARTITION`.
    fn delete_partition(&mut self, number: u32) -> std::result::Result<(), IoctlError>;
    /// `BLKPG_ADD_PARTITION`; `start`/`size` in 512-byte units.
    fn add_partition(&mut self, p: &KernelPartition) -> std::result::Result<(), IoctlError>;
    /// Kernel name of the disk (`sdb`, `nvme0n1`), for [`HostView`].
    fn kernel_name(&self) -> &str;
}

/// Opens the real device. Separate from [`DiskIo`] so tests can hand the
/// session a fake without going near `/dev`.
pub trait DiskOpener {
    type Io: DiskIo;
    fn open(&self, path: &str) -> Result<Self::Io>;
}

/// The partitions a written `spec` should make the kernel show.
///
/// GPT: one per entry at its own index. MBR: primaries at their slot; logical
/// partitions numbered 5, 6, … in on-disk chain order (the order the writer
/// lays the EBRs, by first LBA); and the extended container the writer
/// synthesises, which the kernel registers as a stub of
/// `max(sector_size / 512, 2)` units so tools can still see it.
pub fn expected_partitions(spec: &PartitionTableSpec) -> Vec<KernelPartition> {
    let unit = (spec.sector_size.max(512) as u64) / KERNEL_SECTOR;
    let to_kernel = |number: u32, first: u64, last: u64| KernelPartition {
        number,
        start: first * unit,
        size: (last + 1 - first) * unit,
    };
    let mut out = Vec::new();
    match spec.scheme {
        PartitionScheme::Gpt => {
            for e in &spec.entries {
                out.push(to_kernel(e.index, e.first_lba, e.last_lba));
            }
        }
        PartitionScheme::Mbr => {
            let mut logicals: Vec<_> = spec
                .entries
                .iter()
                .filter(|e| matches!(e.kind, PartitionKind::Mbr { logical: true, .. }))
                .collect();
            logicals.sort_by_key(|e| e.first_lba);
            let mut taken = [false; 4];
            for e in &spec.entries {
                if matches!(e.kind, PartitionKind::Mbr { logical: false, .. }) {
                    if let Some(slot) = (e.index as usize).checked_sub(1).filter(|s| *s < 4) {
                        taken[slot] = true;
                    }
                    out.push(to_kernel(e.index, e.first_lba, e.last_lba));
                }
            }
            if let (Some(first), Some(last)) = (logicals.first(), logicals.last()) {
                if let Some(slot) = taken.iter().position(|t| !t) {
                    let ext_first = first.first_lba - 1;
                    let ext_len = (last.last_lba + 1 - ext_first) * unit;
                    out.push(KernelPartition {
                        number: slot as u32 + 1,
                        start: ext_first * unit,
                        size: ext_len.min(unit.max(2)),
                    });
                }
                for (i, l) in logicals.iter().enumerate() {
                    out.push(to_kernel(5 + i as u32, l.first_lba, l.last_lba));
                }
            }
        }
    }
    out.sort();
    out
}

/// One `BLKPG` step of the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkpgOp {
    Delete(u32),
    Add(KernelPartition),
}

/// Turn what the kernel shows into what the table says, touching only the
/// partitions that differ. All deletions come first: a partition that moved
/// keeps its number, and the kernel refuses to add a number that is still
/// registered or a range that overlaps one.
pub fn blkpg_plan(current: &[KernelPartition], wanted: &[KernelPartition]) -> Vec<BlkpgOp> {
    let mut ops: Vec<BlkpgOp> = current
        .iter()
        .filter(|c| !wanted.contains(c))
        .map(|c| BlkpgOp::Delete(c.number))
        .collect();
    ops.extend(
        wanted
            .iter()
            .filter(|w| !current.contains(w))
            .map(|w| BlkpgOp::Add(*w)),
    );
    ops
}

/// Sysfs, `/dev` and `/run/udev` as one view, rooted anywhere for tests.
#[derive(Debug, Clone)]
pub struct HostView {
    /// `/sys/class/block`.
    pub sys_block: PathBuf,
    /// `/dev`.
    pub dev: PathBuf,
    /// `/run/udev`.
    pub run_udev: PathBuf,
}

impl Default for HostView {
    fn default() -> Self {
        Self::rooted(Path::new("/"))
    }
}

impl HostView {
    /// The view of a system whose root is `root`.
    pub fn rooted(root: &Path) -> Self {
        Self {
            sys_block: root.join("sys/class/block"),
            dev: root.join("dev"),
            run_udev: root.join("run/udev"),
        }
    }

    /// The disk's partitions as the kernel has them now, sorted by number.
    pub fn kernel_partitions(&self, disk: &str) -> Vec<KernelPartition> {
        let read = |p: &str, f: &str| -> Option<u64> {
            std::fs::read_to_string(self.sys_block.join(p).join(f))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let mut out: Vec<KernelPartition> =
            own_backup_common::linux_block::partitions_of(&self.sys_block, disk)
                .iter()
                .filter_map(|p| {
                    Some(KernelPartition {
                        number: read(p, "partition")? as u32,
                        start: read(p, "start")?,
                        size: read(p, "size")?,
                    })
                })
                .collect();
        out.sort();
        out
    }

    /// Whether udevd still has events to process (`/run/udev/queue` exists
    /// while its queue is non-empty — what `udevadm settle` waits on).
    pub fn udev_busy(&self) -> bool {
        self.run_udev.join("queue").exists()
    }

    /// Partitions of `disk` that udev has not finished with: no `/dev` node or
    /// no database record yet.
    pub fn partitions_without_udev(&self, disk: &str) -> Vec<String> {
        own_backup_common::linux_block::partitions_of(&self.sys_block, disk)
            .into_iter()
            .filter(|p| {
                let node = self.dev.join(p).exists();
                let record = own_backup_common::linux_block::dev_numbers(&self.sys_block, p)
                    .is_some_and(|(ma, mi)| {
                        self.run_udev
                            .join("data")
                            .join(format!("b{ma}:{mi}"))
                            .exists()
                    });
                !(node && record)
            })
            .collect()
    }
}

/// How the kernel learned about the new table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelUpdate {
    /// `BLKRRPART` rescanned the whole disk.
    Reread,
    /// `BLKRRPART` was refused; these `BLKPG` steps applied the difference.
    PerPartition(Vec<BlkpgOp>),
    /// The kernel already showed exactly the written table.
    AlreadyCurrent,
}

/// What [`LinuxDiskSession::write_partition_table`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableWritten {
    pub boot_code: BootCodeOutcome,
    pub kernel: KernelUpdate,
    /// What the kernel must show once udev settles.
    pub expected: Vec<KernelPartition>,
}

/// An exclusive, locked session on one whole disk.
pub struct LinuxDiskSession<D: DiskIo> {
    io: Option<D>,
    host: HostView,
}

impl<D: DiskIo> LinuxDiskSession<D> {
    /// Open the whole disk at `path` exclusively and take the udev lock.
    pub fn open<O: DiskOpener<Io = D>>(opener: &O, path: &str, host: HostView) -> Result<Self> {
        let io = opener.open(path)?;
        info!(
            disk = path,
            "whole disk opened exclusively and locked against udev"
        );
        Ok(Self { io: Some(io), host })
    }

    fn io(&self) -> &D {
        self.io.as_ref().expect("session device used after finish")
    }

    fn io_mut(&mut self) -> &mut D {
        self.io.as_mut().expect("session device used after finish")
    }

    pub fn length(&self) -> u64 {
        self.io().length()
    }

    pub fn sector_size(&self) -> u32 {
        self.io().sector_size()
    }

    /// Raw device access inside the session (wiping, verifying).
    pub fn device_mut(&mut self) -> &mut D {
        self.io_mut()
    }

    /// Write `spec` onto the disk and make the kernel see it.
    ///
    /// The device's own length wins over `spec.disk_size_bytes`, as on
    /// Windows: a backup GPT header must sit at the true last sector. The BIOS
    /// boot code already in LBA 0 is carried into the new one
    /// ([`crate::boot_code`]).
    pub fn write_partition_table(&mut self, spec: &PartitionTableSpec) -> Result<TableWritten> {
        let path = self.io().path().to_string();
        let device_sector = self.io().sector_size();
        if spec.sector_size != device_sector {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!(
                    "disk_session_linux.sector_mismatch",
                    planned = spec.sector_size,
                    disk = path,
                    actual = device_sector
                )
                .into_owned(),
            ));
        }
        let mut spec = spec.clone();
        if spec.disk_size_bytes != self.io().length() {
            warn!(
                disk = %path,
                planned = spec.disk_size_bytes,
                actual = self.io().length(),
                "plan's disk size differs from the device's; using the device's"
            );
            spec.disk_size_bytes = self.io().length();
        }

        let mut lba0 = vec![0u8; device_sector.max(512) as usize];
        self.io().read_at(0, &mut lba0)?;
        let (ops, boot_code) = partition_table_write_ops_keeping_boot_code(&spec, &lba0)?;
        info!(disk = %path, ?boot_code, "BIOS boot code in LBA 0");

        let total: usize = ops.iter().map(|o| o.bytes.len()).sum();
        for op in &ops {
            self.io_mut().write_at(op.offset, &op.bytes)?;
        }
        self.io_mut().flush()?;
        info!(
            disk = %path,
            writes = ops.len(),
            bytes = total,
            partitions = spec.entries.len(),
            "partition table written"
        );

        let expected = expected_partitions(&spec);
        let kernel = self.tell_kernel(&expected)?;
        Ok(TableWritten {
            boot_code,
            kernel,
            expected,
        })
    }

    fn tell_kernel(&mut self, expected: &[KernelPartition]) -> Result<KernelUpdate> {
        let path = self.io().path().to_string();
        let disk = self.io().kernel_name().to_string();
        match self.io_mut().reread_partitions() {
            Ok(()) => {
                info!(disk = %path, "kernel re-read the partition table");
                return Ok(KernelUpdate::Reread);
            }
            Err(IoctlError::Busy) => {
                debug!(disk = %path, "BLKRRPART busy — applying the change per partition");
            }
            Err(IoctlError::Other(e)) => {
                return Err(OwnBackupError::Device(
                    tr!("disk_session_linux.reread_failed", disk = path, error = e).into_owned(),
                ))
            }
        }
        let current = self.host.kernel_partitions(&disk);
        let plan = blkpg_plan(&current, expected);
        if plan.is_empty() {
            return Ok(KernelUpdate::AlreadyCurrent);
        }
        for op in &plan {
            let r = match op {
                BlkpgOp::Delete(n) => self.io_mut().delete_partition(*n),
                BlkpgOp::Add(p) => self.io_mut().add_partition(p),
            };
            if let Err(e) = r {
                let number = match op {
                    BlkpgOp::Delete(n) => *n,
                    BlkpgOp::Add(p) => p.number,
                };
                return Err(OwnBackupError::Device(
                    tr!(
                        "disk_session_linux.partition_busy",
                        disk = path,
                        number = number,
                        error = e
                    )
                    .into_owned(),
                ));
            }
        }
        info!(disk = %path, steps = plan.len(), "kernel updated partition by partition");
        Ok(KernelUpdate::PerPartition(plan))
    }

    /// Release the lock, close the disk and wait for udev to bring up the
    /// partitions in `expected` (`None`: nothing was written, nothing to wait
    /// for). `sleep` is how the wait passes time (tests advance a fake clock).
    pub fn finish(
        mut self,
        expected: Option<&[KernelPartition]>,
        timeout: Duration,
        mut sleep: impl FnMut(Duration),
    ) -> Result<()> {
        let io = self.io.take().expect("finish called twice");
        let path = io.path().to_string();
        let disk = io.kernel_name().to_string();
        // Closing a descriptor opened for writing is what makes udevd re-probe
        // the disk; while we hold the lock it would only skip it.
        drop(io);
        let Some(expected) = expected else {
            return Ok(());
        };
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        loop {
            let now = self.host.kernel_partitions(&disk);
            let pending = self.host.partitions_without_udev(&disk);
            let busy = self.host.udev_busy();
            if now == expected && pending.is_empty() && !busy {
                info!(disk = %path, partitions = now.len(), "udev settled on the new table");
                return Ok(());
            }
            if waited >= timeout {
                return Err(OwnBackupError::Device(
                    tr!(
                        "disk_session_linux.udev_timeout",
                        disk = path,
                        seconds = timeout.as_secs(),
                        pending = describe_pending(&now, expected, &pending, busy)
                    )
                    .into_owned(),
                ));
            }
            sleep(step);
            waited += step;
        }
    }
}

fn describe_pending(
    now: &[KernelPartition],
    expected: &[KernelPartition],
    pending: &[String],
    busy: bool,
) -> String {
    let mut parts = Vec::new();
    if now != expected {
        parts.push(format!("kernel {now:?} != table {expected:?}"));
    }
    if !pending.is_empty() {
        parts.push(format!("no udev record: {}", pending.join(", ")));
    }
    if busy {
        parts.push("udev queue not empty".into());
    }
    parts.join("; ")
}

// --- the real device -------------------------------------------------------

const BLKRRPART: libc::c_ulong = 0x125F; // _IO(0x12, 95)
const BLKPG: libc::c_ulong = 0x1269; // _IO(0x12, 105)
const BLKPG_ADD_PARTITION: libc::c_int = 1;
const BLKPG_DEL_PARTITION: libc::c_int = 2;

#[repr(C)]
struct BlkpgPartition {
    start: libc::c_longlong,
    length: libc::c_longlong,
    pno: libc::c_int,
    devname: [libc::c_char; 64],
    volname: [libc::c_char; 64],
}

#[repr(C)]
struct BlkpgIoctlArg {
    op: libc::c_int,
    flags: libc::c_int,
    datalen: libc::c_int,
    data: *mut libc::c_void,
}

/// A whole disk opened `O_RDWR | O_EXCL` and locked with `flock(LOCK_EX)`.
pub struct RealDisk {
    dev: crate::linux_device::LinuxRawDevice,
    fd: std::fs::File,
    name: String,
}

impl RealDisk {
    fn ioctl_blkpg(&self, op: libc::c_int, p: &mut BlkpgPartition) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;
        let mut arg = BlkpgIoctlArg {
            op,
            flags: 0,
            datalen: std::mem::size_of::<BlkpgPartition>() as libc::c_int,
            data: p as *mut BlkpgPartition as *mut libc::c_void,
        };
        // SAFETY: `arg` points at a live blkpg_partition for the call's duration.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), BLKPG, &mut arg) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl DiskIo for RealDisk {
    fn path(&self) -> &str {
        self.dev.path()
    }
    fn length(&self) -> u64 {
        self.dev.length()
    }
    fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.dev.write_at(offset, data)
    }
    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }
    fn reread_partitions(&mut self) -> std::result::Result<(), IoctlError> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: BLKRRPART takes no argument.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), BLKRRPART, 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    fn delete_partition(&mut self, number: u32) -> std::result::Result<(), IoctlError> {
        let mut p = BlkpgPartition {
            start: 0,
            length: 0,
            pno: number as libc::c_int,
            devname: [0; 64],
            volname: [0; 64],
        };
        Ok(self.ioctl_blkpg(BLKPG_DEL_PARTITION, &mut p)?)
    }
    fn add_partition(&mut self, kp: &KernelPartition) -> std::result::Result<(), IoctlError> {
        let mut p = BlkpgPartition {
            start: (kp.start * KERNEL_SECTOR) as libc::c_longlong,
            length: (kp.size * KERNEL_SECTOR) as libc::c_longlong,
            pno: kp.number as libc::c_int,
            devname: [0; 64],
            volname: [0; 64],
        };
        Ok(self.ioctl_blkpg(BLKPG_ADD_PARTITION, &mut p)?)
    }
    fn kernel_name(&self) -> &str {
        &self.name
    }
}

/// Opens whole disks for real: `O_EXCL`, then the udev lock.
pub struct RealOpener;

impl DiskOpener for RealOpener {
    type Io = RealDisk;

    fn open(&self, path: &str) -> Result<RealDisk> {
        use std::os::unix::io::AsRawFd;
        let name =
            own_backup_common::linux_block::kernel_name(Path::new(path)).ok_or_else(|| {
                OwnBackupError::Device(
                    tr!("disk_session_linux.not_a_disk", disk = path).into_owned(),
                )
            })?;
        if own_backup_common::linux_block::is_partition(
            Path::new(own_backup_common::linux_block::SYS_CLASS_BLOCK),
            &name,
        ) {
            return Err(OwnBackupError::Device(
                tr!("disk_session_linux.not_a_disk", disk = path).into_owned(),
            ));
        }
        let dev = crate::linux_device::LinuxRawDevice::open_write(path).map_err(|e| {
            OwnBackupError::Device(
                tr!("disk_session_linux.busy", disk = path, error = e).into_owned(),
            )
        })?;
        // A second descriptor of the same open file for the ioctls and the lock:
        // flock locks belong to the open file description, so this one locks
        // what `dev` writes through.
        let fd = dev.try_clone_file()?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            // SAFETY: plain flock on a descriptor we own.
            if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) || Instant::now() >= deadline {
                return Err(OwnBackupError::Device(
                    tr!("disk_session_linux.lock_failed", disk = path, error = err).into_owned(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(RealDisk { dev, fd, name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::restore_plan::PartitionSpec;
    use std::cell::RefCell;
    use std::rc::Rc;

    const MIB: u64 = 1024 * 1024;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Write(u64),
        Flush,
        Reread,
        Delete(u32),
        Add(KernelPartition),
    }

    struct FakeDisk {
        data: Vec<u8>,
        sector: u32,
        reread_busy: bool,
        busy_partitions: Vec<u32>,
        calls: Rc<RefCell<Vec<Call>>>,
    }

    impl DiskIo for FakeDisk {
        fn path(&self) -> &str {
            "/dev/fake"
        }
        fn length(&self) -> u64 {
            self.data.len() as u64
        }
        fn sector_size(&self) -> u32 {
            self.sector
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            buf.copy_from_slice(&self.data[offset as usize..][..buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            self.calls.borrow_mut().push(Call::Write(offset));
            self.data[offset as usize..][..data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            self.calls.borrow_mut().push(Call::Flush);
            Ok(())
        }
        fn reread_partitions(&mut self) -> std::result::Result<(), IoctlError> {
            self.calls.borrow_mut().push(Call::Reread);
            if self.reread_busy {
                Err(IoctlError::Busy)
            } else {
                Ok(())
            }
        }
        fn delete_partition(&mut self, n: u32) -> std::result::Result<(), IoctlError> {
            self.calls.borrow_mut().push(Call::Delete(n));
            if self.busy_partitions.contains(&n) {
                Err(IoctlError::Busy)
            } else {
                Ok(())
            }
        }
        fn add_partition(&mut self, p: &KernelPartition) -> std::result::Result<(), IoctlError> {
            self.calls.borrow_mut().push(Call::Add(*p));
            Ok(())
        }
        fn kernel_name(&self) -> &str {
            "sdz"
        }
    }

    struct FakeOpener(RefCell<Option<FakeDisk>>);

    impl DiskOpener for FakeOpener {
        type Io = FakeDisk;
        fn open(&self, _path: &str) -> Result<FakeDisk> {
            self.0
                .borrow_mut()
                .take()
                .ok_or_else(|| OwnBackupError::Device("busy".into()))
        }
    }

    fn fake(size: u64) -> (FakeOpener, Rc<RefCell<Vec<Call>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let disk = FakeDisk {
            data: vec![0; size as usize],
            sector: 512,
            reread_busy: false,
            busy_partitions: Vec::new(),
            calls: calls.clone(),
        };
        (FakeOpener(RefCell::new(Some(disk))), calls)
    }

    fn gpt_part(index: u32, first: u64, last: u64) -> PartitionSpec {
        PartitionSpec {
            index,
            first_lba: first,
            last_lba: last,
            kind: PartitionKind::Gpt {
                type_guid: own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM.into(),
                part_guid: String::new(),
                attributes: 0,
                name: String::new(),
            },
        }
    }

    fn mbr_part(index: u32, first: u64, last: u64, logical: bool) -> PartitionSpec {
        PartitionSpec {
            index,
            first_lba: first,
            last_lba: last,
            kind: PartitionKind::Mbr {
                type_byte: 0x83,
                bootable: false,
                logical,
            },
        }
    }

    fn gpt_spec(size: u64, entries: Vec<PartitionSpec>) -> PartitionTableSpec {
        PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: size,
            disk_id: String::new(),
            entries,
        }
    }

    /// A fake /sys + /dev + /run/udev for `sdz` with the given partitions.
    fn host_with(root: &Path, parts: &[KernelPartition], udev_done: bool) -> HostView {
        let host = HostView::rooted(root);
        let disk = host.sys_block.join("sdz");
        let _ = std::fs::remove_dir_all(&disk);
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::create_dir_all(&host.dev).unwrap();
        std::fs::create_dir_all(host.run_udev.join("data")).unwrap();
        for p in parts {
            let name = format!("sdz{}", p.number);
            let dir = disk.join(&name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("partition"), format!("{}\n", p.number)).unwrap();
            std::fs::write(dir.join("start"), format!("{}\n", p.start)).unwrap();
            std::fs::write(dir.join("size"), format!("{}\n", p.size)).unwrap();
            std::fs::write(dir.join("dev"), format!("8:{}\n", 200 + p.number)).unwrap();
            // sys/class/block holds every block device at the top level too.
            let top = host.sys_block.join(&name);
            let _ = std::fs::remove_file(&top);
            std::os::unix::fs::symlink(&dir, &top).unwrap();
            if udev_done {
                std::fs::write(host.dev.join(&name), "").unwrap();
                std::fs::write(
                    host.run_udev
                        .join("data")
                        .join(format!("b8:{}", 200 + p.number)),
                    "",
                )
                .unwrap();
            }
        }
        host
    }

    #[test]
    fn a_table_is_written_synced_and_reread_in_that_order() {
        let size = 64 * MIB;
        let (opener, calls) = fake(size);
        let root = tempfile::tempdir().unwrap();
        let host = host_with(root.path(), &[], true);
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host).unwrap();
        let spec = gpt_spec(size, vec![gpt_part(1, 2048, 4095)]);
        let out = s.write_partition_table(&spec).unwrap();
        assert_eq!(out.kernel, KernelUpdate::Reread);
        assert_eq!(out.boot_code, BootCodeOutcome::NoneOnDisk);
        let calls = calls.borrow();
        let flush = calls.iter().position(|c| *c == Call::Flush).unwrap();
        let reread = calls.iter().position(|c| *c == Call::Reread).unwrap();
        assert!(calls[..flush].iter().all(|c| matches!(c, Call::Write(_))));
        assert!(flush < reread, "the kernel is told only after the sync");
        assert_eq!(
            out.expected,
            vec![KernelPartition {
                number: 1,
                start: 2048,
                size: 2048
            }]
        );
    }

    #[test]
    fn boot_code_already_on_the_disk_is_kept() {
        let size = 64 * MIB;
        let (opener, _) = fake(size);
        {
            let mut d = opener.0.borrow_mut();
            let d = d.as_mut().unwrap();
            d.data[..440].fill(0xEB);
            d.data[0x1BE + 4] = 0xEE;
            d.data[510] = 0x55;
            d.data[511] = 0xAA;
        }
        let root = tempfile::tempdir().unwrap();
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host_with(root.path(), &[], true))
            .unwrap();
        let out = s
            .write_partition_table(&gpt_spec(size, vec![gpt_part(1, 2048, 4095)]))
            .unwrap();
        assert_eq!(out.boot_code, BootCodeOutcome::Kept);
        let mut lba0 = [0u8; 512];
        s.device_mut().read_at(0, &mut lba0).unwrap();
        assert!(lba0[..440].iter().all(|&b| b == 0xEB));
    }

    #[test]
    fn a_busy_reread_falls_back_to_blkpg_for_the_changed_partitions_only() {
        let size = 64 * MIB;
        let (opener, calls) = fake(size);
        opener.0.borrow_mut().as_mut().unwrap().reread_busy = true;
        let root = tempfile::tempdir().unwrap();
        // The kernel has: 1 unchanged, 2 to be resized, 3 to vanish.
        let kept = KernelPartition {
            number: 1,
            start: 2048,
            size: 2048,
        };
        let host = host_with(
            root.path(),
            &[
                kept,
                KernelPartition {
                    number: 2,
                    start: 4096,
                    size: 1000,
                },
                KernelPartition {
                    number: 3,
                    start: 8192,
                    size: 1000,
                },
            ],
            true,
        );
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host).unwrap();
        let spec = gpt_spec(size, vec![gpt_part(1, 2048, 4095), gpt_part(2, 4096, 6143)]);
        let out = s.write_partition_table(&spec).unwrap();
        let resized = KernelPartition {
            number: 2,
            start: 4096,
            size: 2048,
        };
        assert_eq!(
            out.kernel,
            KernelUpdate::PerPartition(vec![
                BlkpgOp::Delete(2),
                BlkpgOp::Delete(3),
                BlkpgOp::Add(resized)
            ])
        );
        let calls = calls.borrow();
        assert!(!calls.contains(&Call::Delete(1)), "partition 1 was touched");
        assert!(calls.contains(&Call::Add(resized)));
    }

    #[test]
    fn a_partition_still_in_use_stops_the_fallback_with_its_number() {
        let size = 64 * MIB;
        let (opener, _) = fake(size);
        {
            let mut d = opener.0.borrow_mut();
            let d = d.as_mut().unwrap();
            d.reread_busy = true;
            d.busy_partitions = vec![2];
        }
        let root = tempfile::tempdir().unwrap();
        let host = host_with(
            root.path(),
            &[KernelPartition {
                number: 2,
                start: 4096,
                size: 1000,
            }],
            true,
        );
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host).unwrap();
        let err = s
            .write_partition_table(&gpt_spec(size, vec![gpt_part(1, 2048, 4095)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains('2'), "{err}");
    }

    #[test]
    fn a_sector_size_mismatch_is_refused_before_anything_is_written() {
        let size = 64 * MIB;
        let (opener, calls) = fake(size);
        let root = tempfile::tempdir().unwrap();
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host_with(root.path(), &[], true))
            .unwrap();
        let mut spec = gpt_spec(size, vec![gpt_part(1, 256, 511)]);
        spec.sector_size = 4096;
        assert!(s.write_partition_table(&spec).is_err());
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn the_device_length_wins_over_the_plan() {
        let size = 64 * MIB;
        let (opener, _) = fake(size);
        let root = tempfile::tempdir().unwrap();
        let mut s = LinuxDiskSession::open(&opener, "/dev/fake", host_with(root.path(), &[], true))
            .unwrap();
        s.write_partition_table(&gpt_spec(size - MIB, vec![gpt_part(1, 2048, 4095)]))
            .unwrap();
        let mut disk = vec![0u8; size as usize];
        s.device_mut().read_at(0, &mut disk).unwrap();
        let layout = crate::gpt_reader::parse_disk_layout(&disk, 512, size).unwrap();
        assert_eq!(layout.partitions.len(), 1);
        // Backup header at the device's real last sector.
        let last = &disk[(size - 512) as usize..];
        assert_eq!(&last[..8], b"EFI PART");
    }

    #[test]
    fn finish_waits_until_kernel_and_udev_agree() {
        let size = 64 * MIB;
        let (opener, _) = fake(size);
        let root = tempfile::tempdir().unwrap();
        let p1 = KernelPartition {
            number: 1,
            start: 2048,
            size: 2048,
        };
        // Kernel already shows the partition, udev has not processed it yet.
        let host = host_with(root.path(), &[p1], false);
        std::fs::write(host.run_udev.join("queue"), "").unwrap();
        let s = LinuxDiskSession::open(&opener, "/dev/fake", host.clone()).unwrap();
        let mut ticks = 0;
        let root_path = root.path().to_path_buf();
        s.finish(Some(&[p1]), Duration::from_secs(5), |_| {
            ticks += 1;
            if ticks == 3 {
                // udev catches up.
                host_with(&root_path, &[p1], true);
                std::fs::remove_file(root_path.join("run/udev/queue")).unwrap();
            }
        })
        .unwrap();
        assert_eq!(ticks, 3);
    }

    #[test]
    fn finish_reports_what_udev_never_finished() {
        let size = 64 * MIB;
        let (opener, _) = fake(size);
        let root = tempfile::tempdir().unwrap();
        let p1 = KernelPartition {
            number: 1,
            start: 2048,
            size: 2048,
        };
        let host = host_with(root.path(), &[p1], false);
        let s = LinuxDiskSession::open(&opener, "/dev/fake", host).unwrap();
        let err = s
            .finish(Some(&[p1]), Duration::from_secs(1), |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("sdz1"), "{err}");
    }

    #[test]
    fn finish_without_a_write_does_not_wait() {
        let (opener, _) = fake(MIB);
        let root = tempfile::tempdir().unwrap();
        let s = LinuxDiskSession::open(&opener, "/dev/fake", host_with(root.path(), &[], false))
            .unwrap();
        s.finish(None, Duration::ZERO, |_| panic!("waited"))
            .unwrap();
    }

    #[test]
    fn mbr_expectations_include_the_extended_stub_and_logical_numbers() {
        let spec = PartitionTableSpec {
            scheme: PartitionScheme::Mbr,
            sector_size: 512,
            disk_size_bytes: 256 * MIB,
            disk_id: "1A2B3C4D".into(),
            entries: vec![
                mbr_part(1, 2048, 4095, false),
                // Out of order on purpose: the chain follows first LBA.
                mbr_part(6, 20480, 30000, true),
                mbr_part(5, 8192, 16383, true),
            ],
        };
        let k = expected_partitions(&spec);
        assert_eq!(
            k,
            vec![
                KernelPartition {
                    number: 1,
                    start: 2048,
                    size: 2048
                },
                KernelPartition {
                    number: 2,
                    start: 8191,
                    size: 2
                },
                KernelPartition {
                    number: 5,
                    start: 8192,
                    size: 8192
                },
                KernelPartition {
                    number: 6,
                    start: 20480,
                    size: 30000 - 20480 + 1
                },
            ]
        );
    }

    #[test]
    fn four_k_sectors_are_reported_in_kernel_units() {
        let mut spec = gpt_spec(64 * MIB, vec![gpt_part(1, 256, 511)]);
        spec.sector_size = 4096;
        assert_eq!(
            expected_partitions(&spec),
            vec![KernelPartition {
                number: 1,
                start: 2048,
                size: 2048
            }]
        );
    }

    #[test]
    fn blkpg_plan_is_empty_when_the_kernel_already_agrees() {
        let p = KernelPartition {
            number: 1,
            start: 2048,
            size: 2048,
        };
        assert!(blkpg_plan(&[p], &[p]).is_empty());
    }
}
