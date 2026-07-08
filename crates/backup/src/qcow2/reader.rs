//! Random-access qcow2 reader for verify/restore/mount. Read-only: refcount
//! structures are ignored. Rejects features we do not support (snapshots,
//! encryption, unknown incompatible bits, deflate-compressed clusters) with
//! explicit errors rather than misreading data.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use own_backup_common::{OwnBackupError, Result};

use super::{
    bad, decode_l2_entry, ClusterDescriptor, COMPRESSION_TYPE_ZSTD, INCOMPAT_COMPRESSION_TYPE,
    QCOW2_MAGIC,
};

/// Cap on cached L2 tables (each table = one cluster; 32 × 64 KiB = 2 MiB).
const L2_CACHE_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    /// No compressed clusters may occur (we refuse deflate on sight).
    Deflate,
    Zstd,
}

pub struct Qcow2Reader<R: Read + Seek> {
    r: R,
    cluster_bits: u32,
    cluster_size: u64,
    virtual_size: u64,
    compression: Compression,
    backing_file: Option<String>,
    l1: Vec<u64>,
    l2_per_table: u64,
    l2_cache: HashMap<u64, Vec<u64>>,
}

impl<R: Read + Seek> Qcow2Reader<R> {
    pub fn open(mut r: R) -> Result<Self> {
        r.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; 112];
        r.read_exact(&mut h[..72])?;

        let magic = u32::from_be_bytes(h[0..4].try_into().unwrap());
        if magic != QCOW2_MAGIC {
            return Err(bad("bad magic — not a qcow2 image"));
        }
        let version = u32::from_be_bytes(h[4..8].try_into().unwrap());
        if !(2..=3).contains(&version) {
            return Err(bad(format!("unsupported version {version}")));
        }

        let backing_file_offset = u64::from_be_bytes(h[8..16].try_into().unwrap());
        let backing_file_size = u32::from_be_bytes(h[16..20].try_into().unwrap());
        let cluster_bits = u32::from_be_bytes(h[20..24].try_into().unwrap());
        if !(9..=21).contains(&cluster_bits) {
            return Err(bad(format!("cluster_bits {cluster_bits} out of range")));
        }
        let virtual_size = u64::from_be_bytes(h[24..32].try_into().unwrap());
        let crypt_method = u32::from_be_bytes(h[32..36].try_into().unwrap());
        if crypt_method != 0 {
            return Err(bad(format!("encrypted image (crypt_method {crypt_method}) not supported")));
        }
        let l1_size = u32::from_be_bytes(h[36..40].try_into().unwrap());
        let l1_table_offset = u64::from_be_bytes(h[40..48].try_into().unwrap());
        let nb_snapshots = u32::from_be_bytes(h[60..64].try_into().unwrap());
        if nb_snapshots != 0 {
            return Err(bad("images with internal snapshots are not supported"));
        }

        // Version 3 tail: features, refcount order, header length, compression.
        let mut compression = Compression::Deflate;
        if version >= 3 {
            r.read_exact(&mut h[72..104])?;
            let incompatible = u64::from_be_bytes(h[72..80].try_into().unwrap());
            let header_length = u32::from_be_bytes(h[100..104].try_into().unwrap());
            let unknown = incompatible & !INCOMPAT_COMPRESSION_TYPE;
            if unknown != 0 {
                return Err(bad(format!(
                    "unknown incompatible features {unknown:#x} — refusing to read"
                )));
            }
            if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 {
                if header_length < 105 {
                    return Err(bad("compression-type feature set but header too short"));
                }
                r.read_exact(&mut h[104..105])?;
                if h[104] != COMPRESSION_TYPE_ZSTD {
                    return Err(bad(format!(
                        "compression type {} not supported (only zstd)",
                        h[104]
                    )));
                }
                compression = Compression::Zstd;
            }
        }

        let backing_file = if backing_file_offset != 0 {
            if backing_file_size == 0 || backing_file_size > 1023 {
                return Err(bad(format!("backing file name size {backing_file_size} out of range")));
            }
            let mut name = vec![0u8; backing_file_size as usize];
            r.seek(SeekFrom::Start(backing_file_offset))?;
            r.read_exact(&mut name)?;
            Some(String::from_utf8(name).map_err(|_| bad("backing file name is not UTF-8"))?)
        } else {
            None
        };

        let cluster_size = 1u64 << cluster_bits;
        // Defensive cap mirroring the sidecar reader: a 1 PiB disk with 64 KiB
        // clusters needs a 32 Mi-entry L1; anything larger is corruption.
        if l1_size > (1 << 25) {
            return Err(bad(format!("L1 size {l1_size} out of range")));
        }
        let mut l1_raw = vec![0u8; l1_size as usize * 8];
        r.seek(SeekFrom::Start(l1_table_offset))?;
        r.read_exact(&mut l1_raw)?;
        let l1: Vec<u64> = l1_raw
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
            .collect();

        Ok(Self {
            r,
            cluster_bits,
            cluster_size,
            virtual_size,
            compression,
            backing_file,
            l1,
            l2_per_table: cluster_size / 8,
            l2_cache: HashMap::new(),
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    pub fn total_clusters(&self) -> u64 {
        self.virtual_size.div_ceil(self.cluster_size)
    }

    /// Backing (parent) image path as stored in the header, if any.
    /// Resolution is the caller's job (chain walking lives with `chains.rs`).
    pub fn backing_file(&self) -> Option<&str> {
        self.backing_file.as_deref()
    }

    /// What this file itself says about a guest cluster (no backing-file
    /// fall-through — the caller resolves chains).
    pub fn descriptor(&mut self, index: u64) -> Result<ClusterDescriptor> {
        if index >= self.total_clusters() {
            return Err(bad(format!("cluster {index} beyond virtual disk")));
        }
        let l1_index = index / self.l2_per_table;
        let l1_entry = match self.l1.get(l1_index as usize) {
            Some(&e) => e,
            None => return Ok(ClusterDescriptor::Unallocated),
        };
        let l2_offset = l1_entry & super::OFFSET_MASK;
        if l2_offset == 0 {
            return Ok(ClusterDescriptor::Unallocated);
        }
        let entry = self.l2_lookup(l1_index, l2_offset, index % self.l2_per_table)?;
        decode_l2_entry(entry, self.cluster_bits)
    }

    /// Read one guest cluster. `Some(data)` for allocated and explicit-zero
    /// clusters, `None` for unallocated ones (backing file / zeros).
    pub fn read_cluster(&mut self, index: u64) -> Result<Option<Vec<u8>>> {
        match self.descriptor(index)? {
            ClusterDescriptor::Unallocated => Ok(None),
            ClusterDescriptor::Zero => Ok(Some(vec![0u8; self.cluster_size as usize])),
            ClusterDescriptor::Standard { host_offset } => {
                let mut data = vec![0u8; self.cluster_size as usize];
                self.r.seek(SeekFrom::Start(host_offset))?;
                self.r.read_exact(&mut data)?;
                Ok(Some(data))
            }
            ClusterDescriptor::Compressed { host_offset, length } => {
                if self.compression == Compression::Deflate {
                    return Err(bad(
                        "deflate-compressed cluster — only zstd qcow2 images are supported",
                    ));
                }
                let mut compressed = vec![0u8; length as usize];
                self.r.seek(SeekFrom::Start(host_offset))?;
                self.r.read_exact(&mut compressed)?;
                self.decompress_zstd(&compressed)
            }
        }
    }

    /// The stored `length` is rounded up to the end of a 512-byte sector, so
    /// the buffer may carry trailing garbage — decode a single frame and stop
    /// at exactly one cluster of output.
    fn decompress_zstd(&self, compressed: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut decoder = zstd::stream::read::Decoder::new(compressed)
            .map_err(|e| OwnBackupError::Compression(e.to_string()))?
            .single_frame();
        let mut data = vec![0u8; self.cluster_size as usize];
        decoder
            .read_exact(&mut data)
            .map_err(|e| OwnBackupError::Compression(format!("zstd cluster: {e}")))?;
        Ok(Some(data))
    }

    fn l2_lookup(&mut self, l1_index: u64, l2_offset: u64, slot: u64) -> Result<u64> {
        if let Some(table) = self.l2_cache.get(&l1_index) {
            return Ok(table[slot as usize]);
        }
        let mut raw = vec![0u8; self.cluster_size as usize];
        self.r.seek(SeekFrom::Start(l2_offset))?;
        self.r.read_exact(&mut raw)?;
        let table: Vec<u64> = raw
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
            .collect();
        let entry = table[slot as usize];
        if self.l2_cache.len() >= L2_CACHE_MAX {
            self.l2_cache.clear();
        }
        self.l2_cache.insert(l1_index, table);
        Ok(entry)
    }
}
