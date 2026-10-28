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
    /// A user's recycle bin on every volume (`X:\$Recycle.Bin\<SID>`).
    RecycleBin,
}

impl Base {
    /// One folder per chosen user, not one for the system.
    pub fn per_user(self) -> bool {
        matches!(self, Base::LocalAppData | Base::RecycleBin)
    }
}

/// A folder and which of its files go.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub base: Base,
    /// Below the base, `\`-separated; empty for the base itself.
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
}

const fn tree(base: Base, path: &'static str, min_age_hours: u32) -> Target {
    Target {
        base,
        path,
        masks: &[],
        recursive: true,
        min_age_hours,
        keep: &[],
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
}

impl Item {
    /// Cleaned in each chosen user's folders (as well).
    pub fn per_user(&self) -> bool {
        self.targets.iter().any(|t| t.base.per_user())
    }
}

/// Groups, in the order the window shows them.
pub const GROUPS: &[&str] = &["temp", "reports", "caches"];

static ITEMS: &[Item] = &[
    // --- temporary files -------------------------------------------------
    Item {
        id: "temp_user",
        group: "temp",
        default_on: true,
        targets: &[tree(Base::LocalAppData, "Temp", DAY)],
    },
    Item {
        id: "temp_windows",
        group: "temp",
        default_on: true,
        targets: &[tree(Base::Windows, "Temp", DAY)],
    },
    Item {
        id: "recycle_bin",
        group: "temp",
        default_on: true,
        targets: &[Target {
            base: Base::RecycleBin,
            path: "",
            masks: &[],
            recursive: true,
            min_age_hours: 0,
            keep: &["desktop.ini"],
        }],
    },
    // --- reports, dumps and logs -----------------------------------------
    Item {
        id: "error_reports",
        group: "reports",
        default_on: true,
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
        targets: &[tree(Base::LocalAppData, r"Microsoft\Windows\INetCache", 0)],
    },
    Item {
        id: "shader_cache",
        group: "caches",
        default_on: true,
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
        targets: &[files(Base::Windows, "Prefetch", &["*.pf"], 0)],
    },
];

/// Everything that can be cleaned on this system. Empty on Linux for now
/// (PLAN §6б: Linux after Windows).
pub fn catalog() -> &'static [Item] {
    if cfg!(windows) || cfg!(test) {
        ITEMS
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
            assert!(!i.targets.is_empty(), "{} has no targets", i.id);
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
}
