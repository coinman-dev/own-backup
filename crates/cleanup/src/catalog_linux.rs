//! What can be cleaned on Linux (PLAN §6б, stage 8; owner's own
//! `wsl-cleanup.sh` as the model).
//!
//! The service runs as root. The system's part — the apt cache, rotated logs,
//! the journal, crash reports, old snap revisions — is root's and cleaned by
//! root, for an administrator only. A user's part is below their home and
//! cleaned with that user's own rights (`users::as_user`). `/tmp` and
//! `/var/tmp` are not here: systemd-tmpfiles cleans them already, and root
//! removing files in a folder everyone writes to is how cleaners get fooled
//! into removing what a link points at.
//!
//! Never part of anything here, as in the owner's script: the AI agents'
//! sessions and memory (`~/.claude`, `~/.codex`, `~/.gemini`, `~/.copilot`,
//! `~/.local/share/opencode`), source trees, `.git`, keys and `.env` files.

use crate::catalog::{files, keep_path, tree, Base, Item, Target, Trim, DAY, WEEK};

/// A path below the user's home.
const fn home(path: &'static str, min_age_hours: u32) -> Target {
    tree(Base::Home, path, min_age_hours)
}

/// A path below the system's root, root's to clean.
const fn root(path: &'static str, min_age_hours: u32) -> Target {
    tree(Base::Root, path, min_age_hours)
}

/// A system command's work ([`crate::linux_system`]).
const fn command(name: &'static str) -> Target {
    Target {
        base: Base::Command,
        path: name,
        masks: &[],
        recursive: false,
        min_age_hours: 0,
        keep: &[],
        programs: &[],
        trim: None,
    }
}

/// Rotated copies: what logrotate leaves beside the live file.
const ROTATED: &[&str] = &[
    "*.gz",
    "*.xz",
    "*.bz2",
    "*.zst",
    "*.old",
    "*.1",
    "*.2",
    "*.3",
    "*.4",
    "*.5",
    "*.6",
    "*.7",
    "*.8",
    "*.9",
    "*-20??????",
];

pub(super) static ITEMS: &[Item] = &[
    // --- the system (an administrator's) ----------------------------------
    Item {
        id: "apt_cache",
        group: "system",
        default_on: true,
        targets: &[
            files(Base::Root, "var/cache/apt/archives", &["*.deb"], 0),
            root("var/cache/apt/archives/partial", DAY),
            files(Base::Root, "var/cache/apt", &["*.bin"], 0),
        ],
        keys: &[],
    },
    Item {
        id: "rotated_logs",
        group: "system",
        default_on: true,
        targets: &[Target {
            base: Base::Root,
            path: "var/log",
            masks: ROTATED,
            recursive: true,
            min_age_hours: DAY,
            keep: &[],
            programs: &[],
            trim: None,
        }],
        keys: &[],
    },
    Item {
        id: "journal",
        group: "system",
        default_on: true,
        targets: &[command("journal")],
        keys: &[],
    },
    Item {
        id: "linux_crashes",
        group: "system",
        default_on: true,
        targets: &[root("var/crash", 0), root("var/lib/systemd/coredump", 0)],
        keys: &[],
    },
    Item {
        id: "snap_old",
        group: "system",
        default_on: true,
        targets: &[command("snap_revisions")],
        keys: &[],
    },
    Item {
        id: "snap_cache",
        group: "system",
        default_on: true,
        targets: &[files(Base::Root, "var/lib/snapd/cache", &[], 0)],
        keys: &[],
    },
    Item {
        id: "apt_autoremove",
        group: "system",
        default_on: false,
        targets: &[command("apt_autoremove")],
        keys: &[],
    },
    Item {
        id: "apt_lists",
        group: "system",
        default_on: false,
        targets: &[Target {
            base: Base::Root,
            path: "var/lib/apt/lists",
            masks: &[],
            recursive: false,
            min_age_hours: 0,
            keep: &["lock"],
            programs: &[],
            trim: None,
        }],
        keys: &[],
    },
    // --- the user's ----------------------------------------------------------
    Item {
        id: "trash",
        group: "temp",
        default_on: true,
        targets: &[
            home(".local/share/Trash/files", 0),
            home(".local/share/Trash/info", 0),
            home(".local/share/Trash/expunged", 0),
        ],
        keys: &[],
    },
    Item {
        id: "thumbs",
        group: "caches",
        default_on: true,
        targets: &[home(".cache/thumbnails", 0)],
        keys: &[],
    },
    Item {
        id: "mesa_cache",
        group: "caches",
        default_on: true,
        targets: &[
            home(".cache/mesa_shader_cache", 0),
            home(".cache/mesa_shader_cache_db", 0),
            home(".cache/nvidia/GLCache", 0),
            home(".nv/GLCache", 0),
        ],
        keys: &[],
    },
    Item {
        id: "font_cache",
        group: "caches",
        default_on: true,
        targets: &[home(".cache/fontconfig", 0)],
        keys: &[],
    },
    Item {
        id: "dev_caches_linux",
        group: "programs",
        default_on: false,
        targets: &[
            home(".npm/_cacache", 0),
            home(".npm/_npx", 0),
            home(".cache/pip", 0),
            home(".cache/go-build", 0),
            home(".cache/yarn", 0),
            home(".cache/node-gyp", 0),
            home(".cache/typescript", 0),
            home(".cache/ms-playwright", 0),
            home(".cache/cargo-xwin", 0),
            home(".cargo/registry/cache", 0),
            home(".gradle/caches/build-cache-1", 0),
        ],
        keys: &[],
    },
    Item {
        id: "editor_junk",
        group: "programs",
        default_on: true,
        targets: &[
            home(".cursor-server/data/CachedExtensionVSIXs", 0),
            files(Base::Home, ".local/share/zed/logs", &["*.log.*"], 0),
            home(".local/share/zed/remote_extensions/work", DAY),
            home(".cache/JNA/temp", DAY),
            home(".gradle/daemon", WEEK),
            home(".cache/staticcheck", 0),
            home(".cache/gopls", 0),
        ],
        keys: &[],
    },
    Item {
        id: "recent_files",
        group: "traces",
        default_on: false,
        targets: &[
            files(
                Base::Home,
                ".local/share",
                &["recently-used.xbel", "recently-used.xbel.*"],
                0,
            ),
            files(
                Base::Home,
                ".local/share/RecentDocuments",
                &["*.desktop"],
                0,
            ),
        ],
        keys: &[],
    },
    Item {
        id: "junk",
        group: "junk",
        default_on: false,
        targets: &[tree(Base::Junk, "", 0)],
        keys: &[],
    },
];

// --- browsers ----------------------------------------------------------------

/// A Chromium browser: its profiles' folder, its cache folder, its processes.
struct Chromium {
    config: &'static str,
    cache: &'static str,
    programs: &'static [&'static str],
}

const CHROMIUM: &[Chromium] = &[
    Chromium {
        config: ".config/google-chrome",
        cache: ".cache/google-chrome",
        programs: &["chrome"],
    },
    Chromium {
        config: ".config/chromium",
        cache: ".cache/chromium",
        programs: &["chromium", "chromium-browser"],
    },
    Chromium {
        config: ".config/microsoft-edge",
        cache: ".cache/microsoft-edge",
        programs: &["msedge"],
    },
    Chromium {
        config: ".config/vivaldi",
        cache: ".cache/vivaldi",
        programs: &["vivaldi-bin"],
    },
    Chromium {
        config: ".config/BraveSoftware/Brave-Browser",
        cache: ".cache/BraveSoftware/Brave-Browser",
        programs: &["brave"],
    },
    Chromium {
        config: ".config/yandex-browser",
        cache: ".cache/yandex-browser",
        programs: &["yandex_browser"],
    },
    Chromium {
        config: ".config/opera",
        cache: ".cache/opera",
        programs: &["opera"],
    },
];

/// A Gecko application: where its profiles are, where their caches are.
struct Gecko {
    profiles: &'static str,
    cache: &'static str,
    programs: &'static [&'static str],
}

const GECKO: &[Gecko] = &[
    Gecko {
        profiles: ".mozilla/firefox",
        cache: ".cache/mozilla/firefox",
        programs: &["firefox", "firefox-bin"],
    },
    // Ubuntu's snap and the Flatpak keep Firefox in folders of their own.
    Gecko {
        profiles: "snap/firefox/common/.mozilla/firefox",
        cache: "snap/firefox/common/.cache/mozilla/firefox",
        programs: &["firefox", "firefox-bin"],
    },
    Gecko {
        profiles: ".var/app/org.mozilla.firefox/.mozilla/firefox",
        cache: ".var/app/org.mozilla.firefox/cache/mozilla/firefox",
        programs: &["firefox", "firefox-bin"],
    },
    Gecko {
        profiles: ".thunderbird",
        cache: ".cache/thunderbird",
        programs: &["thunderbird"],
    },
    Gecko {
        profiles: "snap/thunderbird/common/.thunderbird",
        cache: "snap/thunderbird/common/.cache/thunderbird",
        programs: &["thunderbird"],
    },
];

fn at(path: String, masks: &'static [&'static str], programs: &'static [&'static str]) -> Target {
    Target {
        base: Base::Home,
        path: keep_path(path),
        masks,
        recursive: masks.is_empty(),
        min_age_hours: 0,
        keep: &[],
        programs,
        trim: None,
    }
}

pub(super) fn browser_items() -> Vec<Item> {
    use crate::catalog::{
        CHROMIUM_COOKIES, CHROMIUM_HISTORY, CHROMIUM_HISTORY_DB, CHROMIUM_OLD_SESSIONS,
        GECKO_COOKIES, GECKO_SESSION,
    };
    let (mut cache, mut history, mut cookies, mut sessions, mut crashes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for b in CHROMIUM {
        let p = b.programs;
        let profile = format!("{}/*", b.config);
        cache.push(at(format!("{}/*/Cache", b.cache), &[], p));
        cache.push(at(format!("{}/*/Code Cache", b.cache), &[], p));
        for sub in [
            "Code Cache",
            "GPUCache",
            "Service Worker/CacheStorage",
            "Service Worker/ScriptCache",
        ] {
            cache.push(at(format!("{profile}/{sub}"), &[], p));
        }
        for sub in ["ShaderCache", "GrShaderCache", "GraphiteDawnCache"] {
            cache.push(at(format!("{}/{sub}", b.config), &[], p));
        }
        history.push(Target {
            trim: Some(Trim::ChromiumHistory),
            ..at(profile.clone(), CHROMIUM_HISTORY_DB, p)
        });
        history.push(at(profile.clone(), CHROMIUM_HISTORY, p));
        cookies.push(at(format!("{profile}/Network"), CHROMIUM_COOKIES, p));
        cookies.push(at(profile.clone(), CHROMIUM_COOKIES, p));
        sessions.push(at(format!("{profile}/Sessions"), &[], p));
        sessions.push(at(profile, CHROMIUM_OLD_SESSIONS, p));
        crashes.push(at(format!("{}/Crash Reports", b.config), &[], p));
    }
    for g in GECKO {
        let p = g.programs;
        let profile = format!("{}/*", g.profiles);
        for sub in ["cache2", "startupCache", "thumbnails"] {
            cache.push(at(format!("{}/*/{sub}", g.cache), &[], p));
        }
        cookies.push(at(profile.clone(), GECKO_COOKIES, p));
        sessions.push(at(profile.clone(), GECKO_SESSION, p));
        sessions.push(at(format!("{profile}/sessionstore-backups"), &[], p));
        for sub in ["pending", "submitted"] {
            crashes.push(at(format!("{}/Crash Reports/{sub}", g.profiles), &[], p));
        }
        crashes.push(at(format!("{profile}/minidumps"), &[], p));
    }
    let item = |id, default_on, targets: Vec<Target>| Item {
        id,
        group: "browsers",
        default_on,
        targets: Box::leak(targets.into_boxed_slice()),
        keys: &[],
    };
    vec![
        item("browser_cache", true, cache),
        item("browser_history", false, history),
        item("browser_cookies", false, cookies),
        item("browser_sessions", false, sessions),
        item("browser_crashes", true, crashes),
    ]
}
