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
//! `manage-bde.exe` — он приходит с компонентом `WinPE-SecureStartup`. Носитель
//! у нас на базе **WinRE**, а WinRE обязан уметь открывать BitLocker (иначе не
//! работал бы возврат системы в исходное состояние на шифрованной машине), так
//! что программа с большой вероятностью уже в образе. Проверяется это одной
//! командой — [`manage_bde_path`]; если её нет, сборщику носителя придётся
//! добавлять компонент из ADK, и это отдельная работа.

use std::path::PathBuf;
use std::process::Command;

use tracing::{debug, info, warn};

use own_backup_common::{OwnBackupError, Result};

/// Где лежит `manage-bde.exe`, если он вообще есть в этой системе.
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
        let dev = crate::win_device::WinRawDevice::open_read(&disk)?;
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
        let dev = crate::win_device::WinRawDevice::open_read(&self.disk)?;
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
    let exe = manage_bde_path().ok_or_else(|| {
        OwnBackupError::Config(
            "manage-bde.exe в этой системе нет — для разблокировки силами Windows \
             носитель надо собирать с компонентом WinPE-SecureStartup"
                .into(),
        )
    })?;
    let vol = volume.trim_end_matches('\\').to_string();

    let mut cmd = Command::new(&exe);
    cmd.arg("-unlock").arg(&vol);
    match secret {
        // Ключ восстановления передаётся аргументом — он и задуман как то, что
        // диктуют вслух.
        Secret::RecoveryPassword(key) => {
            cmd.arg("-RecoveryPassword").arg(key);
        }
        // А парольную фразу manage-bde спрашивает сам, в консоли: в аргументах
        // ей не место (командная строка процесса видна всей системе).
        Secret::Passphrase(_) => {
            cmd.arg("-Password");
        }
    }
    debug!(exe = %exe.display(), volume = %vol, "manage-bde -unlock");

    let out = match secret {
        Secret::RecoveryPassword(_) => cmd.output(),
        Secret::Passphrase(pass) => {
            use std::io::Write;
            use std::process::Stdio;
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| bad(format!("manage-bde не запустился: {e}")))?;
            if let Some(stdin) = child.stdin.as_mut() {
                // Дважды: программа просит ввести и подтвердить.
                let _ = writeln!(stdin, "{pass}");
                let _ = writeln!(stdin, "{pass}");
            }
            child.wait_with_output()
        }
    }
    .map_err(|e| bad(format!("manage-bde не запустился: {e}")))?;

    if out.status.success() {
        info!(volume = %vol, "том открыт средствами Windows");
        return Ok(());
    }
    // Вывод manage-bde локализован, поэтому наружу отдаётся он сам — угадывать
    // причину по коду возврата хуже, чем показать то, что сказала программа.
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = text
        .lines()
        .chain(err.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(3)
        .collect();
    Err(bad(format!(
        "manage-bde не открыл {vol}: {}",
        tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
    )))
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("разблокировка средствами Windows: {msg}"))
}
