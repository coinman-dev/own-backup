//! Write-once `.obk` writer: guest clusters go in as sealed records, and
//! `finish()` lays down the index, the sealed metadata, the root record and
//! then the qcow2 tables and header.
//!
//! Both checked header slots are written after the records and tables. The
//! caller syncs the finished file before publishing the backup run; an image
//! left by a crash during its initial write is not a completed restore point.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use own_backup_common::{OwnBackupError, Result};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::header::{finish_dual_header, obk_extensions, ObkOpenHeader, ObkRootPointer};
use super::index::{encode_chunk, ClusterState, MAX_INDEX_ENTRIES_PER_CHUNK};
use super::root::{ChunkRef, IndexChunkRef, ObkRoot, MAX_INDEX_CHUNKS, MAX_META_CHUNKS};
use super::{
    bad, DUAL_HEADER_DATA_START, INCOMPAT_OBK_SEAL, MAX_OBK_CLUSTER_BITS, MIN_OBK_CLUSTER_BITS,
    OBK_OPEN_FORMAT, SECOND_HEADER_OFFSET,
};
use crate::qcow2::layout::{build_header_cluster, HeaderParams, Qcow2Tables};
use crate::qcow2::writer::write_placeholder_header;
use crate::qcow2::{
    encode_compressed_l2_entry, COMPRESSION_TYPE_ZSTD, INCOMPAT_COMPRESSION_TYPE, OFLAG_ZERO,
};
use crate::seal::cluster::{Encoding, Purpose, RecordWriter, MAX_PAYLOAD_LEN};
use crate::seal::keypair::SealedPrivateKey;
use crate::seal::wrap::WrappedChainKey;
use crate::seal::{ChainKey, SCHEME};
use crate::sidecar::ChainRole;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// Compression applied inside a record, before encryption. zstd first, then
/// AES-256-GCM: the ciphertext of compressed data is what reaches the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObkCompression {
    None,
    Zstd { level: i32 },
}

pub struct ObkWriterOptions {
    pub chain_id: Uuid,
    pub backup_id: Uuid,
    pub parent_backup_id: Option<Uuid>,
    pub role: ChainRole,
    /// RFC 3339 creation time, shown in listings before anything is opened.
    pub created_at: String,
    pub virtual_size: u64,
    pub cluster_bits: u32,
    pub compression: ObkCompression,
    /// Parent image file name, stored verbatim in the qcow2 header.
    pub backing_file: Option<String>,
    /// Portable password/recovery envelope of the private job key. Every image
    /// carries it, so a chain can be opened on a machine that has never seen
    /// this job.
    pub key_file: SealedPrivateKey,
    /// The chain master key wrapped for this job's public key.
    pub wrapped_chain_key: WrappedChainKey,
    /// Listing fields carried in the open header so a backup folder can be
    /// grouped into chains, and retention can count versions, without a key.
    pub source_id: Option<String>,
    pub consolidated_increments: u32,
    pub chain_started_at: Option<String>,
}

pub struct ObkWriter<W: Write + Seek> {
    w: W,
    tables: Qcow2Tables,
    opts: ObkWriterOptions,
    compression: ObkCompression,
    data: RecordWriter,
    index: RecordWriter,
    meta: RecordWriter,
    /// Guest cluster -> what this image says about it. Absent clusters are not
    /// listed: they inherit from the parent, or read as zeros without one.
    states: BTreeMap<u64, ClusterState>,
    dual_header: bool,
}

impl ObkWriter<BufWriter<File>> {
    /// Create a new encrypted image (the file must not exist yet).
    pub fn create(path: &Path, chain: &ChainKey, opts: ObkWriterOptions) -> Result<Self> {
        let file = File::create_new(path)?;
        Self::new(BufWriter::new(file), chain, opts)
    }
}

impl<W: Write + Seek> ObkWriter<W> {
    pub fn new(w: W, chain: &ChainKey, opts: ObkWriterOptions) -> Result<Self> {
        Self::new_with_header_mode(w, chain, opts, true)
    }

    #[cfg(test)]
    pub(crate) fn new_legacy(w: W, chain: &ChainKey, opts: ObkWriterOptions) -> Result<Self> {
        Self::new_with_header_mode(w, chain, opts, false)
    }

    fn new_with_header_mode(
        mut w: W,
        chain: &ChainKey,
        opts: ObkWriterOptions,
        dual_header: bool,
    ) -> Result<Self> {
        validate_options(&opts)?;
        let cluster_size = 1u64 << opts.cluster_bits;
        let virtual_size = opts.virtual_size.div_ceil(cluster_size) * cluster_size;
        let chain_id = *opts.chain_id.as_bytes();
        write_placeholder_header(&mut w, cluster_size)?;
        let tables = if dual_header {
            w.seek(SeekFrom::Start(DUAL_HEADER_DATA_START))?;
            Qcow2Tables::resume(
                opts.cluster_bits,
                virtual_size / cluster_size,
                DUAL_HEADER_DATA_START,
            )
        } else {
            Qcow2Tables::new(opts.cluster_bits, virtual_size / cluster_size)
        };

        Ok(Self {
            w,
            tables,
            compression: opts.compression,
            data: RecordWriter::new(chain, chain_id, Purpose::Data)?,
            index: RecordWriter::new(chain, chain_id, Purpose::Index)?,
            meta: RecordWriter::new(chain, chain_id, Purpose::Manifest)?,
            opts: ObkWriterOptions {
                virtual_size,
                ..opts
            },
            states: BTreeMap::new(),
            dual_header,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        self.tables.cluster_size()
    }

    pub fn total_guest_clusters(&self) -> u64 {
        self.tables.total_guest_clusters()
    }

    fn has_parent(&self) -> bool {
        self.opts.parent_backup_id.is_some()
    }

    /// Write one guest cluster. `data.len()` must equal the cluster size — the
    /// caller pads the tail block, exactly as for the plaintext writer.
    ///
    /// An all-zero cluster is not stored: in a full backup it stays absent (and
    /// reads as zeros), in a child it becomes an explicit zero that overrides
    /// the parent. A zero cluster costs no record and no nonce.
    pub fn write_cluster(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.tables.check_new_cluster(index, data.len())?;
        if data.iter().all(|&b| b == 0) {
            return self.mark_zero(index);
        }

        let compressed = match self.compression {
            ObkCompression::Zstd { level } => Some(Zeroizing::new(
                zstd::bulk::compress(data, level)
                    .map_err(|e| OwnBackupError::Compression(e.to_string()))?,
            )),
            ObkCompression::None => None,
        };
        let (encoding, payload) = match &compressed {
            // Strictly smaller, as the record contract requires; a cluster that
            // does not shrink is sealed raw.
            Some(c) if (c.len() as u64) < self.cluster_size() => (Encoding::Zstd, &c[..]),
            _ => (Encoding::Raw, data),
        };
        let record = self
            .data
            .seal(index, encoding, self.cluster_size() as u32, payload)?;
        let host_offset = self.tables.write_packed(&mut self.w, &record.bytes)?;
        let entry = encode_compressed_l2_entry(
            host_offset,
            record.bytes.len() as u64,
            self.opts.cluster_bits,
        )?;
        self.tables.set_entry(index, entry)?;
        self.states.insert(index, ClusterState::Present(record.id));
        Ok(())
    }

    /// Say that a guest cluster is zero without materialising a zero buffer.
    pub fn write_zero_cluster(&mut self, index: u64) -> Result<()> {
        self.tables
            .check_new_cluster(index, self.cluster_size() as usize)?;
        self.mark_zero(index)
    }

    fn mark_zero(&mut self, index: u64) -> Result<()> {
        if self.has_parent() {
            self.tables.set_entry(index, OFLAG_ZERO)?;
            self.states.insert(index, ClusterState::Zero);
        }
        // Without a parent an absent cluster already reads as zeros, and the
        // index says so by not listing it.
        Ok(())
    }

    /// Seal the index and the metadata, then write the tables and the header.
    ///
    /// `sealed_metadata` is opaque to the container — stage E4 hands it the
    /// binary sidecar (disk layout, machine name, volume labels, categories,
    /// UEFI entries, exclusions and the plaintext checksums), which is exactly
    /// what §7.3 wants out of the open half of the image.
    pub fn finish(self, sealed_metadata: &[u8]) -> Result<W> {
        self.finish_with_reserve(sealed_metadata, 0)
    }

    /// The same, leaving room for a **larger** sidecar to replace this one in
    /// place later ([`super::metadata::update_sealed_metadata`]).
    ///
    /// The Windows engine learns the last of what belongs in a sidecar only
    /// after the image is closed — the exclusion figures and the checksums the
    /// deletion pass touched. A plaintext image has a meta partition sized for
    /// that; an encrypted one reserves its room here instead. Reserved metadata
    /// is stored uncompressed so that replacing it cannot change any record's
    /// length, which is what makes the replacement an in-place one.
    pub fn finish_with_reserve(mut self, sealed_metadata: &[u8], reserve: usize) -> Result<W> {
        let reserved = reserve > sealed_metadata.len();
        let padded: Zeroizing<Vec<u8>> = if reserved {
            let mut buf = Zeroizing::new(vec![0u8; reserve]);
            buf[..sealed_metadata.len()].copy_from_slice(sealed_metadata);
            buf
        } else {
            Zeroizing::new(sealed_metadata.to_vec())
        };
        let defined_clusters = self.states.len() as u64;
        let states: Vec<(u64, ClusterState)> =
            std::mem::take(&mut self.states).into_iter().collect();
        if states.len().div_ceil(MAX_INDEX_ENTRIES_PER_CHUNK) > MAX_INDEX_CHUNKS {
            return Err(bad(
                "image defines more clusters than one root record can index",
            ));
        }
        if padded.len().div_ceil(MAX_PAYLOAD_LEN) > MAX_META_CHUNKS {
            return Err(bad("sealed metadata is larger than the format allows"));
        }

        let mut index_chunks = Vec::new();
        for (ordinal, slice) in states.chunks(MAX_INDEX_ENTRIES_PER_CHUNK).enumerate() {
            let payload = Zeroizing::new(encode_chunk(slice)?);
            let chunk = seal_chunk(
                &self.index,
                ordinal as u64,
                &payload,
                &mut self.tables,
                &mut self.w,
            )?;
            index_chunks.push(IndexChunkRef {
                chunk,
                entries: slice.len() as u32,
                first_cluster: slice[0].0,
            });
        }

        let mut meta_chunks = Vec::new();
        for (ordinal, slice) in padded.chunks(MAX_PAYLOAD_LEN).enumerate() {
            // Chunk 0 of the Manifest purpose is the root record itself.
            meta_chunks.push(seal_chunk_with(
                &self.meta,
                ordinal as u64 + 1,
                slice,
                reserved,
                &mut self.tables,
                &mut self.w,
            )?);
        }

        let root = ObkRoot {
            role: self.opts.role,
            chain_id: self.opts.chain_id,
            backup_id: self.opts.backup_id,
            parent_backup_id: self.opts.parent_backup_id,
            virtual_size: self.opts.virtual_size,
            cluster_bits: self.opts.cluster_bits,
            guest_clusters: self.tables.total_guest_clusters(),
            defined_clusters,
            sealed_metadata_len: sealed_metadata.len() as u64,
            index_chunks,
            meta_chunks,
        };
        let root_payload = Zeroizing::new(root.encode()?);
        let root_ref = seal_chunk_with(
            &self.meta,
            0,
            &root_payload,
            reserved,
            &mut self.tables,
            &mut self.w,
        )?;

        let open = ObkOpenHeader {
            format: OBK_OPEN_FORMAT.into(),
            scheme: SCHEME.into(),
            chain_id: self.opts.chain_id,
            backup_id: self.opts.backup_id,
            parent_backup_id: self.opts.parent_backup_id,
            role: self.opts.role,
            created_at: self.opts.created_at.clone(),
            virtual_size: self.opts.virtual_size,
            cluster_bits: self.opts.cluster_bits,
            wrapped_chain_key: B64.encode(self.opts.wrapped_chain_key.to_bytes()),
            key_file: self.opts.key_file.clone(),
            source_id: self.opts.source_id.clone(),
            consolidated_increments: self.opts.consolidated_increments,
            chain_started_at: self.opts.chain_started_at.clone(),
            dual_header: self.dual_header,
            header_generation: 0,
            root: ObkRootPointer::new(root_ref.offset, root_ref.len as usize, root_ref.id),
        };
        let extensions = obk_extensions(&open)?;

        let info = self.tables.finish_tables(&mut self.w)?;
        let header_cluster = build_header_cluster(
            HeaderParams {
                cluster_bits: self.opts.cluster_bits,
                virtual_size: self.opts.virtual_size,
                incompatible_features: INCOMPAT_OBK_SEAL | INCOMPAT_COMPRESSION_TYPE,
                compression_type: COMPRESSION_TYPE_ZSTD,
                backing_file: self.opts.backing_file.as_deref(),
                extensions: &extensions,
            },
            &info,
        )?;
        let header_cluster = if self.dual_header {
            finish_dual_header(header_cluster)?
        } else {
            header_cluster
        };
        if self.dual_header {
            self.w.seek(SeekFrom::Start(SECOND_HEADER_OFFSET))?;
            self.w.write_all(&header_cluster)?;
        }
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&header_cluster)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Compress if that helps, seal, append to the file, and describe the result.
/// Free function so the record writer and the file bookkeeping can be borrowed
/// at the same time.
fn seal_chunk<W: Write>(
    writer: &RecordWriter,
    ordinal: u64,
    payload: &[u8],
    tables: &mut Qcow2Tables,
    w: &mut W,
) -> Result<ChunkRef> {
    seal_chunk_with(writer, ordinal, payload, false, tables, w)
}

/// `uncompressed` forces Raw: the record's length then depends only on the
/// payload's length, which is what lets a later sidecar of the same size take
/// this one's place byte for byte.
pub(crate) fn seal_chunk_with<W: Write>(
    writer: &RecordWriter,
    ordinal: u64,
    payload: &[u8],
    uncompressed: bool,
    tables: &mut Qcow2Tables,
    w: &mut W,
) -> Result<ChunkRef> {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD_LEN {
        return Err(bad("metadata chunk size out of range"));
    }
    let compressed = if uncompressed {
        Zeroizing::new(Vec::new())
    } else {
        Zeroizing::new(
            zstd::bulk::compress(payload, crate::qcow2::DEFAULT_ZSTD_LEVEL)
                .map_err(|e| OwnBackupError::Compression(e.to_string()))?,
        )
    };
    let (encoding, bytes) = if !uncompressed && compressed.len() < payload.len() {
        (Encoding::Zstd, &compressed[..])
    } else {
        (Encoding::Raw, payload)
    };
    let record = writer.seal(ordinal, encoding, payload.len() as u32, bytes)?;
    let offset = tables.write_packed(w, &record.bytes)?;
    Ok(ChunkRef {
        id: record.id,
        offset,
        len: record.bytes.len() as u32,
    })
}

fn validate_options(opts: &ObkWriterOptions) -> Result<()> {
    if opts.virtual_size == 0 {
        return Err(bad("virtual size must be non-zero"));
    }
    if !(MIN_OBK_CLUSTER_BITS..=MAX_OBK_CLUSTER_BITS).contains(&opts.cluster_bits) {
        return Err(bad(format!(
            "cluster_bits {} out of range {MIN_OBK_CLUSTER_BITS}..={MAX_OBK_CLUSTER_BITS}",
            opts.cluster_bits
        )));
    }
    if let Some(name) = &opts.backing_file {
        if name.is_empty() || name.len() > 1023 {
            return Err(bad("backing file name empty or longer than 1023 bytes"));
        }
    }
    if opts.parent_backup_id.is_some() != opts.backing_file.is_some() {
        return Err(bad(
            "a child image needs both a parent identity and a backing file",
        ));
    }
    match (opts.role, opts.parent_backup_id) {
        (ChainRole::Full, None) => {}
        (ChainRole::Incremental | ChainRole::Differential, Some(parent)) => {
            if parent == opts.backup_id {
                return Err(bad("image claims to be its own parent"));
            }
        }
        _ => return Err(bad("chain role and parent identity disagree")),
    }
    if opts.chain_id.is_nil() || opts.backup_id.is_nil() {
        return Err(bad("nil chain/backup identity"));
    }
    if &opts.wrapped_chain_key.chain_id != opts.chain_id.as_bytes() {
        return Err(bad("wrapped chain key belongs to another chain"));
    }
    if opts.created_at.len() > 64 || opts.created_at.chars().any(char::is_control) {
        return Err(bad("invalid creation timestamp"));
    }
    opts.key_file.validate()
}
