//! Raw Windows device I/O shared by the backup reader, the restore writer and
//! the VHDX raw writer.
//!
//! Opens physical drives (`\\.\PhysicalDrive0`), volumes (`\\.\C:`) or VSS
//! snapshot device paths with `FILE_FLAG_NO_BUFFERING` and performs
//! sector-aligned reads/writes through an internally-allocated aligned buffer.

#![cfg(target_os = "windows")]

use std::alloc::{alloc, dealloc, Layout};

use tracing::{debug, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOLEAN, CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    DISK_ATTRIBUTE_OFFLINE, DISK_ATTRIBUTE_READ_ONLY, DISK_GEOMETRY, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_DRIVE_GEOMETRY, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_SET_DISK_ATTRIBUTES,
    IOCTL_DISK_UPDATE_PROPERTIES, SET_DISK_ATTRIBUTES,
};
use windows::Win32::System::IO::DeviceIoControl;

use own_backup_common::{OwnBackupError, Result};

/// Convert a Rust string to a NUL-terminated UTF-16 buffer for Win32 calls.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Normalize a user-supplied device spec to a Win32 device path.
///
/// `C:` / `C:\` -> `\\.\C:`; paths already starting with `\\` are kept as-is
/// (covers `\\.\PhysicalDrive0`, `\\.\C:` and VSS snapshot device paths like
/// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`).
pub fn normalize_device_path(path: &str) -> String {
    if path.starts_with('\\') {
        return path.to_string();
    }
    let bytes = path.as_bytes();
    if matches!(bytes.len(), 2 | 3)
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\')
    {
        return format!(r"\\.\{}:", bytes[0] as char);
    }
    path.to_string()
}

/// Heap buffer aligned for `FILE_FLAG_NO_BUFFERING` I/O.
struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize, align: usize) -> Result<Self> {
        let layout = Layout::from_size_align(size, align)
            .map_err(|e| OwnBackupError::Device(format!("bad buffer layout: {e}")))?;
        // SAFETY: layout has non-zero size (callers guarantee size > 0).
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(OwnBackupError::Device("aligned buffer allocation failed".into()));
        }
        Ok(Self { ptr, layout })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: same as above.
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with the same layout in new().
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// A raw device opened for direct, sector-aligned I/O.
pub struct WinRawDevice {
    handle: HANDLE,
    path: String,
    /// Accessible length in bytes. For a windowed partition source this is the
    /// partition size, NOT the whole physical drive.
    length: u64,
    /// Byte offset added to every seek — non-zero only for a windowed partition
    /// source (`part:<disk>:<offset>:<len>`), so reads at logical offset 0 land
    /// at the partition start on the physical drive.
    base_offset: u64,
    sector_size: u32,
}

/// Why a raw write did not happen.
pub enum WriteFailure {
    /// Rejected before reaching the device (alignment, length, allocation).
    Refused(String),
    /// The device said no.
    Win32(windows::core::Error),
}

impl WriteFailure {
    /// Whether this is Windows protecting sectors it thinks belong to a volume.
    ///
    /// Three codes mean the same thing to a restore, and none of them means the
    /// disk is broken:
    ///
    /// - `ERROR_ACCESS_DENIED` — the sectors are inside a *mounted* volume.
    ///   Writing the first megabytes of a filesystem is enough for Windows to
    ///   recognise one and mount it, and from that moment the rest of the write
    ///   is refused. A restore hits this halfway through its own first
    ///   partition, which reads as a permissions problem and is not one.
    /// - `ERROR_NO_SUCH_DEVICE` — the partition was created moments ago and the
    ///   device stack is still being rebuilt.
    /// - `ERROR_NOT_READY` — same story, seen from the volume side.
    ///
    /// All three are cured by taking the volume down again and retrying, which
    /// is what the caller does.
    pub fn volume_in_the_way(&self) -> bool {
        const ACCESS_DENIED: i32 = 0x8007_0005u32 as i32;
        const NO_SUCH_DEVICE: i32 = 0x8007_01B1u32 as i32;
        const NOT_READY: i32 = 0x8007_0015u32 as i32;
        match self {
            WriteFailure::Win32(e) => {
                matches!(e.code().0, ACCESS_DENIED | NO_SUCH_DEVICE | NOT_READY)
            }
            WriteFailure::Refused(_) => false,
        }
    }

    /// The message a user reads, naming the device, the offset and the size —
    /// everything needed to find the write in the log.
    pub fn into_error(self, path: &str, offset: u64, len: usize) -> OwnBackupError {
        match self {
            WriteFailure::Refused(msg) => {
                OwnBackupError::Device(format!("write to {path} at {offset}: {msg}"))
            }
            WriteFailure::Win32(e) => OwnBackupError::Device(format!(
                "WriteFile({path}) {len} bytes at {offset} failed: {e}"
            )),
        }
    }
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteFailure::Refused(m) => write!(f, "{m}"),
            WriteFailure::Win32(e) => write!(f, "{e}"),
        }
    }
}

/// Parse a windowed partition source `part:<disk>:<offset>:<len>` into
/// `(disk_number, offset_bytes, length_bytes)`.
fn parse_partition_spec(device_path: &str) -> Option<(u32, u64, u64)> {
    let rest = device_path.strip_prefix("part:")?;
    let mut it = rest.split(':');
    let disk = it.next()?.parse::<u32>().ok()?;
    let offset = it.next()?.parse::<u64>().ok()?;
    let len = it.next()?.parse::<u64>().ok()?;
    Some((disk, offset, len))
}

// SAFETY: the HANDLE is owned exclusively by this struct and all access goes
// through &mut self; moving it across threads is safe.
unsafe impl Send for WinRawDevice {}

impl WinRawDevice {
    /// Open a device for reading (backup source, attached VHDX for verify).
    pub fn open_read(device_path: &str) -> Result<Self> {
        Self::open(device_path, false)
    }

    /// Open a device for writing (restore target, attached VHDX for backup).
    pub fn open_write(device_path: &str) -> Result<Self> {
        Self::open(device_path, true)
    }

    fn open(device_path: &str, writable: bool) -> Result<Self> {
        // Windowed partition-by-offset source: open the physical drive and
        // restrict I/O to [offset, offset+len). Used for letterless system
        // partitions (EFI/hidden) that Windows exposes no volume for.
        if let Some((disk, offset, len)) = parse_partition_spec(device_path) {
            return Self::open_partition_window(disk, offset, len, writable);
        }

        let path = normalize_device_path(device_path);
        let wide = to_wide(&path);

        let access = if writable {
            GENERIC_READ.0 | GENERIC_WRITE.0
        } else {
            GENERIC_READ.0
        };
        let flags = if writable {
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
        } else {
            FILE_FLAG_NO_BUFFERING
        };

        // SAFETY: `wide` outlives the call; all pointers are valid.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!("CreateFileW({path}) failed: {e}"))
        })?;

        let mut device = Self {
            handle,
            path: path.clone(),
            length: 0,
            base_offset: 0,
            sector_size: 512,
        };

        device.length = device.query_length()?;
        device.sector_size = device.query_sector_size();
        debug!(
            path = %device.path,
            length = device.length,
            sector_size = device.sector_size,
            "Opened raw device"
        );
        Ok(device)
    }

    /// Open a partition as a window over its physical drive: reads/writes at
    /// logical offset 0 map to `offset` on `\\.\PhysicalDrive{disk}`, and the
    /// accessible length is the partition size. VSS is not involved (callers
    /// skip it for non-lettered sources).
    fn open_partition_window(disk: u32, offset: u64, len: u64, writable: bool) -> Result<Self> {
        let phys = format!(r"\\.\PhysicalDrive{disk}");
        let mut dev = Self::open(&phys, writable)?;
        let sector = dev.sector_size as u64;
        if offset % sector != 0 {
            return Err(OwnBackupError::Device(format!(
                "partition offset {offset} is not aligned to sector size {sector}"
            )));
        }
        // Round the window up to a whole sector; never past the physical end.
        let full = dev.length;
        let win = len.div_ceil(sector) * sector;
        let end = offset.checked_add(win).ok_or_else(|| {
            OwnBackupError::Device(format!("partition window {offset}+{win} overflows"))
        })?;
        if end > full {
            return Err(OwnBackupError::Device(format!(
                "partition window [{offset}, {end}) exceeds drive length {full}"
            )));
        }
        dev.base_offset = offset;
        dev.length = win;
        dev.path = format!("part:{disk}:{offset}:{len}");
        debug!(
            drive = %phys,
            offset,
            length = win,
            "Opened partition window"
        );
        Ok(dev)
    }

    /// Total device length in bytes (`IOCTL_DISK_GET_LENGTH_INFO`).
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Logical sector size in bytes.
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// The normalized device path this device was opened with.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }

    fn query_length(&self) -> Result<u64> {
        let mut info = GET_LENGTH_INFORMATION::default();
        let mut returned = 0u32;
        // SAFETY: out pointer is valid for the size passed.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                0,
                Some(&mut info as *mut _ as *mut _),
                std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "IOCTL_DISK_GET_LENGTH_INFO({}) failed: {e}",
                self.path
            ))
        })?;
        Ok(info.Length as u64)
    }

    fn query_sector_size(&self) -> u32 {
        let mut geometry = DISK_GEOMETRY::default();
        let mut returned = 0u32;
        // SAFETY: out pointer is valid for the size passed.
        let result = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY,
                None,
                0,
                Some(&mut geometry as *mut _ as *mut _),
                std::mem::size_of::<DISK_GEOMETRY>() as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) if geometry.BytesPerSector > 0 => geometry.BytesPerSector,
            _ => {
                warn!(path = %self.path, "IOCTL_DISK_GET_DRIVE_GEOMETRY failed, assuming 512-byte sectors");
                512
            }
        }
    }

    fn seek(&mut self, offset: u64) -> Result<()> {
        // Windowed partition sources add a base offset so logical 0 maps to the
        // partition start on the physical drive.
        let physical = self.base_offset + offset;
        // SAFETY: handle is valid.
        unsafe { SetFilePointerEx(self.handle, physical as i64, None, FILE_BEGIN) }.map_err(|e| {
            OwnBackupError::Device(format!("seek({}) to {physical} failed: {e}", self.path))
        })
    }

    /// Read exactly `buf.len()` bytes at `offset`.
    ///
    /// `offset` must be sector-aligned. `buf.len()` may have any length up to
    /// the device end; the read is internally rounded up to sector size.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let sector = self.sector_size as u64;
        if offset % sector != 0 {
            return Err(OwnBackupError::Device(format!(
                "read offset {offset} is not aligned to sector size {sector}"
            )));
        }
        if offset + buf.len() as u64 > self.length {
            return Err(OwnBackupError::Device(format!(
                "read of {} bytes at {offset} exceeds device length {}",
                buf.len(),
                self.length
            )));
        }

        let padded = (buf.len() as u64).div_ceil(sector) * sector;
        // Never read past the device end even when rounding up.
        let padded = padded.min(self.length - offset) as usize;
        let mut aligned = AlignedBuf::new(padded, self.sector_size as usize)?;

        self.seek(offset)?;
        let mut read = 0u32;
        // SAFETY: buffer is valid for `padded` bytes.
        unsafe {
            ReadFile(
                self.handle,
                Some(&mut aligned.as_mut_slice()[..padded]),
                Some(&mut read),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "ReadFile({}) {padded} bytes at {offset} failed: {e}",
                self.path
            ))
        })?;
        let read = read as usize;
        if read < buf.len() {
            // A volume handle opened with FILE_FLAG_NO_BUFFERING can return
            // slightly fewer bytes than IOCTL_DISK_GET_LENGTH_INFO reports for
            // the final sector(s) — NTFS keeps its backup boot sector at the very
            // end and it is not always readable through the volume handle. Only
            // at the device tail do we tolerate this: copy what we got and
            // zero-fill the remainder (the primary structures live at offset 0,
            // so a zeroed tail sector does not stop the image from mounting).
            let at_tail = offset + padded as u64 >= self.length;
            if at_tail && read > 0 {
                buf[..read].copy_from_slice(&aligned.as_slice()[..read]);
                for b in &mut buf[read..] {
                    *b = 0;
                }
                tracing::debug!(
                    path = %self.path,
                    offset,
                    wanted = buf.len(),
                    got = read,
                    "Short read at device tail — zero-filled remainder"
                );
                return Ok(());
            }
            return Err(OwnBackupError::Device(format!(
                "short read on {}: wanted {} got {read} at offset {offset}",
                self.path,
                buf.len()
            )));
        }

        buf.copy_from_slice(&aligned.as_slice()[..buf.len()]);
        Ok(())
    }

    /// Write `data` at `offset`. Both must be sector-aligned; a write must not
    /// extend past the device end.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.try_write_at(offset, data).map_err(|f| f.into_error(&self.path, offset, data.len()))
    }

    /// The same write, with the Win32 error kept intact.
    ///
    /// Raw sector writes to a live disk fail for reasons that are worth telling
    /// apart — a volume mounted itself on top of the sectors mid-write, the
    /// partition has not appeared yet — and a caller that can do something about
    /// those needs the code, not a sentence (see [`WriteFailure`]).
    pub fn try_write_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<(), WriteFailure> {
        let sector = self.sector_size as u64;
        if offset % sector != 0 || data.len() as u64 % sector != 0 {
            return Err(WriteFailure::Refused(format!(
                "write of {} bytes at {offset} is not aligned to sector size {sector}",
                data.len()
            )));
        }
        if offset + data.len() as u64 > self.length {
            return Err(WriteFailure::Refused(format!(
                "write of {} bytes at {offset} exceeds device length {}",
                data.len(),
                self.length
            )));
        }
        if data.is_empty() {
            return Ok(());
        }

        let mut aligned = AlignedBuf::new(data.len(), self.sector_size as usize)
            .map_err(|e| WriteFailure::Refused(e.to_string()))?;
        aligned.as_mut_slice().copy_from_slice(data);

        self.seek(offset).map_err(|e| WriteFailure::Refused(e.to_string()))?;
        let mut written = 0u32;
        // SAFETY: buffer valid for data.len() bytes.
        unsafe {
            WriteFile(
                self.handle,
                Some(&aligned.as_slice()[..data.len()]),
                Some(&mut written),
                None,
            )
        }
        .map_err(WriteFailure::Win32)?;
        if (written as usize) != data.len() {
            return Err(WriteFailure::Refused(format!(
                "short write: wanted {} wrote {written}",
                data.len()
            )));
        }
        Ok(())
    }

    /// Flush OS buffers for this handle.
    pub fn flush(&mut self) -> Result<()> {
        // SAFETY: handle is valid.
        unsafe { FlushFileBuffers(self.handle) }
            .map_err(|e| OwnBackupError::Device(format!("flush({}) failed: {e}", self.path)))
    }

    fn set_disk_attributes(&self, attrs: &SET_DISK_ATTRIBUTES, what: &str) -> Result<()> {
        let mut returned = 0u32;
        // SAFETY: input buffer valid for the size passed; no output buffer.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_SET_DISK_ATTRIBUTES,
                Some(attrs as *const _ as *const _),
                std::mem::size_of::<SET_DISK_ATTRIBUTES>() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "IOCTL_DISK_SET_DISK_ATTRIBUTES({what}) on {} failed: {e}",
                self.path
            ))
        })
    }

    /// Take this (attached virtual) disk offline and clear its read-only flag
    /// so raw sector writes through the handle are not blocked by a mounted
    /// filesystem.
    ///
    /// A differencing VHDX inherits a complete NTFS filesystem from its parent;
    /// Windows auto-mounts that volume the instant the disk is attached, and
    /// afterwards raw writes to the volume's sectors fail with
    /// `ERROR_ACCESS_DENIED (0x80070005)`. Setting the disk offline dismounts
    /// the inner volume(s) while leaving this physical-drive handle writable.
    ///
    /// With `persist = true` the offline attribute is stored in the registry
    /// keyed by the disk's GPT GUID, so the NEXT attach of this disk — or of
    /// any differencing child inheriting the same GPT — arrives offline from
    /// the start: no transient auto-mount at all (and no slow detach waiting
    /// for the mounted volume's teardown). Call it only once partmgr knows
    /// the disk's partition table (see [`Self::update_properties`]).
    pub fn set_offline_writable(&self, persist: bool) -> Result<()> {
        let attrs = SET_DISK_ATTRIBUTES {
            Version: std::mem::size_of::<SET_DISK_ATTRIBUTES>() as u32,
            Persist: BOOLEAN(persist as u8),
            Attributes: DISK_ATTRIBUTE_OFFLINE as u64,
            AttributesMask: (DISK_ATTRIBUTE_OFFLINE | DISK_ATTRIBUTE_READ_ONLY) as u64,
            ..Default::default()
        };
        self.set_disk_attributes(&attrs, "offline")?;
        debug!(path = %self.path, persist, "Attached disk set offline (writable) for raw imaging");
        Ok(())
    }

    /// Bring the disk online for THIS session only (registry state untouched,
    /// read-only flag untouched): volumes surface and mount. Used when the
    /// user mounts a backup image to browse it — backup attaches keep the
    /// disk offline via the persisted attribute.
    pub fn set_online_this_session(&self) -> Result<()> {
        let attrs = SET_DISK_ATTRIBUTES {
            Version: std::mem::size_of::<SET_DISK_ATTRIBUTES>() as u32,
            Persist: BOOLEAN(0),
            Attributes: 0,
            AttributesMask: DISK_ATTRIBUTE_OFFLINE as u64,
            ..Default::default()
        };
        self.set_disk_attributes(&attrs, "online")?;
        debug!(path = %self.path, "Attached disk brought online for browsing");
        Ok(())
    }

    /// Ask partmgr to re-read the partition table (`IOCTL_DISK_UPDATE_PROPERTIES`).
    /// Needed after writing a GPT to a raw attached disk, so the disk gains its
    /// identity (GPT GUID) and a subsequent persisted attribute has a key to
    /// stick to. Safe while the disk is offline — no volumes surface.
    pub fn update_properties(&self) -> Result<()> {
        let mut returned = 0u32;
        // SAFETY: no in/out buffers.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_UPDATE_PROPERTIES,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            OwnBackupError::Device(format!(
                "IOCTL_DISK_UPDATE_PROPERTIES on {} failed: {e}",
                self.path
            ))
        })?;
        debug!(path = %self.path, "Partition table re-read by partmgr");
        Ok(())
    }
}

impl Drop for WinRawDevice {
    fn drop(&mut self) {
        // SAFETY: handle owned by us and closed exactly once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
