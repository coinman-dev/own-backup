//! The three registry operations this project needs, and nothing else.
//!
//! Two machine-wide storage settings live in the registry and used to be read
//! and written by shelling out — the new-disk (SAN) policy through PowerShell's
//! `Get-StorageSetting`/`Set-StorageSetting`, and the auto-mount flag through
//! `reg query`. Neither tool exists in WinPE, where the boot media has to run
//! the whole program, so both now go straight to the value the driver itself
//! reads (`.ai/no-powershell-plan_2026-07-30.md`).
//!
//! Every function is best-effort and returns an `Option`/`bool` rather than an
//! error type: a machine setting that cannot be read is not a reason to fail a
//! backup, it is a reason to log and carry on.

#![cfg(target_os = "windows")]

use tracing::debug;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, RRF_RT_REG_DWORD,
};

use crate::win_device::to_wide;

/// Read a `REG_DWORD` under `HKEY_LOCAL_MACHINE`.
///
/// `None` covers both "no such value" and "could not read it": the caller
/// treats an absent value as the setting's documented default, which is the
/// same thing the driver does.
pub fn read_dword_hklm(subkey: &str, value: &str) -> Option<u32> {
    let key = to_wide(subkey);
    let name = to_wide(value);
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings outlive the call; the out buffer is a live u32.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// Open a key under `HKEY_LOCAL_MACHINE` for writing.
fn open_for_write(subkey: &str) -> Option<HKEY> {
    let key = to_wide(subkey);
    let mut handle = HKEY::default();
    // SAFETY: `key` outlives the call; `handle` is a live out-parameter.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut handle,
        )
    };
    (status == ERROR_SUCCESS).then_some(handle)
}

/// Write a `REG_DWORD` under `HKEY_LOCAL_MACHINE`; `true` on success.
pub fn write_dword_hklm(subkey: &str, value: &str, data: u32) -> bool {
    let Some(handle) = open_for_write(subkey) else { return false };
    let name = to_wide(value);
    // SAFETY: name and data outlive the call; the key handle is closed below.
    let status = unsafe {
        RegSetValueExW(handle, PCWSTR(name.as_ptr()), 0, REG_DWORD, Some(&data.to_le_bytes()))
    };
    // SAFETY: handle from a successful RegOpenKeyExW, closed exactly once.
    let _ = unsafe { RegCloseKey(handle) };
    let ok = status == ERROR_SUCCESS;
    debug!(subkey, value, data, ok, "registry: wrote a DWORD");
    ok
}

/// Delete a value under `HKEY_LOCAL_MACHINE`; `true` if it is gone afterwards.
///
/// This is how a setting that was *absent* before we touched it gets put back:
/// writing the default value instead would leave the machine carrying an
/// explicit setting it never had.
pub fn delete_value_hklm(subkey: &str, value: &str) -> bool {
    let Some(handle) = open_for_write(subkey) else { return false };
    let name = to_wide(value);
    // SAFETY: `name` outlives the call; the key handle is closed below.
    let status = unsafe { RegDeleteValueW(handle, PCWSTR(name.as_ptr())) };
    // SAFETY: handle from a successful RegOpenKeyExW, closed exactly once.
    let _ = unsafe { RegCloseKey(handle) };
    let ok = status == ERROR_SUCCESS;
    debug!(subkey, value, ok, "registry: deleted a value");
    ok
}
