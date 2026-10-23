//! Putting a whole-disk copy's data back with its LUKS containers (D19) —
//! shared by the Linux and the Windows restore.
//!
//! A whole-disk copy of a partition whose container was open — by the system
//! on Linux, by the engine with the passphrase on either system — holds the
//! data decrypted and the container's header beside it (`info.luks_headers`).
//! The restore writes that header back and re-encrypts the data under the
//! master key the header's keyslots give for the container's own passphrase;
//! plaintext never lands where a container was ([`write_disk_data`]).

use std::collections::HashMap;
use std::path::Path;

use own_backup_common::engine_dto::LuksPassphraseNeedDto;
use own_backup_common::image::{LuksHeaderBackup, VolumeCaptureMode, VolumeEncryptionKind};
use own_backup_common::{tr, OwnBackupError, Result};

use crate::checksum::compute_block_checksum;
use crate::engine_core::read::ImageReadSource;
use crate::fat_map::RawVolume;
use crate::sector_restore::SectorTarget;
use crate::sidecar::Sidecar;
use zeroize::Zeroizing;

/// A LUKS container the copy holds decrypted, which the restore puts back.
#[derive(Debug, Clone)]
pub struct Container {
    /// Its partition: the index in the copy's table, which the restore
    /// writes as it is — on the source and on the target alike.
    pub part_index: u32,
    /// Its slot on the disk, in bytes.
    pub offset: u64,
    pub size: u64,
    /// The header area the copy kept.
    pub header: LuksHeaderBackup,
}

/// Partitions of a whole-disk copy captured in the clear from a LUKS
/// container — with a saved header or without one. Written as they are, they
/// would put plaintext where the restored system waits for a container.
pub fn luks_in_clear(sidecar: &Sidecar) -> Vec<u32> {
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<u32> = dm
        .layout
        .partitions
        .iter()
        .map(|p| p.index)
        .filter(|&index| {
            sidecar
                .info
                .luks_headers
                .iter()
                .any(|h| h.part_index == index)
                || decrypted_luks(sidecar, index)
        })
        .collect();
    out.sort_unstable();
    out
}

fn decrypted_luks(sidecar: &Sidecar, index: u32) -> bool {
    sidecar
        .disk_manifest
        .iter()
        .flat_map(|dm| &dm.images)
        .find(|i| i.part_index == index)
        .and_then(|i| i.encryption.as_ref())
        .is_some_and(|e| {
            matches!(
                e.kind,
                VolumeEncryptionKind::Luks1 | VolumeEncryptionKind::Luks2
            ) && e.capture_mode == VolumeCaptureMode::Plaintext
        })
}

/// The LUKS containers a whole-disk restore of `sidecar`'s copy has to put
/// back: every partition whose header the copy kept. Refused — before
/// anything is written — when a partition was captured decrypted from a
/// LUKS container whose header the copy does not have (plaintext would land
/// where the restored system waits for a container), when the container is
/// one this program cannot rebuild (a cipher it does not implement,
/// authenticated encryption, a reencryption under way, no passphrase slot),
/// or when its encryption sectors do not line up with the copy's blocks on
/// that partition.
pub fn containers_to_rebuild(sidecar: &Sidecar) -> Result<Vec<Container>> {
    let Some(dm) = sidecar.disk_manifest.as_ref() else {
        return Ok(Vec::new());
    };
    let sector = dm.layout.sector_size.max(1) as u64;
    let bs = sidecar.manifest.block_size as u64;
    let mut out = Vec::new();
    for p in &dm.layout.partitions {
        let header = sidecar
            .info
            .luks_headers
            .iter()
            .find(|h| h.part_index == p.index);
        match header {
            Some(h) => {
                let bytes = crate::luks::decode_stored_header(h)?;
                let why = crate::luks::restore_obstacles(&bytes)?;
                if !why.is_empty() {
                    return Err(OwnBackupError::RestoreIncompatible(
                        tr!(
                            "restore_disk_linux.luks_cannot_rebuild",
                            index = p.index,
                            why = why.join("; ")
                        )
                        .into_owned(),
                    ));
                }
                let offset = p.first_lba * sector;
                // Every slice the restore encrypts starts on a block boundary
                // or at the partition's start, measured from the partition:
                // each must fall on one of the container's sectors.
                let ss = h.sector_size.max(512) as u64;
                if bs == 0 || !bs.is_multiple_of(ss) || !(offset % bs).is_multiple_of(ss) {
                    return Err(OwnBackupError::RestoreIncompatible(
                        tr!(
                            "restore_disk_linux.luks_sector_misaligned",
                            index = p.index,
                            sector = ss,
                            offset = offset,
                            block = bs
                        )
                        .into_owned(),
                    ));
                }
                out.push(Container {
                    part_index: p.index,
                    offset,
                    size: (p.last_lba + 1 - p.first_lba) * sector,
                    header: h.clone(),
                });
            }
            None if decrypted_luks(sidecar, p.index) => {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("restore_disk_linux.luks_no_header", index = p.index).into_owned(),
                ))
            }
            None => {}
        }
    }
    Ok(out)
}

/// What the passphrases settle: the containers still needing one (missing
/// or wrong), and the keys the right ones gave, by partition — so the write
/// does not derive them again.
pub struct Checked {
    pub needs: Vec<LuksPassphraseNeedDto>,
    pub keys: HashMap<u32, crate::luks::LuksParams>,
}

/// Try each container's passphrase against its saved header. A passphrase is
/// wrong only when no keyslot accepted it (the digest check failed); any
/// other failure is the error it is.
pub fn check_passphrases(
    containers: &[Container],
    passphrases: &HashMap<u32, Zeroizing<String>>,
) -> Result<Checked> {
    let mut out = Checked {
        needs: Vec::new(),
        keys: HashMap::new(),
    };
    for c in containers {
        let wrong = match passphrases.get(&c.part_index) {
            None => false,
            Some(p) => {
                let bytes = crate::luks::decode_stored_header(&c.header)?;
                match crate::luks::open_luks_from_header(&bytes, p.as_bytes()) {
                    Ok(params) => {
                        out.keys.insert(c.part_index, params);
                        continue;
                    }
                    Err(e) if crate::luks::is_wrong_passphrase(&e) => true,
                    Err(e) => return Err(e),
                }
            }
        };
        out.needs.push(LuksPassphraseNeedDto {
            part_index: c.part_index,
            kind: c.header.kind.clone(),
            wrong,
        });
    }
    Ok(out)
}

/// The containers whose passphrase is missing from `passphrases` or wrong
/// ([`check_passphrases`] without the keys).
pub fn passphrase_needs(
    containers: &[Container],
    passphrases: &HashMap<u32, Zeroizing<String>>,
) -> Result<Vec<LuksPassphraseNeedDto>> {
    check_passphrases(containers, passphrases).map(|c| c.needs)
}

/// What `needs` asks for, in words.
pub fn describe_needs(needs: &[LuksPassphraseNeedDto]) -> String {
    let list: Vec<String> = needs
        .iter()
        .map(|n| {
            if n.wrong {
                tr!(
                    "restore_disk_linux.luks_passphrase_wrong",
                    index = n.part_index
                )
            } else {
                tr!(
                    "restore_disk_linux.luks_passphrase_missing",
                    index = n.part_index
                )
            }
            .into_owned()
        })
        .collect();
    tr!(
        "restore_disk_linux.luks_passphrases_needed",
        list = list.join(", ")
    )
    .into_owned()
}

/// The parts of the stretch `[start, start + len)` outside every range
/// `[a, b)` of `skip`, as `(offset, len)` in order.
pub fn outside(start: u64, len: u64, skip: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut pieces = vec![(start, start + len)];
    for &(a, b) in skip {
        let mut next = Vec::with_capacity(pieces.len() + 1);
        for (s, e) in pieces {
            if b <= s || a >= e {
                next.push((s, e));
                continue;
            }
            if s < a {
                next.push((s, a));
            }
            if b < e {
                next.push((b, e));
            }
        }
        pieces = next;
    }
    pieces.sort_unstable();
    pieces.into_iter().map(|(s, e)| (s, e - s)).collect()
}

/// `[offset, offset + size)` of a whole-disk target, addressed from zero —
/// one container's slot.
struct Slot<'a> {
    dev: &'a mut dyn SectorTarget,
    offset: u64,
    size: u64,
}

impl SectorTarget for Slot<'_> {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match offset.checked_add(data.len() as u64) {
            Some(end) if end <= self.size => self.dev.write_at(self.offset + offset, data),
            _ => Err(OwnBackupError::Device(
                tr!(
                    "restore_disk_linux.past_partition_end",
                    offset = offset,
                    len = data.len(),
                    size = self.size
                )
                .into_owned(),
            )),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }

    fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }
}

/// What [`write_disk_data`] writes, and from where.
pub struct DiskData<'a> {
    pub image_path: &'a Path,
    /// Keys of an encrypted copy.
    pub keys: Option<&'a crate::seal::context::ArchiveKeys>,
    pub chain_key: Option<&'a crate::seal::ChainKey>,
    pub verify_checksums: bool,
    /// The containers' passphrases, by source partition.
    pub passphrases: &'a HashMap<u32, Zeroizing<String>>,
    /// The target, for messages and the log.
    pub label: &'a str,
    /// Its length in bytes.
    pub target_size: u64,
}

/// Progress of [`write_disk_data`]: one count across the plain blocks and
/// every container's.
pub struct DataTick {
    /// 0 while the plain blocks are written, then the container's position.
    pub partition_index: usize,
    pub part_label: String,
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_written: u64,
}

/// What [`write_disk_data`] did: bytes written, and the count its progress
/// ran to — for the phases after it to report on the same scale.
pub struct DataWritten {
    pub bytes: u64,
    pub blocks_total: u64,
}

/// Write the copy's data onto the whole disk `dev`: every stored block at its
/// own offset, except inside the slots of `containers`, which are put back as
/// the containers they were — header first, then the data re-encrypted
/// (`sector_restore::write_container_blocks_with`, with the key `known`
/// recovered when the passphrase was checked). A block wholly inside a slot
/// is read only once, by the container's pass. `read_back(offset, size)`
/// reads a slot from the disk itself, past every cache.
pub fn write_disk_data(
    job: &DiskData<'_>,
    sidecar: &Sidecar,
    containers: &[Container],
    known: &HashMap<u32, crate::luks::LuksParams>,
    dev: &mut dyn SectorTarget,
    read_back: &mut dyn FnMut(u64, u64) -> Result<Box<dyn RawVolume>>,
    progress: &mut dyn FnMut(&DataTick) -> Result<()>,
) -> Result<DataWritten> {
    use crate::sector_restore::{
        container_fitting, write_container_blocks_with, ReencryptRequest, SectorRestoreOptions,
    };

    let bs = sidecar.manifest.block_size as u64;
    let image_size = sidecar.manifest.source_disk.size_bytes;
    let skip: Vec<(u64, u64)> = containers
        .iter()
        .map(|c| (c.offset, c.offset + c.size))
        .collect();
    let block_of = |entry: &crate::sidecar::BlockChecksum| {
        let offset = entry.block_number * bs;
        (offset, bs.min(image_size.saturating_sub(offset)))
    };
    // A block across a slot's edge counts in both passes.
    let plain_blocks = sidecar
        .checksums
        .iter()
        .filter(|e| {
            let (offset, len) = block_of(e);
            !outside(offset, len, &skip).is_empty()
        })
        .count() as u64;
    let container_blocks: u64 = containers
        .iter()
        .map(|c| {
            sidecar
                .checksums
                .iter()
                .filter(|e| {
                    let (offset, len) = block_of(e);
                    offset < c.offset + c.size && offset + len > c.offset
                })
                .count() as u64
        })
        .sum();
    let total = plain_blocks + container_blocks;

    let mut image =
        ImageReadSource::open_with(job.image_path, &sidecar.info.container, job.chain_key)?;
    let data_label = tr!("engine_win.label_partition_data").into_owned();
    let mut bytes = 0u64;
    let mut done = 0u64;
    let mut buf = vec![0u8; bs as usize];
    for entry in &sidecar.checksums {
        let (offset, len) = block_of(entry);
        let pieces = outside(offset, len, &skip);
        if pieces.is_empty() {
            continue;
        }
        if offset + len > job.target_size {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!(
                    "restore_disk_linux.block_past_disk",
                    offset = offset,
                    size = job.target_size
                )
                .into_owned(),
            ));
        }
        let data = &mut buf[..len as usize];
        image.read_at(offset, data)?;
        if job.verify_checksums {
            let actual = compute_block_checksum(data);
            if actual != entry.checksum {
                return Err(OwnBackupError::ChecksumMismatch {
                    block_index: entry.block_number,
                    expected: entry.checksum,
                    actual,
                });
            }
        }
        // The decrypted data of a container never lands in the clear.
        for (at, piece) in pieces {
            let from = (at - offset) as usize;
            dev.write_at(at, &data[from..from + piece as usize])?;
            bytes += piece;
        }
        done += 1;
        progress(&DataTick {
            partition_index: 0,
            part_label: data_label.clone(),
            blocks_done: done,
            blocks_total: total,
            bytes_written: bytes,
        })?;
    }
    drop(image);

    // Never set: cancelling is the progress callback's, which every loop
    // asks after each block.
    let cancel = own_backup_common::cancel::CancelToken::new(uuid::Uuid::new_v4());
    for (n, c) in containers.iter().enumerate() {
        let passphrase = job.passphrases.get(&c.part_index).ok_or_else(|| {
            OwnBackupError::RestoreIncompatible(describe_needs(&[LuksPassphraseNeedDto {
                part_index: c.part_index,
                kind: c.header.kind.clone(),
                wrong: false,
            }]))
        })?;
        let creq = ReencryptRequest {
            header: &c.header,
            passphrase: passphrase.as_str(),
        };
        let fit = container_fitting(sidecar, c.part_index, c.size, &creq)?;
        let label = format!("{}:{}:{}", job.label, c.offset, c.size);
        let part_label =
            tr!("restore_disk_linux.label_container", index = c.part_index).into_owned();
        let (offset, size) = (c.offset, c.size);
        let (base, base_done) = (bytes, done);
        let mut slot = Slot {
            dev: &mut *dev,
            offset,
            size,
        };
        let summary = write_container_blocks_with(
            job.image_path,
            job.keys,
            sidecar,
            c.part_index,
            fit,
            &mut slot,
            &mut || read_back(offset, size),
            &label,
            SectorRestoreOptions {
                verify_checksums: job.verify_checksums,
                wipe_free_space: false,
                disk_already_offline: true,
            },
            &creq,
            known.get(&c.part_index),
            &cancel,
            &mut |pr| {
                progress(&DataTick {
                    partition_index: n + 1,
                    part_label: part_label.clone(),
                    blocks_done: (base_done + pr.blocks_done).min(total),
                    blocks_total: total,
                    bytes_written: base + pr.bytes_written,
                })
            },
        )?;
        bytes += summary.bytes_restored;
        done += summary.blocks_restored;
        tracing::info!(
            part = c.part_index,
            bytes = summary.bytes_restored,
            "LUKS container put back from its saved header"
        );
    }
    Ok(DataWritten {
        bytes,
        blocks_total: total,
    })
}
