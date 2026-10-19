//! The used-block map of a volume copy: which blocks of the source hold data.
//!
//! The map the system answers for comes first, then the engine's own parsers
//! — FAT32/exFAT, ext, btrfs, xfs; NTFS too inside an unlocked container —
//! and every block is copied when none of them knows the volume.

use tracing::{info, warn};

use own_backup_common::Result;

use super::platform::{EnginePlatform, NativeUsedMap, SourceDevice};
use super::source::VolumeSource;
use super::volume::VolumeBackupRequest;
use crate::used_sectors::{AllUsedMap, UsedSectorMap};

/// The maps found for one volume. They stay typed until the exclusions are
/// settled: removing excluded clusters edits or rebuilds them.
pub(crate) struct VolumeMaps<P: EnginePlatform> {
    pub(crate) ntfs_map: Option<P::NativeMap>,
    pub(crate) fat_map: Option<crate::fat_map::FatUsedMap>,
    pub(crate) ext_map: Option<crate::ext_map::ExtUsedMap>,
    pub(crate) btrfs_map: Option<crate::btrfs_map::BtrfsUsedMap>,
    pub(crate) xfs_map: Option<crate::xfs_map::XfsUsedMap>,
    /// A map built over the decrypting view of a container, other than NTFS
    /// and FAT (see [`Self::build`]).
    pub(crate) luks_used: Option<Box<dyn UsedSectorMap>>,
    pub(crate) enc_ntfs_map: Option<crate::ntfs_map::NtfsUsedMap>,
    pub(crate) enc_fat_map: Option<crate::fat_map::FatUsedMap>,
    /// The filesystem found inside the volume (inside the container, for an
    /// encrypted source).
    pub(crate) detected_fs: Option<String>,
    /// A map was found: only used blocks are copied.
    pub(crate) smart_mode: bool,
}

impl<P: EnginePlatform> VolumeMaps<P> {
    /// Find the used-block map of the volume at `read_path`, opened as
    /// `opened`.
    pub(crate) fn build(
        opened: &VolumeSource<P>,
        read_path: &str,
        req: &VolumeBackupRequest,
    ) -> Result<Self> {
        let (luks, bitlocker) = (&opened.luks, &opened.bitlocker);

        // --- Used-sector map: NTFS → FAT32/exFAT → ext → btrfs → xfs → full copy ---
        let mut ntfs_map: Option<P::NativeMap> = None;
        let mut fat_map: Option<crate::fat_map::FatUsedMap> = None;
        let mut ext_map: Option<crate::ext_map::ExtUsedMap> = None;
        let mut btrfs_map: Option<crate::btrfs_map::BtrfsUsedMap> = None;
        let mut xfs_map: Option<crate::xfs_map::XfsUsedMap> = None;
        // For an encrypted source the map is built over the decrypting view and
        // returned boxed directly (no exclusions apply to a Linux volume).
        let mut luks_used: Option<Box<dyn UsedSectorMap>> = None;
        // NTFS внутри LUKS/BitLocker: карта держится типизированной, пока не
        // решится вопрос исключений — их вычитание перестраивает её.
        let mut enc_ntfs_map: Option<crate::ntfs_map::NtfsUsedMap> = None;
        // FAT/exFAT внутри контейнера — то же самое (BitLocker To Go на флешке
        // обычно именно exFAT, и таких носителей больше, чем ReFS-томов).
        let mut enc_fat_map: Option<crate::fat_map::FatUsedMap> = None;
        // The filesystem actually found inside the volume. For a LUKS/BitLocker
        // source this is what is INSIDE the container, because that is what the
        // image holds — the partition table outside still says "unknown", and a
        // restore screen reading only that would show a padlock over a plain
        // btrfs (D14, R8).
        let mut detected_fs: Option<String> = None;
        let smart_mode = if req.all_sectors {
            info!("Sector-by-sector mode requested; copying all blocks");
            false
        } else if luks.is_some() || bitlocker.is_some() {
            // A decrypted volume: only the RawVolume-based parsers apply. The NTFS
            // one among them — a BitLocker volume holds NTFS, and its $Bitmap is
            // inside the ciphertext, so Windows' own FSCTL cannot reach it while
            // the volume is locked. The reader below can.
            let mut src = opened.open_source()?;
            let map: Option<Box<dyn UsedSectorMap>> = if let Ok(m) =
                crate::ntfs_map::build_ntfs_used_map(src.as_mut(), req.block_size)
            {
                info!(fs = "NTFS", "Using NTFS map over decrypted volume");
                detected_fs = Some("NTFS".into());
                // НЕ боксим: NTFS внутри контейнера умеет исключения ДО копии
                // (план exclusions-before-copy, B3), а для этого карту надо
                // будет перестроить без исключённых кластеров.
                enc_ntfs_map = Some(m);
                None
            } else if let Ok(m) = crate::fat_map::build_fat_used_map(src.as_mut(), req.block_size) {
                info!(fs = m.filesystem, "Using FAT map over decrypted volume");
                detected_fs = Some(m.filesystem.to_string());
                // Как и NTFS, держим типизированной: исключения перестроят её
                // без исключённых кластеров (этап B4).
                enc_fat_map = Some(m);
                None
            } else if let Ok(m) = crate::ext_map::build_ext_used_map(src.as_mut(), req.block_size) {
                info!(fs = "ext4", "Using ext map over decrypted volume");
                detected_fs = Some("ext4".into());
                Some(Box::new(m))
            } else if let Ok(m) =
                crate::btrfs_map::build_btrfs_used_map(src.as_mut(), req.block_size)
            {
                info!(fs = "btrfs", "Using btrfs map over decrypted volume");
                detected_fs = Some("btrfs".into());
                Some(Box::new(m))
            } else if let Ok(m) = crate::xfs_map::build_xfs_used_map(src.as_mut(), req.block_size) {
                info!(fs = "xfs", "Using xfs map over decrypted volume");
                detected_fs = Some("xfs".into());
                Some(Box::new(m))
            } else {
                warn!("no used-block map for the decrypted volume — sector-by-sector");
                None
            };
            let smart = map.is_some() || enc_ntfs_map.is_some() || enc_fat_map.is_some();
            luks_used = map;
            smart
        } else {
            match P::NativeMap::open(read_path, req.block_size) {
                Ok(map) => {
                    ntfs_map = Some(map);
                    detected_fs = Some("NTFS".into());
                    true
                }
                Err(ntfs_err) => {
                    // Not NTFS — try parsing FAT32/exFAT, then ext2/3/4, structures
                    // directly (each on its own device handle).
                    let fat_attempt = P::Dev::open_read(read_path).and_then(|mut dev| {
                        crate::fat_map::build_fat_used_map(&mut dev, req.block_size)
                    });
                    match fat_attempt {
                        Ok(map) => {
                            info!(fs = map.filesystem, "Using FAT used-cluster map");
                            detected_fs = Some(map.filesystem.to_string());
                            fat_map = Some(map);
                            true
                        }
                        Err(fat_err) => {
                            let ext_attempt = P::Dev::open_read(read_path).and_then(|mut dev| {
                                crate::ext_map::build_ext_used_map(&mut dev, req.block_size)
                            });
                            match ext_attempt {
                                Ok(map) => {
                                    info!(fs = "ext4", "Using ext used-block map");
                                    detected_fs = Some("ext4".into());
                                    ext_map = Some(map);
                                    true
                                }
                                Err(ext_err) => {
                                    let btrfs_attempt =
                                        P::Dev::open_read(read_path).and_then(|mut dev| {
                                            crate::btrfs_map::build_btrfs_used_map(
                                                &mut dev,
                                                req.block_size,
                                            )
                                        });
                                    match btrfs_attempt {
                                        Ok(map) => {
                                            info!(fs = "btrfs", "Using btrfs used-block map");
                                            detected_fs = Some("btrfs".into());
                                            btrfs_map = Some(map);
                                            true
                                        }
                                        Err(btrfs_err) => {
                                            let xfs_attempt = P::Dev::open_read(read_path)
                                                .and_then(|mut dev| {
                                                    crate::xfs_map::build_xfs_used_map(
                                                        &mut dev,
                                                        req.block_size,
                                                    )
                                                });
                                            match xfs_attempt {
                                                Ok(map) => {
                                                    info!(fs = "xfs", "Using xfs used-block map");
                                                    detected_fs = Some("xfs".into());
                                                    xfs_map = Some(map);
                                                    true
                                                }
                                                Err(xfs_err) => {
                                                    warn!(
                                                        ntfs = %ntfs_err,
                                                        fat = %fat_err,
                                                        ext = %ext_err,
                                                        btrfs = %btrfs_err,
                                                        xfs = %xfs_err,
                                                        "no used-block map available, falling back to sector-by-sector"
                                                    );
                                                    false
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };

        Ok(Self {
            ntfs_map,
            fat_map,
            ext_map,
            btrfs_map,
            xfs_map,
            luks_used,
            enc_ntfs_map,
            enc_fat_map,
            detected_fs,
            smart_mode,
        })
    }

    /// The map the copy runs on, once the exclusions are settled; every block
    /// of `total_blocks` when no filesystem was recognised.
    pub(crate) fn into_used(self, total_blocks: u64) -> Box<dyn UsedSectorMap> {
        let Self {
            ntfs_map,
            fat_map,
            ext_map,
            btrfs_map,
            xfs_map,
            luks_used,
            enc_ntfs_map,
            enc_fat_map,
            ..
        } = self;
        if let Some(map) = enc_ntfs_map {
            Box::new(map)
        } else if let Some(map) = enc_fat_map {
            Box::new(map)
        } else if let Some(map) = luks_used {
            map
        } else {
            match (ntfs_map, fat_map, ext_map, btrfs_map, xfs_map) {
                (Some(map), ..) => Box::new(map),
                (None, Some(map), ..) => Box::new(map),
                (None, None, Some(map), ..) => Box::new(map),
                (None, None, None, Some(map), _) => Box::new(map),
                (None, None, None, None, Some(map)) => Box::new(map),
                (None, None, None, None, None) => Box::new(AllUsedMap::new(total_blocks)),
            }
        }
    }
}
