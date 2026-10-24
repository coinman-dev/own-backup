//! The source of a volume copy, opened: the device itself, or a decrypting
//! view over it when it is a BitLocker or LUKS container the passphrase
//! opens — and what is known about it once it is open.

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use super::platform::{EnginePlatform, SourceDevice};
use super::volume::VolumeBackupRequest;
use crate::fat_map::RawVolume;

/// A BitLocker source: the key, and the window onto the disk its ciphertext
/// lies in.
///
/// The volume device is not usable — locked, it refuses every read — so each
/// reader is opened straight onto `\\.\PhysicalDriveN`, bounded to the
/// partition.
pub(crate) struct BitLockerSource {
    pub(crate) params: crate::bitlocker::BitLockerParams,
    pub(crate) disk: String,
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

/// Размер сектора диска — по геометрии устройства, а не по догадке.
///
/// Нужен, когда BitLocker не заполнил поле в BPB: внутри exFAT или ReFS там
/// ноль, и без этого числа контейнер не открыть (см. `bitlocker::
/// resolve_sector_size`). Стоит одного открытия устройства.
pub(crate) fn disk_sector_size<D: SourceDevice>(disk: &str) -> Option<u32> {
    D::open_read(disk).ok().map(|d| d.sector_size())
}

/// A reader bounded to one partition of a disk, aligned for a device opened
/// without buffering (the FVE dataset starts 0x40 into its block, which is not
/// a sector boundary).
pub(crate) fn disk_window<D: SourceDevice>(
    disk: &str,
    offset: u64,
    length: u64,
) -> Result<crate::aligned_volume::AlignedVolume<crate::volume_window::VolumeWindow>> {
    Ok(crate::aligned_volume::AlignedVolume::new(
        crate::volume_window::VolumeWindow::new(Box::new(D::open_read(disk)?), offset, length)?,
    ))
}

impl BitLockerSource {
    pub(crate) fn window<D: SourceDevice>(
        &self,
    ) -> Result<crate::aligned_volume::AlignedVolume<crate::volume_window::VolumeWindow>> {
        disk_window::<D>(&self.disk, self.offset, self.length)
    }
}

/// Is this source a BitLocker volume, and does the passphrase open it?
///
/// `None` when it is not one (a LUKS container, or a plain volume); an error
/// when it is one and the passphrase does not fit, because by then the user
/// has been asked for it and silently copying the ciphertext would be worse
/// than stopping.
pub(crate) fn open_bitlocker_source<P: EnginePlatform>(
    platform: &P,
    read_path: &str,
    passphrase: &str,
) -> Result<Option<BitLockerSource>> {
    let locked = match P::Dev::open_read(read_path) {
        Ok(mut dev) => {
            let mut header = vec![0u8; 4096];
            match dev.read_at(0, &mut header) {
                // Readable and BitLocker-shaped: a locked volume on a disk
                // moved here from another machine.
                Ok(()) => crate::bitlocker::is_bitlocker(&header),
                Err(e) if is_bitlocker_locked(&e) => true,
                Err(e) => return Err(e),
            }
        }
        Err(_) => false,
    };
    if !locked {
        return Ok(None);
    }

    let extents = platform.ciphertext_extents(read_path)?;
    let [extent] = extents.as_slice() else {
        return Err(OwnBackupError::Device(
            tr!(
                "engine_win.bitlocker_multi_extent",
                volume = read_path,
                count = extents.len()
            )
            .into_owned(),
        ));
    };
    let disk = extent.device.clone();
    let mut probe = disk_window::<P::Dev>(&disk, extent.offset, extent.length)?;
    let params = crate::bitlocker::open_bitlocker_with(
        &mut probe,
        passphrase,
        disk_sector_size::<P::Dev>(&disk),
    )?;
    info!(
        algo = format!("{:#x}", params.algorithm),
        disk = %disk,
        offset = extent.offset,
        "BitLocker source unlocked; backing up decrypted data"
    );
    Ok(Some(BitLockerSource {
        params,
        disk,
        offset: extent.offset,
        length: extent.length,
    }))
}

/// Лежит ли внутри расшифрованного контейнера ReFS.
///
/// Признак тот же, что у инвентаризации (`fs_probe`): имя «ReFS» на смещении 3
/// И сигнатура структуры «FSRS» на 0x10 — одного имени мало, оно встречается и
/// в мусоре. Ошибка чтения значит «не знаем», а не «ReFS».
pub(crate) fn inner_fs_is_refs<D: SourceDevice>(src: &BitLockerSource) -> bool {
    let Ok(window) = src.window::<D>() else {
        return false;
    };
    let mut vol = crate::bitlocker::BitLockerVolume::new(Box::new(window), src.params.clone());
    let mut boot = vec![0u8; 4096];
    if crate::fat_map::RawVolume::read_at(&mut vol, 0, &mut boot).is_err() {
        return false;
    }
    &boot[3..7] == b"ReFS" && &boot[0x10..0x14] == b"FSRS"
}

/// Did this read fail because the volume is a locked BitLocker volume?
///
/// `FVE_E_LOCKED_VOLUME` (`0x80310000`) is what the filesystem stack returns for
/// every read of a locked volume through its volume handle. The code is carried
/// in the formatted Windows error — its text is localised, the hex is not, so
/// that is what is matched.
pub(crate) fn is_bitlocker_locked(e: &OwnBackupError) -> bool {
    e.to_string().contains("0x80310000")
}

/// The source of one volume copy, opened for the length of the copy.
pub(crate) struct VolumeSource<P: EnginePlatform> {
    /// What every read of the source opens: the session's read path.
    read_path: String,
    /// The key and the disk window of a BitLocker volume our own reader opened.
    pub(crate) bitlocker: Option<BitLockerSource>,
    /// The LUKS container the passphrase opened.
    pub(crate) luks: Option<crate::luks::LuksParams>,
    /// What is backed up: the device, or the decrypted data area of the
    /// container on it.
    pub(crate) volume_len: u64,
    /// Logical sector size of the source device.
    pub(crate) sector_size: u32,
    /// The write ban on the source disk while a volume the OS had to unlock
    /// stays mounted (see [`Self::open`]): it lives as long as the source.
    _source_ro_guard: Option<P::OsUnlock>,
}

impl<P: EnginePlatform> VolumeSource<P> {
    /// Open the source at `read_path` for the copy `req` asks for: unlock its
    /// container when a passphrase came with the request, and measure it.
    pub(crate) fn open(platform: &P, read_path: &str, req: &VolumeBackupRequest) -> Result<Self> {
        let read_path = read_path.to_string();

        // --- Optional LUKS unlock ---
        // When a passphrase is supplied and the source is a LUKS container, we back
        // up its DECRYPTED data area (used blocks only). The inner filesystem is
        // Linux (btrfs/ext/xfs), so the NTFS-specific steps below are skipped.
        // A BitLocker source is read through a window onto its DISK: locked, the
        // volume device answers FVE_E_LOCKED_VOLUME to everything, and the FVE
        // filter does not sit above the disk. Confirmed on a real volume
        // 2026-08-05 (Windows 10, XTS-AES 128, password protector).
        // Запрет записи на диск-источник, если том пришлось открывать силами
        // Windows: она его СМОНТИРУЕТ, а смонтированная ФС пишет сама. Страж живёт
        // до конца копирования — не до конца разблокировки.
        let mut _source_ro_guard: Option<P::OsUnlock> = None;
        let bitlocker = match &req.passphrase {
            Some(pass) => match open_bitlocker_source(platform, &read_path, pass) {
                Ok(src) => src,
                // Своё чтение FVE знает только AES-XTS 128/256. Шифр вне этого
                // списка (AES-CBC, Elephant diffuser) — не повод сдаваться: то же
                // самое умеет сама Windows, и через неё том станет обычным
                // смонтированным, со всем быстрым путём (план B5).
                Err(e) => {
                    warn!(error = %e, "свой разбор BitLocker не справился — прошу открыть Windows");
                    match platform.unlock_with_os(&read_path, pass) {
                        Ok(guard) => {
                            _source_ro_guard = Some(guard);
                            None // том теперь обычный: читаем его как все
                        }
                        Err(e2) => {
                            warn!(error = %e2, "Windows тоже не открыла том");
                            return Err(e); // наружу — первая ошибка, она по делу
                        }
                    }
                }
            },
            None => None,
        };

        // ReFS внутри контейнера: расшифровать её мы умеем, а перечислить в ней
        // файлы — нет, спецификации ReFS не существует. Хуже того, без разбора нет
        // и карты занятого: том пришлось бы копировать посекторно, целиком. Если
        // Windows готова открыть его сама, том станет обычным смонтированным — и
        // заработает и карта (FSCTL_GET_VOLUME_BITMAP на ReFS отвечает), и
        // исключения обходом дерева. Это и есть смысл этапа B5.
        //
        // Живая проверка 2026-08-20 показала, что разбор FVE отказывался от
        // ReFS-контейнера раньше (размер сектора из BPB, у ReFS там ноль), и сюда
        // не доходило. На стенде 2026-10-18 (Windows 10 LTSC 19044, XTS-AES 128)
        // разбор контейнер ОТКРЫЛ — и сработала именно эта ветка: Windows
        // смонтировала том, дальше он копируется как обычный ReFS. Без неё был бы
        // тихий откат к посекторной копии тома целиком.
        let bitlocker = match (&bitlocker, &req.passphrase) {
            (Some(b), Some(pass)) if inner_fs_is_refs::<P::Dev>(b) => {
                info!("внутри контейнера ReFS — прошу Windows открыть том, иначе её не прочесть");
                match platform.unlock_with_os(&read_path, pass) {
                    Ok(guard) => {
                        _source_ro_guard = Some(guard);
                        None
                    }
                    Err(e) => {
                        // Не беда: копия снимется нашим чтением, просто посекторно
                        // и без исключений — и об этом будет сказано ниже.
                        warn!(error = %e, "Windows не открыла ReFS-том — копирую своим чтением");
                        bitlocker
                    }
                }
            }
            _ => bitlocker,
        };

        let mut src_probe = P::Dev::open_read(&read_path)?;
        let luks: Option<crate::luks::LuksParams> = match (&req.passphrase, &bitlocker) {
            (Some(pass), None) => {
                let mut header = vec![0u8; 4096];
                src_probe.read_at(0, &mut header)?;
                if crate::luks::is_luks(&header) {
                    let params = crate::luks::open_luks(&mut src_probe, pass.as_bytes())?;
                    info!(kind = ?params.kind, "LUKS source unlocked; backing up decrypted data");
                    Some(params)
                } else {
                    None
                }
            }
            _ => None,
        };
        let device_len = src_probe.len();
        let sector_size = src_probe.sector_size();
        drop(src_probe);

        // For an encrypted source the backed-up "volume" is the decrypted data area.
        let volume_len = match (&luks, &bitlocker) {
            (Some(p), _) => device_len.saturating_sub(p.data_offset),
            (_, Some(b)) => b.params.encrypted_size.min(b.length),
            _ => device_len,
        };

        Ok(Self {
            read_path,
            bitlocker,
            luks,
            volume_len,
            sector_size,
            _source_ro_guard,
        })
    }

    /// Opens a fresh read source for the used-map builders and the block reader:
    /// the device itself, or a decrypting view over it — over the volume for
    /// LUKS, over a window onto the disk for BitLocker.
    pub(crate) fn open_source(&self) -> Result<Box<dyn crate::fat_map::RawVolume + Send>> {
        if let Some(b) = &self.bitlocker {
            return Ok(Box::new(crate::bitlocker::BitLockerVolume::new(
                Box::new(b.window::<P::Dev>()?),
                b.params.clone(),
            )));
        }
        let dev = P::Dev::open_read(&self.read_path)?;
        Ok(match &self.luks {
            Some(p) => Box::new(crate::decrypt::DecryptingVolume::new(
                Box::new(dev),
                p.clone(),
            )),
            None => Box::new(dev),
        })
    }
}
