//! Write-once qcow2 image writer (pattern of `image_writer.rs` /
//! `vhdx_format.rs`): stream guest clusters in, then `finish()` lays down the
//! L2 tables, L1 table, refcount structures and the final header.
//!
//! The physical bookkeeping lives in [`super::layout`], shared with the
//! encrypted `.obk` writer.
//!
//! Layout produced (all metadata after the data, header patched last):
//! ```text
//! cluster 0            header + extensions + backing file name
//! clusters 1..         guest data: compressed clusters packed byte-to-byte,
//!                      incompressible/uncompressed clusters cluster-aligned
//! ...                  L2 tables (one cluster each)
//! ...                  L1 table
//! ...                  refcount blocks, refcount table
//! ```

use std::io::{Seek, SeekFrom, Write};

use own_backup_common::{OwnBackupError, Result};

use super::layout::{build_header_cluster, HeaderParams, Qcow2Tables};
use super::{
    bad, encode_compressed_l2_entry, COMPRESSION_TYPE_DEFLATE, COMPRESSION_TYPE_ZSTD,
    DEFAULT_CLUSTER_BITS, INCOMPAT_COMPRESSION_TYPE, OFLAG_COPIED, OFLAG_ZERO,
};

/// Compression applied to guest clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qcow2Compression {
    /// All clusters stored raw (cluster-aligned).
    None,
    /// zstd per cluster; clusters that do not shrink are stored raw
    /// (mirrors qemu behaviour and `compression.rs`).
    Zstd { level: i32 },
}

pub struct Qcow2WriterOptions {
    /// Virtual disk size in bytes; rounded up to a whole cluster internally
    /// (the manifest stays authoritative for the exact source size).
    pub virtual_size: u64,
    /// log2 of the cluster size; [`DEFAULT_CLUSTER_BITS`] = 64 KiB.
    pub cluster_bits: u32,
    pub compression: Qcow2Compression,
    /// Backing (parent) image path for incremental/differential children,
    /// stored verbatim in the header. `None` for full backups.
    pub backing_file: Option<String>,
}

impl Default for Qcow2WriterOptions {
    fn default() -> Self {
        Self {
            virtual_size: 0,
            cluster_bits: DEFAULT_CLUSTER_BITS,
            compression: Qcow2Compression::Zstd {
                level: super::DEFAULT_ZSTD_LEVEL,
            },
            backing_file: None,
        }
    }
}

pub struct Qcow2Writer<W: Write + Seek> {
    w: W,
    tables: Qcow2Tables,
    cluster_bits: u32,
    virtual_size: u64,
    compression: Qcow2Compression,
    backing_file: Option<String>,
}

/// Shared option checks for both qcow2 writers.
pub(crate) fn validate_geometry(
    virtual_size: u64,
    cluster_bits: u32,
    backing_file: Option<&str>,
) -> Result<u64> {
    if virtual_size == 0 {
        return Err(bad("virtual size must be non-zero"));
    }
    if !(9..=21).contains(&cluster_bits) {
        return Err(bad(format!(
            "cluster_bits {cluster_bits} out of range 9..=21"
        )));
    }
    if let Some(name) = backing_file {
        if name.is_empty() || name.len() > 1023 {
            return Err(bad("backing file name empty or longer than 1023 bytes"));
        }
    }
    let cluster_size = 1u64 << cluster_bits;
    Ok(virtual_size.div_ceil(cluster_size) * cluster_size)
}

/// Write the placeholder header cluster the real header replaces in `finish`.
pub(crate) fn write_placeholder_header<W: Write + Seek>(
    w: &mut W,
    cluster_size: u64,
) -> Result<()> {
    w.seek(SeekFrom::Start(0))?;
    w.write_all(&vec![0u8; cluster_size as usize])?;
    Ok(())
}

impl<W: Write + Seek> Qcow2Writer<W> {
    pub fn new(mut w: W, opts: Qcow2WriterOptions) -> Result<Self> {
        let virtual_size = validate_geometry(
            opts.virtual_size,
            opts.cluster_bits,
            opts.backing_file.as_deref(),
        )?;
        let cluster_size = 1u64 << opts.cluster_bits;
        write_placeholder_header(&mut w, cluster_size)?;

        Ok(Self {
            w,
            tables: Qcow2Tables::new(opts.cluster_bits, virtual_size / cluster_size),
            cluster_bits: opts.cluster_bits,
            virtual_size,
            compression: opts.compression,
            backing_file: opts.backing_file,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        self.tables.cluster_size()
    }

    pub fn total_guest_clusters(&self) -> u64 {
        self.tables.total_guest_clusters()
    }

    /// Write one guest cluster (`data.len()` must equal the cluster size —
    /// the caller pads the tail block). All-zero clusters are not stored:
    /// they become "unallocated" in a standalone image and an explicit
    /// zero-cluster in a child (so they override the backing file).
    pub fn write_cluster(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.tables.check_new_cluster(index, data.len())?;

        if data.iter().all(|&b| b == 0) {
            if self.backing_file.is_some() {
                self.tables.set_entry(index, OFLAG_ZERO)?;
            }
            // No backing file: leave unallocated — reads back as zeros.
            return Ok(());
        }

        if let Qcow2Compression::Zstd { level } = self.compression {
            let compressed = zstd::bulk::compress(data, level)
                .map_err(|e| OwnBackupError::Compression(e.to_string()))?;
            if (compressed.len() as u64) < self.cluster_size() {
                let host_offset = self.tables.write_packed(&mut self.w, &compressed)?;
                let entry = encode_compressed_l2_entry(
                    host_offset,
                    compressed.len() as u64,
                    self.cluster_bits,
                )?;
                return self.tables.set_entry(index, entry);
            }
            // Did not shrink — fall through to a raw aligned cluster.
        }
        let host_offset = self.tables.write_aligned(&mut self.w, data)?;
        self.tables.set_entry(index, OFLAG_COPIED | host_offset)
    }

    /// Write L2/L1/refcount metadata and the final header. Returns the inner
    /// writer (flushed).
    pub fn finish(mut self) -> Result<W> {
        let info = self.tables.finish_tables(&mut self.w)?;
        let (incompatible_features, compression_type) = match self.compression {
            Qcow2Compression::None => (0u64, COMPRESSION_TYPE_DEFLATE),
            Qcow2Compression::Zstd { .. } => (INCOMPAT_COMPRESSION_TYPE, COMPRESSION_TYPE_ZSTD),
        };
        let header_cluster = build_header_cluster(
            HeaderParams {
                cluster_bits: self.cluster_bits,
                virtual_size: self.virtual_size,
                incompatible_features,
                compression_type,
                backing_file: self.backing_file.as_deref(),
                extensions: &[],
            },
            &info,
        )?;
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&header_cluster)?;
        self.w.flush()?;
        Ok(self.w)
    }
}
