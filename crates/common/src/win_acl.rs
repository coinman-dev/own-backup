//! Who owns a file, on Windows, and a file opened so that it is known to be
//! the one its path names.
//!
//! Both serve the log mirror (`log_mirror`, and the service's settings check):
//! the service is SYSTEM, and the folder it appends its log to is one a
//! person at the console named. `ProgramData\OwnBackup` lets every user create
//! files, so whoever made `config.toml` first owned it; the service now
//! follows a mirror folder from that file only when SYSTEM, the
//! Administrators or the reading process's own user owns it, and opens the
//! mirror file by a handle it then checks.

#![cfg(windows)]

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileDispositionInfo, FileRenameInfo, GetFileInformationByHandle,
    GetFinalPathNameByHandleW, GetVolumeInformationW, GetVolumePathNameW,
    SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA,
    GETFINALPATHNAMEBYHANDLE_FLAGS, OPEN_EXISTING, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const SYSTEM_SID: &str = "S-1-5-18";
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// `GetVolumeInformationW` flag: the filesystem keeps ACLs.
const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// `S-1-5-…` of a SID.
///
/// # Safety
/// `sid` must point at a valid SID for the duration of the call.
pub unsafe fn sid_string(sid: PSID) -> Option<String> {
    let mut text = PWSTR::null();
    ConvertSidToStringSidW(sid, &mut text).ok()?;
    let out = text.to_string().ok();
    let _ = LocalFree(HLOCAL(text.0.cast()));
    out
}

/// The owner of `path` as `S-1-5-…`. `None` when the security descriptor
/// carries no owner.
pub fn owner_sid(path: &Path) -> io::Result<Option<String>> {
    let name = wide(path);
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `name` is NUL-terminated; on success `descriptor` is a LocalAlloc'd
    // block that `owner` points into, freed below after the SID is copied out.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner as *mut PSID),
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc.0 as i32));
    }
    // SAFETY: `owner` is null or points into `descriptor`, still alive.
    let sid = if owner.0.is_null() {
        None
    } else {
        unsafe { sid_string(owner) }
    };
    // SAFETY: allocated by GetNamedSecurityInfoW, freed once.
    unsafe {
        let _ = LocalFree(HLOCAL(descriptor.0));
    }
    Ok(sid)
}

/// The user this process runs as, `S-1-5-…`.
pub fn process_user_sid() -> Option<String> {
    // SAFETY: the token handle is closed below; TOKEN_USER is read from a
    // buffer sized by the first call, and its SID points into that buffer.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u8; needed.max(std::mem::size_of::<TOKEN_USER>() as u32) as usize];
        let got = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            &mut needed,
        );
        let _ = CloseHandle(token);
        got.ok()?;
        let user = std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_USER>());
        sid_string(user.User.Sid)
    }
}

/// Whether a settings file owned by `owner` may steer this process: SYSTEM,
/// the Administrators, or the very user this process runs as (who could write
/// anything the file makes it write anyway).
pub fn owner_may_steer(owner: &str) -> bool {
    owner == SYSTEM_SID
        || owner == ADMINISTRATORS_SID
        || process_user_sid().is_some_and(|me| me == owner)
}

/// Whether the volume holding `path` keeps owners and permissions at all.
/// FAT and exFAT do not: every user may write anywhere on such a stick.
pub fn has_persistent_acls(path: &Path) -> io::Result<bool> {
    let name = wide(path);
    let mut root = [0u16; 512];
    let mut flags = 0u32;
    // SAFETY: NUL-terminated input, buffers sized as passed.
    unsafe {
        GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut root)?;
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            Some(&mut flags),
            None,
        )?;
    }
    Ok(flags & FILE_PERSISTENT_ACLS != 0)
}

/// `path` spelled the way `GetFinalPathNameByHandleW` spells the same file:
/// components joined by single backslashes, no `\\?\`, compared caselessly.
fn plain(path: &Path) -> String {
    let rebuilt: PathBuf = path.components().collect();
    let text = rebuilt.to_string_lossy().replace('/', "\\");
    let text = text
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(&text).to_string());
    text.to_lowercase()
}

fn final_path(file: &File) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    // SAFETY: a live handle and a buffer of the length passed.
    let n = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(file.as_raw_handle()),
            &mut buf,
            GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
        )
    } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(plain(Path::new(&String::from_utf16_lossy(&buf[..n]))))
}

fn info(file: &File) -> Option<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a live handle and a writable structure.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.ok()?;
    Some(info)
}

/// A file opened for appending at `path`, created if absent, refused unless it
/// is exactly what the path says: a plain file with one name, reached without
/// a junction, symbolic link or mount point on the way (the handle's final
/// path equals `path`). A file this call created where it should not have —
/// a folder swapped for a junction in between — is removed again through the
/// same handle. Returns the file and whether it was created.
pub fn open_append_checked(path: &Path) -> io::Result<(File, bool)> {
    let name = wide(path);
    let access = FILE_APPEND_DATA.0
        | FILE_WRITE_ATTRIBUTES.0
        | FILE_WRITE_EA.0
        | FILE_READ_ATTRIBUTES.0
        | DELETE.0
        | SYNCHRONIZE.0;
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let open = |disposition| {
        // SAFETY: NUL-terminated name; the handle is owned by the File below.
        unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                share,
                None,
                disposition,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
    };
    let (handle, made) = match open(OPEN_EXISTING) {
        Ok(h) => (h, false),
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => (open(CREATE_NEW)?, true),
        Err(e) => return Err(e.into()),
    };
    // SAFETY: a fresh handle, owned from here on.
    let file = unsafe { File::from_raw_handle(handle.0) };
    let plain_file = info(&file).is_some_and(|i| {
        i.nNumberOfLinks == 1
            && i.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0)
                == 0
    });
    let where_named = final_path(&file).is_some_and(|f| f == plain(path));
    if plain_file && where_named {
        return Ok((file, made));
    }
    if made {
        delete_by_handle(&file);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "not a plain file at the named path",
    ))
}

/// Mark an open file for deletion (it goes when the last handle closes).
pub fn delete_by_handle(file: &File) {
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: true.into(),
    };
    // SAFETY: a live handle with DELETE access and a structure of the size passed.
    let _ = unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
}

/// Rename an open file to `new_name` in the folder it is in, replacing a file
/// of that name there. A bare name makes it a rename in place: no path is
/// resolved, so nothing on the way can redirect it.
pub fn rename_in_place(file: &File, new_name: &str) -> io::Result<()> {
    if new_name.contains(['\\', '/', ':']) || matches!(new_name, "" | "." | "..") {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let name: Vec<u16> = new_name.encode_utf16().collect();
    let header = std::mem::size_of::<FILE_RENAME_INFO>();
    let size = header + name.len() * 2;
    // u64 storage keeps the structure's alignment.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` is zeroed, aligned and large enough for the header plus the
    // name written at FileName's offset.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true.into();
        (*info).FileNameLength = (name.len() * 2) as u32;
        let dst = std::ptr::addr_of_mut!((*info).FileName).cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            info.cast(),
            size as u32,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_as_windows_spells_them() {
        assert_eq!(plain(Path::new(r"\\?\C:\Logs\x.log")), r"c:\logs\x.log");
        assert_eq!(plain(Path::new(r"C:/Logs//x.log")), r"c:\logs\x.log");
        assert_eq!(plain(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
    }
}
