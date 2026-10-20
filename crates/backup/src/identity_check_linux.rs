//! Duplicate identities before a restore or a mount on Linux — the machine's
//! side of [`crate::identity_clash`].
//!
//! What the copy carries is read out of the image itself (the first bytes and
//! the tail of each partition it holds, the GPT partition GUIDs a whole-disk
//! restore would write); what the machine has comes from the udev database.
//! The devices about to be overwritten are left out of the comparison: putting
//! a partition back where it came from duplicates nothing.

#![cfg(target_os = "linux")]

use std::path::Path;

use own_backup_common::engine_dto::IdentityClashDto;
use own_backup_common::Result;

use crate::engine_core::read::ImageReadSource;
use crate::identity_clash::{
    find_clashes, host_identities, identities_of, parse_kernel_version, Clash, Identity,
    IdentityKind, Probe, Reidentify, Severity,
};
use crate::sidecar::Sidecar;

/// A stretch of an image seen as one device.
struct ImageWindow<'a> {
    image: &'a mut ImageReadSource,
    start: u64,
    len: u64,
}

impl Probe for ImageWindow<'_> {
    fn size(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if offset.checked_add(len as u64)? > self.len {
            return None;
        }
        let mut buf = vec![0u8; len];
        self.image.read_at(self.start + offset, &mut buf).ok()?;
        Some(buf)
    }
}

/// A device node (or an image file) read as a [`Probe`].
pub struct DeviceProbe(pub crate::linux_device::LinuxRawDevice);

impl Probe for DeviceProbe {
    fn size(&self) -> u64 {
        self.0.length()
    }

    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if offset.checked_add(len as u64)? > self.0.length() {
            return None;
        }
        let mut buf = vec![0u8; len];
        self.0.read_at(offset, &mut buf).ok()?;
        Some(buf)
    }
}

/// The identities the device at `node` carries now.
pub fn identities_on(node: &str) -> Result<Vec<Identity>> {
    let dev = crate::linux_device::LinuxRawDevice::open_read(node)?;
    Ok(identities_of(&mut DeviceProbe(dev)))
}

/// Whether mounting the btrfs on `node` cannot land on a filesystem this
/// machine already has: no other device carries its fsid, or the kernel gives
/// a clone a temporary fsid (`identity_clash`: such a duplicate is not
/// critical). Anything unreadable is not safe.
pub fn btrfs_mount_safe(node: &str) -> bool {
    let Ok(ids) = identities_on(node) else {
        return false;
    };
    let restored: Vec<(Option<u32>, Identity)> = ids
        .into_iter()
        .filter(|i| i.kind == IdentityKind::Btrfs)
        .map(|i| (None, i))
        .collect();
    let own: Vec<String> = own_backup_common::linux_block::kernel_name(Path::new(node))
        .into_iter()
        .collect();
    !clashes_here(&restored, &own)
        .iter()
        .any(|c| c.severity == "critical")
}

/// The identities of what restoring `src_indices` out of the image would put
/// on a disk, each with the source partition it comes from (`None` for a
/// volume copy). With `with_partuuids`, a whole-disk copy's GPT partition
/// GUIDs too — the table a whole-disk restore writes carries them.
pub fn restored_identities(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    src_indices: &[u32],
    with_partuuids: bool,
) -> Result<Vec<(Option<u32>, Identity)>> {
    let chain_key = match keys {
        Some(k)
            if crate::sidecar::detect_container_format(image_path).ok()
                == Some(crate::seal::obk::OBK_EXTENSION) =>
        {
            Some(k.open_chain_of(image_path)?.key)
        }
        _ => None,
    };
    let mut image =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;
    let mut out = Vec::new();
    match sidecar.disk_manifest.as_ref() {
        None => {
            let mut w = ImageWindow {
                image: &mut image,
                start: sidecar.info.volume_data_offset,
                len: sidecar.manifest.source_disk.size_bytes,
            };
            out.extend(identities_of(&mut w).into_iter().map(|i| (None, i)));
        }
        Some(dm) => {
            let sector = dm.layout.sector_size.max(512) as u64;
            for p in dm
                .layout
                .partitions
                .iter()
                .filter(|p| src_indices.contains(&p.index))
            {
                let mut w = ImageWindow {
                    image: &mut image,
                    start: p.first_lba * sector,
                    len: (p.last_lba + 1 - p.first_lba) * sector,
                };
                out.extend(
                    identities_of(&mut w)
                        .into_iter()
                        .map(|i| (Some(p.index), i)),
                );
                if with_partuuids && !p.part_guid.is_empty() {
                    out.push((
                        Some(p.index),
                        Identity::new(IdentityKind::PartUuid, &p.part_guid),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// The first `len` bytes of a volume copy's volume (decrypted, for an OBK),
/// for telling which filesystem it holds; `None` for a whole-disk copy.
pub fn volume_head(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
    sidecar: &Sidecar,
    len: usize,
) -> Result<Option<Vec<u8>>> {
    if sidecar.disk_manifest.is_some() {
        return Ok(None);
    }
    let chain_key = match keys {
        Some(k)
            if crate::sidecar::detect_container_format(image_path).ok()
                == Some(crate::seal::obk::OBK_EXTENSION) =>
        {
            Some(k.open_chain_of(image_path)?.key)
        }
        _ => None,
    };
    let mut image =
        ImageReadSource::open_with(image_path, &sidecar.info.container, chain_key.as_ref())?;
    let len = (len as u64).min(sidecar.manifest.source_disk.size_bytes) as usize;
    let mut buf = vec![0u8; len];
    image.read_at(sidecar.info.volume_data_offset, &mut buf)?;
    Ok(Some(buf))
}

/// The LUKS identity of a container the restore puts back from its saved
/// header (re-encryption, D19): the copy holds the volume decrypted, so the
/// image itself does not show it, and the restored partition carries the
/// source's container UUID all the same.
pub fn saved_container_identity(
    header: &own_backup_common::image::LuksHeaderBackup,
) -> Option<Identity> {
    let bytes = crate::luks::decode_stored_header(header).ok()?;
    identities_of(&mut bytes.as_slice())
        .into_iter()
        .find(|i| i.kind == IdentityKind::Luks)
}

/// Which of `restored` this machine already carries, leaving out the
/// `overwritten` devices (kernel names) and everything stacked on them.
pub fn clashes_here(
    restored: &[(Option<u32>, Identity)],
    overwritten: &[String],
) -> Vec<IdentityClashDto> {
    let present = host_identities(
        Path::new(own_backup_common::linux_block::UDEV_DATA),
        Path::new("/sys/dev/block"),
    );
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .and_then(|r| parse_kernel_version(&r));
    let sys = Path::new(own_backup_common::linux_block::SYS_CLASS_BLOCK);
    let mut skip: Vec<String> = Vec::new();
    for name in overwritten {
        skip.extend(own_backup_common::linux_block::stack_above(sys, name));
    }
    clashes_against(restored, &present, &skip, kernel)
}

/// [`clashes_here`] with the machine's side given (tests).
pub fn clashes_against(
    restored: &[(Option<u32>, Identity)],
    present: &std::collections::BTreeMap<String, Vec<Identity>>,
    overwritten: &[String],
    kernel: Option<crate::identity_clash::KernelVersion>,
) -> Vec<IdentityClashDto> {
    let ids: Vec<Identity> = restored.iter().map(|(_, i)| i.clone()).collect();
    find_clashes(&ids, present, overwritten, kernel)
        .into_iter()
        .map(|c| {
            let part = restored
                .iter()
                .find(|(_, i)| i.kind == c.identity.kind && i.key == c.identity.key)
                .and_then(|(p, _)| *p);
            to_dto(&c, part)
        })
        .collect()
}

fn to_dto(c: &Clash, part_index: Option<u32>) -> IdentityClashDto {
    IdentityClashDto {
        kind: match c.identity.kind {
            IdentityKind::Ext => "ext",
            IdentityKind::Xfs => "xfs",
            IdentityKind::Btrfs => "btrfs",
            IdentityKind::Luks => "luks",
            IdentityKind::LvmPv => "lvm_pv",
            IdentityKind::LvmVg => "lvm_vg",
            IdentityKind::LvmVgName => "lvm_vg_name",
            IdentityKind::Md => "md",
            IdentityKind::PartUuid => "partuuid",
            IdentityKind::Fat => "vfat",
        }
        .to_string(),
        value: c.identity.shown.clone(),
        part_index,
        already_on: c.already_on.clone(),
        severity: match c.severity {
            Severity::Ambiguous => "ambiguous",
            Severity::Mount => "mount",
            Severity::Critical => "critical",
        }
        .to_string(),
        change_command: match c.reidentify {
            Reidentify::Tune2fs => "e2fsck -f <device> && tune2fs -U random <device>",
            Reidentify::XfsAdmin => "xfs_admin -U generate <device>",
            Reidentify::Btrfstune => "btrfstune -m <device>",
            Reidentify::CryptsetupLuksUuid => "cryptsetup luksUUID --uuid $(uuidgen) <device>",
            Reidentify::Vgimportclone => "vgimportclone --basevgname <new-name> <device>",
            Reidentify::MdadmUpdateUuid => "mdadm --assemble --update=uuid <array> <device>",
            Reidentify::Fatlabel => "fatlabel -i <device> <new-serial>",
            Reidentify::None => "",
        }
        .to_string(),
        mount_hint: c.mount_hint.unwrap_or_default().to_string(),
    }
}

/// One line per duplicate, for a refusal or a summary.
pub fn describe(clashes: &[IdentityClashDto]) -> String {
    clashes
        .iter()
        .map(|c| {
            format!(
                "{} {} ({}): {}",
                c.kind,
                c.value,
                c.severity,
                c.already_on.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Mount options a read of the copy needs beside the original (`nouuid` for a
/// duplicate XFS), comma-joined; empty when none.
pub fn mount_options_for(clashes: &[IdentityClashDto]) -> String {
    let mut opts: Vec<&str> = Vec::new();
    for c in clashes {
        if c.mount_hint == "nouuid" && !opts.contains(&"nouuid") {
            opts.push("nouuid");
        }
    }
    opts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const U: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    fn present(dev: &str, ids: Vec<Identity>) -> BTreeMap<String, Vec<Identity>> {
        let mut m = BTreeMap::new();
        m.insert(dev.to_string(), ids);
        m
    }

    #[test]
    fn a_duplicate_names_its_partition_severity_and_command() {
        let restored = vec![(Some(2), Identity::new(IdentityKind::Xfs, U))];
        let here = present("sda2", vec![Identity::new(IdentityKind::Xfs, U)]);
        let got = clashes_against(&restored, &here, &[], Some((6, 8)));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "xfs");
        assert_eq!(got[0].part_index, Some(2));
        assert_eq!(got[0].severity, "mount");
        assert_eq!(got[0].mount_hint, "nouuid");
        assert_eq!(got[0].already_on, vec!["sda2".to_string()]);
        assert!(got[0].change_command.starts_with("xfs_admin"));
        assert_eq!(mount_options_for(&got), "nouuid");
    }

    #[test]
    fn the_overwritten_device_is_not_a_duplicate() {
        let restored = vec![(None, Identity::new(IdentityKind::Ext, U))];
        let here = present("sdb1", vec![Identity::new(IdentityKind::Ext, U)]);
        assert!(clashes_against(&restored, &here, &["sdb1".into()], None).is_empty());
    }

    #[test]
    fn a_container_put_back_from_its_saved_header_shows_its_luks_uuid() {
        use base64::Engine;
        let mut header = vec![0u8; 16384];
        header[0..6].copy_from_slice(b"LUKS\xba\xbe");
        header[168..168 + U.len()].copy_from_slice(U.as_bytes());
        let saved = own_backup_common::image::LuksHeaderBackup {
            part_index: 3,
            data_offset: header.len() as u64,
            sector_size: 512,
            kind: own_backup_common::image::VolumeEncryptionKind::Luks2,
            header_b64: base64::engine::general_purpose::STANDARD.encode(&header),
        };
        assert_eq!(
            saved_container_identity(&saved),
            Some(Identity::new(IdentityKind::Luks, U))
        );
        let short = own_backup_common::image::LuksHeaderBackup {
            data_offset: 1,
            ..saved
        };
        assert_eq!(
            saved_container_identity(&short),
            None,
            "a damaged blob says nothing"
        );
    }

    #[test]
    fn a_partuuid_is_reported_without_a_command() {
        let restored = vec![(Some(1), Identity::new(IdentityKind::PartUuid, U))];
        let here = present("nvme0n1p1", vec![Identity::new(IdentityKind::PartUuid, U)]);
        let got = clashes_against(&restored, &here, &[], None);
        assert_eq!(got[0].kind, "partuuid");
        assert_eq!(got[0].severity, "ambiguous");
        assert!(got[0].change_command.is_empty());
    }
}
