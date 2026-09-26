//! Enable the backup/restore privileges for the current process.
//!
//! `System Volume Information`, protected app-data folders, etc. have ACLs that
//! deny even LocalSystem by default. A backup tool bypasses those ACLs by
//! opening files/dirs with `FILE_FLAG_BACKUP_SEMANTICS` — but ONLY once
//! `SeBackupPrivilege` is *enabled* in the token. The privilege is present in
//! the LocalSystem token yet disabled by default, so we enable it before the
//! exclusion scan / backup.
//!
//! Best-effort: if a privilege isn't held (an unelevated run), we log and carry
//! on — the scan then just excludes fewer files, which is safe.

#![cfg(target_os = "windows")]

use tracing::{debug, info};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Enable `SeBackupPrivilege` and `SeRestorePrivilege` in the current process
/// token. Idempotent and cheap; safe to call before every backup.
pub fn enable_backup_privileges() {
    let mut enabled = Vec::new();
    for name in [w!("SeBackupPrivilege"), w!("SeRestorePrivilege")] {
        match enable_one(name) {
            Ok(true) => enabled.push(name),
            Ok(false) => debug!("privilege not held by this token (unelevated?)"),
            Err(e) => debug!(error = %e, "could not enable a backup privilege"),
        }
    }
    if !enabled.is_empty() {
        info!(count = enabled.len(), "backup privileges enabled");
    }
}

/// Enable `SeShutdownPrivilege` — what putting the machine to sleep or turning
/// it off asks of the caller, LocalSystem included (held, disabled by default).
pub fn enable_shutdown_privilege() -> bool {
    matches!(enable_one(w!("SeShutdownPrivilege")), Ok(true))
}

/// Enable one privilege by name. `Ok(true)` = enabled, `Ok(false)` = the token
/// doesn't hold it (ERROR_NOT_ALL_ASSIGNED).
fn enable_one(name: PCWSTR) -> windows::core::Result<bool> {
    // SAFETY: standard token-privilege dance; the token handle is closed on
    // every path.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )?;

        let mut luid = LUID::default();
        let lookup = LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid);
        if let Err(e) = lookup {
            let _ = CloseHandle(token);
            return Err(e);
        }

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let adjust = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        // AdjustTokenPrivileges succeeds even when the privilege isn't held; the
        // "not all assigned" case surfaces via GetLastError.
        let not_all_assigned = windows::Win32::Foundation::GetLastError()
            == windows::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED;
        let _ = CloseHandle(token);
        adjust?;
        Ok(!not_all_assigned)
    }
}
