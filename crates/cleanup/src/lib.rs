//! Disk cleanup — the window's «Очистка» (PLAN §6б).
//!
//! What can go is a fixed [`catalog`]: temporary files, error reports, crash
//! dumps, logs, caches and the recycle bin, each a few folders and file masks
//! below a known place — the Windows folder, ProgramData, a user's local
//! application data, a user's recycle bin. Nothing outside them is touched.
//!
//! [`engine`] goes through those folders twice the same way: first to say how
//! much each item takes (analysis), then to remove it. Removal is plain and
//! fast — a deleted file is gone as any other deleted file is; nothing is
//! overwritten (owner, 2026-10-28). It never enters a link or a junction,
//! never leaves the folder it was given, and a file in use is left with a
//! count of what was left.
//!
//! [`users`] finds the profiles of this computer, whose folders the user items
//! clean, and the volumes whose recycle bins they empty. [`registry`] reaches
//! a user's hive for the Windows traces — the lists of recent documents, the
//! Run box's commands, Remote Desktop's servers.

// Item names and descriptions in Russian and English (locales/cleanup.json),
// through own_backup_common::tr! — the language the window was set to.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod catalog;
mod catalog_linux;
pub mod engine;
pub mod history;
pub mod junk;
pub mod linux_system;
pub mod registry;
pub mod users;
pub mod volume_cache;

pub use catalog::{catalog, Base, Item, Target};
pub use engine::{run, run_only, Found, Listing, Mode, Options, Report, Roots, Tally};
pub use users::User;
