//! The language of every message a person reads: Russian or English.
//!
//! Chosen in the window («Параметры → Оформление → Язык»); «Как в Windows» is
//! resolved there, and the result — `ru` or `en` — is written to
//! `config.toml` as `ui_language` and handed to the GUI process directly. The
//! service follows the file while running, the same way it follows the log
//! level ([`crate::log_level`]), so a job that runs at night reports in the
//! language the window last spoke.
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
    AppConfig::load_from_file(path).map(|c| c.ui_language).unwrap_or_default()
}

/// How often the config file is consulted for a changed language.
const RECHECK: Duration = Duration::from_secs(3);

/// Apply the config's language now, then keep following it: a detached
/// thread re-reads the file every few seconds. For the service, whose
/// messages are asked for by a window that may switch language at any time.
pub fn follow_config(path: PathBuf) {
    set(&configured(&path));
    let _ = std::thread::Builder::new().name("ui-language".into()).spawn(move || loop {
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
    fn russian_unless_told_english() {
        assert_eq!(code(), "ru");
        assert_eq!(parse("en"), EN);
        assert_eq!(parse("EN "), EN);
        assert_eq!(parse("ru"), RU);
        assert_eq!(parse(""), RU);
        assert_eq!(parse("de"), RU);
    }
}
