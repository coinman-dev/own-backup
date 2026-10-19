//! The language of every message a person reads: Russian or English.
//!
//! Chosen in the window («Параметры → Оформление → Язык»); «Как в системе» is
//! resolved there, and the result — `ru` or `en` — is written to
//! `config.toml` as `ui_language` (by the window on Windows, by the service
//! on its behalf on Linux, where the file is root's) and handed to the GUI
//! process directly. The service follows the file while running, the same way
//! it follows the log level ([`crate::log_level`]), so a job that runs at
//! night reports in the language the window last spoke.
//!
//! Russian until told otherwise: the CLI, every test, and a config written
//! before the setting existed. That is also why messages go through
//! [`crate::tr!`] and not `rust_i18n::t!` directly — rust-i18n's own current
//! locale starts as English, and nothing would set it in a test.
//!
//! Texts live next to each crate in `locales/*.json` (rust-i18n format v2: a
//! key, then its `ru` and `en`). `tests/i18n_catalogs.rs` fails the build when
//! a key used in code is missing, when the two languages disagree on their
//! `%{parameters}`, or when a call passes different arguments than the text
//! takes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::config::AppConfig;

const RU: u8 = 0;
const EN: u8 = 1;

static LANG: AtomicU8 = AtomicU8::new(RU);

/// `ru` or `en` — the locale code the catalogs are keyed by.
pub fn code() -> &'static str {
    match LANG.load(Ordering::Relaxed) {
        EN => "en",
        _ => "ru",
    }
}

/// Whether messages come out in English.
pub fn is_english() -> bool {
    LANG.load(Ordering::Relaxed) == EN
}

/// Switch this process's messages. `en` is English; anything else — `ru`, an
/// empty value from an older config, garbage — is Russian.
pub fn set(code: &str) {
    LANG.store(parse(code), Ordering::Relaxed);
}

fn parse(code: &str) -> u8 {
    if code.trim().eq_ignore_ascii_case("en") {
        EN
    } else {
        RU
    }
}

/// The language the config at `path` asks for; Russian when there is none.
pub fn configured(path: &Path) -> String {
    AppConfig::load_from_file(path)
        .map(|c| c.ui_language)
        .unwrap_or_default()
}

/// The language of the system's interface, decided the way the window decides
/// «Как в системе»: Russian for a Russian interface, English for any other.
/// Windows: the user's UI language. Elsewhere: the session's locale, read
/// the way GLib (and so the window's WebKitGTK) reads it.
pub fn system() -> &'static str {
    #[cfg(windows)]
    {
        // SAFETY: no arguments; returns the user's UI language id.
        from_langid(unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() })
    }
    #[cfg(not(windows))]
    {
        let var = |name: &str| std::env::var(name).ok();
        from_posix_locale([
            var("LANGUAGE"),
            var("LC_ALL"),
            var("LC_MESSAGES"),
            var("LANG"),
        ])
    }
}

/// The first of `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG` that is set, as
/// GLib's `g_get_language_names` takes it: a `:`-separated list of tags
/// (`ru_RU.UTF-8`, `en_US:ru`). Russian when any of them is, as the window's
/// `navigator.languages` check is; English for `C`, `POSIX` and nothing.
#[cfg_attr(windows, allow(dead_code))]
fn from_posix_locale(values: [Option<String>; 4]) -> &'static str {
    let first = values
        .into_iter()
        .flatten()
        .find(|v| !v.trim().is_empty())
        .unwrap_or_default();
    let russian = first
        .split(':')
        .any(|tag| tag.trim().to_ascii_lowercase().starts_with("ru"));
    if russian {
        "ru"
    } else {
        "en"
    }
}

/// `ru` for any Russian locale id (primary language 0x19), `en` otherwise.
fn from_langid(id: u16) -> &'static str {
    if id & 0x3FF == 0x19 {
        "ru"
    } else {
        "en"
    }
}

/// The config's language, or the Windows interface's when the config has
/// none yet — what the window would choose with its default «Как в системе».
fn configured_or_system(path: &Path) -> String {
    let configured = configured(path);
    if configured.trim().is_empty() {
        system().to_string()
    } else {
        configured
    }
}

/// [`follow_config`] for something on screen that can start before the window
/// has said its language: the boot medium's taskbar, the tray icon.
///
/// The medium boots with no config at all, so the taskbar used to come up in
/// Russian and turn English a few seconds later, once the window had resolved
/// «Как в системе» on an English recovery environment and written it down
/// (2026-10-15). Deciding the same way from the start leaves nothing to switch.
pub fn follow_config_or_system(path: PathBuf) {
    set(&configured_or_system(&path));
    let _ = std::thread::Builder::new()
        .name("ui-language".into())
        .spawn(move || loop {
            std::thread::sleep(RECHECK);
            set(&configured_or_system(&path));
        });
}

/// How often the config file is consulted for a changed language.
const RECHECK: Duration = Duration::from_secs(3);

/// Apply the config's language now, then keep following it: a detached
/// thread re-reads the file every few seconds. For the service, whose
/// messages are asked for by a window that may switch language at any time.
pub fn follow_config(path: PathBuf) {
    set(&configured(&path));
    let _ = std::thread::Builder::new()
        .name("ui-language".into())
        .spawn(move || loop {
            std::thread::sleep(RECHECK);
            set(&configured(&path));
        });
}

/// A message from the calling crate's `locales/*.json`, in the language of
/// [`code`]. Arguments are named and fill `%{name}` in the text:
///
/// ```ignore
/// tr!("engine.open_failed", path = p.display(), error = e)
/// ```
///
/// Returns `Cow<str>`; `.to_string()` / `.into()` for a `String`. The key must
/// be a string literal — the catalog test finds calls by it. Every crate that
/// calls this has `rust_i18n::i18n!("locales", fallback = "ru");` at its root.
#[macro_export]
macro_rules! tr {
    ($key:literal $(, $($rest:tt)*)?) => {
        $crate::rust_i18n::t!($key, locale = $crate::lang::code() $(, $($rest)*)?)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // The global itself is never switched in a test: tests run in parallel,
    // and every other one expects the Russian texts.
    #[test]
    fn the_session_locale_decides_like_glib() {
        let some = |v: &str| Some(v.to_string());
        assert_eq!(
            from_posix_locale([None, None, None, some("ru_RU.UTF-8")]),
            "ru"
        );
        assert_eq!(
            from_posix_locale([None, None, None, some("en_US.UTF-8")]),
            "en"
        );
        // LANGUAGE first, and any Russian tag in it.
        assert_eq!(
            from_posix_locale([some("en_US:ru"), None, None, some("C.UTF-8")]),
            "ru"
        );
        // LC_ALL over LANG; an empty variable is not set.
        assert_eq!(
            from_posix_locale([some(""), some("C"), None, some("ru_RU.UTF-8")]),
            "en"
        );
        assert_eq!(
            from_posix_locale([None, None, some("ru_UA.UTF-8"), some("en_GB.UTF-8")]),
            "ru"
        );
        assert_eq!(from_posix_locale([None, None, None, None]), "en");
    }

    #[test]
    fn russian_unless_told_english() {
        assert_eq!(code(), "ru");
        assert_eq!(parse("en"), EN);
        assert_eq!(parse("EN "), EN);
        assert_eq!(parse("ru"), RU);
        assert_eq!(parse(""), RU);
        assert_eq!(parse("de"), RU);
    }

    #[test]
    fn the_windows_interface_language_is_russian_or_english() {
        assert_eq!(from_langid(0x0419), "ru"); // ru-RU
        assert_eq!(from_langid(0x0819), "ru"); // ru-MD
        assert_eq!(from_langid(0x0409), "en"); // en-US, the medium's WinPE
        assert_eq!(from_langid(0x0422), "en"); // uk-UA: not Russian
        assert_eq!(from_langid(0x0407), "en"); // de-DE
    }

    #[test]
    fn an_empty_config_defers_to_windows_and_a_set_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(configured_or_system(&path), system());
        AppConfig::update_file(&path, |c| c.ui_language = "en".into()).unwrap();
        assert_eq!(configured_or_system(&path), "en");
        AppConfig::update_file(&path, |c| c.ui_language = "ru".into()).unwrap();
        assert_eq!(configured_or_system(&path), "ru");
    }
}
