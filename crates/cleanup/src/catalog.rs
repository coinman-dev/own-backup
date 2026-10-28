//! What can be cleaned: items of a few targets each, below known places.
//!
//! Every target is a folder below a [`Base`] — never a mask over a whole disk.
//! The owner's machine (2026-10-28) showed why: another cleaner, removing
//! `*.prv`, `*.bak` and `*log.txt` wherever they were, took the Acronis
//! agent's keys (`rsa.prv`), a document's backup (`Работа.odt.bak`) and
//! DaVinci Resolve's settings with the junk.
//!
//! Temporary folders are cleaned of files a day old and more: that same
//! cleaner deleted files a running program had written minutes before.

use own_backup_common::lang::Localized;
use own_backup_common::{tr, tr_both};
use serde::Serialize;

/// Where a target's folder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The Windows folder (`%SystemRoot%`).
    Windows,
    /// `%ProgramData%`.
    ProgramData,
    /// A user's local application data (`<profile>\AppData\Local`).
    LocalAppData,
    /// A user's roaming application data (`<profile>\AppData\Roaming`).
    RoamingAppData,
    /// A user's recycle bin on every volume (`X:\$Recycle.Bin\<SID>`).
    RecycleBin,
    /// A user's profile folder (`C:\Users\<name>`).
    Profile,
}

impl Base {
    /// One folder per chosen user, not one for the system.
    pub fn per_user(self) -> bool {
        matches!(
            self,
            Base::LocalAppData | Base::RoamingAppData | Base::RecycleBin | Base::Profile
        )
    }
}

/// A key of each chosen user's registry ([`crate::registry`]) whose values —
/// and, if asked, subkeys — go. The key itself stays.
#[derive(Debug, Clone, Copy)]
pub struct RegTarget {
    /// Below the user's hive (`Software\Microsoft\…\Explorer\RunMRU`).
    pub path: &'static str,
    /// Value names that go (`*` and `?`); empty — every value but the
    /// default one, which is never touched.
    pub values: &'static [&'static str],
    /// Its subkeys go as well, whole.
    pub subkeys: bool,
}

const fn reg(path: &'static str, subkeys: bool) -> RegTarget {
    RegTarget {
        path,
        values: &[],
        subkeys,
    }
}

/// A folder and which of its files go.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub base: Base,
    /// Below the base, `\`-separated; empty for the base itself. A part with
    /// `*` or `?` stands for every folder its name matches (a browser's
    /// profiles: `User Data\*\Cache`).
    pub path: &'static str,
    /// File names that go (`*` and `?`, case-insensitive); empty — every file.
    pub masks: &'static [&'static str],
    /// The whole tree, or only the files directly in the folder.
    pub recursive: bool,
    /// Only what has not been written for this many hours.
    pub min_age_hours: u32,
    /// Names never removed, wherever they are (the recycle bin's
    /// `desktop.ini`).
    pub keep: &'static [&'static str],
    /// Programs (`chrome.exe`) whose files these are: while one runs, the
    /// folder is left — its databases are open and in use.
    pub programs: &'static [&'static str],
    /// The file is edited rather than removed when the run asks to keep its
    /// last days ([`crate::engine::Options`]).
    pub trim: Option<Trim>,
}

/// How a database is cut down instead of removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trim {
    /// A Chromium browser's `History` ([`crate::history`]).
    ChromiumHistory,
}

const fn tree(base: Base, path: &'static str, min_age_hours: u32) -> Target {
    Target {
        base,
        path,
        masks: &[],
        recursive: true,
        min_age_hours,
        keep: &[],
        programs: &[],
        trim: None,
    }
}

const fn files(
    base: Base,
    path: &'static str,
    masks: &'static [&'static str],
    min_age_hours: u32,
) -> Target {
    Target {
        base,
        path,
        masks,
        recursive: false,
        min_age_hours,
        keep: &[],
        programs: &[],
        trim: None,
    }
}

/// A day: what a running program or installer still uses is younger.
const DAY: u32 = 24;
/// A week: logs that may still explain a recent failure are younger.
const WEEK: u32 = 7 * 24;

/// One line of the window's list.
#[derive(Debug, Clone, Copy)]
pub struct Item {
    pub id: &'static str,
    pub group: &'static str,
    /// Ticked when nothing was chosen before.
    pub default_on: bool,
    pub targets: &'static [Target],
    /// Registry keys of each chosen user.
    pub keys: &'static [RegTarget],
}

impl Item {
    /// Cleaned in each chosen user's folders or registry (as well).
    pub fn per_user(&self) -> bool {
        !self.keys.is_empty() || self.targets.iter().any(|t| t.base.per_user())
    }
}

/// Groups, in the order the window shows them.
pub const GROUPS: &[&str] = &["temp", "reports", "caches", "browsers", "traces"];

static ITEMS: &[Item] = &[
    // --- temporary files -------------------------------------------------
    Item {
        id: "temp_user",
        group: "temp",
        default_on: true,
        keys: &[],
        targets: &[tree(Base::LocalAppData, "Temp", DAY)],
    },
    Item {
        id: "temp_windows",
        group: "temp",
        default_on: true,
        keys: &[],
        targets: &[tree(Base::Windows, "Temp", DAY)],
    },
    Item {
        id: "recycle_bin",
        group: "temp",
        default_on: true,
        keys: &[],
        targets: &[Target {
            base: Base::RecycleBin,
            path: "",
            masks: &[],
            recursive: true,
            min_age_hours: 0,
            keep: &["desktop.ini"],
            programs: &[],
            trim: None,
        }],
    },
    // --- reports, dumps and logs -----------------------------------------
    Item {
        id: "error_reports",
        group: "reports",
        default_on: true,
        keys: &[],
        targets: &[
            tree(Base::ProgramData, r"Microsoft\Windows\WER\ReportArchive", 0),
            tree(Base::ProgramData, r"Microsoft\Windows\WER\ReportQueue", 0),
            tree(Base::ProgramData, r"Microsoft\Windows\WER\Temp", DAY),
            tree(Base::LocalAppData, r"Microsoft\Windows\WER", 0),
        ],
    },
    Item {
        id: "crash_dumps",
        group: "reports",
        default_on: true,
        keys: &[],
        targets: &[
            files(Base::Windows, "Minidump", &["*.dmp"], 0),
            files(Base::Windows, "", &["MEMORY.DMP"], 0),
            files(Base::LocalAppData, "CrashDumps", &["*.dmp"], 0),
        ],
    },
    Item {
        id: "windows_logs",
        group: "reports",
        default_on: true,
        keys: &[],
        targets: &[
            files(Base::Windows, r"Logs\CBS", &["*.log", "*.cab"], WEEK),
            files(Base::Windows, r"Logs\DISM", &["*.log"], WEEK),
            files(Base::Windows, r"Logs\WindowsUpdate", &["*.etl"], WEEK),
        ],
    },
    // --- caches ------------------------------------------------------------
    Item {
        id: "inet_cache",
        group: "caches",
        default_on: true,
        keys: &[],
        targets: &[tree(Base::LocalAppData, r"Microsoft\Windows\INetCache", 0)],
    },
    Item {
        id: "shader_cache",
        group: "caches",
        default_on: true,
        keys: &[],
        targets: &[
            tree(Base::LocalAppData, "D3DSCache", 0),
            tree(Base::LocalAppData, r"NVIDIA\DXCache", 0),
            tree(Base::LocalAppData, r"NVIDIA\GLCache", 0),
            tree(Base::LocalAppData, r"AMD\DxCache", 0),
            tree(Base::LocalAppData, r"AMD\DxcCache", 0),
            tree(Base::LocalAppData, r"AMD\GLCache", 0),
            tree(Base::ProgramData, r"NVIDIA Corporation\NV_Cache", 0),
        ],
    },
    Item {
        id: "thumbnails",
        group: "caches",
        default_on: false,
        keys: &[],
        targets: &[files(
            Base::LocalAppData,
            r"Microsoft\Windows\Explorer",
            &["thumbcache_*.db", "iconcache_*.db"],
            0,
        )],
    },
    Item {
        id: "prefetch",
        group: "caches",
        default_on: false,
        keys: &[],
        targets: &[files(Base::Windows, "Prefetch", &["*.pf"], 0)],
    },
    // --- Windows traces ----------------------------------------------------
    //
    // What a user opened, typed and connected to. Nothing here frees space,
    // and the lists are the user's own convenience: off unless chosen.
    Item {
        id: "recent_docs",
        group: "traces",
        default_on: false,
        targets: &[files(
            Base::RoamingAppData,
            r"Microsoft\Windows\Recent",
            &["*.lnk"],
            0,
        )],
        keys: &[
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\RecentDocs",
                true,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\OpenSavePidlMRU",
                true,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\OpenSaveMRU",
                true,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\LastVisitedPidlMRU",
                false,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\LastVisitedPidlMRULegacy",
                false,
            ),
        ],
    },
    Item {
        id: "jump_lists",
        group: "traces",
        default_on: false,
        targets: &[
            Target {
                base: Base::RoamingAppData,
                path: r"Microsoft\Windows\Recent\AutomaticDestinations",
                masks: &["*.automaticDestinations-ms"],
                recursive: false,
                min_age_hours: 0,
                // Explorer's: the folders pinned to Quick access.
                keep: &[
                    "f01b4d95cf55d32a.automaticDestinations-ms",
                    "5f7b5f1e01b83767.automaticDestinations-ms",
                ],
                programs: &[],
                trim: None,
            },
            files(
                Base::RoamingAppData,
                r"Microsoft\Windows\Recent\CustomDestinations",
                &["*.customDestinations-ms"],
                0,
            ),
        ],
        keys: &[],
    },
    Item {
        id: "run_history",
        group: "traces",
        default_on: false,
        targets: &[],
        keys: &[
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\RunMRU",
                false,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\TypedPaths",
                false,
            ),
            reg(
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\WordWheelQuery",
                true,
            ),
        ],
    },
    Item {
        id: "rdp_history",
        group: "traces",
        default_on: false,
        // The last connection's settings; the window makes a new one.
        targets: &[files(Base::Profile, "Documents", &["Default.rdp"], 0)],
        keys: &[
            RegTarget {
                path: r"Software\Microsoft\Terminal Server Client\Default",
                values: &["MRU*"],
                subkeys: false,
            },
            reg(r"Software\Microsoft\Terminal Server Client\Servers", true),
        ],
    },
];

// --- browsers ---------------------------------------------------------------
//
// One list of browsers, the items built from it: every Chromium browser keeps
// its profiles the same way (`User Data\<profile>\…`), every Gecko one as
// `Profiles\<profile>\…`. Passwords (`Login Data`) and autofill (`Web Data`,
// which also holds the search engines) are not touched at all; Firefox's
// history shares `places.sqlite` with the bookmarks, so it is not removed
// either — that needs the database edited, not deleted.

/// How a browser lays out its profiles.
#[derive(Debug, Clone, Copy)]
enum Layout {
    /// `User Data\<profile>` below local application data.
    Chromium { user_data: &'static str },
    /// One profile in roaming data, its cache in local data (Opera).
    Opera { dir: &'static str },
    /// `Profiles\<profile>` below both local (cache) and roaming data.
    Gecko { dir: &'static str },
}

#[derive(Debug)]
struct Browser {
    /// As the window names it when it has to be closed.
    name: &'static str,
    exe: &'static str,
    layout: Layout,
}

static BROWSERS: [Browser; 9] = [
    Browser {
        name: "Google Chrome",
        exe: "chrome.exe",
        layout: Layout::Chromium {
            user_data: r"Google\Chrome\User Data",
        },
    },
    Browser {
        name: "Microsoft Edge",
        exe: "msedge.exe",
        layout: Layout::Chromium {
            user_data: r"Microsoft\Edge\User Data",
        },
    },
    Browser {
        name: "Vivaldi",
        exe: "vivaldi.exe",
        layout: Layout::Chromium {
            user_data: r"Vivaldi\User Data",
        },
    },
    Browser {
        name: "Brave",
        exe: "brave.exe",
        layout: Layout::Chromium {
            user_data: r"BraveSoftware\Brave-Browser\User Data",
        },
    },
    Browser {
        name: "Yandex Browser",
        exe: "browser.exe",
        layout: Layout::Chromium {
            user_data: r"Yandex\YandexBrowser\User Data",
        },
    },
    Browser {
        name: "Opera",
        exe: "opera.exe",
        layout: Layout::Opera {
            dir: r"Opera Software\Opera Stable",
        },
    },
    Browser {
        name: "Opera GX",
        exe: "opera.exe",
        layout: Layout::Opera {
            dir: r"Opera Software\Opera GX Stable",
        },
    },
    Browser {
        name: "Mozilla Firefox",
        exe: "firefox.exe",
        layout: Layout::Gecko {
            dir: r"Mozilla\Firefox\Profiles",
        },
    },
    Browser {
        name: "Mozilla Thunderbird",
        exe: "thunderbird.exe",
        layout: Layout::Gecko {
            dir: r"Thunderbird\Profiles",
        },
    },
];

/// Chromium's history database and its journal: removed, or cut down to
/// the last days ([`Trim::ChromiumHistory`]) — then both stay as they are
/// but for the old rows.
const CHROMIUM_HISTORY_DB: &[&str] = &["History", "History-journal"];
/// What is derived from the history (visited links, most visited, the
/// address bar's suggestions): removed either way, the browser rebuilds it.
const CHROMIUM_HISTORY: &[&str] = &[
    "Visited Links",
    "Top Sites",
    "Top Sites-journal",
    "Shortcuts",
    "Shortcuts-journal",
    "Network Action Predictor",
    "Network Action Predictor-journal",
];
const CHROMIUM_COOKIES: &[&str] = &["Cookies", "Cookies-journal"];
/// Sessions of browsers that kept them in the profile folder itself.
const CHROMIUM_OLD_SESSIONS: &[&str] = &[
    "Current Session",
    "Current Tabs",
    "Last Session",
    "Last Tabs",
];
const GECKO_COOKIES: &[&str] = &[
    "cookies.sqlite",
    "cookies.sqlite-wal",
    "cookies.sqlite-shm",
    "cookies.sqlite-journal",
];
const GECKO_SESSION: &[&str] = &["sessionstore.jsonlz4"];

/// A path made once and kept for the life of the program, as the targets'
/// `&'static` paths are.
fn keep(path: String) -> &'static str {
    Box::leak(path.into_boxed_str())
}

fn of(b: &'static Browser, base: Base, path: String, masks: &'static [&'static str]) -> Target {
    Target {
        base,
        path: keep(path),
        masks,
        recursive: masks.is_empty(),
        min_age_hours: 0,
        keep: &[],
        programs: std::slice::from_ref(&b.exe),
        trim: None,
    }
}

/// A history database target that is cut down, not removed, when the last
/// days are kept.
fn trimmed(t: Target) -> Target {
    Target {
        trim: Some(Trim::ChromiumHistory),
        ..t
    }
}

fn browser_items() -> Vec<Item> {
    let (mut cache, mut history, mut cookies, mut sessions, mut crashes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for b in &BROWSERS {
        match b.layout {
            Layout::Chromium { user_data } => {
                let local = Base::LocalAppData;
                let profile = format!(r"{user_data}\*");
                for sub in [
                    "Cache",
                    "Code Cache",
                    "GPUCache",
                    r"Service Worker\CacheStorage",
                    r"Service Worker\ScriptCache",
                ] {
                    cache.push(of(b, local, format!(r"{profile}\{sub}"), &[]));
                }
                for sub in ["ShaderCache", "GrShaderCache", "GraphiteDawnCache"] {
                    cache.push(of(b, local, format!(r"{user_data}\{sub}"), &[]));
                }
                history.push(trimmed(of(b, local, profile.clone(), CHROMIUM_HISTORY_DB)));
                history.push(of(b, local, profile.clone(), CHROMIUM_HISTORY));
                cookies.push(of(
                    b,
                    local,
                    format!(r"{profile}\Network"),
                    CHROMIUM_COOKIES,
                ));
                cookies.push(of(b, local, profile.clone(), CHROMIUM_COOKIES));
                sessions.push(of(b, local, format!(r"{profile}\Sessions"), &[]));
                sessions.push(of(b, local, profile, CHROMIUM_OLD_SESSIONS));
                crashes.push(of(b, local, format!(r"{user_data}\Crashpad\reports"), &[]));
            }
            Layout::Opera { dir } => {
                let roaming = Base::RoamingAppData;
                cache.push(of(b, Base::LocalAppData, format!(r"{dir}\Cache"), &[]));
                for sub in [
                    "Code Cache",
                    "GPUCache",
                    "ShaderCache",
                    r"Service Worker\CacheStorage",
                ] {
                    cache.push(of(b, roaming, format!(r"{dir}\{sub}"), &[]));
                }
                history.push(trimmed(of(
                    b,
                    roaming,
                    dir.to_string(),
                    CHROMIUM_HISTORY_DB,
                )));
                history.push(of(b, roaming, dir.to_string(), CHROMIUM_HISTORY));
                cookies.push(of(b, roaming, format!(r"{dir}\Network"), CHROMIUM_COOKIES));
                cookies.push(of(b, roaming, dir.to_string(), CHROMIUM_COOKIES));
                sessions.push(of(b, roaming, format!(r"{dir}\Sessions"), &[]));
            }
            Layout::Gecko { dir } => {
                let profile = format!(r"{dir}\*");
                for sub in ["cache2", "startupCache", "thumbnails"] {
                    cache.push(of(b, Base::LocalAppData, format!(r"{profile}\{sub}"), &[]));
                }
                let roaming = Base::RoamingAppData;
                cookies.push(of(b, roaming, profile.clone(), GECKO_COOKIES));
                sessions.push(of(b, roaming, profile.clone(), GECKO_SESSION));
                sessions.push(of(
                    b,
                    roaming,
                    format!(r"{profile}\sessionstore-backups"),
                    &[],
                ));
                // `Crash Reports` is beside `Profiles`.
                let app = dir.rsplit_once('\\').map_or(dir, |(app, _)| app);
                for sub in ["pending", "submitted"] {
                    crashes.push(of(b, roaming, format!(r"{app}\Crash Reports\{sub}"), &[]));
                }
                crashes.push(of(b, roaming, format!(r"{profile}\minidumps"), &[]));
            }
        }
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

/// The name of the program behind `exe` (`chrome.exe` → «Google Chrome»).
pub fn program_name(exe: &str) -> &str {
    BROWSERS
        .iter()
        .find(|b| b.exe.eq_ignore_ascii_case(exe))
        .map(|b| b.name)
        .unwrap_or(exe)
}

static CATALOG: std::sync::LazyLock<Vec<Item>> = std::sync::LazyLock::new(|| {
    let mut all = ITEMS.to_vec();
    all.extend(browser_items());
    all
});

/// Everything that can be cleaned on this system. Empty on Linux for now
/// (PLAN §6б: Linux after Windows).
pub fn catalog() -> &'static [Item] {
    if cfg!(windows) || cfg!(test) {
        &CATALOG
    } else {
        &[]
    }
}

pub fn item(id: &str) -> Option<&'static Item> {
    catalog().iter().find(|i| i.id == id)
}

/// The item's name in the window.
pub fn label(id: &str) -> String {
    label_both(id).current().to_string()
}

/// The item's name in both languages — for the history, which a window in
/// the other language may read later.
pub fn label_both(id: &str) -> Localized {
    match id {
        "temp_user" => tr_both!("cleanup.temp_user"),
        "temp_windows" => tr_both!("cleanup.temp_windows"),
        "recycle_bin" => tr_both!("cleanup.recycle_bin"),
        "error_reports" => tr_both!("cleanup.error_reports"),
        "crash_dumps" => tr_both!("cleanup.crash_dumps"),
        "windows_logs" => tr_both!("cleanup.windows_logs"),
        "inet_cache" => tr_both!("cleanup.inet_cache"),
        "shader_cache" => tr_both!("cleanup.shader_cache"),
        "thumbnails" => tr_both!("cleanup.thumbnails"),
        "prefetch" => tr_both!("cleanup.prefetch"),
        "browser_cache" => tr_both!("cleanup.browser_cache"),
        "browser_history" => tr_both!("cleanup.browser_history"),
        "browser_cookies" => tr_both!("cleanup.browser_cookies"),
        "browser_sessions" => tr_both!("cleanup.browser_sessions"),
        "browser_crashes" => tr_both!("cleanup.browser_crashes"),
        "recent_docs" => tr_both!("cleanup.recent_docs"),
        "jump_lists" => tr_both!("cleanup.jump_lists"),
        "run_history" => tr_both!("cleanup.run_history"),
        "rdp_history" => tr_both!("cleanup.rdp_history"),
        _ => Localized::plain(id),
    }
}

/// What the item is and what removing it means.
pub fn about(id: &str) -> String {
    match id {
        "temp_user" => tr!("cleanup.temp_user_about"),
        "temp_windows" => tr!("cleanup.temp_windows_about"),
        "recycle_bin" => tr!("cleanup.recycle_bin_about"),
        "error_reports" => tr!("cleanup.error_reports_about"),
        "crash_dumps" => tr!("cleanup.crash_dumps_about"),
        "windows_logs" => tr!("cleanup.windows_logs_about"),
        "inet_cache" => tr!("cleanup.inet_cache_about"),
        "shader_cache" => tr!("cleanup.shader_cache_about"),
        "thumbnails" => tr!("cleanup.thumbnails_about"),
        "prefetch" => tr!("cleanup.prefetch_about"),
        "browser_cache" => tr!("cleanup.browser_cache_about"),
        "browser_history" => tr!("cleanup.browser_history_about"),
        "browser_cookies" => tr!("cleanup.browser_cookies_about"),
        "browser_sessions" => tr!("cleanup.browser_sessions_about"),
        "browser_crashes" => tr!("cleanup.browser_crashes_about"),
        "recent_docs" => tr!("cleanup.recent_docs_about"),
        "jump_lists" => tr!("cleanup.jump_lists_about"),
        "run_history" => tr!("cleanup.run_history_about"),
        "rdp_history" => tr!("cleanup.rdp_history_about"),
        _ => return String::new(),
    }
    .into_owned()
}

/// A group's heading.
pub fn group_label(id: &str) -> String {
    match id {
        "temp" => tr!("cleanup.group_temp"),
        "reports" => tr!("cleanup.group_reports"),
        "caches" => tr!("cleanup.group_caches"),
        "browsers" => tr!("cleanup.group_browsers"),
        "traces" => tr!("cleanup.group_traces"),
        _ => return id.to_string(),
    }
    .into_owned()
}

/// The catalogue as the window shows it.
#[derive(Debug, Clone, Serialize)]
pub struct GroupDto {
    pub id: &'static str,
    pub label: String,
    pub items: Vec<ItemDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemDto {
    pub id: &'static str,
    pub label: String,
    pub about: String,
    pub default_on: bool,
    pub per_user: bool,
}

pub fn groups() -> Vec<GroupDto> {
    GROUPS
        .iter()
        .map(|g| GroupDto {
            id: g,
            label: group_label(g),
            items: catalog()
                .iter()
                .filter(|i| i.group == *g)
                .map(|i| ItemDto {
                    id: i.id,
                    label: label(i.id),
                    about: about(i.id),
                    default_on: i.default_on,
                    per_user: i.per_user(),
                })
                .collect(),
        })
        .filter(|g| !g.items.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_is_named_described_and_grouped() {
        let mut seen = std::collections::BTreeSet::new();
        for i in catalog() {
            assert!(seen.insert(i.id), "{} twice", i.id);
            assert!(GROUPS.contains(&i.group), "{}: group {}", i.id, i.group);
            assert_ne!(label(i.id), i.id, "{} has no name", i.id);
            assert!(!about(i.id).is_empty(), "{} has no description", i.id);
            assert!(
                !i.targets.is_empty() || !i.keys.is_empty(),
                "{} has no targets",
                i.id
            );
        }
        for g in GROUPS {
            assert_ne!(group_label(g), *g);
        }
    }

    /// No target is a whole place: a mask, or a folder below the base —
    /// never everything in the Windows folder or in ProgramData.
    #[test]
    fn no_target_is_a_whole_system_folder() {
        for i in catalog() {
            for t in i.targets {
                if matches!(
                    t.base,
                    Base::Windows | Base::ProgramData | Base::LocalAppData
                ) && t.path.is_empty()
                {
                    assert!(
                        !t.masks.is_empty() && !t.recursive,
                        "{}: the whole {:?} folder",
                        i.id,
                        t.base
                    );
                }
            }
        }
    }

    /// Passwords, bookmarks, autofill and Firefox's history-with-bookmarks
    /// are never a browser item's; every browser folder waits for its
    /// browser to be closed.
    #[test]
    fn browser_items_leave_passwords_bookmarks_and_autofill_alone() {
        const NEVER: &[&str] = &[
            "Login Data",
            "Web Data",
            "Bookmarks",
            "Preferences",
            "places.sqlite",
            "logins.json",
            "key4.db",
        ];
        for i in catalog().iter().filter(|i| i.group == "browsers") {
            for t in i.targets {
                assert!(
                    !t.programs.is_empty(),
                    "{}: {} has no program",
                    i.id,
                    t.path
                );
                for never in NEVER {
                    assert!(!t.path.contains(never), "{}: {}", i.id, t.path);
                    assert!(
                        !t.masks.iter().any(|m| m.eq_ignore_ascii_case(never)),
                        "{}: {never}",
                        i.id
                    );
                }
                // A profile folder itself is never taken whole.
                if t.path.ends_with('*') || t.path.ends_with("Stable") {
                    assert!(!t.masks.is_empty(), "{}: whole profile {}", i.id, t.path);
                }
            }
        }
        assert_eq!(program_name("CHROME.EXE"), "Google Chrome");
        assert_eq!(program_name("other.exe"), "other.exe");
    }
}
