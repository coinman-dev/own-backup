//! Windows implementation: account-scope DPAPI and a protected single-owner DACL.
//! No LocalMachine DPAPI flag and no impersonated-thread fallback.

use std::fs::File;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use own_backup_common::{tr, OwnBackupError, Result};
use uuid::Uuid;
use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, BOOLEAN, ERROR_ALREADY_EXISTS, ERROR_NO_TOKEN, GENERIC_READ,
    GENERIC_WRITE, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::Security::{
    AclSizeInformation, CopySid, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation,
    GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FileDispositionInfo, GetFileInformationByHandle,
    SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use zeroize::{Zeroize, Zeroizing};

use super::{bad, Backend, KEY_DIRECTORY_NAME, MAX_LOCAL_BLOB};

fn win(error: windows::core::Error) -> OwnBackupError {
    OwnBackupError::Encryption(format!("Windows key store: {error}"))
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Word alignment matters for the TOKEN_USER/SID buffers passed to Win32.
struct Sid(Vec<u32>);
impl Sid {
    fn ptr(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut _)
    }

    fn system() -> Result<Self> {
        let sid = Self(vec![0; 17]); // SECURITY_MAX_SID_SIZE = 68.
        let mut len = 68;
        unsafe { CreateWellKnownSid(WinLocalSystemSid, PSID::default(), sid.ptr(), &mut len) }
            .map_err(win)?;
        Ok(sid)
    }

    fn administrators() -> Result<Self> {
        let sid = Self(vec![0; 17]);
        let mut len = 68;
        unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                PSID::default(),
                sid.ptr(),
                &mut len,
            )
        }
        .map_err(win)?;
        Ok(sid)
    }

    fn current() -> Result<Self> {
        let mut handle = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) }.map_err(win)?;
        let token = Token(handle);
        let mut len = 0;
        unsafe {
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
        }
        if len == 0 || len > 65536 {
            return Err(bad("invalid process token information size"));
        }
        let mut buffer = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                len,
                &mut len,
            )
            .map_err(win)?;
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let sid_len = GetLengthSid(user.User.Sid);
            if !(8..=68).contains(&sid_len) {
                return Err(bad("invalid process SID size"));
            }
            let sid = Self(vec![0; (sid_len as usize).div_ceil(4)]);
            CopySid(sid_len, sid.ptr(), user.User.Sid).map_err(win)?;
            Ok(sid)
        }
    }

    fn text(&self) -> Result<String> {
        unsafe {
            let mut out = PWSTR::null();
            ConvertSidToStringSidW(self.ptr(), &mut out).map_err(win)?;
            let allocation = Allocation(out.0.cast());
            let result = out
                .to_string()
                .map_err(|_| bad("invalid SID string encoding"));
            drop(allocation);
            result
        }
    }
}

fn no_impersonation() -> Result<()> {
    let mut handle = HANDLE::default();
    match unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut handle) } {
        Ok(()) => {
            drop(Token(handle));
            Err(bad(tr!("seal_local_windows.impersonating_thread")))
        }
        Err(e) if e.code() == HRESULT::from_win32(ERROR_NO_TOKEN.0) => Ok(()),
        Err(e) => Err(win(e)),
    }
}

struct Allocation(*mut std::ffi::c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }
}

struct Security(Allocation);
impl Security {
    fn new(sid: &Sid) -> Result<Self> {
        let sid = sid.text()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(win)?;
        Ok(Self(Allocation(descriptor.0)))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}

pub(super) struct WindowsBackend {
    root: PathBuf,
    // Neither handle shares DELETE: pin the directories while the store is open.
    _parent: File,
    directory: File,
    owner: Sid,
}

/// Pins and validates the SYSTEM-only directory before SQLite may create the
/// private password database and its WAL/SHM files. Existing files are checked
/// by handle so a permissive ACL, hard link or reparse point fails closed.
pub struct PrivateSecretDatabase {
    backend: WindowsBackend,
    path: PathBuf,
}

impl PrivateSecretDatabase {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let backend = WindowsBackend::new(data_dir)?;
        let path = backend.root.join("volume-secrets.db");
        let store = Self { backend, path };
        store.validate_files()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn validate_files(&self) -> Result<()> {
        self.backend.check_context()?;
        for suffix in ["", "-wal", "-shm"] {
            let path = self.backend.root.join(format!("volume-secrets.db{suffix}"));
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
            let wide_path = wide(&path)?;
            let h = unsafe {
                CreateFileW(
                    PCWSTR(wide_path.as_ptr()),
                    READ_CONTROL.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    None,
                )
            }
            .map_err(win)?;
            let file = unsafe { File::from_raw_handle(h.0) };
            // SQLite inherits the safe directory's SYSTEM-only ACE. Its owner
            // and sole full-access ACE must still match the directory.
            check_file_policy(&file, &self.backend.owner, false, true)?;
        }
        Ok(())
    }
}

impl WindowsBackend {
    pub(super) fn new(data_dir: &Path) -> Result<Self> {
        Self::with_owner(data_dir, Sid::system()?)
    }

    fn with_owner(data_dir: &Path, owner: Sid) -> Result<Self> {
        no_impersonation()?;
        unsafe { EqualSid(Sid::current()?.ptr(), owner.ptr()) }.map_err(|_| {
            bad(tr!("seal_local_windows.service_account_only"))
        })?;
        // Resolve the configured parent once, then pin it and the private directory.
        let parent_path = std::fs::canonicalize(data_dir)?;
        let parent = directory_handle(&parent_path)?;
        let root = parent_path.join(KEY_DIRECTORY_NAME);
        let security = Security::new(&owner)?;
        let attrs = security.attributes();
        let wide = wide(&root)?;
        match unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&attrs)) } {
            Ok(()) => {}
            Err(e) if e.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) => {}
            Err(e) => return Err(win(e)),
        }
        let directory = directory_handle(&root)?;
        check_file(&directory, &owner, true)?;
        Ok(Self {
            root,
            _parent: parent,
            directory,
            owner,
        })
    }

    fn check_context(&self) -> Result<()> {
        no_impersonation()?;
        unsafe { EqualSid(Sid::current()?.ptr(), self.owner.ptr()) }
            .map_err(|_| bad(tr!("seal_local_windows.account_not_authorized")))?;
        check_file(&self.directory, &self.owner, true)
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.dpapi"))
    }

    fn open_file(&self, id: Uuid, delete: bool) -> Result<File> {
        self.check_context()?;
        let path = wide(&self.path(id))?;
        let access = GENERIC_READ.0 | READ_CONTROL.0 | if delete { DELETE.0 } else { 0 };
        let h = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(win)?;
        let file = unsafe { File::from_raw_handle(h.0) };
        check_file(&file, &self.owner, false)?;
        Ok(file)
    }
}

impl Backend for WindowsBackend {
    fn directory(&self) -> &Path {
        &self.root
    }

    fn protect(&self, entropy: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
        self.check_context()?;
        let mut output = DpapiBuffer::default();
        unsafe {
            CryptProtectData(
                &blob(key),
                PCWSTR::null(),
                Some(&blob(entropy)),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
        .map_err(win)?;
        Ok(output.copy()?.to_vec()) // ciphertext; no secret in the returned Vec.
    }

    fn unprotect(&self, entropy: &[u8], data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.check_context()?;
        if data.is_empty() || data.len() > MAX_LOCAL_BLOB {
            return Err(bad("invalid DPAPI input size"));
        }
        let mut output = DpapiBuffer::default();
        unsafe {
            CryptUnprotectData(
                &blob(data),
                None,
                Some(&blob(entropy)),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
        .map_err(win)?;
        output.copy()
    }

    fn write_new(&self, id: Uuid, bytes: &[u8]) -> Result<()> {
        self.check_context()?;
        if bytes.len() > MAX_LOCAL_BLOB {
            return Err(bad("local key file too large"));
        }
        let security = Security::new(&self.owner)?;
        let attrs = security.attributes();
        let path = wide(&self.path(id))?;
        let h = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0 | READ_CONTROL.0 | DELETE.0,
                FILE_SHARE_READ,
                Some(&attrs),
                CREATE_NEW,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(win)?;
        let mut file = unsafe { File::from_raw_handle(h.0) };
        let result = (|| {
            check_file(&file, &self.owner, false)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = delete_handle(&file);
        }
        result
    }

    fn read(&self, id: Uuid) -> Result<Vec<u8>> {
        let mut file = self.open_file(id, false)?;
        if file.metadata()?.len() > MAX_LOCAL_BLOB as u64 {
            return Err(bad("local key file too large"));
        }
        let mut data = Vec::new();
        (&mut file)
            .take(MAX_LOCAL_BLOB as u64 + 1)
            .read_to_end(&mut data)?;
        if data.len() > MAX_LOCAL_BLOB {
            return Err(bad("local key file grew beyond limit"));
        }
        Ok(data)
    }

    /// The keys on disk, by name. A file that is not `<uuid>.dpapi` is not one
    /// of ours and is left alone: this list feeds a delete, and guessing at an
    /// unknown neighbour's file would be the one way to make that dangerous.
    fn list_ids(&self) -> Result<Vec<Uuid>> {
        self.check_context()?;
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.root)
            .map_err(|e| bad(tr!("seal_local_windows.dir_unreadable", error = e)))?
        {
            let entry = entry.map_err(|e| bad(tr!("seal_local_windows.dir_entry_error", error = e)))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".dpapi") else {
                continue;
            };
            if let Ok(id) = Uuid::parse_str(stem) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    fn remove(&self, id: Uuid) -> Result<()> {
        match self.open_file(id, true) {
            Ok(file) => delete_handle(&file),
            Err(e) => {
                // Do not hide permission/ACL errors behind "already removed".
                if matches!(std::fs::symlink_metadata(self.path(id)), Err(missing) if missing.kind() == std::io::ErrorKind::NotFound)
                {
                    self.check_context()?;
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

fn directory_handle(path: &Path) -> Result<File> {
    let path = wide(path)?;
    let h = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            READ_CONTROL.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win)?;
    let file = unsafe { File::from_raw_handle(h.0) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.map_err(win)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
    {
        return Err(bad("key-store directory must not be a reparse point"));
    }
    Ok(file)
}

fn wide(path: &Path) -> Result<Vec<u16>> {
    let value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(bad("NUL in key-store path"));
    }
    Ok(value.into_iter().chain([0]).collect())
}

fn check_file(file: &File, owner: &Sid, directory: bool) -> Result<()> {
    check_file_policy(file, owner, directory, false)
}

fn check_file_policy(file: &File, owner: &Sid, directory: bool, allow_inherited: bool) -> Result<()> {
    let h = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(h, &mut info) }.map_err(win)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(bad(
            "unexpected file type, reparse point or hard link in key store",
        ));
    }

    let mut actual_owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut actual_owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
        .ok()
        .map_err(win)?;
    }
    let _allocation = Allocation(descriptor.0);
    let mut control = 0u16;
    let mut revision = 0u32;
    unsafe {
        let owner_is_system = EqualSid(actual_owner, owner.ptr()).is_ok();
        let owner_is_admins = allow_inherited
            && EqualSid(actual_owner, Sid::administrators()?.ptr()).is_ok();
        if !owner_is_system && !owner_is_admins {
            return Err(bad("unexpected key-store owner"));
        }
        GetSecurityDescriptorControl(descriptor, &mut control, &mut revision).map_err(win)?;
    }
    if dacl.is_null() || (!allow_inherited && control & SE_DACL_PROTECTED.0 == 0) {
        return Err(bad("key-store DACL is missing or inherits permissions"));
    }
    let mut acl = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            (&mut acl as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
        .map_err(win)?;
    }
    if acl.AceCount != 1 {
        return Err(bad("key-store DACL grants unexpected access"));
    }
    let mut ace = std::ptr::null_mut();
    unsafe {
        GetAce(dacl, 0, &mut ace).map_err(win)?;
        let header = &*ace.cast::<ACE_HEADER>();
        // ACCESS_ALLOWED_ACE_TYPE = 0; INHERIT_ONLY_ACE = 0x08.
        if header.AceType != 0 || header.AceFlags & 0x08 != 0 || header.AceSize < 16 {
            return Err(bad("unexpected key-store access rule"));
        }
        let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        if allowed.Mask != FILE_ALL_ACCESS.0 {
            return Err(bad("unexpected key-store access rule"));
        }
        let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut _);
        let sid_bytes = sid.0.cast::<u8>();
        let subauthorities = *sid_bytes.add(1) as usize;
        if *sid_bytes != 1
            || subauthorities > 15
            || 8 + 8 + 4 * subauthorities > header.AceSize as usize
        {
            return Err(bad("invalid SID length in key-store access rule"));
        }
        EqualSid(sid, owner.ptr()).map_err(|_| bad("key-store DACL permits another principal"))?;
    }
    Ok(())
}

fn delete_handle(file: &File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: BOOLEAN(1),
    };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(win)
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut _,
    }
}

#[derive(Default)]
struct DpapiBuffer(CRYPT_INTEGER_BLOB);
impl DpapiBuffer {
    fn copy(&self) -> Result<Zeroizing<Vec<u8>>> {
        if self.0.cbData == 0 || self.0.cbData as usize > MAX_LOCAL_BLOB || self.0.pbData.is_null()
        {
            return Err(bad("invalid DPAPI output size"));
        }
        Ok(Zeroizing::new(unsafe {
            std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize).to_vec()
        }))
    }
}
impl Drop for DpapiBuffer {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            unsafe {
                std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                let _ = LocalFree(HLOCAL(self.0.pbData.cast()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_and_private_files_survive_reopen_under_same_account() {
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let entropy = b"test job binding";
        let blob = {
            // Test-only same code path under the test process account. Production
            // construction always requires LocalSystem.
            let store = WindowsBackend::with_owner(temp.path(), Sid::current().unwrap()).unwrap();
            let protected = store.protect(entropy, &[7; 32]).unwrap();
            store.write_new(id, &protected).unwrap();
            protected
        };
        let store = WindowsBackend::with_owner(temp.path(), Sid::current().unwrap()).unwrap();
        let disk = store.read(id).unwrap();
        assert_eq!(disk, blob);
        assert_eq!(&*store.unprotect(entropy, &disk).unwrap(), &[7; 32]);
        assert!(store.unprotect(b"another job", &disk).is_err());
        assert!(store.write_new(id, b"overwrite").is_err());
        store.remove(id).unwrap();
        assert!(store.read(id).is_err());
    }

    #[test]
    fn an_existing_directory_with_inherited_acl_is_not_adopted() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(KEY_DIRECTORY_NAME)).unwrap();
        assert!(WindowsBackend::with_owner(temp.path(), Sid::current().unwrap()).is_err());
    }

    #[test]
    fn ordinary_account_cannot_create_the_production_store() {
        let current = Sid::current().unwrap();
        let system = Sid::system().unwrap();
        if unsafe { EqualSid(current.ptr(), system.ptr()) }.is_ok() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        assert!(WindowsBackend::new(temp.path()).is_err());
        assert!(!temp.path().join(KEY_DIRECTORY_NAME).exists());
    }

    #[test]
    fn hard_links_and_oversized_files_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let store = WindowsBackend::with_owner(temp.path(), Sid::current().unwrap()).unwrap();
        let id = Uuid::new_v4();
        store.write_new(id, b"protected fixture").unwrap();
        let alias = temp.path().join("hard-link");
        std::fs::hard_link(store.path(id), &alias).unwrap();
        assert!(store.read(id).is_err());
        assert!(store.remove(id).is_err());
        assert!(store.path(id).exists());
        std::fs::remove_file(alias).unwrap();
        assert_eq!(store.read(id).unwrap(), b"protected fixture");
        std::fs::write(store.path(id), vec![0; MAX_LOCAL_BLOB + 1]).unwrap();
        assert!(store.read(id).is_err());
        store.remove(id).unwrap();
    }

    #[test]
    fn even_same_user_impersonation_is_refused() {
        use windows::Win32::Security::{ImpersonateSelf, RevertToSelf, SecurityImpersonation};
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                unsafe {
                    RevertToSelf().unwrap();
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let store = WindowsBackend::with_owner(temp.path(), Sid::current().unwrap()).unwrap();
        unsafe {
            ImpersonateSelf(SecurityImpersonation).unwrap();
        }
        let _revert = Revert;
        assert!(store.protect(b"binding", &[7; 32]).is_err());
    }
}
