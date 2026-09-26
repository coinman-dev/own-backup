//! Creating a filesystem on a partition (R3c).
//!
//! Windows has no documented formatting API. What every partitioning tool
//! actually uses is `FormatEx` out of `fmifs.dll` — the same entry point
//! `format.com` and the Disk Management snap-in call. It is undocumented but
//! has been stable since Windows NT, and it is the only way to get a progress
//! callback and to format ReFS at all.
//!
//! Two things about it are worth knowing before reading the code:
//!
//! - **It reports failure through a callback, not a return value.** `FormatEx`
//!   returns `void`. Success arrives as a `DONE` callback carrying a boolean,
//!   and the interesting failures (`VOLUMEINUSE`, `FSNOTSUPPORTED`,
//!   `INSUFFICIENTRIGHTS`) arrive as their own callback commands. A caller that
//!   only checks that the call returned will believe every format succeeded.
//!
//! - **The callback carries no context pointer.** State goes through a
//!   thread-local, which is sound because `FormatEx` is synchronous: it calls
//!   back on the same thread and has returned before the thread-local is read.
//!
//! If `fmifs.dll` cannot be loaded at all, we fall back to running `format.com`.
//! It is slower and gives no progress, but it fails loudly, which beats not
//! formatting.

#![cfg(target_os = "windows")]

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::c_void;

use tracing::{debug, info, warn};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use own_backup_common::restore_plan::FormatFs;
use own_backup_common::{tr, OwnBackupError, Result};

use crate::win_device::to_wide;

/// `FMIFS_MEDIA_FLAG::FMIFS_HARDDISK`.
const FMIFS_HARDDISK: u32 = 12;

// CALLBACKCOMMAND values we act on. The enum has gaps of undocumented members;
// naming only the ones that matter is more honest than inventing names for the
// rest.
const CB_PROGRESS: u32 = 0;
const CB_INSUFFICIENT_RIGHTS: u32 = 6;
const CB_FS_NOT_SUPPORTED: u32 = 7;
const CB_VOLUME_IN_USE: u32 = 8;
const CB_DONE: u32 = 11;
const CB_CLUSTER_SIZE_TOO_SMALL: u32 = 17;

type FmifsCallback =
    unsafe extern "system" fn(command: u32, sub_action: u32, action_info: *mut c_void) -> BOOLEAN;

type FormatExFn = unsafe extern "system" fn(
    drive_root: PCWSTR,
    media_flag: u32,
    format: PCWSTR,
    label: PCWSTR,
    quick: BOOLEAN,
    cluster_size: u32,
    callback: FmifsCallback,
);

/// What the callback collected during one `FormatEx` call.
#[derive(Default)]
struct FormatOutcome {
    /// `Some(true)` once a DONE callback said the format succeeded.
    done: Option<bool>,
    /// The first diagnostic callback, if any — far more informative than DONE.
    /// Kept as the callback command; [`failure_text`] words it for the window.
    failure: Option<u32>,
    /// Last decile logged, so a full format leaves a trail without flooding.
    logged_decile: u32,
}

thread_local! {
    static OUTCOME: RefCell<FormatOutcome> = RefCell::new(FormatOutcome::default());
}

/// SAFETY: called by `FormatEx` on the calling thread, synchronously.
unsafe extern "system" fn callback(command: u32, _sub_action: u32, action_info: *mut c_void) -> BOOLEAN {
    OUTCOME.with(|o| {
        let mut o = o.borrow_mut();
        match command {
            CB_PROGRESS => {
                if !action_info.is_null() {
                    // SAFETY: for PROGRESS the API documents a DWORD percentage.
                    let percent = unsafe { *(action_info as *const u32) };
                    // A full format of a large volume takes many minutes with no
                    // other sign of life; a decile in the log is that sign.
                    if percent / 10 > o.logged_decile {
                        o.logged_decile = percent / 10;
                        debug!(percent, "formatting");
                    }
                }
            }
            CB_DONE => {
                if !action_info.is_null() {
                    // SAFETY: for DONE the API documents a BOOLEAN success flag.
                    o.done = Some(unsafe { *(action_info as *const BOOLEAN) }.0 != 0);
                }
            }
            CB_INSUFFICIENT_RIGHTS
            | CB_FS_NOT_SUPPORTED
            | CB_VOLUME_IN_USE
            | CB_CLUSTER_SIZE_TOO_SMALL => {
                o.failure.get_or_insert(command);
            }
            _ => {}
        }
    });
    BOOLEAN(1) // keep going
}

/// A diagnostic callback command in the window's words.
fn failure_text(command: u32) -> Cow<'static, str> {
    match command {
        CB_INSUFFICIENT_RIGHTS => tr!("format_win.insufficient_rights"),
        CB_FS_NOT_SUPPORTED => tr!("format_win.fs_not_supported"),
        CB_VOLUME_IN_USE => tr!("format_win.volume_in_use"),
        CB_CLUSTER_SIZE_TOO_SMALL => tr!("format_win.cluster_too_small"),
        other => tr!("format_win.callback_code", code = other),
    }
}

/// Load `FormatEx` from `fmifs.dll`.
fn load_format_ex() -> Option<FormatExFn> {
    let name = to_wide("fmifs.dll");
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let module = unsafe { LoadLibraryW(PCWSTR(name.as_ptr())) }.ok()?;
    // SAFETY: valid module handle; the symbol name is a literal NUL-terminated string.
    let proc = unsafe { GetProcAddress(module, PCSTR(c"FormatEx".as_ptr() as *const u8)) }?;
    // SAFETY: FormatEx has had this signature since Windows NT.
    Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, FormatExFn>(proc) })
}

/// Create a filesystem on the volume at `volume_root`.
///
/// `volume_root` must be a mount point with a trailing backslash — either a
/// drive root (`"E:\"`) or a volume GUID path (`\\?\Volume{…}\`). Partitions
/// without a mount point must be given one first; see
/// [`crate::letters_win::TempDriveLetter`].
pub fn format_volume(volume_root: &str, fs: FormatFs, label: &str, quick: bool) -> Result<()> {
    let Some(format_ex) = load_format_ex() else {
        warn!("fmifs.dll unavailable — falling back to format.com");
        return format_via_command(volume_root, fs, label, quick);
    };

    OUTCOME.with(|o| *o.borrow_mut() = FormatOutcome::default());

    let root = to_wide(volume_root);
    let fs_name = to_wide(fs.as_str());
    let label_w = to_wide(label);

    debug!(volume = %volume_root, fs = fs.as_str(), quick, "FormatEx starting");
    // SAFETY: every buffer is NUL-terminated and outlives the synchronous call.
    unsafe {
        format_ex(
            PCWSTR(root.as_ptr()),
            FMIFS_HARDDISK,
            PCWSTR(fs_name.as_ptr()),
            PCWSTR(label_w.as_ptr()),
            BOOLEAN(quick as u8),
            0, // default cluster size for the volume's size
            callback,
        )
    };

    let (done, failure) = OUTCOME.with(|o| {
        let o = o.borrow();
        (o.done, o.failure)
    });

    match (done, failure) {
        (Some(true), None) => {
            info!(volume = %volume_root, fs = fs.as_str(), label, quick, "volume formatted");
            Ok(())
        }
        (_, Some(why)) => Err(OwnBackupError::Device(
            tr!(
                "format_win.format_failed",
                volume = volume_root,
                fs = fs.as_str(),
                why = failure_text(why)
            )
            .into_owned(),
        )),
        // No DONE at all means FormatEx bailed out before starting. Treating
        // that as success is how an unformatted partition ends up in a restore.
        (None, None) => Err(OwnBackupError::Device(
            tr!("format_win.not_started", volume = volume_root, fs = fs.as_str()).into_owned(),
        )),
        (Some(false), None) => Err(OwnBackupError::Device(
            tr!("format_win.ended_with_error", volume = volume_root, fs = fs.as_str())
                .into_owned(),
        )),
    }
}

/// Fallback for a machine where `fmifs.dll` will not load: shell out to
/// `format.com`. No progress and no ReFS, but it reports failure honestly.
fn format_via_command(volume_root: &str, fs: FormatFs, label: &str, quick: bool) -> Result<()> {
    // format.com only understands drive letters.
    let drive = volume_root.trim_end_matches('\\');
    if !(drive.len() == 2 && drive.ends_with(':')) {
        return Err(OwnBackupError::Device(
            tr!("format_win.needs_drive_letter", volume = volume_root).into_owned(),
        ));
    }
    let mut cmd = std::process::Command::new("format.com");
    cmd.arg(drive)
        .arg(format!("/FS:{}", fs.as_str()))
        .arg("/Y");
    if quick {
        cmd.arg("/Q");
    }
    if !label.is_empty() {
        cmd.arg(format!("/V:{label}"));
    }
    let out = cmd.output().map_err(|e| {
        OwnBackupError::Device(tr!("format_win.format_com_start_failed", error = e).into_owned())
    })?;
    if !out.status.success() {
        return Err(OwnBackupError::Device(
            tr!(
                "format_win.format_com_failed",
                drive = drive,
                code = out.status.code().unwrap_or(-1),
                output = String::from_utf8_lossy(&out.stderr).trim()
            )
            .into_owned(),
        ));
    }
    info!(volume = %volume_root, fs = fs.as_str(), "volume formatted via format.com");
    Ok(())
}
