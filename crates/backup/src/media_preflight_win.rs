//! Годится ли это окружение для интерфейса — и что сказать, если нет.
//!
//! Загрузочный носитель поднимает интерфейс в WinPE, где три вещи могут
//! отсутствовать: среда выполнения WebView2 (её кладёт сборщик РЯДОМ с
//! программой, вне `boot.wim`), место на `X:` (стандартный WinPE даёт 32 МБ
//! scratch, браузерный движок на них не заводится — сборщик поднимает до 512) и
//! память. Раньше при любой из этих бед пользователь видел пустой экран: окно
//! не создавалось, а процесс жил дальше и держал единственную консоль.
//!
//! Правило носителя — «ничего не удерживать»: если интерфейс не поднять, надо
//! честно сказать почему и оставить человеку рабочий CLI, которым копия
//! снимается целиком. Поэтому проверки живут отдельно от интерфейса и зовутся
//! дважды: `own-backup-cli media-preflight` (синхронно из `start-media.cmd`, до
//! запуска чего-либо) и сам GUI при старте — тем же кодом, чтобы диагноз был
//! один и тот же.
//!
//! Разбор на «сбор данных» и «суждение» здесь не украшение: системные вызовы
//! есть только в Windows, а пороги и формулировки проверяются юнит-тестами на
//! любой машине.

use own_backup_common::tr;
use std::path::{Path, PathBuf};

/// Насколько плохо то, что нашлось.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Всё на месте.
    Ok,
    /// Работать будет, но на грани — сказать и продолжить.
    Warn,
    /// Интерфейс поднимать бессмысленно.
    Fail,
}

/// Одна проверка с готовым человеческим текстом.
#[derive(Clone, Debug)]
pub struct Check {
    /// Короткое имя для лога: `webview2`, `scratch`, `memory`.
    pub name: &'static str,
    pub level: Level,
    /// Что именно нашлось — эту строку видит пользователь в консоли.
    pub message: String,
}

impl Check {
    fn new(name: &'static str, level: Level, message: impl Into<String>) -> Self {
        Check {
            name,
            level,
            message: message.into(),
        }
    }
}

/// Меньше этого свободного места на диске с временным каталогом — интерфейс не
/// поднимется: распаковка движка и его кеш идут именно туда. Стандартный WinPE
/// даёт 32 МБ, сборщик поднимает до 512.
pub const SCRATCH_MIN_BYTES: u64 = 256 * 1024 * 1024;
/// Ниже этого доступной памяти WebView2 ещё запускается, но впритык (сам движок
/// берёт около 200 МБ).
pub const MEMORY_WARN_BYTES: u64 = 700 * 1024 * 1024;

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

// --- суждения (чистые, проверяются тестами на любой платформе) --------------

/// Вердикт по среде выполнения WebView2.
pub fn judge_webview2(found: Option<&Path>, searched: &[PathBuf]) -> Check {
    match found {
        Some(p) => Check::new("webview2", Level::Ok, format!("WebView2: {}", p.display())),
        None => {
            let mut m = tr!("media_preflight_win.webview2_missing").into_owned();
            for s in searched {
                m.push_str(&format!("\n      {}", s.display()));
            }
            m.push_str("\n    ");
            m.push_str(&tr!("media_preflight_win.webview2_missing_advice"));
            Check::new("webview2", Level::Fail, m)
        }
    }
}

/// Вердикт по свободному месту под временный каталог.
pub fn judge_scratch(free_bytes: Option<u64>, writable: bool, dir: &Path) -> Check {
    if !writable {
        return Check::new(
            "scratch",
            Level::Fail,
            tr!(
                "media_preflight_win.scratch_not_writable",
                dir = dir.display()
            ),
        );
    }
    match free_bytes {
        None => Check::new(
            "scratch",
            Level::Warn,
            tr!("media_preflight_win.scratch_unknown", dir = dir.display()),
        ),
        Some(free) if free < SCRATCH_MIN_BYTES => Check::new(
            "scratch",
            Level::Fail,
            tr!(
                "media_preflight_win.scratch_too_small",
                dir = dir.display(),
                free = mib(free),
                need = mib(SCRATCH_MIN_BYTES)
            ),
        ),
        Some(free) => Check::new(
            "scratch",
            Level::Ok,
            tr!(
                "media_preflight_win.scratch_ok",
                dir = dir.display(),
                free = mib(free)
            ),
        ),
    }
}

/// Вердикт по доступной памяти.
pub fn judge_memory(available_bytes: Option<u64>) -> Check {
    match available_bytes {
        None => Check::new(
            "memory",
            Level::Warn,
            tr!("media_preflight_win.memory_unknown"),
        ),
        Some(a) if a < MEMORY_WARN_BYTES => Check::new(
            "memory",
            Level::Warn,
            tr!("media_preflight_win.memory_low", available = mib(a)),
        ),
        Some(a) => Check::new(
            "memory",
            Level::Ok,
            tr!("media_preflight_win.memory_ok", available = mib(a)),
        ),
    }
}

/// Худший из вердиктов — по нему решается, запускать ли интерфейс.
pub fn worst(checks: &[Check]) -> Level {
    if checks.iter().any(|c| c.level == Level::Fail) {
        Level::Fail
    } else if checks.iter().any(|c| c.level == Level::Warn) {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// Шпаргалка на случай, когда интерфейса не будет: этим CLI копия снимается и
/// восстанавливается целиком, ради этого всё и затевалось.
pub fn cli_cheat_sheet() -> String {
    tr!("media_preflight_win.cli_cheat_sheet").into_owned()
}

/// Файл-маркер «интерфейс поднялся»: GUI создаёт его после того, как окно
/// действительно построено. `media-wait` ждёт именно его, а не факт живого
/// процесса — процесс может жить и без окна, ровно с этого началась вся история.
pub fn gui_up_marker() -> PathBuf {
    std::env::temp_dir().join("own-backup-gui-up")
}

// --- сбор данных ------------------------------------------------------------

/// Где на этой машине лежит среда выполнения WebView2, и где её искали.
///
/// Порядок тот же, каким её находит загрузчик WebView2, иначе проверка сказала
/// бы «нашёл», а интерфейс не нашёл: переменная окружения, затем папка рядом с
/// программой (так обслуживается машина без Edge и так носитель несёт свою
/// копию), затем установленный Evergreen — из них берётся самая новая версия.
pub fn find_webview2_runtime(program_dir: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut searched = Vec::new();
    if let Ok(env_dir) = std::env::var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") {
        let p = PathBuf::from(env_dir);
        searched.push(p.join("msedgewebview2.exe"));
        if p.join("msedgewebview2.exe").exists() {
            return (Some(p), searched);
        }
    }
    let beside = program_dir.join("WebView2");
    searched.push(beside.join("msedgewebview2.exe"));
    if beside.join("msedgewebview2.exe").exists() {
        return (Some(beside), searched);
    }
    let mut best: Option<(String, PathBuf)> = None;
    for base in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(root) = std::env::var(base) else {
            continue;
        };
        let apps = PathBuf::from(root).join(r"Microsoft\EdgeWebView\Application");
        searched.push(
            apps.join(tr!("media_preflight_win.version_placeholder").into_owned())
                .join("msedgewebview2.exe"),
        );
        let Ok(rd) = std::fs::read_dir(&apps) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            if !e.path().join("msedgewebview2.exe").exists() {
                continue;
            }
            if best.as_ref().map(|(n, _)| name > *n).unwrap_or(true) {
                best = Some((name, e.path()));
            }
        }
    }
    (best.map(|(_, p)| p), searched)
}

/// Свободные байты на томе с этим каталогом.
#[cfg(target_os = "windows")]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_to_caller: u64 = 0;
    // SAFETY: путь заканчивается нулём, выходной указатель валиден.
    let ok = unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free_to_caller), None, None)
    };
    ok.ok().map(|_| free_to_caller)
}

#[cfg(not(target_os = "windows"))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Доступная (не общая) память: именно её не хватает движку.
#[cfg(target_os = "windows")]
fn available_memory() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut st = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: размер структуры проставлен, как требует API.
    let ok = unsafe { GlobalMemoryStatusEx(&mut st) };
    ok.ok().map(|_| st.ullAvailPhys)
}

#[cfg(not(target_os = "windows"))]
fn available_memory() -> Option<u64> {
    None
}

/// Можно ли писать в каталог — проверяется записью, а не правами: на носителе
/// каталог может лежать на переполненном RAM-диске, и права тут ни при чём.
fn is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join("own-backup-write-probe.tmp");
    let ok = std::fs::write(&probe, b"probe").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Все проверки разом. `program_dir` — каталог с исполняемыми файлами.
pub fn run_preflight(program_dir: &Path) -> Vec<Check> {
    let (found, searched) = find_webview2_runtime(program_dir);
    let tmp = std::env::temp_dir();
    vec![
        judge_webview2(found.as_deref(), &searched),
        judge_scratch(free_space(&tmp), is_writable(&tmp), &tmp),
        judge_memory(available_memory()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_runtime_fails_and_says_where_it_looked() {
        let searched = vec![PathBuf::from(
            r"X:\Program Files\OwnBackup\WebView2\msedgewebview2.exe",
        )];
        let c = judge_webview2(None, &searched);
        assert_eq!(c.level, Level::Fail);
        assert!(c.message.contains("WebView2"), "{}", c.message);
        assert!(
            c.message.contains(r"X:\Program Files"),
            "путь должен быть в тексте: {}",
            c.message
        );
    }

    #[test]
    fn a_found_runtime_passes() {
        let p = PathBuf::from(r"X:\Program Files\OwnBackup\WebView2");
        assert_eq!(judge_webview2(Some(&p), &[]).level, Level::Ok);
    }

    /// Тот самый случай: образ загрузился со стандартным для WinPE scratch.
    #[test]
    fn the_default_winpe_scratch_is_refused_with_the_reason() {
        let c = judge_scratch(Some(32 * 1024 * 1024), true, Path::new(r"X:\Windows\Temp"));
        assert_eq!(c.level, Level::Fail);
        assert!(
            c.message.contains("32 МБ"),
            "нужна подсказка про scratch: {}",
            c.message
        );
        assert!(
            c.message.contains("512"),
            "нужна подсказка, чем чинить: {}",
            c.message
        );
    }

    #[test]
    fn a_roomy_scratch_passes_and_an_unwritable_one_does_not() {
        assert_eq!(
            judge_scratch(Some(600 * 1024 * 1024), true, Path::new("X:")).level,
            Level::Ok
        );
        assert_eq!(
            judge_scratch(Some(600 * 1024 * 1024), false, Path::new("X:")).level,
            Level::Fail
        );
    }

    /// Неизвестное — не отказ: не всякую машину мы умеем расспросить, а
    /// запретить работу из-за собственного незнания хуже, чем предупредить.
    #[test]
    fn what_cannot_be_measured_only_warns() {
        assert_eq!(
            judge_scratch(None, true, Path::new("X:")).level,
            Level::Warn
        );
        assert_eq!(judge_memory(None).level, Level::Warn);
    }

    #[test]
    fn tight_memory_warns_but_does_not_block() {
        assert_eq!(judge_memory(Some(400 * 1024 * 1024)).level, Level::Warn);
        assert_eq!(judge_memory(Some(4 * 1024 * 1024 * 1024)).level, Level::Ok);
    }

    #[test]
    fn the_worst_verdict_wins() {
        let ok = Check::new("a", Level::Ok, "");
        let warn = Check::new("b", Level::Warn, "");
        let fail = Check::new("c", Level::Fail, "");
        assert_eq!(worst(&[ok.clone(), warn.clone()]), Level::Warn);
        assert_eq!(worst(&[ok.clone(), warn, fail]), Level::Fail);
        assert_eq!(worst(&[ok]), Level::Ok);
    }

    #[test]
    fn the_cheat_sheet_names_the_commands_that_actually_exist() {
        let s = cli_cheat_sheet();
        for cmd in ["list-disks", "backup", "restore-disk", "finalize-boot"] {
            assert!(s.contains(cmd), "в шпаргалке нет {cmd}");
        }
    }
}
