//! Reads of any shape over a device that only accepts sector-aligned ones.
//!
//! Windows opens raw devices with `FILE_FLAG_NO_BUFFERING`, so every read must
//! start on a sector boundary and span whole sectors. Most of our parsers
//! already read in filesystem blocks and never notice. The ext scanner does:
//! its inodes are 128 or 256 bytes, so half of them begin mid-sector — the
//! ROOT inode among them. Every read failed, every subtree was skipped, and the
//! scan reported `excluded_files=0` while looking, in the log, like it had run:
//!
//! ```text
//! ext exclusions: unreadable directory inode — skipping subtree ino=2
//!   error=read offset 594176 is not aligned to sector size 512
//! ```
//!
//! Which means the whole ext exclusion path — Stage 1 and Stage 2 alike — had
//! never once executed on Windows, only under the Linux tests, where reads are
//! buffered and nothing complains.
//!
//! This wrapper reads the aligned window that contains the request and hands
//! back the slice. It keeps the last window, which is not an optimisation
//! detail: walking a directory reads dozens of inodes out of the same 4 KiB,
//! and without the cache each one would be a separate device round-trip.

use own_backup_common::Result;

use crate::fat_map::RawVolume;

/// Sector size to align to. 4096 covers both 512e and 4Kn devices: a read
/// aligned to 4096 is aligned to 512 as well.
const WINDOW: u64 = 4096;

pub struct AlignedVolume<V: RawVolume> {
    inner: V,
    /// Start offset of the cached window, and its bytes.
    cached: Option<(u64, Vec<u8>)>,
}

impl<V: RawVolume> AlignedVolume<V> {
    pub fn new(inner: V) -> Self {
        Self { inner, cached: None }
    }

    /// Fill `out` from the window starting at `win_start`, reading it if it is
    /// not the one already held.
    fn window(&mut self, win_start: u64, win_len: usize) -> Result<&[u8]> {
        let hit = matches!(&self.cached, Some((s, b)) if *s == win_start && b.len() >= win_len);
        if !hit {
            let mut buf = vec![0u8; win_len];
            self.inner.read_at(win_start, &mut buf)?;
            self.cached = Some((win_start, buf));
        }
        Ok(&self.cached.as_ref().unwrap().1[..win_len])
    }
}

impl<V: RawVolume> RawVolume for AlignedVolume<V> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let start = offset / WINDOW * WINDOW;
        let end = (offset + buf.len() as u64).div_ceil(WINDOW) * WINDOW;
        // A request that is already aligned and big goes straight through: no
        // point copying megabytes of filesystem blocks through a window buffer.
        if start == offset && end == offset + buf.len() as u64 && buf.len() as u64 >= WINDOW {
            return self.inner.read_at(offset, buf);
        }
        let win = self.window(start, (end - start) as usize)?;
        let from = (offset - start) as usize;
        buf.copy_from_slice(&win[from..from + buf.len()]);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::OwnBackupError;

    /// A device with Windows' manners: it refuses anything that does not start
    /// on a sector boundary or span whole sectors.
    struct PickyVolume {
        data: Vec<u8>,
        sector: u64,
        reads: usize,
    }

    impl RawVolume for PickyVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            if offset % self.sector != 0 {
                return Err(OwnBackupError::Device(format!(
                    "read offset {offset} is not aligned to sector size {}",
                    self.sector
                )));
            }
            if buf.len() as u64 % self.sector != 0 {
                return Err(OwnBackupError::Device("read length not a multiple of the sector".into()));
            }
            let o = offset as usize;
            if o + buf.len() > self.data.len() {
                return Err(OwnBackupError::Device("read past end".into()));
            }
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            self.reads += 1;
            Ok(())
        }
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
    }

    fn picky() -> PickyVolume {
        // Byte i = i mod 251, so every offset is identifiable.
        let data: Vec<u8> = (0..65536usize).map(|i| (i % 251) as u8).collect();
        PickyVolume { data, sector: 512, reads: 0 }
    }

    #[test]
    fn an_unaligned_read_reaches_the_right_bytes() {
        let mut v = AlignedVolume::new(picky());
        // The offset that broke ext on Windows: an inode 256 bytes into a block.
        let mut buf = [0u8; 256];
        v.read_at(594176 % 65536 + 256, &mut buf).unwrap();
        let base = (594176 % 65536 + 256) as usize;
        for (i, b) in buf.iter().enumerate() {
            assert_eq!(*b, ((base + i) % 251) as u8, "byte {i}");
        }
    }

    #[test]
    fn a_read_spanning_two_windows_is_stitched() {
        let mut v = AlignedVolume::new(picky());
        let off = 4090u64;
        let mut buf = [0u8; 16]; // crosses the 4096 boundary
        v.read_at(off, &mut buf).unwrap();
        for (i, b) in buf.iter().enumerate() {
            assert_eq!(*b, ((off as usize + i) % 251) as u8, "byte {i}");
        }
    }

    #[test]
    fn repeated_reads_inside_one_window_hit_the_device_once() {
        let mut v = AlignedVolume::new(picky());
        for i in 0..16u64 {
            let mut buf = [0u8; 256];
            v.read_at(i * 256, &mut buf).unwrap();
        }
        assert_eq!(v.inner.reads, 1, "16 inodes in one 4 KiB window = one device read");
    }

    #[test]
    fn a_big_aligned_read_is_passed_straight_through() {
        let mut v = AlignedVolume::new(picky());
        let mut buf = vec![0u8; 8192];
        v.read_at(4096, &mut buf).unwrap();
        assert!(v.cached.is_none(), "no window buffer for an already-aligned block");
        assert_eq!(buf[0], (4096 % 251) as u8);
    }

    #[test]
    fn without_the_wrapper_the_same_read_fails() {
        // The bug this file exists for: the device itself refuses it.
        let mut raw = picky();
        let mut buf = [0u8; 256];
        assert!(raw.read_at(594176 % 65536 + 256, &mut buf).is_err());
    }
}
