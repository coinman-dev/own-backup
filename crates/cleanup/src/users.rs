//! The users whose folders are cleaned, and the volumes whose recycle bins are.

use std::path::PathBuf;

use serde::Serialize;

/// A user with a profile on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    /// `S-1-5-21-…` — how the service names the user, and the recycle bin's
    /// folder name.
    pub sid: String,
    /// What the window shows: the profile folder's name («Volodya Bolshoy»).
    pub name: String,
    /// `C:\Users\Volodya Bolshoy`.
    pub profile: PathBuf,
}

/// The people's profiles of this computer (Windows: the `ProfileList` keys of
/// local and domain accounts, `S-1-5-21-…`; the system's own profiles are not
/// among them). A profile whose folder is gone is left out.
pub fn profiles() -> Vec<User> {
    #[cfg(windows)]
    {
        win::profiles()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The roots of fixed volumes with a letter (`C:\`), whose recycle bins the
/// recycle bin item empties.
pub fn fixed_volumes() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        win::fixed_volumes()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The programs running now, by executable name in lower case
/// (`chrome.exe`) — of every session: a browser another person has open is
/// as open as the window's own.
pub fn running_programs() -> std::collections::HashSet<String> {
    #[cfg(windows)]
    {
        win::running_programs()
    }
    #[cfg(not(windows))]
    {
        std::collections::HashSet::new()
    }
}

#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_READ, RRF_RT_REG_SZ,
    };

    use super::User;

    const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";
    /// `GetDriveTypeW`'s DRIVE_FIXED.
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn profiles() -> Vec<User> {
        let mut key = HKEY::default();
        let list = wide(PROFILE_LIST);
        // SAFETY: the name outlives the call; `key` is a live out-parameter.
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(list.as_ptr()),
                0,
                KEY_READ,
                &mut key,
            )
        };
        if opened != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut users = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: the buffer and its length are live; the key is open.
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    PWSTR(name.as_mut_ptr()),
                    &mut len,
                    None,
                    PWSTR::null(),
                    None,
                    None,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                continue;
            }
            let sid = String::from_utf16_lossy(&name[..len as usize]);
            if !sid.starts_with("S-1-5-21-") {
                continue;
            }
            let Some(profile) = profile_path(key, &sid) else {
                continue;
            };
            if !profile.is_dir() {
                continue;
            }
            let name = profile
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| sid.clone());
            users.push(User { sid, name, profile });
        }
        // SAFETY: opened above, closed once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        users.sort_by_key(|u| u.name.to_lowercase());
        users
    }

    /// `ProfileImagePath` of one profile, expanded (`%SystemDrive%\Users\…`).
    fn profile_path(list: HKEY, sid: &str) -> Option<PathBuf> {
        let sub = wide(sid);
        let value = wide("ProfileImagePath");
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: names and buffer outlive the call; REG_EXPAND_SZ comes back
        // expanded because RRF_NOEXPAND is not given.
        let status = unsafe {
            RegGetValueW(
                list,
                PCWSTR(sub.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let chars = (size as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..chars]);
        let text = text.trim_end_matches('\0');
        (!text.is_empty()).then(|| PathBuf::from(text))
    }

    pub(super) fn running_programs() -> std::collections::HashSet<String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut names = std::collections::HashSet::new();
        // SAFETY: the snapshot is closed below; the entry carries its size,
        // and each successful call leaves a terminated name in it.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return names;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.insert(String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase());
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        names
    }

    pub(super) fn fixed_volumes() -> Vec<PathBuf> {
        // SAFETY: no arguments; a bit per present drive letter.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| format!("{}:\\", (b'A' + bit) as char))
            .filter(|root| {
                let w = wide(root);
                // SAFETY: the root string outlives the call.
                unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) == DRIVE_FIXED }
            })
            .map(PathBuf::from)
            .collect()
    }
}
