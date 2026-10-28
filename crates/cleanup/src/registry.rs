//! A user's traces in the registry: the lists Windows keeps of what was
//! opened, typed and connected to (`RunMRU`, `RecentDocs`, the open and save
//! dialogs' lists, Remote Desktop's servers).
//!
//! The service reaches a user's hive where it is: `HKEY_USERS\<SID>` while the
//! user is signed in, otherwise the profile's `NTUSER.DAT`, loaded privately
//! for the moment it takes and released at once (`RegLoadAppKeyW`: no other
//! process sees it, and a hive in use is refused rather than shared). Only the
//! values and subkeys of the listed keys go; the keys themselves stay.

use std::io;

use crate::users::User;

/// A value of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub name: String,
    /// Its data's length.
    pub size: u64,
    /// What it says, when it is text (`RunMRU`'s commands) or begins with a
    /// name (`RecentDocs`: the document's) — for the list the window shows.
    pub text: Option<String>,
}

/// A subkey of a key, with the length of all data below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subkey {
    pub name: String,
    pub size: u64,
}

/// One user's hive, open. Paths are below its root, `\`-separated; a key that
/// is not there has nothing in it.
pub trait Hive {
    fn values(&self, key: &str) -> Vec<Value>;
    fn subkeys(&self, key: &str) -> Vec<Subkey>;
    fn delete_value(&self, key: &str, name: &str) -> io::Result<()>;
    /// The subkey and everything below it.
    fn delete_subkey(&self, key: &str, name: &str) -> io::Result<()>;
}

/// How a run reaches a user's hive (`write`: for removal). The tests give
/// their own.
pub type OpenHive = fn(&User, bool) -> io::Result<Box<dyn Hive>>;

/// The user's hive on this computer. `NotFound`: the user has none to clean.
pub fn open_user(user: &User, write: bool) -> io::Result<Box<dyn Hive>> {
    #[cfg(windows)]
    {
        win::open_user(user, write)
    }
    #[cfg(not(windows))]
    {
        let _ = (user, write);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Text a value's data shows: a string as it is; binary data whose start is
/// a UTF-16 name (`RecentDocs`, `WordWheelQuery`, `LastVisitedPidlMRU`) — that
/// name. Anything else (a list of numbers, an item id) shows nothing.
pub fn text_of(kind: u32, data: &[u8]) -> Option<String> {
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const REG_BINARY: u32 = 3;
    if !matches!(kind, REG_SZ | REG_EXPAND_SZ | REG_BINARY) {
        return None;
    }
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    let text = String::from_utf16(&units).ok()?;
    let printable = !text.is_empty() && !text.chars().any(char::is_control);
    printable.then(|| text.chars().take(260).collect())
}

#[cfg(windows)]
mod win {
    use std::io;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegLoadAppKeyW,
        RegOpenKeyExW, RegQueryInfoKeyW, HKEY, HKEY_USERS, KEY_READ, KEY_WRITE, REG_PROCESS_APPKEY,
        REG_SAM_FLAGS,
    };

    use super::{Hive, Subkey, Value};
    use crate::users::User;

    /// `DELETE` — removing a key below one opened with it.
    const DELETE: u32 = 0x0001_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(status: WIN32_ERROR) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    /// An open key, closed when dropped.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: opened by us, closed once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    impl Key {
        fn open(parent: HKEY, path: &str, access: REG_SAM_FLAGS) -> io::Result<Key> {
            let name = wide(path);
            let mut key = HKEY::default();
            // SAFETY: the name outlives the call; `key` is a live out-parameter.
            check(unsafe { RegOpenKeyExW(parent, PCWSTR(name.as_ptr()), 0, access, &mut key) })?;
            Ok(Key(key))
        }

        /// The values: name, type, data.
        fn values(&self) -> Vec<(String, u32, Vec<u8>)> {
            let (mut count, mut max_name, mut max_data) = (0u32, 0u32, 0u32);
            // SAFETY: the key is open; only the counts are asked for.
            let status = unsafe {
                RegQueryInfoKeyW(
                    self.0,
                    PWSTR::null(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(&mut count),
                    Some(&mut max_name),
                    Some(&mut max_data),
                    None,
                    None,
                )
            };
            if status != ERROR_SUCCESS {
                return Vec::new();
            }
            let mut name = vec![0u16; max_name as usize + 1];
            let mut data = vec![0u8; max_data as usize];
            let mut out = Vec::new();
            for index in 0..count {
                let mut name_len = name.len() as u32;
                let mut data_len = data.len() as u32;
                let mut kind = 0u32;
                // SAFETY: both buffers and their lengths are live.
                let status = unsafe {
                    RegEnumValueW(
                        self.0,
                        index,
                        PWSTR(name.as_mut_ptr()),
                        &mut name_len,
                        None,
                        Some(&mut kind),
                        Some(data.as_mut_ptr()),
                        Some(&mut data_len),
                    )
                };
                // A value written since the count, longer than the buffers:
                // not listed, so not removed either.
                if status == ERROR_MORE_DATA {
                    continue;
                }
                if status != ERROR_SUCCESS {
                    break;
                }
                out.push((
                    String::from_utf16_lossy(&name[..name_len as usize]),
                    kind,
                    data[..data_len as usize].to_vec(),
                ));
            }
            out
        }

        fn subkeys(&self) -> Vec<String> {
            let mut out = Vec::new();
            for index in 0.. {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                // SAFETY: the buffer and its length are live.
                let status = unsafe {
                    RegEnumKeyExW(
                        self.0,
                        index,
                        PWSTR(name.as_mut_ptr()),
                        &mut len,
                        None,
                        PWSTR::null(),
                        None,
                        None,
                    )
                };
                if status != ERROR_SUCCESS {
                    break;
                }
                out.push(String::from_utf16_lossy(&name[..len as usize]));
            }
            out
        }

        /// All data below this key, its own values included.
        fn size(&self, depth: u32) -> u64 {
            let own: u64 = self.values().iter().map(|v| v.2.len() as u64).sum();
            if depth == 0 {
                return own;
            }
            own + self
                .subkeys()
                .iter()
                .filter_map(|s| Key::open(self.0, s, KEY_READ).ok())
                .map(|k| k.size(depth - 1))
                .sum::<u64>()
        }
    }

    pub(super) struct WinHive {
        root: Key,
    }

    pub(super) fn open_user(user: &User, write: bool) -> io::Result<Box<dyn Hive>> {
        let access = if write {
            KEY_READ | KEY_WRITE | REG_SAM_FLAGS(DELETE)
        } else {
            KEY_READ
        };
        match Key::open(HKEY_USERS, &user.sid, access) {
            Ok(root) => return Ok(Box::new(WinHive { root })),
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND.0 as i32) => {}
            Err(e) => return Err(e),
        }
        // Not signed in: the profile's own file. `RegLoadAppKeyW` would make
        // a new, empty hive where there is none — so only a plain file there.
        let file = user.profile.join("NTUSER.DAT");
        let plain = std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file());
        if !plain {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let name = wide(&file.to_string_lossy());
        let mut key = HKEY::default();
        // SAFETY: the path outlives the call; `key` is a live out-parameter.
        // The hive is this process's alone and unloads when the key closes.
        check(unsafe {
            RegLoadAppKeyW(
                PCWSTR(name.as_ptr()),
                &mut key,
                access.0,
                REG_PROCESS_APPKEY,
                0,
            )
        })?;
        Ok(Box::new(WinHive { root: Key(key) }))
    }

    impl WinHive {
        fn key(&self, path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
            Key::open(self.root.0, path, access).ok()
        }
    }

    impl Hive for WinHive {
        fn values(&self, key: &str) -> Vec<Value> {
            let Some(k) = self.key(key, KEY_READ) else {
                return Vec::new();
            };
            k.values()
                .into_iter()
                .map(|(name, kind, data)| Value {
                    name,
                    size: data.len() as u64,
                    text: super::text_of(kind, &data),
                })
                .collect()
        }

        fn subkeys(&self, key: &str) -> Vec<Subkey> {
            let Some(k) = self.key(key, KEY_READ) else {
                return Vec::new();
            };
            k.subkeys()
                .into_iter()
                .map(|name| {
                    let size = Key::open(k.0, &name, KEY_READ)
                        .map(|s| s.size(8))
                        .unwrap_or(0);
                    Subkey { name, size }
                })
                .collect()
        }

        fn delete_value(&self, key: &str, name: &str) -> io::Result<()> {
            let k = Key::open(self.root.0, key, KEY_READ | KEY_WRITE)?;
            let name = wide(name);
            // SAFETY: the name outlives the call; the key is open for writing.
            check(unsafe { RegDeleteValueW(k.0, PCWSTR(name.as_ptr())) })
        }

        fn delete_subkey(&self, key: &str, name: &str) -> io::Result<()> {
            let k = Key::open(
                self.root.0,
                key,
                KEY_READ | KEY_WRITE | REG_SAM_FLAGS(DELETE),
            )?;
            let sub = wide(name);
            // SAFETY: the name outlives the call; the key is open with the
            // rights RegDeleteTreeW asks for. Named, the subkey goes as well
            // as everything below it.
            check(unsafe { RegDeleteTreeW(k.0, PCWSTR(sub.as_ptr())) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn text_of_strings_and_named_binary_data() {
        assert_eq!(text_of(1, &utf16(r"cmd\1")).as_deref(), Some(r"cmd\1"));
        // RecentDocs: the name, then an item id.
        let mut doc = utf16("Отчёт.docx");
        doc.extend([0x14, 0, 0x1f, 0x50, 0xe0, 0x4f]);
        assert_eq!(text_of(3, &doc).as_deref(), Some("Отчёт.docx"));
        // MRUListEx: numbers, not a name.
        let list: Vec<u8> = [3u32, 1, 0, u32::MAX]
            .iter()
            .flat_map(|n| n.to_le_bytes())
            .collect();
        assert_eq!(text_of(3, &list), None);
        assert_eq!(text_of(4, &[1, 0, 0, 0]), None);
        assert_eq!(text_of(3, &[]), None);
    }
}
