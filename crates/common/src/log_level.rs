//! The «Уровень журнала» setting, applied while running.
//!
//! Two levels, because two questions get asked of a log. `info` — what
//! happened and what went wrong: every crate at `info`, warnings and errors
//! included. `debug` — everything our own code says, for a bug report; third
//! party crates stay at `info` even then, since hyper and h2 at `debug` write a
//! line per gRPC frame and would bury ours.
//!
//! Before this the setting was saved and never read: both processes logged
//! with a filter fixed at build time, whatever the settings screen said.
//!
//! Like [`crate::log_mirror`], the value is re-read while running — the user
//! turns on `debug` because something is about to go wrong, and "restart the
//! service first" is not an instruction worth giving then. This module knows
//! nothing about `tracing-subscriber`: the caller hands [`follow_config`] a
//! closure that swaps its filter.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::AppConfig;

/// How often the config file is consulted for a changed level.
const RECHECK: Duration = Duration::from_secs(3);

/// The `EnvFilter` directives a `log_level` value stands for. Anything that is
/// not `debug` (or the older `trace`) is `info` — including the `warn` and
/// `error` an older settings screen could save.
pub fn filter_directives(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "debug" | "trace" => "info,own_backup=debug",
        _ => "info",
    }
}

/// The level the config at `path` asks for.
///
/// With no config to read, `fallback`. The boot medium passes `debug` here:
/// its config lives on a RAM disk that starts empty, and a session there is
/// the one whose log has to explain itself afterwards.
pub fn configured_level(path: &Path, fallback: &str) -> String {
    AppConfig::load_from_file(path).map(|c| c.log_level).unwrap_or_else(|_| fallback.to_string())
}

/// Watch the config at `path` and call `apply` with the new directives each
/// time the level it names changes. `current` is what the filter was built
/// from, so the first call only comes with an actual change.
///
/// A detached thread, alive for the process: it sleeps between reads and the
/// file is a few hundred bytes.
pub fn follow_config(
    path: PathBuf,
    fallback: &'static str,
    current: String,
    apply: impl Fn(&'static str) + Send + 'static,
) {
    let _ = std::thread::Builder::new().name("log-level".into()).spawn(move || {
        let mut current = filter_directives(&current);
        loop {
            std::thread::sleep(RECHECK);
            let wanted = filter_directives(&configured_level(&path, fallback));
            if wanted != current {
                apply(wanted);
                current = wanted;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_levels_whatever_was_saved() {
        assert_eq!(filter_directives("info"), "info");
        assert_eq!(filter_directives("debug"), "info,own_backup=debug");
        // What an older screen could save still means one of the two.
        assert_eq!(filter_directives("trace"), "info,own_backup=debug");
        assert_eq!(filter_directives("warn"), "info");
        assert_eq!(filter_directives("error"), "info");
        assert_eq!(filter_directives(" Debug "), "info,own_backup=debug");
        assert_eq!(filter_directives(""), "info");
    }

    #[test]
    fn no_config_means_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(configured_level(&dir.path().join("none.toml"), "debug"), "debug");
        let path = dir.path().join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.log_level = "debug".into();
        cfg.save_to_file(&path).unwrap();
        assert_eq!(configured_level(&path, "info"), "debug");
    }
}
