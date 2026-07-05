//! Raw block-device I/O on Linux — the `/dev` analogue of [`crate::win_device`].
//!
//! Uses positional `pread`/`pwrite` ([`std::os::unix::fs::FileExt`]), so reads
//! and writes need no cursor and no sector alignment (buffered I/O). Device
//! length comes from `BLKGETSIZE64` for block devices and from the file size for
//! regular files (backup images / tests). Implements [`crate::fat_map::RawVolume`]
//! so the filesystem parsers and the block reader work over it unchanged.

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::os::unix::io::AsRawFd;

use own_backup_common::{OwnBackupError, Result};

// asm-generic ioctl numbers (x86_64 / arm64).
const BLKSSZGET: libc::c_ulong = 0x1268; // _IO(0x12, 104)  -> logical sector size (int)
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272; // _IOR(0x12, 114, size_t) -> u64 bytes

/// A block device or image file opened for raw positional I/O.
pub struct LinuxRawDevice {
    file: File,
    path: String,
    length: u64,
    sector_size: u32,
}

// SAFETY: the File is owned; all access is positional and thread-safe.
unsafe impl Send for LinuxRawDevice {}

impl LinuxRawDevice {
    /// Open `path` (`/dev/sdb2`, a loop device, or an image file) for reading.
    pub fn open_read(path: &str) -> Result<Self> {
        Self::open(OpenOptions::new().read(true), path)
    }

    /// Open `path` for reading and writing (restore target / image writer).
    pub fn open_write(path: &str) -> Result<Self> {
        Self::open(OpenOptions::new().read(true).write(true), path)
    }

    fn open(opts: &OpenOptions, path: &str) -> Result<Self> {
        let file = opts
            .open(path)
            .map_err(|e| OwnBackupError::Device(format!("open({path}): {e}")))?;
        let (length, sector_size) = geometry(&file, path)?;
        Ok(Self { file, path: path.to_string(), length, sector_size })
    }

    /// Accessible length in bytes.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Logical sector size in bytes (512 for images / on failure).
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Read exactly `buf.len()` bytes at `offset`.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.length {
            return Err(OwnBackupError::Device(format!(
                "read of {} bytes at {offset} exceeds device length {}",
                buf.len(),
                self.length
            )));
        }
        self.file
            .read_exact_at(buf, offset)
            .map_err(|e| OwnBackupError::Device(format!("read({}) at {offset}: {e}", self.path)))
    }

    /// Write all of `data` at `offset`.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if offset + data.len() as u64 > self.length {
            return Err(OwnBackupError::Device(format!(
                "write of {} bytes at {offset} exceeds device length {}",
                data.len(),
                self.length
            )));
        }
        self.file
            .write_all_at(data, offset)
            .map_err(|e| OwnBackupError::Device(format!("write({}) at {offset}: {e}", self.path)))
    }

    /// Flush written data to the device.
    pub fn flush(&mut self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(|e| OwnBackupError::Device(format!("fsync({}): {e}", self.path)))
    }
}

/// Length (bytes) and logical sector size of an opened device/file.
fn geometry(file: &File, path: &str) -> Result<(u64, u32)> {
    let meta = file
        .metadata()
        .map_err(|e| OwnBackupError::Device(format!("stat({path}): {e}")))?;

    if !meta.file_type().is_block_device() {
        // Regular file (image / test): size from metadata, assume 512 sectors.
        return Ok((meta.len(), 512));
    }

    let fd = file.as_raw_fd();
    let mut size: u64 = 0;
    // SAFETY: fd is valid for the call; `size` is a u64 out-param BLKGETSIZE64 fills.
    if unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size as *mut u64) } != 0 {
        return Err(OwnBackupError::Device(format!(
            "BLKGETSIZE64({path}): {}",
            std::io::Error::last_os_error()
        )));
    }
    let mut ss: libc::c_int = 0;
    // SAFETY: as above; failure is tolerated (default 512).
    let sector = if unsafe { libc::ioctl(fd, BLKSSZGET, &mut ss as *mut libc::c_int) } == 0 && ss > 0
    {
        ss as u32
    } else {
        512
    };
    Ok((size, sector))
}

impl crate::fat_map::RawVolume for LinuxRawDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        LinuxRawDevice::read_at(self, offset, buf)
    }
    fn len(&self) -> u64 {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fat_map::RawVolume;
    use std::io::Write;

    // End-to-end: LinuxRawDevice + the real ext4 used-block map on a genuine
    // ext4 image. `cargo test -p own-backup-backup -- --ignored` (needs mkfs.ext4).
    #[test]
    #[ignore]
    fn ext_map_over_real_ext4_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.ext4");
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(128 * 1024 * 1024).unwrap();
        drop(f);
        assert!(std::process::Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "reallbl", img.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        let mut dev = LinuxRawDevice::open_read(img.to_str().unwrap()).unwrap();
        let total = dev.length().div_ceil(1024 * 1024);
        let map = crate::ext_map::build_ext_used_map(&mut dev, 1024 * 1024).unwrap();
        let used = crate::used_sectors::UsedSectorMap::used_block_count(&map);
        eprintln!("ext4 128 MiB: used {used} of {total} 1-MiB blocks");
        assert!(used > 0 && used < total, "smart map should skip free space");
    }

    #[test]
    fn reads_and_writes_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.bin");
        let data: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        std::fs::File::create(&path).unwrap().write_all(&data).unwrap();

        let mut dev = LinuxRawDevice::open_read(path.to_str().unwrap()).unwrap();
        assert_eq!(dev.length(), 4096);
        assert_eq!(dev.sector_size(), 512);

        // Positional read via the inherent method and via the RawVolume trait.
        let mut buf = [0u8; 256];
        dev.read_at(1000, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[1000..1256]);
        let mut buf2 = [0u8; 256];
        RawVolume::read_at(&mut dev, 1000, &mut buf2).unwrap();
        assert_eq!(buf, buf2);

        // Reading past the end fails cleanly.
        let mut tail = [0u8; 200];
        assert!(dev.read_at(4000, &mut tail).is_err());

        // Write-back through a writable handle.
        let mut w = LinuxRawDevice::open_write(path.to_str().unwrap()).unwrap();
        w.write_at(0, &[0xAB; 16]).unwrap();
        w.flush().unwrap();
        let mut check = [0u8; 16];
        w.read_at(0, &mut check).unwrap();
        assert_eq!(check, [0xAB; 16]);
    }
}
