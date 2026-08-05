//! A slice of a device, seen as a volume of its own.
//!
//! Needed because a locked BitLocker volume cannot be read through its own
//! volume device: the FVE filter sits above the volume and answers every read
//! with `FVE_E_LOCKED_VOLUME` (`0x80310000`). It does not sit above the DISK,
//! so the ciphertext is there for the taking at the partition's offset — which
//! is exactly how our own inventory already recognises such a volume
//! ([`own_backup_inventory::fs_probe::probe_partition_fs`] reads
//! `\\.\PhysicalDriveN`).
//!
//! So the reader that the BitLocker parser is handed is not the volume but a
//! window onto the disk: offset 0 of the window is the first byte of the
//! partition, and reads past its end are refused rather than wandering into
//! the next partition.

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;

/// `[start, start + length)` of `inner`, addressed from zero.
pub struct VolumeWindow {
    inner: Box<dyn RawVolume + Send>,
    start: u64,
    length: u64,
}

impl VolumeWindow {
    /// Fails when the window does not fit in the device: a partition entry
    /// that points past the end of its disk is not something to read around.
    pub fn new(inner: Box<dyn RawVolume + Send>, start: u64, length: u64) -> Result<Self> {
        let device = inner.len();
        if length == 0 || start.checked_add(length).is_none_or(|end| end > device) {
            return Err(OwnBackupError::Device(format!(
                "раздел [{start}, +{length}) не помещается в устройство длиной {device}"
            )));
        }
        Ok(Self { inner, start, length })
    }
}

impl RawVolume for VolumeWindow {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| OwnBackupError::Device("длина чтения переполнилась".into()))?;
        if end > self.length {
            return Err(OwnBackupError::Device(format!(
                "чтение {} байт со смещения {offset} выходит за раздел длиной {}",
                buf.len(),
                self.length
            )));
        }
        self.inner.read_at(self.start + offset, buf)
    }

    fn len(&self) -> u64 {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            if e > self.0.len() {
                return Err(OwnBackupError::Device("read past end".into()));
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn disk() -> Box<dyn RawVolume + Send> {
        // Three 512-byte "partitions", each filled with its own byte.
        let mut v = Vec::new();
        for tag in [b'A', b'B', b'C'] {
            v.extend(std::iter::repeat_n(tag, 512));
        }
        Box::new(MemVolume(v))
    }

    #[test]
    fn a_window_reads_from_its_own_zero() {
        let mut w = VolumeWindow::new(disk(), 512, 512).unwrap();
        assert_eq!(w.len(), 512);
        let mut buf = [0u8; 4];
        w.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"BBBB", "offset 0 is the partition's first byte");
    }

    /// The whole point of a window: a parser that reads a little too far must
    /// not silently get the next partition's bytes.
    #[test]
    fn a_read_past_the_end_is_refused_not_continued() {
        let mut w = VolumeWindow::new(disk(), 512, 512).unwrap();
        let mut buf = [0u8; 8];
        assert!(w.read_at(508, &mut buf).is_err());
    }

    #[test]
    fn a_window_that_does_not_fit_is_refused_at_the_door() {
        assert!(VolumeWindow::new(disk(), 1024, 1024).is_err(), "past the device end");
        assert!(VolumeWindow::new(disk(), 0, 0).is_err(), "an empty window is a mistake");
        assert!(VolumeWindow::new(disk(), 1024, 512).is_ok());
    }
}
