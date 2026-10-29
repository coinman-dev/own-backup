//! The users whose folders are cleaned, and the volumes whose recycle bins are.
//! On Linux: `/etc/passwd`'s people, and the rights a user's folders are
//! cleaned with ([`as_user`]).

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
    #[cfg(target_os = "linux")]
    {
        linux::profiles()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    {
        linux::running_programs()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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

/// Whether the person `uid` administers this computer: root, or a member of
/// `sudo`, `wheel` or `admin` (the groups the distributions give that right).
/// Linux only; elsewhere nobody is, by this test.
pub fn is_admin(uid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::is_admin(uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = uid;
        false
    }
}

/// While it lives, this thread touches files as `user` does (Linux, when the
/// service runs as root): its filesystem user and group, and the user's
/// groups. A user's folder is cleaned with the user's own rights — a link or
/// a folder swapped in under the walk cannot make root remove what the user
/// could not, and a history database edited on a copy stays the user's.
/// `Ok(None)`: nothing to change (not root, not Linux). `Err`: the user's
/// rights could not be taken — their folders are left.
pub fn as_user(user: &User) -> std::io::Result<Option<AsUser>> {
    #[cfg(target_os = "linux")]
    {
        linux::as_user(user)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = user;
        Ok(None)
    }
}

/// See [`as_user`]; restores root's filesystem rights when dropped.
pub struct AsUser {
    #[cfg(target_os = "linux")]
    groups: Vec<libc::gid_t>,
}

#[cfg(target_os = "linux")]
impl Drop for AsUser {
    fn drop(&mut self) {
        linux::restore(&self.groups);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    use super::{AsUser, User};

    /// `/etc/passwd`'s people: uid 1000 up to 60000 (the distributions'
    /// `UID_MIN`/`UID_MAX`), a login shell, a home that is there.
    pub(super) fn profiles() -> Vec<User> {
        let Ok(text) = std::fs::read_to_string("/etc/passwd") else {
            return Vec::new();
        };
        let mut users: Vec<User> = text
            .lines()
            .filter_map(passwd_line)
            .filter(|e| (1000..60000).contains(&e.uid))
            .filter(|e| !e.shell.ends_with("nologin") && !e.shell.ends_with("/false"))
            .filter(|e| e.home.is_dir())
            .map(|e| User {
                sid: e.uid.to_string(),
                name: e.name,
                profile: e.home,
            })
            .collect();
        users.sort_by_key(|u| u.name.to_lowercase());
        users
    }

    pub(super) struct Entry {
        pub name: String,
        pub uid: u32,
        pub gid: u32,
        pub home: PathBuf,
        pub shell: String,
    }

    pub(super) fn passwd_line(line: &str) -> Option<Entry> {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            return None;
        }
        Some(Entry {
            name: f[0].to_string(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: PathBuf::from(f[5]),
            shell: f[6].to_string(),
        })
    }

    fn entry(uid: u32) -> Option<Entry> {
        std::fs::read_to_string("/etc/passwd")
            .ok()?
            .lines()
            .filter_map(passwd_line)
            .find(|e| e.uid == uid)
    }

    /// The names of `/proc/<pid>/comm` and the executable's file name, in
    /// lower case: `firefox`, `vivaldi-bin`.
    pub(super) fn running_programs() -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return names;
        };
        for e in dir.flatten() {
            let p = e.path();
            if !e
                .file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                continue;
            }
            if let Ok(comm) = std::fs::read_to_string(p.join("comm")) {
                names.insert(comm.trim().to_lowercase());
            }
            if let Ok(exe) = std::fs::read_link(p.join("exe")) {
                if let Some(n) = exe.file_name() {
                    names.insert(n.to_string_lossy().to_lowercase());
                }
            }
        }
        names
    }

    /// Members of a group line `sudo:x:27:anna,boris` — and whoever has it as
    /// the primary group.
    pub(super) fn group_has(line: &str, name: &str, gid: u32) -> bool {
        let f: Vec<&str> = line.split(':').collect();
        f.len() >= 4
            && (f[2].parse::<u32>().ok() == Some(gid) || f[3].split(',').any(|m| m.trim() == name))
    }

    pub(super) fn is_admin(uid: u32) -> bool {
        if uid == 0 {
            return true;
        }
        let Some(e) = entry(uid) else {
            return false;
        };
        std::fs::read_to_string("/etc/group").is_ok_and(|text| {
            text.lines().any(|line| {
                matches!(line.split(':').next(), Some("sudo" | "wheel" | "admin"))
                    && group_has(line, &e.name, e.gid)
            })
        })
    }

    fn current_groups() -> Vec<libc::gid_t> {
        // SAFETY: asks for the count first, then fills a buffer of that size.
        unsafe {
            let n = libc::getgroups(0, std::ptr::null_mut());
            if n <= 0 {
                return Vec::new();
            }
            let mut v = vec![0 as libc::gid_t; n as usize];
            let got = libc::getgroups(n, v.as_mut_ptr());
            v.truncate(got.max(0) as usize);
            v
        }
    }

    /// The thread's own credentials (raw system calls: glibc's `setgroups`
    /// would change every thread of the service).
    fn set_thread(uid: u32, gid: u32, groups: &[libc::gid_t]) -> std::io::Result<()> {
        // SAFETY: plain system calls on this thread's credentials; the
        // buffer outlives the call.
        unsafe {
            if libc::syscall(libc::SYS_setgroups, groups.len(), groups.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::syscall(libc::SYS_setfsgid, gid);
            libc::syscall(libc::SYS_setfsuid, uid);
            // setfsuid answers the previous value, never an error: read back.
            let now = libc::syscall(libc::SYS_setfsuid, u32::MAX) as u32;
            if now != uid {
                return Err(std::io::Error::other("setfsuid did not take"));
            }
        }
        Ok(())
    }

    pub(super) fn as_user(user: &User) -> std::io::Result<Option<AsUser>> {
        // SAFETY: no arguments.
        if unsafe { libc::geteuid() } != 0 {
            return Ok(None);
        }
        let uid: u32 = user
            .sid
            .parse()
            .map_err(|_| std::io::Error::other("not a uid"))?;
        let e = entry(uid).ok_or(std::io::ErrorKind::NotFound)?;
        let mut groups: Vec<libc::gid_t> = vec![e.gid];
        if let Ok(text) = std::fs::read_to_string("/etc/group") {
            for line in text.lines() {
                if group_has(line, &e.name, u32::MAX) {
                    if let Some(g) = line.split(':').nth(2).and_then(|g| g.parse().ok()) {
                        groups.push(g);
                    }
                }
            }
        }
        let saved = current_groups();
        let guard = AsUser { groups: saved };
        set_thread(uid, e.gid, &groups)?;
        Ok(Some(guard))
    }

    pub(super) fn restore(groups: &[libc::gid_t]) {
        if let Err(e) = set_thread(0, 0, groups) {
            // A thread that stays someone else is unusable for the service.
            tracing::error!(error = %e, "cleanup: root's rights could not be restored");
            std::process::abort();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn passwd_and_group_lines_are_read() {
            let e = passwd_line("anna:x:1000:1000:Anna,,,:/home/anna:/bin/bash").unwrap();
            assert_eq!((e.name.as_str(), e.uid, e.gid), ("anna", 1000, 1000));
            assert_eq!(e.home, PathBuf::from("/home/anna"));
            assert!(passwd_line("broken:x").is_none());
            assert!(group_has("sudo:x:27:boris,anna", "anna", 1000));
            assert!(!group_has("sudo:x:27:boris", "anna", 1000));
            assert!(group_has("wheel:x:10:", "anna", 10), "primary group");
        }
    }
}
