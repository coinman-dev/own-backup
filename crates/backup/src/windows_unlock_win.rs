//! Этап B5 плана `.ai/exclusions-before-copy-plan_2026-08-19.md`: отдать
//! разблокировку BitLocker самой Windows.
//!
//! Своё чтение FVE открывает том, но только AES-XTS 128/256 и только чтобы
//! ЧИТАТЬ: тома для Windows при этом не существует, каталог в нём перечислить
//! нечем, кроме нашего разбора $MFT. Для NTFS этого хватает, а для **ReFS —
//! нет**: спецификации нет, парсера не будет, и единственный способ увидеть в
//! ней файлы — заставить Windows смонтировать том по-настоящему. Тот же путь
//! попутно открывает шифры, которым наш парсер честно отказывает (AES-CBC,
//! Elephant diffuser).
//!
//! ## Диск переводится в режим только чтения ДО разблокировки
//!
//! Смонтированная файловая система пишет сама, без спроса: проигрывает журнал,
//! правит метки времени, заводит `System Volume Information`. Источник копии
//! менять нельзя — мы пришли его читать. Поэтому порядок такой и никакой
//! другой: read-only на диск → разблокировка → работа → снятие read-only.
//! Страж [`ReadOnlyDisk`] снимает атрибут в `Drop`, то есть и на панике, и на
//! раннем `?`; `Persist` не выставляется, так что перезагрузка снимет его в
//! любом случае.
//!
//! ## Чем разблокируем
//!
//! Через **WMI**, класс `Win32_EncryptableVolume` — тот самый, которым
//! пользуется `manage-bde`. Саму программу звать нельзя: `-unlock -Password`
//! читает пароль ИЗ КОНСОЛИ, перенаправленный stdin не видит и отвечает
//! 0x80070006 «неверный дескриптор» (проверено 2026-08-20). Ключ восстановления
//! она приняла бы аргументом, но тогда путь для пароля и путь для ключа были бы
//! разными, а у WMI он один — и вместо локализованного текста приходит код
//! `FVE_E_*`, по которому видно, что именно не так.
//!
//! Наличие `manage-bde.exe` остаётся дешёвым признаком того, что компонент
//! BitLocker в образе есть вообще ([`manage_bde_path`]): нет программы — не
//! будет и провайдера. Носитель у нас на базе **WinRE**, а он обязан уметь
//! открывать BitLocker, так что обычно всё на месте — на живой машине
//! 2026-08-20 нашлось.

use std::path::PathBuf;

use tracing::{info, warn};

use own_backup_common::{OwnBackupError, Result};

/// Где лежит `manage-bde.exe`, если он вообще есть в этой системе.
///
/// Сама разблокировка идёт не через него, а через WMI (он читает пароль из
/// консоли и в перенаправленный ввод не смотрит). Но его наличие — простой и
/// честный признак того, что компонент BitLocker в образе есть вообще: нет
/// программы — не будет и провайдера.
pub fn manage_bde_path() -> Option<PathBuf> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"X:\Windows".into());
    for rel in ["System32\\manage-bde.exe", "Sysnative\\manage-bde.exe"] {
        let p = PathBuf::from(&root).join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Диск, которому запрещена запись, пока страж жив.
pub struct ReadOnlyDisk {
    disk: String,
    armed: bool,
}

impl ReadOnlyDisk {
    /// Запретить запись на `\\.\PhysicalDriveN`.
    ///
    /// Отказ — это отказ: если запрет не встал, разблокировку начинать нельзя,
    /// иначе Windows примонтирует том на запись и изменит источник.
    pub fn arm(disk_number: u32) -> Result<Self> {
        let disk = format!(r"\\.\PhysicalDrive{disk_number}");
        // Дескриптор — на ЗАПИСЬ, хотя ставим запрет записи: сам по себе
        // IOCTL_DISK_SET_DISK_ATTRIBUTES требует права на изменение устройства,
        // и с читающим дескриптором отвечает «отказано в доступе» (0x80070005).
        // Проверено на живой машине 2026-08-20.
        let dev = crate::win_device::WinRawDevice::open_write(&disk)?;
        dev.set_read_only_this_session()?;
        info!(disk = %disk, "источник закрыт на запись перед разблокировкой");
        Ok(Self { disk, armed: true })
    }

    /// Снять запрет заранее (иначе он снимется в `Drop`).
    pub fn disarm(mut self) -> Result<()> {
        self.clear()
    }

    fn clear(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        // Снятие запрета — тот же IOCTL, те же требования к дескриптору.
        let dev = crate::win_device::WinRawDevice::open_write(&self.disk)?;
        dev.clear_read_only_this_session()?;
        info!(disk = %self.disk, "запрет записи снят");
        Ok(())
    }
}

impl Drop for ReadOnlyDisk {
    fn drop(&mut self) {
        if let Err(e) = self.clear() {
            // Ругаемся громко, но не паникуем: атрибут не сохраняется в реестр,
            // и перезагрузка снимет его в любом случае.
            warn!(disk = %self.disk, error = %e,
                  "не удалось снять запрет записи — снимется при перезагрузке");
        }
    }
}

/// Чем открывать том.
pub enum Secret<'a> {
    /// 48-значный ключ восстановления (`123456-…-123456`).
    RecoveryPassword(&'a str),
    /// Парольная фраза тома.
    Passphrase(&'a str),
}

/// Попросить Windows открыть заблокированный том.
///
/// `volume` — буква (`G:`) или путь тома. Диск при этом уже должен быть закрыт
/// на запись стражем [`ReadOnlyDisk`]: разблокировка ведёт к монтированию, а
/// монтирование — к записи.
pub fn unlock_with_windows(volume: &str, secret: Secret<'_>) -> Result<()> {
    let letter = drive_letter(volume)?;
    // Провайдер BitLocker живёт в WMI, и обращаться надо к нему, а не к
    // программе, которая к нему обращается. `manage-bde -unlock -Password`
    // читает пароль ИЗ КОНСОЛИ: перенаправленный stdin он не видит и отвечает
    // 0x80070006 «неверный дескриптор» (проверено на живой машине 2026-08-20).
    // Через WMI то же самое делается без консоли вовсе — и с внятным кодом
    // вместо локализованного текста.
    let code = wmi_unlock(&letter, &secret)?;
    match code {
        0 => {
            info!(volume = %letter, "том открыт средствами Windows (WMI)");
            Ok(())
        }
        0x8031_0027 => Err(bad("пароль или ключ не подходит этому тому")),
        0x8031_0008 => Err(bad("том не зашифрован BitLocker")),
        0x8031_0033 => Err(bad(
            "у тома нет такого предохранителя: паролем открывают том ДАННЫХ, \
             системный том — только ключом восстановления",
        )),
        other => Err(bad(format!("BitLocker вернул {other:#010x}"))),
    }
}

/// `\\.\C:`, `C:\` или `C:` → `C:`, как ждёт свойство `DriveLetter`.
fn drive_letter(volume: &str) -> Result<String> {
    let b = volume.as_bytes();
    for i in 1..b.len() {
        if b[i] == b':' && b[i - 1].is_ascii_alphabetic() {
            return Ok(format!("{}:", (b[i - 1] as char).to_ascii_uppercase()));
        }
    }
    Err(bad(format!(
        "{volume}: разблокировка средствами Windows работает по букве тома"
    )))
}

/// Позвать `Win32_EncryptableVolume::UnlockWith…` для тома с этой буквой.
///
/// Возвращает то, что вернул сам метод: 0 — открыт, иначе код семейства
/// `FVE_E_*`. `Err` — это другое: до метода не дошли (нет провайдера, нет
/// такого тома, отказ COM), и путать одно с другим нельзя.
fn wmi_unlock(letter: &str, secret: &Secret<'_>) -> Result<u32> {
    use windows::core::{BSTR, PCWSTR, VARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CoSetProxyBlanket,
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_DEFAULT,
        RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows::Win32::System::Wmi::{
        IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
        WBEM_FLAG_RETURN_WBEM_COMPLETE, WBEM_INFINITE,
    };

    let (method, arg_name, arg_value) = match secret {
        Secret::Passphrase(p) => ("UnlockWithPassphrase", "Passphrase", *p),
        Secret::RecoveryPassword(k) => ("UnlockWithNumericalPassword", "NumericalPassword", *k),
    };

    // SAFETY: обычная последовательность COM/WMI; каждый указатель живёт до
    // конца своего вызова, строки — до конца блока.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Уровень безопасности ставится один раз на процесс; повторный вызов
        // отвечает RPC_E_TOO_LATE, и это не ошибка.
        let _ = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        );

        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| bad(format!("WMI недоступен: {e}")))?;
        let services: IWbemServices = locator
            .ConnectServer(
                &BSTR::from(r"ROOT\CIMV2\Security\MicrosoftVolumeEncryption"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )
            .map_err(|e| {
                bad(format!(
                    "провайдера BitLocker в WMI нет ({e}) — носитель собран без \
                     компонента WinPE-SecureStartup?"
                ))
            })?;
        CoSetProxyBlanket(
            &services,
            10, // RPC_C_AUTHN_WINNT
            0,  // RPC_C_AUTHZ_NONE
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .map_err(|e| bad(format!("COM: {e}")))?;

        // 1. Найти том по букве и взять его __PATH — по нему зовётся метод.
        let query = format!("SELECT * FROM Win32_EncryptableVolume WHERE DriveLetter='{letter}'");
        let enumerator = services
            .ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(query.as_str()),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_WBEM_COMPLETE,
                None,
            )
            .map_err(|e| bad(format!("запрос тома: {e}")))?;
        let mut found: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;
        let _ = enumerator.Next(WBEM_INFINITE, &mut found, &mut returned);
        let Some(instance) = found[0].take() else {
            return Err(bad(format!(
                "{letter}: WMI не знает такого тома — он под BitLocker?"
            )));
        };
        let mut path_var = VARIANT::new();
        instance
            .Get(
                PCWSTR::from_raw(wide("__PATH").as_ptr()),
                0,
                &mut path_var,
                None,
                None,
            )
            .map_err(|e| bad(format!("путь тома: {e}")))?;
        // Обёртка VARIANT чистит себя в Drop — ручное освобождение не нужно.
        let obj_path = BSTR::try_from(&path_var).map(|b| b.to_string()).unwrap_or_default();
        if obj_path.is_empty() {
            return Err(bad("у тома нет пути в WMI"));
        }

        // 2. Подписи метода: класс отдаёт входные параметры отдельным объектом.
        let mut class_obj: Option<IWbemClassObject> = None;
        services
            .GetObject(
                &BSTR::from("Win32_EncryptableVolume"),
                WBEM_FLAG_RETURN_WBEM_COMPLETE,
                None,
                Some(&mut class_obj),
                None,
            )
            .map_err(|e| bad(format!("класс тома: {e}")))?;
        let class_obj = class_obj.ok_or_else(|| bad("класс тома не получен"))?;
        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        class_obj
            .GetMethod(
                PCWSTR::from_raw(wide(method).as_ptr()),
                0,
                &mut in_sig,
                &mut out_sig,
            )
            .map_err(|e| bad(format!("метод {method}: {e}")))?;
        let in_params = in_sig
            .ok_or_else(|| bad(format!("у метода {method} нет входных параметров")))?
            .SpawnInstance(0)
            .map_err(|e| bad(format!("параметры {method}: {e}")))?;

        // 3. Положить секрет и позвать.
        let secret_var = VARIANT::from(arg_value);
        in_params
            .Put(
                PCWSTR::from_raw(wide(arg_name).as_ptr()),
                0,
                &secret_var,
                0,
            )
            .map_err(|e| bad(format!("параметр {arg_name}: {e}")))?;
        let mut out: Option<IWbemClassObject> = None;
        let call = services.ExecMethod(
            &BSTR::from(obj_path.as_str()),
            &BSTR::from(method),
            Default::default(),
            None,
            &in_params,
            Some(&mut out),
            None,
        );
        // Секрет уходит из памяти вместе с вариантом (Drop), независимо от исхода.
        drop(secret_var);
        call.map_err(|e| bad(format!("вызов {method}: {e}")))?;

        let out = out.ok_or_else(|| bad(format!("{method} ничего не вернул")))?;
        let mut ret = VARIANT::new();
        out.Get(
            PCWSTR::from_raw(wide("ReturnValue").as_ptr()),
            0,
            &mut ret,
            None,
            None,
        )
        .map_err(|e| bad(format!("код возврата: {e}")))?;
        Ok(u32::try_from(&ret).unwrap_or(u32::MAX))
    }
}

/// UTF-16 с завершающим нулём — для `PCWSTR`.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("разблокировка средствами Windows: {msg}"))
}
