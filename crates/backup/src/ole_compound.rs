//! Reading a compound file — the container Microsoft installers keep inside
//! themselves.
//!
//! Needed for exactly one thing: VMware Tools' `setup.exe` has an MSI appended
//! to it, and the driver cabinets are streams inside that MSI. A stream is
//! stored in sectors that need not be next to each other, so the bytes of a
//! cabinet are interleaved with everything else in the file. Cutting the range
//! out with a knife gives a cabinet whose header reads perfectly — it fits in
//! the first sector — and whose data is noise; `expand` opens such a file and
//! then fails with `0x8007000D`, which is precisely what happened.
//!
//! Only reading is implemented, and only what a stream needs: header, FAT,
//! directory. Small streams (below the header's cutoff) live in a mini-stream
//! of their own and are not read — nothing we look for is that small.
//!
//! Verified against a real VMware Tools installer: 144 streams, 26 of them
//! cabinets, every data block of every one matching its own checksum.

/// The signature a compound file starts with.
pub const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const MAX_SPECIAL: u32 = 0xFFFF_FFFA;
/// Directory entry kind: a stream (2), as opposed to a storage or the root.
const KIND_STREAM: u8 = 2;

/// A compound file inside a larger file.
pub struct Compound<'a> {
    data: &'a [u8],
    base: usize,
    sector: usize,
    fat: Vec<u32>,
    dir_start: u32,
    /// Streams smaller than this live in the mini-stream and are skipped.
    mini_cutoff: u32,
}

/// One stream in the directory.
#[derive(Debug, Clone, Copy)]
pub struct StreamRef {
    pub start: u32,
    pub size: u64,
}

impl<'a> Compound<'a> {
    /// Find and open the first compound file in `data`.
    pub fn find(data: &'a [u8]) -> Option<Compound<'a>> {
        let base = data.windows(8).position(|w| w == MAGIC)?;
        Self::at(data, base)
    }

    pub fn at(data: &'a [u8], base: usize) -> Option<Compound<'a>> {
        if base + 512 > data.len() {
            return None;
        }
        let u16at =
            |o: usize| u16::from_le_bytes(data[base + o..base + o + 2].try_into().unwrap_or([0; 2]));
        let u32at =
            |o: usize| u32::from_le_bytes(data[base + o..base + o + 4].try_into().unwrap_or([0; 4]));

        let sector_shift = u16at(30) as u32;
        if !(7..=16).contains(&sector_shift) {
            return None;
        }
        let sector = 1usize << sector_shift;
        let n_fat = u32at(44) as usize;
        let dir_start = u32at(48);
        let mini_cutoff = u32at(56);
        let difat_start = u32at(68);

        let mut c = Compound {
            data,
            base,
            sector,
            fat: Vec::new(),
            dir_start,
            mini_cutoff,
        };

        // The DIFAT says which sectors hold the FAT: 109 entries in the header,
        // the rest in a chain of sectors, each ending with the next one's id.
        let mut difat: Vec<u32> = (0..109).map(|i| u32at(76 + i * 4)).collect();
        let mut next = difat_start;
        let mut guard = 0;
        while next < MAX_SPECIAL && guard < 4096 {
            let s = c.raw(next);
            if s.len() < c.sector {
                break;
            }
            for i in 0..(c.sector / 4 - 1) {
                difat.push(u32::from_le_bytes(s[i * 4..i * 4 + 4].try_into().unwrap()));
            }
            next = u32::from_le_bytes(s[c.sector - 4..].try_into().unwrap());
            guard += 1;
        }

        for &fs in difat.iter().take(n_fat) {
            if fs >= MAX_SPECIAL {
                continue;
            }
            let s = c.raw(fs);
            if s.len() < c.sector {
                break;
            }
            for i in 0..(c.sector / 4) {
                c.fat.push(u32::from_le_bytes(s[i * 4..i * 4 + 4].try_into().unwrap()));
            }
        }
        (!c.fat.is_empty()).then_some(c)
    }

    /// Byte offset of sector `n`.
    ///
    /// Sector 0 starts one WHOLE sector into the file, not 512 bytes in: with
    /// 4096-byte sectors the 512-byte header is padded out to a full sector.
    /// Reading it at 512 shifts everything by 3584 bytes and turns every chain
    /// in the file into noise — which is how this was found.
    fn offset_of(&self, n: u32) -> usize {
        self.base + (n as usize + 1) * self.sector
    }

    fn raw(&self, n: u32) -> &'a [u8] {
        let p = self.offset_of(n);
        let end = (p + self.sector).min(self.data.len());
        if p >= end {
            return &[];
        }
        &self.data[p..end]
    }

    /// The sectors of one chain, at most `max` of them.
    fn chain(&self, start: u32, max: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = start;
        while c < MAX_SPECIAL && out.len() < max {
            out.push(c);
            match self.fat.get(c as usize) {
                Some(&n) => c = n,
                None => break,
            }
        }
        out
    }

    /// Every stream in the directory that is big enough to be read here.
    pub fn streams(&self) -> Vec<StreamRef> {
        let per_sector = self.sector / 128;
        let max_dir = self.fat.len().min(1 << 20);
        let mut out = Vec::new();
        for ds in self.chain(self.dir_start, max_dir) {
            let sec = self.raw(ds);
            if sec.len() < self.sector {
                break;
            }
            for k in 0..per_sector {
                let e = &sec[k * 128..(k + 1) * 128];
                let name_len = u16::from_le_bytes(e[64..66].try_into().unwrap());
                if name_len < 2 || e[66] != KIND_STREAM {
                    continue;
                }
                let start = u32::from_le_bytes(e[116..120].try_into().unwrap());
                let size = u64::from_le_bytes(e[120..128].try_into().unwrap());
                if size < self.mini_cutoff as u64 || size > u32::MAX as u64 {
                    continue;
                }
                out.push(StreamRef { start, size });
            }
        }
        out
    }

    /// The bytes of one stream, put back together in order.
    pub fn read(&self, s: StreamRef) -> Vec<u8> {
        let need = (s.size as usize).div_ceil(self.sector);
        let mut out = Vec::with_capacity(s.size as usize);
        for c in self.chain(s.start, need) {
            out.extend_from_slice(self.raw(c));
        }
        out.truncate(s.size as usize);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule that cost an evening: with 4096-byte sectors, sector 0 is at
    /// 4096, not at 512.
    #[test]
    fn sector_zero_is_a_whole_sector_in() {
        let mut data = vec![0u8; 4096 * 4];
        data[..8].copy_from_slice(&MAGIC);
        data[30..32].copy_from_slice(&12u16.to_le_bytes()); // 4096-byte sectors
        data[44..48].copy_from_slice(&1u32.to_le_bytes()); // one FAT sector
        data[48..52].copy_from_slice(&1u32.to_le_bytes()); // directory at sector 1
        data[56..60].copy_from_slice(&4096u32.to_le_bytes()); // mini cutoff
        data[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        data[76..80].copy_from_slice(&0u32.to_le_bytes()); // FAT is sector 0
        let c = Compound::find(&data).expect("header should parse");
        assert_eq!(c.offset_of(0), 4096);
        assert_eq!(c.offset_of(1), 8192);
    }

    /// A stream whose sectors are out of order — which is the whole reason this
    /// module exists.
    #[test]
    fn a_scattered_stream_is_put_back_in_order() {
        let sector = 512usize;
        let sectors = 8;
        let mut data = vec![0u8; sector * (sectors + 1)];
        data[..8].copy_from_slice(&MAGIC);
        data[30..32].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
        data[44..48].copy_from_slice(&1u32.to_le_bytes());
        data[48..52].copy_from_slice(&1u32.to_le_bytes()); // directory: sector 1
        data[56..60].copy_from_slice(&512u32.to_le_bytes());
        data[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        data[76..80].copy_from_slice(&0u32.to_le_bytes()); // FAT: sector 0

        let put = |data: &mut Vec<u8>, n: usize, off: usize, bytes: &[u8]| {
            let p = (n + 1) * sector + off;
            data[p..p + bytes.len()].copy_from_slice(bytes);
        };
        // FAT: 2 → 5 → 3 → end. Everything else free.
        let mut fat = vec![0xFFFF_FFFFu32; sector / 4];
        fat[0] = 0xFFFF_FFFD; // the FAT sector itself
        fat[1] = END_OF_CHAIN; // directory
        fat[2] = 5;
        fat[5] = 3;
        fat[3] = END_OF_CHAIN;
        for (i, v) in fat.iter().enumerate() {
            put(&mut data, 0, i * 4, &v.to_le_bytes());
        }
        // Directory entry 0: a stream of 1100 bytes starting at sector 2.
        put(&mut data, 1, 64, &4u16.to_le_bytes()); // name length
        put(&mut data, 1, 66, &[KIND_STREAM]);
        put(&mut data, 1, 116, &2u32.to_le_bytes());
        put(&mut data, 1, 120, &1100u64.to_le_bytes());
        // The payload, marked so the order is checkable.
        for (n, tag) in [(2usize, b'A'), (5, b'B'), (3, b'C')] {
            put(&mut data, n, 0, &[tag; 512][..]);
        }

        let c = Compound::find(&data).unwrap();
        let streams = c.streams();
        assert_eq!(streams.len(), 1);
        let blob = c.read(streams[0]);
        assert_eq!(blob.len(), 1100);
        assert!(blob[..512].iter().all(|&b| b == b'A'));
        assert!(blob[512..1024].iter().all(|&b| b == b'B'));
        assert!(blob[1024..].iter().all(|&b| b == b'C'), "the third sector must follow");
    }
}
