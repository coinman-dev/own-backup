//! Loop devices through their ioctls (`/dev/loop-control`, `LOOP_CONFIGURE`),
//! without `losetup`: the copy-on-write store of a device-mapper snapshot is a
//! file, and device-mapper needs a block device.

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use own_backup_common::linux_block;

const LOOP_SET_FD: u32 = 0x4C00;
const LOOP_CLR_FD: u32 = 0x4C01;
const LOOP_SET_STATUS64: u32 = 0x4C04;
const LOOP_CONFIGURE: u32 = 0x4C0A;
const LOOP_CTL_GET_FREE: u32 = 0x4C82;
/// Detach by itself once the last opener closes it — device-mapper included.
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LO_NAME_SIZE: usize = 64;

/// `struct loop_info64` (232 bytes).
#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; LO_NAME_SIZE],
    lo_crypt_name: [u8; LO_NAME_SIZE],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

/// `struct loop_config` (304 bytes).
#[repr(C)]
struct LoopConfig {
    fd: u32,
    block_size: u32,
    info: LoopInfo64,
    reserved: [u64; 8],
}

fn info_for(path: &Path) -> LoopInfo64 {
    let mut name = [0u8; LO_NAME_SIZE];
    let bytes = path.as_os_str().as_encoded_bytes();
    let n = bytes.len().min(LO_NAME_SIZE - 1);
    name[..n].copy_from_slice(&bytes[..n]);
    LoopInfo64 {
        lo_device: 0,
        lo_inode: 0,
        lo_rdevice: 0,
        lo_offset: 0,
        lo_sizelimit: 0,
        lo_number: 0,
        lo_encrypt_type: 0,
        lo_encrypt_key_size: 0,
        lo_flags: LO_FLAGS_AUTOCLEAR,
        lo_file_name: name,
        lo_crypt_name: [0; LO_NAME_SIZE],
        lo_encrypt_key: [0; 32],
        lo_init: [0; 2],
    }
}

fn ioctl_ptr<T>(fd: &File, req: u32, arg: *mut T) -> io::Result<i32> {
    // SAFETY: `arg` points at a live value of the type the request expects.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn ioctl_int(fd: &File, req: u32, arg: libc::c_ulong) -> io::Result<i32> {
    // SAFETY: requests that take an integer argument (or none).
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// An attached loop device, held open for as long as this value lives. Its
/// autoclear flag detaches it when the last opener is gone — so a killed run
/// leaves it attached only while device-mapper still holds it.
pub struct LoopDevice {
    file: File,
    index: u32,
    dev: (u32, u32),
}

impl LoopDevice {
    /// Attach `backing` (opened read-write) as a free loop device.
    pub fn attach(backing: &File, path: &Path) -> io::Result<LoopDevice> {
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/loop-control")?;
        let mut last = io::Error::other("no free loop device");
        // Another process may take the free device between the two calls.
        for _ in 0..16 {
            let index = ioctl_int(&control, LOOP_CTL_GET_FREE, 0)? as u32;
            let dev_path = format!("/dev/loop{index}");
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&dev_path)
            {
                Ok(f) => f,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let mut config = LoopConfig {
                fd: backing.as_raw_fd() as u32,
                block_size: 0,
                info: info_for(path),
                reserved: [0; 8],
            };
            let attached = match ioctl_ptr(&file, LOOP_CONFIGURE, &mut config) {
                Ok(_) => Ok(()),
                // Kernels before 5.8: the two-step form.
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) => {
                    ioctl_int(&file, LOOP_SET_FD, backing.as_raw_fd() as libc::c_ulong).and_then(
                        |_| {
                            let mut info = info_for(path);
                            ioctl_ptr(&file, LOOP_SET_STATUS64, &mut info)
                                .map(|_| ())
                                .inspect_err(|_| {
                                    let _ = ioctl_int(&file, LOOP_CLR_FD, 0);
                                })
                        },
                    )
                }
                Err(e) => Err(e),
            };
            match attached {
                Ok(()) => {
                    let rdev = file.metadata()?.rdev();
                    return Ok(LoopDevice {
                        file,
                        index,
                        dev: linux_block::split_dev(rdev),
                    });
                }
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    pub fn dev(&self) -> (u32, u32) {
        self.dev
    }

    pub fn path(&self) -> String {
        format!("/dev/loop{}", self.index)
    }

    /// Detach it now (it is then free even if someone else still has it open).
    pub fn detach(&self) -> io::Result<()> {
        match ioctl_int(&self.file, LOOP_CLR_FD, 0) {
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => Ok(()), // already free
            other => other.map(|_| ()),
        }
    }
}

/// The file behind loop device `kernel` (`loop3`), from sysfs; `None` when it
/// is not attached. A deleted file is reported by its former path.
pub fn backing_file(sys: &Path, kernel: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(sys.join(kernel).join("loop/backing_file")).ok()?;
    let text = text.trim_end_matches('\n');
    let text = text.strip_suffix(" (deleted)").unwrap_or(text);
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Detach loop device `kernel` if it is still backed by `expected` — checked
/// right before, so a device reused by someone else meanwhile is left alone.
pub fn detach_if_backed_by(sys: &Path, kernel: &str, expected: &Path) -> io::Result<bool> {
    if backing_file(sys, kernel).as_deref() != Some(expected) {
        return Ok(false);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(format!("/dev/{kernel}"))?;
    if backing_file(sys, kernel).as_deref() != Some(expected) {
        return Ok(false);
    }
    match ioctl_int(&file, LOOP_CLR_FD, 0) {
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => Ok(false),
        other => other.map(|_| true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_kernel_header() {
        assert_eq!(std::mem::size_of::<LoopInfo64>(), 232);
        assert_eq!(std::mem::size_of::<LoopConfig>(), 304);
        assert_eq!(std::mem::offset_of!(LoopInfo64, lo_flags), 52);
        assert_eq!(std::mem::offset_of!(LoopInfo64, lo_file_name), 56);
        assert_eq!(std::mem::offset_of!(LoopConfig, info), 8);
    }

    #[test]
    fn long_names_are_cut_to_fit() {
        let long = PathBuf::from(format!("/{}", "d".repeat(200)));
        let info = info_for(&long);
        assert_eq!(info.lo_file_name[LO_NAME_SIZE - 1], 0);
        assert_eq!(info.lo_flags, LO_FLAGS_AUTOCLEAR);
    }

    #[test]
    fn backing_file_from_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("loop3/loop")).unwrap();
        std::fs::write(
            dir.path().join("loop3/loop/backing_file"),
            "/mnt/img/.own-backup-cow-ab.tmp (deleted)\n",
        )
        .unwrap();
        assert_eq!(
            backing_file(dir.path(), "loop3"),
            Some(PathBuf::from("/mnt/img/.own-backup-cow-ab.tmp"))
        );
        assert_eq!(backing_file(dir.path(), "loop4"), None);
    }
}
