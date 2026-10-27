//! `OwnBackup.wfx64` — a Total Commander file system plugin: the files of
//! Linux partitions on this computer, read-only.
//!
//! Windows cannot read ext4, LVM or LUKS, and WSL can read them only by taking
//! the whole disk away from Windows. The service reads them in place instead
//! (`own_backup_backup::linux_browse`, behind `LinuxBrowse` in the service);
//! this plugin shows what it reads as ordinary Total Commander panels in the
//! network neighbourhood: partitions, then — after the passphrase when the
//! partition is encrypted — their files, which are copied out (F5), viewed
//! (F3) and opened (Enter) like any others. Nothing can be written.
//!
//! [`browse`] decides what a path means and is tested on any system; [`rpc`]
//! reaches the service as the window does; `ffi` is what Total Commander
//! calls, on Windows.

// Messages in Russian and English (locales/tc.json), through
// own_backup_common::tr! — the language the window was set to.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod browse;
pub mod rpc;

// Generated: the service's client, of which only LinuxBrowse is used.
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("ownbackup");
}

#[cfg(windows)]
mod ffi;
