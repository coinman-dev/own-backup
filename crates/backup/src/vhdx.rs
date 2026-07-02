//! Native VHDX handling via the Windows Virtual Disk API (`virtdisk.dll`).
//!
//! VHDX is the native backup container (decision 2026-07-01):
//! - dynamic VHDX stores only written regions → together with the NTFS
//!   used-sector map, free space is never written;
//! - differencing VHDX (AVHDX) gives incremental/differential chains;
//! - `AttachVirtualDisk` mounts a backup as a real disk (read-only by
//!   default), `MergeVirtualDisk` consolidates chains.
//!
//! Backup writes go through [`AttachedVhdx::physical_path`] +
//! [`crate::win_device::WinRawDevice`] — the image is attached without a
//! drive letter and written to as a raw disk.

#![cfg(target_os = "windows")]

use std::path::Path;

use tracing::{debug, info};
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WIN32_ERROR};
use windows::Win32::Storage::Vhd::{
    AttachVirtualDisk, CreateVirtualDisk, DetachVirtualDisk, GetVirtualDiskPhysicalPath,
    MergeVirtualDisk, OpenVirtualDisk, ATTACH_VIRTUAL_DISK_FLAG,
    ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER,
    ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME, ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
    CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
    CREATE_VIRTUAL_DISK_VERSION_2, DETACH_VIRTUAL_DISK_FLAG_NONE,
    MERGE_VIRTUAL_DISK_FLAG_NONE, MERGE_VIRTUAL_DISK_PARAMETERS, MERGE_VIRTUAL_DISK_VERSION_1,
    OPEN_VIRTUAL_DISK_FLAG_NONE, OPEN_VIRTUAL_DISK_PARAMETERS, OPEN_VIRTUAL_DISK_VERSION_1,
    VIRTUAL_DISK_ACCESS_MASK, VIRTUAL_DISK_ACCESS_ATTACH_RO, VIRTUAL_DISK_ACCESS_ATTACH_RW,
    VIRTUAL_DISK_ACCESS_DETACH, VIRTUAL_DISK_ACCESS_GET_INFO, VIRTUAL_DISK_ACCESS_METAOPS,
    VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE, VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
    VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
};

use own_backup_common::{OwnBackupError, Result};

use crate::win_device::to_wide;

fn vhdx_storage_type() -> VIRTUAL_STORAGE_TYPE {
    VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    }
}

fn check(err: WIN32_ERROR, what: &str) -> Result<()> {
    if err.is_ok() {
        Ok(())
    } else {
        Err(OwnBackupError::Vhdx(format!(
            "{what} failed: {}",
            std::io::Error::from_raw_os_error(err.0 as i32)
        )))
    }
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| OwnBackupError::Vhdx(format!("non-UTF8 VHDX path: {}", path.display())))
}

/// Options for creating a new dynamic VHDX.
pub struct VhdxCreateOptions {
    /// Virtual disk size in bytes; rounded up to a whole MiB internally.
    pub max_size: u64,
    /// Logical sector size of the virtual disk — MUST match the source device
    /// (512 or 4096), otherwise NTFS inside the image will not mount cleanly.
    pub logical_sector_size: u32,
    /// Physical sector size to report (512 or 4096).
    pub physical_sector_size: u32,
}

/// Create a new empty **dynamic** VHDX at `path`.
pub fn create_dynamic_vhdx(path: &Path, opts: &VhdxCreateOptions) -> Result<()> {
    const MIB: u64 = 1024 * 1024;
    let max_size = opts.max_size.div_ceil(MIB) * MIB;

    let wide = to_wide(path_str(path)?);
    let storage_type = vhdx_storage_type();

    let mut params = CREATE_VIRTUAL_DISK_PARAMETERS::default();
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.UniqueId = GUID::zeroed(); // let the system generate one
    params.Anonymous.Version2.MaximumSize = max_size;
    // 1 MiB VHDX block (minimum): the default 32 MiB allocates a full 32 MiB
    // region for every touched block and bloats sparse images.
    params.Anonymous.Version2.BlockSizeInBytes = 1024 * 1024;
    params.Anonymous.Version2.SectorSizeInBytes = opts.logical_sector_size;
    params.Anonymous.Version2.PhysicalSectorSizeInBytes = opts.physical_sector_size;

    let mut handle = HANDLE::default();
    // SAFETY: all pointers valid for the duration of the call.
    let err = unsafe {
        CreateVirtualDisk(
            &storage_type,
            PCWSTR(wide.as_ptr()),
            VIRTUAL_DISK_ACCESS_NONE,
            None,
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            None,
            &mut handle,
        )
    };
    check(err, &format!("CreateVirtualDisk({})", path.display()))?;
    // SAFETY: handle returned by a successful CreateVirtualDisk.
    let _ = unsafe { CloseHandle(handle) };

    info!(path = %path.display(), size = max_size, "Created dynamic VHDX");
    Ok(())
}

/// Create a **differencing** VHDX (AVHDX) at `path` with `parent` as its base.
///
/// Incremental chain: parent = the previous backup version.
/// Differential: parent = the full backup.
pub fn create_differencing_vhdx(path: &Path, parent: &Path) -> Result<()> {
    let wide = to_wide(path_str(path)?);
    let parent_wide = to_wide(path_str(parent)?);
    let storage_type = vhdx_storage_type();

    let mut params = CREATE_VIRTUAL_DISK_PARAMETERS::default();
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.ParentPath = PCWSTR(parent_wide.as_ptr());

    let mut handle = HANDLE::default();
    // SAFETY: all pointers (incl. parent_wide) valid for the duration of the call.
    let err = unsafe {
        CreateVirtualDisk(
            &storage_type,
            PCWSTR(wide.as_ptr()),
            VIRTUAL_DISK_ACCESS_NONE,
            None,
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            None,
            &mut handle,
        )
    };
    check(err, &format!("CreateVirtualDisk(diff {})", path.display()))?;
    // SAFETY: handle returned by a successful CreateVirtualDisk.
    let _ = unsafe { CloseHandle(handle) };

    info!(path = %path.display(), parent = %parent.display(), "Created differencing VHDX");
    Ok(())
}

fn open_vhdx(path: &Path, access: VIRTUAL_DISK_ACCESS_MASK, rw_depth: u32) -> Result<HANDLE> {
    let wide = to_wide(path_str(path)?);
    let storage_type = vhdx_storage_type();

    let mut params = OPEN_VIRTUAL_DISK_PARAMETERS::default();
    params.Version = OPEN_VIRTUAL_DISK_VERSION_1;
    params.Anonymous.Version1.RWDepth = rw_depth;

    let mut handle = HANDLE::default();
    // SAFETY: all pointers valid for the duration of the call.
    let err = unsafe {
        OpenVirtualDisk(
            &storage_type,
            PCWSTR(wide.as_ptr()),
            access,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            Some(&params),
            &mut handle,
        )
    };
    check(err, &format!("OpenVirtualDisk({})", path.display()))?;
    Ok(handle)
}

/// How to attach a VHDX.
pub enum AttachMode {
    /// Read-write, no drive letter — used while writing a backup into the image.
    /// Auto-detaches when the [`AttachedVhdx`] is dropped.
    RawWrite,
    /// Read-only, volumes get mounted (drive letters assigned by mountmgr).
    /// Survives process exit (`PERMANENT_LIFETIME`); detach explicitly via
    /// [`detach_vhdx`].
    ReadOnlyMounted,
    /// Read-only, no drive letter — for verify passes.
    /// Auto-detaches when the [`AttachedVhdx`] is dropped.
    RawRead,
}

/// An attached virtual disk. Detaches on drop unless attached with
/// `PERMANENT_LIFETIME` (see [`AttachMode`]).
pub struct AttachedVhdx {
    handle: HANDLE,
    physical_path: String,
    detach_on_drop: bool,
}

// SAFETY: the HANDLE is owned exclusively by this struct.
unsafe impl Send for AttachedVhdx {}

impl AttachedVhdx {
    /// `\\.\PhysicalDriveN` path of the attached virtual disk. Open it with
    /// [`crate::win_device::WinRawDevice`] for raw block I/O.
    pub fn physical_path(&self) -> &str {
        &self.physical_path
    }
}

impl Drop for AttachedVhdx {
    fn drop(&mut self) {
        if self.detach_on_drop {
            // SAFETY: handle is valid; detach failure at teardown is non-fatal.
            let _ = unsafe { DetachVirtualDisk(self.handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
        }
        // SAFETY: handle owned by us and closed exactly once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Attach a VHDX and return the attachment (with its physical disk path).
pub fn attach_vhdx(path: &Path, mode: AttachMode) -> Result<AttachedVhdx> {
    let (access, flags, detach_on_drop) = match mode {
        AttachMode::RawWrite => (
            VIRTUAL_DISK_ACCESS_MASK(
                VIRTUAL_DISK_ACCESS_ATTACH_RW.0 | VIRTUAL_DISK_ACCESS_GET_INFO.0,
            ),
            ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER,
            true,
        ),
        AttachMode::ReadOnlyMounted => (
            VIRTUAL_DISK_ACCESS_MASK(
                VIRTUAL_DISK_ACCESS_ATTACH_RO.0 | VIRTUAL_DISK_ACCESS_GET_INFO.0,
            ),
            ATTACH_VIRTUAL_DISK_FLAG(
                ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY.0
                    | ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME.0,
            ),
            false,
        ),
        AttachMode::RawRead => (
            VIRTUAL_DISK_ACCESS_MASK(
                VIRTUAL_DISK_ACCESS_ATTACH_RO.0 | VIRTUAL_DISK_ACCESS_GET_INFO.0,
            ),
            ATTACH_VIRTUAL_DISK_FLAG(
                ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY.0
                    | ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER.0,
            ),
            true,
        ),
    };

    let handle = open_vhdx(path, access, 1)?;

    // SAFETY: handle is valid; no params needed (defaults).
    let err = unsafe { AttachVirtualDisk(handle, None, flags, 0, None, None) };
    if let Err(e) = check(err, &format!("AttachVirtualDisk({})", path.display())) {
        // SAFETY: handle valid, close before propagating.
        let _ = unsafe { CloseHandle(handle) };
        return Err(e);
    }

    let physical_path = match physical_path_of(handle) {
        Ok(p) => p,
        Err(e) => {
            // SAFETY: best-effort cleanup of a just-attached disk.
            let _ = unsafe { DetachVirtualDisk(handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
            let _ = unsafe { CloseHandle(handle) };
            return Err(e);
        }
    };

    debug!(path = %path.display(), physical = %physical_path, "VHDX attached");
    Ok(AttachedVhdx {
        handle,
        physical_path,
        detach_on_drop,
    })
}

fn physical_path_of(handle: HANDLE) -> Result<String> {
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer valid for `size` bytes.
    let err =
        unsafe { GetVirtualDiskPhysicalPath(handle, &mut size, PWSTR(buf.as_mut_ptr())) };
    check(err, "GetVirtualDiskPhysicalPath")?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// Detach a VHDX that was attached with `PERMANENT_LIFETIME` (user-mounted).
pub fn detach_vhdx(path: &Path) -> Result<()> {
    let handle = open_vhdx(path, VIRTUAL_DISK_ACCESS_DETACH, 1)?;
    // SAFETY: handle is valid.
    let err = unsafe { DetachVirtualDisk(handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
    let result = check(err, &format!("DetachVirtualDisk({})", path.display()));
    // SAFETY: handle owned here, closed exactly once.
    let _ = unsafe { CloseHandle(handle) };
    result?;
    info!(path = %path.display(), "VHDX detached");
    Ok(())
}

/// Merge a differencing VHDX one level down into its parent (retention
/// consolidation). The child file becomes obsolete after a successful merge
/// and should be deleted by the caller (together with its metadata).
pub fn merge_into_parent(child: &Path) -> Result<()> {
    // RWDepth = 2: the child and its immediate parent are opened for write.
    let handle = open_vhdx(
        child,
        VIRTUAL_DISK_ACCESS_MASK(VIRTUAL_DISK_ACCESS_METAOPS.0 | VIRTUAL_DISK_ACCESS_GET_INFO.0),
        2,
    )?;

    let mut params = MERGE_VIRTUAL_DISK_PARAMETERS::default();
    params.Version = MERGE_VIRTUAL_DISK_VERSION_1;
    params.Anonymous.Version1.MergeDepth = 1;

    // SAFETY: handle and params valid for the duration of the call.
    let err = unsafe { MergeVirtualDisk(handle, MERGE_VIRTUAL_DISK_FLAG_NONE, &params, None) };
    let result = check(err, &format!("MergeVirtualDisk({})", child.display()));
    // SAFETY: handle owned here, closed exactly once.
    let _ = unsafe { CloseHandle(handle) };
    result?;
    info!(child = %child.display(), "Differencing VHDX merged into parent");
    Ok(())
}
