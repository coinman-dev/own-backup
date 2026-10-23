//! Which LUKS containers a whole-disk restore has to put back, and with what
//! (D19) — shared by the Linux restore, which does it, and the Windows one,
//! which cannot yet and refuses rather than write plaintext where a
//! container was.
//!
//! A whole-disk copy of a partition whose container was open holds the data
//! decrypted and the container's header beside it (`info.luks_headers`). The
//! restore writes that header back and re-encrypts the data under the master
//! key the header's keyslots give for the container's own passphrase.

use std::collections::HashMap;

use own_backup_common::engine_dto::LuksPassphraseNeedDto;
use own_backup_common::image::{LuksHeaderBackup, VolumeCaptureMode, VolumeEncryptionKind};
use own_backup_common::{tr, OwnBackupError, Result};

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
