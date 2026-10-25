//! An opened LUKS container in a whole-disk copy: the copy reads the data in
//! the clear through the container's mapping (its snapshot, its logical
//! volumes' snapshots), and keeps the container's own header from the
//! partition, so that the restore puts the container back exactly as it was
//! — same UUID, same keyslots, same passphrase (D19).
//!
//! The header is read without a passphrase: the data area's offset sits in
//! the clear, and the master key inside stays wrapped under the passphrase,
//! which the restore asks for. What is checked here is that the header and
//! the mapping describe the same data area — a mapping that starts elsewhere
//! (a detached header, a hand-made table) would make the restore put the
//! data where the container does not look.

use tracing::{info, warn};

use own_backup_common::image::{LuksHeaderBackup, VolumeEncryptionKind};
use own_backup_common::{lang::Localized, tr_both};
use own_backup_common::{tr, OwnBackupError, Result};

use super::platform::{OpenedContainer, SourceDevice};

/// What the copy keeps of an opened container.
pub(crate) struct CapturedContainer {
    pub(crate) kind: VolumeEncryptionKind,
    pub(crate) header: LuksHeaderBackup,
    /// Why a restore will not be able to put this container back, when it
    /// will not (said in the run's summary; the copy itself is fine).
    pub(crate) note: Option<Localized>,
}

/// Read the header of the container `c` is the mapping of; the mapping
/// shows `plaintext_len` bytes. Partition `part_index` of the disk.
pub(crate) fn capture_opened<D: SourceDevice>(
    c: &OpenedContainer,
    part_index: u32,
    plaintext_len: u64,
) -> Result<CapturedContainer> {
    let mut dev = D::open_read(&c.partition)?;
    let mut head = vec![0u8; 4096];
    dev.read_at(0, &mut head)?;
    if !crate::luks::is_luks(&head) {
        return Err(OwnBackupError::Config(
            tr!(
                "engine.luks_no_header_on_partition",
                partition = c.partition.as_str(),
                mapping = c.mapping.as_str()
            )
            .into_owned(),
        ));
    }
    let geometry = crate::luks::header_geometry(&mut dev)?;
    if geometry.data_offset != c.data_offset
        || geometry.data_offset.saturating_add(plaintext_len) > dev.len()
    {
        return Err(OwnBackupError::Config(
            tr!(
                "engine.luks_mapping_mismatch",
                partition = c.partition.as_str(),
                mapping = c.mapping.as_str(),
                header = geometry.data_offset,
                table = c.data_offset
            )
            .into_owned(),
        ));
    }
    let area = read_whole::<D>(c, &mut dev, geometry.data_offset)?;
    let note = rebuild_note(part_index, &area)?;
    info!(
        part = part_index,
        mapping = %c.mapping,
        bytes = area.len(),
        "header of the opened LUKS container captured"
    );
    use base64::Engine;
    Ok(CapturedContainer {
        kind: geometry.kind.clone(),
        header: LuksHeaderBackup {
            part_index,
            data_offset: geometry.data_offset,
            sector_size: geometry.sector_size,
            kind: geometry.kind,
            header_b64: base64::engine::general_purpose::STANDARD.encode(&area),
        },
        note,
    })
}

/// Reads of a header being rewritten, tried before the copy gives up.
const HEADER_READS: usize = 3;

/// The header area `[0, data_offset)` of `c`'s partition as one consistent
/// version ([`crate::luks::check_header_consistent`]): read past the page
/// cache, and again — a moment later — when the read caught cryptsetup
/// between its writes. Refused when it never reads whole.
fn read_whole<D: SourceDevice>(
    c: &OpenedContainer,
    dev: &mut D,
    data_offset: u64,
) -> Result<Vec<u8>> {
    let mut why = String::new();
    for attempt in 0..HEADER_READS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // Two reads in a row, each past the cache, must agree — a rewrite in
        // between shows even where no checksum covers it (LUKS1, the keyslot
        // areas of LUKS2) — and LUKS2's two headers must agree with their
        // checksums and with each other.
        dev.drop_cache()?;
        let area = crate::luks::read_header_area_at(dev, data_offset)?;
        dev.drop_cache()?;
        let again = crate::luks::read_header_area_at(dev, data_offset)?;
        let whole = if area == again {
            crate::luks::check_header_consistent(&area)
        } else {
            Err(tr!("luks.torn_reads_differ").into_owned())
        };
        match whole {
            Ok(()) => return Ok(area),
            Err(e) => {
                warn!(partition = %c.partition, attempt, error = %e,
                      "the LUKS header did not read whole");
                why = e;
            }
        }
    }
    Err(OwnBackupError::Device(
        tr!(
            "engine.luks_header_torn",
            partition = c.partition.as_str(),
            why = why
        )
        .into_owned(),
    ))
}

/// Read the header of the opened container `c` again at the end of the
/// copy and compare it with the one `captured` at its start. The same: `None`.
/// Changed in what does not touch the data (a key slot added or removed, a
/// token, the UUID or label): the header as it is now, which is what the
/// restored container should carry. Changed in its cipher, data area or
/// master key: refused — the copy would hold data no saved header opens; a
/// reencryption started meanwhile is said as such (a retry helps only once
/// it is over).
pub(crate) fn recheck<D: SourceDevice>(
    c: &OpenedContainer,
    part_index: u32,
    captured: &LuksHeaderBackup,
) -> Result<Option<CapturedContainer>> {
    let mut dev = D::open_read(&c.partition)?;
    let now = read_whole::<D>(c, &mut dev, captured.data_offset)?;
    let then = crate::luks::decode_stored_header(captured)?;
    if now == then {
        return Ok(None);
    }
    if crate::luks::reencryption_in_progress(&now) {
        warn!(partition = %c.partition, "the LUKS container started a reencryption during the copy");
        return Err(OwnBackupError::Device(
            tr!("engine.luks_reencrypting", partition = c.partition.as_str()).into_owned(),
        ));
    }
    let geometry = crate::luks::header_geometry(&mut HeaderBytes(&now))?;
    if geometry.data_offset != captured.data_offset
        || geometry.sector_size != captured.sector_size
        || crate::luks::data_identity(&now)? != crate::luks::data_identity(&then)?
    {
        warn!(partition = %c.partition, "the LUKS header changed its data key or layout during the copy");
        return Err(OwnBackupError::Device(
            tr!(
                "engine.luks_header_changed",
                partition = c.partition.as_str()
            )
            .into_owned(),
        ));
    }
    warn!(partition = %c.partition,
          "the LUKS header changed during the copy (key slots, tokens, UUID): the copy keeps it as it is now");
    use base64::Engine;
    Ok(Some(CapturedContainer {
        kind: captured.kind.clone(),
        header: LuksHeaderBackup {
            header_b64: base64::engine::general_purpose::STANDARD.encode(&now),
            ..captured.clone()
        },
        // A header that now opens only by a token (TPM) is said, as at the
        // start of the copy.
        note: rebuild_note(part_index, &now)?,
    }))
}

/// [`CapturedContainer::note`] for the header area `area` of partition
/// `part_index`: why a restore cannot put it back, when it cannot.
fn rebuild_note(part_index: u32, area: &[u8]) -> Result<Option<Localized>> {
    Ok(match crate::luks::restore_obstacles(area)? {
        why if why.is_empty() => None,
        why => {
            warn!(part = part_index, why = ?why,
                  "the container's header is kept, but a restore cannot put it back");
            Some(tr_both!(
                "engine.luks_not_rebuildable",
                index = part_index,
                why = why.join("; ")
            ))
        }
    })
}

/// A header area in memory, read as a volume.
struct HeaderBytes<'a>(&'a [u8]);

impl crate::fat_map::RawVolume for HeaderBytes<'_> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let o = offset as usize;
        let end = o
            .checked_add(buf.len())
            .filter(|&e| e <= self.0.len())
            .ok_or_else(|| OwnBackupError::Device("read past the header area".into()))?;
        buf.copy_from_slice(&self.0[o..end]);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

/// A test disk with an opened LUKS container, for the engine's tests and the
/// restore's.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    use own_backup_common::gpt::{DiskLayout, GptPartitionEntry};
    use own_backup_common::types::{BackupType, ImageFormat};

    use crate::engine_core::disk_image::{DiskBackupRequest, DiskPartitionBackup};
    use crate::engine_core::test_platform::{
        register_device, unique_path, TestPlatform, TestSource,
    };

    pub(crate) const MIB: u64 = 1024 * 1024;
    pub(crate) const BS: u32 = 64 * 1024;
    pub(crate) const DISK: u32 = 9;
    pub(crate) const PASS: &str = "luks words";
    /// Where `luks::test_luks1_container` starts its data.
    pub(crate) const HEADER: u64 = 2 * MIB;
    pub(crate) const ESP_AT: u64 = MIB;
    pub(crate) const PART_AT: u64 = 4 * MIB;
    /// Not a whole number of blocks: the last block of the container's slot
    /// runs past the end of its data.
    pub(crate) const PAYLOAD: u64 = 6 * MIB + 8 * 1024;
    const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    const LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

    /// An ESP-like raw partition, and a LUKS1 container whose mapping is
    /// open: the partition holds the container, the mapping its data.
    pub(crate) struct LuksDisk {
        pub(crate) device: String,
        pub(crate) esp: String,
        pub(crate) esp_data: Vec<u8>,
        pub(crate) part: String,
        pub(crate) mapping: String,
        pub(crate) container: Vec<u8>,
        pub(crate) plain: Vec<u8>,
        pub(crate) layout: DiskLayout,
    }

    impl LuksDisk {
        pub(crate) fn new() -> Self {
            Self::holding(
                (0..PAYLOAD)
                    .map(|i| (i / 512 % 239) as u8 ^ (i % 13) as u8 ^ 0x6B)
                    .collect(),
            )
        }

        /// The container holding `plain` ([`PAYLOAD`] bytes).
        pub(crate) fn holding(plain: Vec<u8>) -> Self {
            assert_eq!(plain.len() as u64, PAYLOAD);
            let esp_data: Vec<u8> = (0..3 * MIB).map(|i| (i % 251) as u8 ^ 0x11).collect();
            let container = crate::luks::test_luks1_container(PASS.as_bytes(), &plain);
            assert_eq!(container.len() as u64, HEADER + PAYLOAD);
            let size = PART_AT + HEADER + PAYLOAD + MIB;
            let layout = DiskLayout {
                disk_guid: "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F9".into(),
                sector_size: 512,
                size_bytes: size,
                partitions: vec![
                    GptPartitionEntry {
                        index: 1,
                        type_guid: ESP_TYPE.into(),
                        part_guid: "22222222-0000-0000-0000-000000000001".into(),
                        first_lba: ESP_AT / 512,
                        last_lba: (ESP_AT + esp_data.len() as u64) / 512 - 1,
                        attributes: 0,
                        name: "EFI system partition".into(),
                    },
                    GptPartitionEntry {
                        index: 2,
                        type_guid: LINUX_FS.into(),
                        part_guid: "22222222-0000-0000-0000-000000000002".into(),
                        first_lba: PART_AT / 512,
                        last_lba: (PART_AT + container.len() as u64) / 512 - 1,
                        attributes: 0,
                        name: "root".into(),
                    },
                ],
            };
            let disk = Self {
                device: unique_path("disk"),
                esp: unique_path("esp"),
                esp_data,
                part: unique_path("luks-part"),
                mapping: unique_path("luks-mapping"),
                container,
                plain,
                layout,
            };
            let mut bytes = vec![0u8; size as usize];
            for op in crate::gpt_writer::disk_gpt_write_ops(&disk.layout, size).unwrap() {
                let at = op.offset as usize;
                bytes[at..at + op.bytes.len()].copy_from_slice(&op.bytes);
            }
            bytes[ESP_AT as usize..ESP_AT as usize + disk.esp_data.len()]
                .copy_from_slice(&disk.esp_data);
            bytes[PART_AT as usize..PART_AT as usize + disk.container.len()]
                .copy_from_slice(&disk.container);
            register_device(&disk.device, bytes, 512);
            register_device(&disk.esp, disk.esp_data.clone(), 512);
            register_device(&disk.part, disk.container.clone(), 512);
            register_device(&disk.mapping, disk.plain.clone(), 512);
            disk
        }

        /// An ext4 of [`PAYLOAD`] bytes made by `mkfs.ext4 -d` (no mount, no
        /// root) holding `keep.txt`; `None` without the tool.
        pub(crate) fn ext4() -> Option<Vec<u8>> {
            let dir = tempfile::tempdir().unwrap();
            let content = dir.path().join("content");
            std::fs::create_dir(&content).unwrap();
            std::fs::write(content.join("keep.txt"), b"kept\n").unwrap();
            let img = dir.path().join("fs.img");
            std::fs::File::create(&img)
                .unwrap()
                .set_len(PAYLOAD)
                .unwrap();
            let made = std::process::Command::new("mkfs.ext4")
                .args(["-q", "-F", "-b", "4096", "-d"])
                .arg(&content)
                .arg(&img)
                .output()
                .ok()?
                .status
                .success();
            made.then(|| std::fs::read(&img).unwrap())
        }

        /// The platform: the ESP read directly, the container through its
        /// mapping, said to start `data_offset` into the partition.
        pub(crate) fn platform(&self, data_offset: u64) -> TestPlatform {
            TestPlatform::new()
                .source(&self.esp, TestSource::Direct { frozen: true })
                .source(
                    &self.part,
                    TestSource::Opened {
                        mapping: self.mapping.clone(),
                        data_offset,
                        pv: None,
                    },
                )
                .slot(&self.esp, DISK, ESP_AT)
                .slot(&self.part, DISK, PART_AT)
        }

        pub(crate) fn request(&self, image: &Path) -> DiskBackupRequest {
            DiskBackupRequest {
                disk_number: DISK,
                disk_device: Some(self.device.clone()),
                disk_model: "LUKS test disk".into(),
                partitions: vec![
                    DiskPartitionBackup {
                        device: self.esp.clone(),
                        part_index: 1,
                        passphrase: None,
                    },
                    // No passphrase: the system opened the container.
                    DiskPartitionBackup {
                        device: self.part.clone(),
                        part_index: 2,
                        passphrase: None,
                    },
                ],
                image_path: image.to_path_buf(),
                full_image_path: None,
                block_size: BS,
                image_format: ImageFormat::Qcow2,
                job_id: Some("disk-luks-tests".into()),
                encryption: None,
                exclusion_patterns_windows: Vec::new(),
                exclusion_patterns_linux: Vec::new(),
                kind: BackupType::Full,
                parent_image: None,
                source_disk: None,
                bootable: false,
                zstd_level: 0,
            }
        }

        /// Copy the disk into `image`.
        pub(crate) fn backup(&self, image: &Path) {
            self.backup_sealed(image, None);
        }

        /// The same, into an encrypted `.obk` when `keys` are given.
        pub(crate) fn backup_sealed(
            &self,
            image: &Path,
            keys: Option<crate::seal::context::ArchiveKeys>,
        ) {
            let req = DiskBackupRequest {
                encryption: keys,
                ..self.request(image)
            };
            crate::engine_core::disk::backup_disk(&self.platform(HEADER), &req, |_| Ok(()))
                .unwrap();
        }
    }

    /// Our private files left in `dir` (`.own-backup-new-*`): a refused or
    /// failed copy must leave none.
    pub(crate) fn private_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".own-backup-new-") || n.starts_with(".own-backup-cow-"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use crate::engine_core::read::{load_sidecar_of, ImageReadSource};
    use crate::engine_core::test_platform::register_device;
    use own_backup_common::image::{VolumeCaptureMode, VolumeEncryptionKind};

    fn image_bytes(image: &std::path::Path, offset: u64, len: usize) -> Vec<u8> {
        let sidecar = load_sidecar_of(image).unwrap();
        let mut reader = ImageReadSource::open(image, &sidecar.info.container).unwrap();
        let mut out = vec![0u8; len];
        reader.read_at(offset, &mut out).unwrap();
        out
    }

    /// The copy holds the data in the clear where the partition starts, and
    /// keeps the container's header — the bytes before its data, verbatim —
    /// so a restore can put the container back.
    #[test]
    fn an_opened_container_is_copied_in_the_clear_with_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let disk = LuksDisk::new();
        let image = dir.path().join("disk.qcow2");
        disk.backup(&image);

        assert!(image_bytes(&image, PART_AT, disk.plain.len()) == disk.plain);
        assert!(image_bytes(&image, ESP_AT, disk.esp_data.len()) == disk.esp_data);
        let sidecar = load_sidecar_of(&image).unwrap();
        let [h] = sidecar.info.luks_headers.as_slice() else {
            panic!("one header: {}", sidecar.info.luks_headers.len());
        };
        assert_eq!(h.part_index, 2);
        assert_eq!(h.data_offset, HEADER);
        assert_eq!(h.kind, VolumeEncryptionKind::Luks1);
        let saved = crate::luks::decode_stored_header(h).unwrap();
        assert!(saved == disk.container[..HEADER as usize]);
        let dm = sidecar.disk_manifest.unwrap();
        assert!(dm.images[0].encryption.is_none());
        let enc = dm.images[1]
            .encryption
            .as_ref()
            .expect("the container is named");
        assert_eq!(enc.kind, VolumeEncryptionKind::Luks1);
        assert_eq!(enc.capture_mode, VolumeCaptureMode::Plaintext);
    }

    /// The service's key directory inside an opened container (decision
    /// 6/E4): the filesystem read through the mapping is walked and the
    /// directory left out; a container whose contents nothing walks is
    /// refused rather than copied with the keys.
    #[test]
    fn the_key_directory_inside_an_opened_container_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let Some(ext4) = LuksDisk::ext4() else {
            return;
        };
        let disk = LuksDisk::holding(ext4);
        let platform = disk.platform(HEADER).key_store_on(&disk.part);
        let image = dir.path().join("keys.qcow2");
        let summary =
            crate::engine_core::disk::backup_disk(&platform, &disk.request(&image), |_| Ok(()))
                .unwrap();
        assert!(summary.key_store_excluded);
        let sidecar = load_sidecar_of(&image).unwrap();
        assert_eq!(sidecar.info.luks_headers.len(), 1, "the header still kept");

        let raw = LuksDisk::new();
        let platform = raw.platform(HEADER).key_store_on(&raw.part);
        let refused = dir.path().join("refused.qcow2");
        let e =
            crate::engine_core::disk::backup_disk(&platform, &raw.request(&refused), |_| Ok(()))
                .err()
                .expect("refused");
        assert!(e.to_string().contains(&raw.part), "{e}");
        assert!(!refused.exists());
        assert!(private_leftovers(dir.path()).is_empty());
    }

    /// Run the copy with the partition's bytes replaced by `changed` once
    /// blocks are being copied — after the header was captured.
    fn backup_changing(
        disk: &LuksDisk,
        image: &std::path::Path,
        changed: Vec<u8>,
    ) -> own_backup_common::Result<()> {
        let mut changed = Some(changed);
        crate::engine_core::disk::backup_disk(&disk.platform(HEADER), &disk.request(image), |p| {
            if p.backup_id != "prepare" {
                if let Some(bytes) = changed.take() {
                    register_device(&disk.part, bytes, 512);
                }
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// A key slot changed while the disk was copied: the copy keeps the
    /// header as it is at the end. The master key changed: no copy — the
    /// data would be under a key no kept header opens.
    #[test]
    fn a_header_changed_during_the_copy_is_kept_as_it_is_now_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        let disk = LuksDisk::new();
        let mut slot = disk.container.clone();
        slot[256 + 4] ^= 1; // keyslot 1's iterations: no data touched
        let image = dir.path().join("slot.qcow2");
        backup_changing(&disk, &image, slot.clone()).unwrap();
        let sidecar = load_sidecar_of(&image).unwrap();
        let saved = crate::luks::decode_stored_header(&sidecar.info.luks_headers[0]).unwrap();
        assert!(saved == slot[..HEADER as usize], "the header as it is now");

        let disk = LuksDisk::new();
        let mut key = disk.container.clone();
        key[112] ^= 1; // the master key's digest
        let image = dir.path().join("key.qcow2");
        let e = backup_changing(&disk, &image, key).expect_err("refused");
        assert!(e.to_string().contains(&disk.part), "{e}");
        assert!(!image.exists());
        assert!(private_leftovers(dir.path()).is_empty());
    }

    /// A mapping that does not start where the header says, and a partition
    /// without a header of its own (`--header`): no copy.
    #[test]
    fn a_container_the_restore_could_not_rebuild_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let disk = LuksDisk::new();
        let image = dir.path().join("odd.qcow2");
        let e = crate::engine_core::disk::backup_disk(
            &disk.platform(MIB),
            &disk.request(&image),
            |_| Ok(()),
        )
        .err()
        .expect("refused");
        assert!(e.to_string().contains(&disk.mapping), "{e}");
        assert!(!image.exists());
        assert!(private_leftovers(dir.path()).is_empty());

        register_device(&disk.part, vec![0u8; disk.container.len()], 512);
        let e = crate::engine_core::disk::backup_disk(
            &disk.platform(HEADER),
            &disk.request(&image),
            |_| Ok(()),
        )
        .err()
        .expect("refused");
        assert!(e.to_string().contains("--header"), "{e}");
        assert!(!image.exists());
        assert!(private_leftovers(dir.path()).is_empty());
    }
}
