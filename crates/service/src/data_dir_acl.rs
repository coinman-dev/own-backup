//! The installed Windows service's data folder, `C:\ProgramData\OwnBackup`,
//! writable by SYSTEM and the Administrators alone (PLAN 6а).
//!
//! `ProgramData` lets every user create files and folders in a subfolder it
//! did not make itself, and makes the creator the owner (CREATOR OWNER: full
//! control). Nothing in this folder is a user's to write: the window, the
//! tray and the command line read `config.toml` (language, log level, log
//! mirror) and the service token (a protected DACL of its own), and change
//! anything only through the service; their logs go to a folder of their own
//! when this one refuses them (`log_files::log_dir`). So at every start the
//! installed service makes the folder the Administrators', with full control
//! for SYSTEM and the Administrators and read and execute for Users
//! ([`ROOT_SDDL`]), and takes back what users made in it:
//!
//! * anything below that a user owns or may write to: owner the
//!   Administrators, the DACL the folder's gives it by inheritance;
//! * a junction or symbolic link somebody other than SYSTEM or the
//!   Administrators made in it: removed — the link, never what it leads to;
//! * a file with a second name somewhere else that needs a change: this name
//!   removed, because the other name shares its security descriptor;
//! * left alone: the SYSTEM-only key directory (its own protected DACL, which
//!   `seal::local_windows` checks exactly) and the unpacked Windows image of
//!   a boot-medium build (`media\work\tree`, rebuilt by every build). A
//!   folder an administrator already gave a protected DACL that lets nobody
//!   else write is left as it is, and so is what is in it.
//!
//! The folder itself being a link is different: the service would keep its
//! state wherever the link leads. One an administrator made is followed as
//! before, its permissions untouched; one somebody else made stops the start
//! (`check_root`).
//!
//! The walk goes top-down: a folder is locked before what is in it is
//! opened, so nobody can swap a name for a link behind the check. Descriptors
//! are read and written by handle (`GetKernelObjectSecurity`,
//! `SetKernelObjectSecurity`), opened without following a link, and nothing
//! propagates by itself: `SetNamedSecurityInfo` would walk the tree on its
//! own, through whatever a user left in it.

/// SYSTEM.
const SYSTEM: &str = "S-1-5-18";
/// BUILTIN\Administrators.
const ADMINISTRATORS: &str = "S-1-5-32-544";
/// BUILTIN\Users.
const USERS: &str = "S-1-5-32-545";

const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// Read & execute: read data, attributes, extended attributes and the
/// descriptor, list and traverse, synchronize.
const READ_AND_EXECUTE: u32 = 0x0012_00A9;
const OBJECT_INHERIT: u8 = 0x01;
const CONTAINER_INHERIT: u8 = 0x02;

/// Rights that change an entry or what a folder holds: write data / add
/// file, append data / add subfolder, write extended attributes, delete
/// child, write attributes, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_ALL,
/// GENERIC_WRITE.
const WRITE_RIGHTS: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0040
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x1000_0000
    | 0x4000_0000;

/// The folder: SYSTEM and the Administrators full control, Users read and
/// execute, all inherited by everything below; protected from `ProgramData`.
pub(crate) const ROOT_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
/// What a folder below inherits from [`ROOT_SDDL`].
const DIR_SDDL: &str = "D:AI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;0x1200a9;;;BU)";
/// What a file below inherits from [`ROOT_SDDL`].
const FILE_SDDL: &str = "D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;0x1200a9;;;BU)";

/// One entry of a DACL, as far as the decision needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ace {
    /// An access-allowed entry of any kind; deny entries and anything else
    /// grant nothing.
    pub allow: bool,
    pub sid: String,
    pub mask: u32,
    pub flags: u8,
}

/// Owner and DACL of a file or folder.
#[derive(Debug, Clone)]
pub(crate) struct Descriptor {
    pub owner: Option<String>,
    /// `SE_DACL_PROTECTED`: nothing inherited from the parent.
    pub protected: bool,
    /// `None`: no DACL at all, which lets everyone do everything.
    pub dacl: Option<Vec<Ace>>,
}

/// Why an entry is not the service's alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    /// Owned by someone other than SYSTEM or the Administrators (or nobody).
    Owner(Option<String>),
    /// No DACL.
    NoDacl,
    /// This SID may write.
    Writer(String),
    /// The folder inherits from `ProgramData` (which lets Users create).
    Inherits,
}

fn trusted(sid: &str) -> bool {
    sid == SYSTEM || sid == ADMINISTRATORS
}

fn problems(d: &Descriptor) -> Vec<Problem> {
    let mut found = Vec::new();
    if !d.owner.as_deref().is_some_and(trusted) {
        found.push(Problem::Owner(d.owner.clone()));
    }
    match &d.dacl {
        None => found.push(Problem::NoDacl),
        Some(aces) => {
            for ace in aces {
                // Inherit-only entries count too: they are what the next
                // file created in a folder gets.
                if ace.allow && ace.mask & WRITE_RIGHTS != 0 && !trusted(&ace.sid) {
                    let p = Problem::Writer(ace.sid.clone());
                    if !found.contains(&p) {
                        found.push(p);
                    }
                }
            }
        }
    }
    found
}

/// What to do with the folder itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootPlan {
    /// Exactly [`ROOT_SDDL`] (with any trusted owner): check what is in it.
    Ours,
    /// A protected DACL an administrator chose, letting nobody else write:
    /// left as it is, and so is what is in it.
    Custom,
    /// Give it [`ROOT_SDDL`] (and the Administrators as owner if `owner`).
    Fix { owner: bool, why: Vec<Problem> },
}

pub(crate) fn plan_root(d: &Descriptor) -> RootPlan {
    let mut why = problems(d);
    if !d.protected {
        why.push(Problem::Inherits);
    }
    if !why.is_empty() {
        let owner = why.iter().any(|p| matches!(p, Problem::Owner(_)));
        return RootPlan::Fix { owner, why };
    }
    if d.dacl.as_deref().is_some_and(is_root_standard) {
        RootPlan::Ours
    } else {
        RootPlan::Custom
    }
}

/// The ACEs of [`ROOT_SDDL`], in any order.
fn is_root_standard(aces: &[Ace]) -> bool {
    let both = OBJECT_INHERIT | CONTAINER_INHERIT;
    let want = [
        (SYSTEM, FILE_ALL_ACCESS),
        (ADMINISTRATORS, FILE_ALL_ACCESS),
        (USERS, READ_AND_EXECUTE),
    ];
    aces.len() == want.len()
        && want.iter().all(|(sid, mask)| {
            aces.iter()
                .any(|a| a.allow && a.sid == *sid && a.mask == *mask && a.flags == both)
        })
}

/// What to do with an entry below the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryPlan {
    /// Only SYSTEM and the Administrators may change it.
    Keep,
    /// The inherited DACL, and the Administrators as owner if `owner`.
    Reset { owner: bool, why: Vec<Problem> },
    /// A link SYSTEM or the Administrators made: not followed, kept.
    LeaveLink,
    /// A link somebody else made: the link goes.
    RemoveLink,
    /// A file with another name elsewhere that would need a change: this
    /// name goes, the descriptor both share stays as it is.
    RemoveName { why: Vec<Problem> },
}

/// `link`: a name-surrogate reparse point (junction, symbolic link).
/// `names`: the file's hard-link count.
pub(crate) fn plan_entry(d: &Descriptor, link: bool, names: u32) -> EntryPlan {
    if link {
        return if d.owner.as_deref().is_some_and(trusted) {
            EntryPlan::LeaveLink
        } else {
            EntryPlan::RemoveLink
        };
    }
    let why = problems(d);
    if why.is_empty() {
        EntryPlan::Keep
    } else if names > 1 {
        EntryPlan::RemoveName { why }
    } else {
        let owner = why.iter().any(|p| matches!(p, Problem::Owner(_)));
        EntryPlan::Reset { owner, why }
    }
}

/// Entries below the folder that are never touched, nor anything in them:
/// the key directory and a boot-medium build's Windows image.
fn left_alone(rel: &[&str]) -> bool {
    let is = |want: &[&str]| {
        rel.len() == want.len() && rel.iter().zip(want).all(|(a, b)| a.eq_ignore_ascii_case(b))
    };
    is(&[own_backup_backup::seal::local::KEY_DIRECTORY_NAME]) || is(&["media", "work", "tree"])
}

/// How deep below the folder the check goes. The service keeps nothing
/// deeper than a medium build's output; whatever lies deeper is not opened by
/// it.
const MAX_DEPTH: usize = 8;

#[cfg(windows)]
pub(crate) use win::{check_root, protect, RootKind};

#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;

    use own_backup_common::tr;
    use tracing::{info, warn};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, BOOL, ERROR_ACCESS_DENIED, ERROR_NOT_ALL_ASSIGNED,
        HANDLE, HLOCAL, LUID,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        AdjustTokenPrivileges, GetAce, GetKernelObjectSecurity, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, LookupPrivilegeValueW,
        SetKernelObjectSecurity, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES,
        OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SE_BACKUP_NAME, SE_DACL_PROTECTED, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
        SE_TAKE_OWNERSHIP_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileAttributeTagInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{
        left_alone, plan_entry, plan_root, Ace, Descriptor, EntryPlan, RootPlan, DIR_SDDL,
        FILE_SDDL, MAX_DEPTH, ROOT_SDDL,
    };

    /// `IsReparseTagNameSurrogate`: the reparse point names another file
    /// (junction, symbolic link), as opposed to holding data of its own.
    const NAME_SURROGATE: u32 = 0x2000_0000;

    /// What the folder is.
    pub(crate) enum RootKind {
        /// A folder: [`protect`] it.
        Folder,
        /// A link SYSTEM or the Administrators made: followed as before,
        /// permissions left alone.
        AdminLink,
    }

    /// Refuse a data folder that is a link anyone else made — the service's
    /// jobs, history, database and token would go wherever it leads.
    /// Called before the service writes anything there.
    pub(crate) fn check_root(dir: &Path) -> Result<RootKind, Box<dyn std::error::Error>> {
        let file = match open(dir, FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0) {
            Ok(f) => f,
            // A descriptor SYSTEM may not read is not one SYSTEM gave it.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
                open(dir, FILE_READ_ATTRIBUTES.0)?
            }
            Err(e) => return Err(e.into()),
        };
        let (attributes, _) = attribute_tag(&file)?;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
            return Ok(RootKind::Folder);
        }
        let owner = descriptor(&file).ok().and_then(|d| d.owner);
        if owner.as_deref().is_some_and(super::trusted) {
            warn!(
                folder = %dir.display(),
                owner = ?owner,
                "the data folder is a link an administrator made: followed, its permissions left as they are"
            );
            return Ok(RootKind::AdminLink);
        }
        tracing::error!(
            folder = %dir.display(),
            owner = ?owner,
            "the data folder is a link somebody other than SYSTEM or the Administrators made — refusing to start"
        );
        Err(tr!(
            "data_dir_acl.foreign_link",
            path = dir.display(),
            owner = owner.as_deref().unwrap_or("?")
        )
        .into_owned()
        .into())
    }

    #[derive(Default)]
    struct Tally {
        reset: usize,
        removed: usize,
        failed: usize,
    }

    /// Make the folder SYSTEM's and the Administrators' and take back what
    /// users made in it (module note). A failure is logged and the service
    /// starts all the same: the folder is then no worse than it was, and the
    /// secrets in it have protection of their own.
    pub(crate) fn protect(dir: &Path) {
        let mut privilege = Privileges::default();
        match protect_root(dir, &mut privilege) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                warn!(folder = %dir.display(), error = %e, "the data folder could not be protected");
                return;
            }
        }
        let mut tally = Tally::default();
        walk(dir, &mut Vec::new(), &mut privilege, &mut tally);
        if tally.reset + tally.removed + tally.failed > 0 {
            info!(
                folder = %dir.display(),
                reset = tally.reset,
                removed = tally.removed,
                failed = tally.failed,
                "data folder: entries users could change taken back"
            );
        }
    }

    /// The folder itself; `Ok(true)` when what is in it is to be checked.
    fn protect_root(dir: &Path, privilege: &mut Privileges) -> io::Result<bool> {
        let file = open_owned(dir, privilege)?;
        let (attributes, _) = attribute_tag(&file)?;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            // `check_root` let it through as an administrator's link.
            return Ok(false);
        }
        match plan_root(&descriptor(&file)?) {
            RootPlan::Ours => Ok(true),
            RootPlan::Custom => {
                info!(
                    folder = %dir.display(),
                    "the data folder has a protected DACL of an administrator's that lets nobody \
                     else write: left as it is, and so is what is in it"
                );
                Ok(false)
            }
            RootPlan::Fix { owner, why } => {
                apply(&file, ROOT_SDDL, owner)?;
                info!(
                    folder = %dir.display(),
                    ?why,
                    "the data folder is now SYSTEM's and the Administrators' (Users read)"
                );
                Ok(true)
            }
        }
    }

    fn walk(dir: &Path, rel: &mut Vec<String>, privilege: &mut Privileges, tally: &mut Tally) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(folder = %dir.display(), error = %e, "data folder: could not be listed");
                tally.failed += 1;
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            rel.push(name);
            let parts: Vec<&str> = rel.iter().map(String::as_str).collect();
            if !left_alone(&parts) {
                let path = entry.path();
                match secure_entry(&path, privilege, tally) {
                    Ok(true) if rel.len() < MAX_DEPTH => walk(&path, rel, privilege, tally),
                    Ok(true) => warn!(
                        folder = %path.display(),
                        "data folder: nested too deep, what is inside was not checked"
                    ),
                    Ok(false) => {}
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "data folder: entry could not be secured");
                        tally.failed += 1;
                    }
                }
            }
            rel.pop();
        }
    }

    /// One entry below the folder; `Ok(true)` for a folder to go into.
    fn secure_entry(
        path: &Path,
        privilege: &mut Privileges,
        tally: &mut Tally,
    ) -> io::Result<bool> {
        let file = open_owned(path, privilege)?;
        let (attributes, tag) = attribute_tag(&file)?;
        let link = is_link(attributes, tag);
        let d = descriptor(&file)?;
        match plan_entry(&d, link, names(&file)?) {
            EntryPlan::Keep => {}
            EntryPlan::LeaveLink => info!(
                path = %path.display(),
                owner = ?d.owner,
                "data folder: a link an administrator made, not followed"
            ),
            EntryPlan::RemoveLink => {
                remove(&file)?;
                warn!(path = %path.display(), owner = ?d.owner, "data folder: a link a user made removed (not what it leads to)");
                tally.removed += 1;
            }
            EntryPlan::RemoveName { why } => {
                remove(&file)?;
                warn!(path = %path.display(), ?why, "data folder: a second name of a file elsewhere removed");
                tally.removed += 1;
            }
            EntryPlan::Reset { owner, why } => {
                let directory = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                apply(&file, if directory { DIR_SDDL } else { FILE_SDDL }, owner)?;
                info!(path = %path.display(), ?why, "data folder: entry reset to the folder's permissions");
                tally.reset += 1;
            }
        }
        // Never into a reparse point of any kind.
        Ok(attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
            && attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            && !link)
    }

    const FULL: u32 =
        READ_CONTROL.0 | WRITE_DAC.0 | WRITE_OWNER.0 | DELETE.0 | FILE_READ_ATTRIBUTES.0;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// The entry itself — a link is opened, not followed; a folder can be.
    fn open(path: &Path, access: u32) -> io::Result<File> {
        let name = wide(path);
        // SAFETY: NUL-terminated name; the handle is owned by the File.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }?;
        // SAFETY: a fresh handle, owned from here on.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }

    /// [`open`] with [`FULL`] access — when the entry's descriptor shuts
    /// SYSTEM out, with the backup, restore and take-ownership privileges,
    /// which grant exactly these rights whatever the DACL says.
    fn open_owned(path: &Path, privilege: &mut Privileges) -> io::Result<File> {
        match open(path, FULL) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
                privilege.enable()?;
                let file = open(path, FULL)?;
                warn!(path = %path.display(), "data folder: an entry that shut SYSTEM out opened with the backup and restore privileges");
                Ok(file)
            }
            other => other,
        }
    }

    fn raw(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle())
    }

    fn attribute_tag(file: &File) -> io::Result<(u32, u32)> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: a live handle and a structure of the size passed.
        unsafe {
            GetFileInformationByHandleEx(
                raw(file),
                FileAttributeTagInfo,
                std::ptr::addr_of_mut!(info).cast(),
                std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        }?;
        Ok((info.FileAttributes, info.ReparseTag))
    }

    fn is_link(attributes: u32, tag: u32) -> bool {
        attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 && tag & NAME_SURROGATE != 0
    }

    fn names(file: &File) -> io::Result<u32> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: a live handle and a writable structure.
        unsafe { GetFileInformationByHandle(raw(file), &mut info) }?;
        Ok(info.nNumberOfLinks)
    }

    fn remove(file: &File) -> io::Result<()> {
        own_backup_common::win_acl::delete_by_handle(file)
    }

    /// Owner and DACL, read by handle.
    fn descriptor(file: &File) -> io::Result<Descriptor> {
        let wanted = (OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION).0;
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer.
        let _ = unsafe {
            GetKernelObjectSecurity(
                raw(file),
                wanted,
                PSECURITY_DESCRIPTOR::default(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 storage keeps the self-relative descriptor aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let sd = PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast());
        // SAFETY: `buf` holds `needed` bytes and outlives every pointer read
        // out of the descriptor below.
        unsafe {
            GetKernelObjectSecurity(raw(file), wanted, sd, needed, &mut needed)?;
            let mut control = 0u16;
            let mut revision = 0u32;
            GetSecurityDescriptorControl(sd, &mut control, &mut revision)?;
            let mut owner = PSID::default();
            let mut defaulted = BOOL::default();
            GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted)?;
            let owner = if owner.0.is_null() {
                None
            } else {
                own_backup_common::win_acl::sid_string(owner)
            };
            let mut present = BOOL::default();
            let mut acl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted)?;
            let dacl = if !present.as_bool() || acl.is_null() {
                None
            } else {
                Some(aces(acl)?)
            };
            Ok(Descriptor {
                owner,
                protected: control & SE_DACL_PROTECTED.0 != 0,
                dacl,
            })
        }
    }

    /// # Safety
    /// `acl` points at a valid ACL.
    unsafe fn aces(acl: *mut ACL) -> io::Result<Vec<Ace>> {
        let mut out = Vec::new();
        for i in 0..(*acl).AceCount {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            GetAce(acl, u32::from(i), &mut ace)?;
            let base = ace.cast::<u8>();
            let header = std::ptr::read_unaligned(base.cast::<ACE_HEADER>());
            let mask = std::ptr::read_unaligned(base.add(4).cast::<u32>());
            // Allowed: plain, object, callback, callback object.
            let allow = matches!(header.AceType, 0x0 | 0x5 | 0x9 | 0xB);
            let sid_at = match header.AceType {
                // ACCESS_{ALLOWED,DENIED}[_CALLBACK]_ACE: header, mask, SID.
                0x0 | 0x1 | 0x9 | 0xA => Some(8usize),
                // Object entries: header, mask, flags, up to two GUIDs, SID.
                0x5 | 0x6 | 0xB | 0xC => {
                    let flags = std::ptr::read_unaligned(base.add(8).cast::<u32>());
                    Some(12 + 16 * ((flags & 1) + ((flags >> 1) & 1)) as usize)
                }
                _ => None,
            };
            let sid = match sid_at {
                Some(at) if at < usize::from(header.AceSize) => {
                    own_backup_common::win_acl::sid_string(PSID(base.add(at).cast()))
                        .unwrap_or_default()
                }
                _ => String::new(),
            };
            out.push(Ace {
                allow,
                sid,
                mask,
                flags: header.AceFlags,
            });
        }
        Ok(out)
    }

    /// Write the parts of `sddl` named by `what`, by handle. Nothing below
    /// is touched: this does not propagate.
    fn set(file: &File, sddl: &str, what: OBJECT_SECURITY_INFORMATION) -> io::Result<()> {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: NUL-terminated text; the descriptor is LocalAlloc'd and
        // freed once below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
            let result = SetKernelObjectSecurity(raw(file), what, sd);
            let _ = LocalFree(HLOCAL(sd.0));
            result?;
        }
        Ok(())
    }

    /// `dacl` as the DACL, and the Administrators as the owner if `owner`.
    fn apply(file: &File, dacl: &str, owner: bool) -> io::Result<()> {
        if owner {
            set(
                file,
                &format!("O:BA{dacl}"),
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            )
        } else {
            set(file, dacl, DACL_SECURITY_INFORMATION)
        }
    }

    /// Backup, restore and take-ownership privileges, enabled only when an
    /// entry shuts SYSTEM out, and put back as they were when the check is
    /// over: with them every open that asks for backup semantics ignores
    /// DACLs, and nothing else in the service should get that by accident.
    #[derive(Default)]
    struct Privileges {
        token: Option<HANDLE>,
        /// What each enabling changed (nothing for one already on).
        previous: Vec<TOKEN_PRIVILEGES>,
        /// Whether all three are on; asked once.
        enabled: Option<bool>,
    }

    impl Privileges {
        fn enable(&mut self) -> io::Result<()> {
            match self.enabled {
                Some(true) => return Ok(()),
                Some(false) => return Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                None => {}
            }
            let result = self.try_enable();
            self.enabled = Some(result.is_ok());
            result
        }

        fn try_enable(&mut self) -> io::Result<()> {
            // SAFETY: the token handle is kept and closed in Drop; the
            // structures are of the sizes passed.
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut token,
                )?;
                self.token = Some(token);
                for name in [SE_BACKUP_NAME, SE_RESTORE_NAME, SE_TAKE_OWNERSHIP_NAME] {
                    let mut luid = LUID::default();
                    LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid)?;
                    let wanted = TOKEN_PRIVILEGES {
                        PrivilegeCount: 1,
                        Privileges: [LUID_AND_ATTRIBUTES {
                            Luid: luid,
                            Attributes: SE_PRIVILEGE_ENABLED,
                        }],
                    };
                    let mut previous = TOKEN_PRIVILEGES::default();
                    let mut len = 0u32;
                    AdjustTokenPrivileges(
                        token,
                        false,
                        Some(&wanted),
                        std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                        Some(&mut previous),
                        Some(&mut len),
                    )?;
                    // Success with ERROR_NOT_ALL_ASSIGNED: the token lacks it.
                    if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                        return Err(io::Error::from_raw_os_error(
                            ERROR_NOT_ALL_ASSIGNED.0 as i32,
                        ));
                    }
                    self.previous.push(previous);
                }
            }
            Ok(())
        }
    }

    impl Drop for Privileges {
        fn drop(&mut self) {
            let Some(token) = self.token else { return };
            // SAFETY: the handle opened in `enable`, closed once here.
            unsafe {
                for previous in self.previous.iter().rev() {
                    if previous.PrivilegeCount > 0 {
                        let _ = AdjustTokenPrivileges(token, false, Some(previous), 0, None, None);
                    }
                }
                let _ = CloseHandle(token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";
    const EVERYONE: &str = "S-1-1-0";
    const CREATOR_OWNER: &str = "S-1-3-0";
    const INTERACTIVE: &str = "S-1-5-4";
    const OICI: u8 = OBJECT_INHERIT | CONTAINER_INHERIT;
    const INHERITED: u8 = 0x10;
    const INHERIT_ONLY: u8 = 0x08;

    fn allow(sid: &str, mask: u32, flags: u8) -> Ace {
        Ace {
            allow: true,
            sid: sid.into(),
            mask,
            flags,
        }
    }

    fn deny(sid: &str, mask: u32) -> Ace {
        Ace {
            allow: false,
            sid: sid.into(),
            mask,
            flags: 0,
        }
    }

    fn desc(owner: &str, protected: bool, aces: Vec<Ace>) -> Descriptor {
        Descriptor {
            owner: Some(owner.into()),
            protected,
            dacl: Some(aces),
        }
    }

    fn ours() -> Vec<Ace> {
        vec![
            allow(SYSTEM, FILE_ALL_ACCESS, OICI),
            allow(ADMINISTRATORS, FILE_ALL_ACCESS, OICI),
            allow(USERS, READ_AND_EXECUTE, OICI),
        ]
    }

    /// What a folder made in `ProgramData` inherits there by default.
    fn programdata_inherited() -> Vec<Ace> {
        let id = INHERITED;
        vec![
            allow(SYSTEM, FILE_ALL_ACCESS, OICI | id),
            allow(ADMINISTRATORS, FILE_ALL_ACCESS, OICI | id),
            allow(CREATOR_OWNER, FILE_ALL_ACCESS, OICI | INHERIT_ONLY | id),
            allow(USERS, READ_AND_EXECUTE, OICI | id),
            // Users: create files, create folders, write (extended)
            // attributes — this folder and subfolders.
            allow(USERS, 0x0000_0116, CONTAINER_INHERIT | id),
        ]
    }

    #[test]
    fn a_folder_a_user_made_in_programdata_is_fixed_owner_and_all() {
        let plan = plan_root(&desc(USER, false, programdata_inherited()));
        let RootPlan::Fix { owner, why } = plan else {
            panic!("{plan:?}")
        };
        assert!(owner);
        assert!(why.contains(&Problem::Owner(Some(USER.into()))));
        assert!(why.contains(&Problem::Writer(USERS.into())));
        assert!(why.contains(&Problem::Writer(CREATOR_OWNER.into())));
        assert!(why.contains(&Problem::Inherits));
    }

    #[test]
    fn the_services_own_inheriting_folder_is_fixed_without_a_new_owner() {
        let plan = plan_root(&desc(ADMINISTRATORS, false, programdata_inherited()));
        assert!(
            matches!(plan, RootPlan::Fix { owner: false, .. }),
            "{plan:?}"
        );
    }

    #[test]
    fn inheriting_alone_is_reason_enough() {
        // Nothing grants writing today, but the parent decides tomorrow.
        let plan = plan_root(&desc(SYSTEM, false, ours()));
        assert_eq!(
            plan,
            RootPlan::Fix {
                owner: false,
                why: vec![Problem::Inherits]
            }
        );
    }

    #[test]
    fn the_folder_as_this_module_leaves_it_is_ours() {
        assert_eq!(
            plan_root(&desc(ADMINISTRATORS, true, ours())),
            RootPlan::Ours
        );
        assert_eq!(plan_root(&desc(SYSTEM, true, ours())), RootPlan::Ours);
        let mut reordered = ours();
        reordered.reverse();
        assert_eq!(plan_root(&desc(SYSTEM, true, reordered)), RootPlan::Ours);
    }

    #[test]
    fn a_users_ownership_alone_is_fixed_even_with_our_dacl() {
        // The owner may always rewrite the DACL.
        let plan = plan_root(&desc(USER, true, ours()));
        assert!(
            matches!(plan, RootPlan::Fix { owner: true, .. }),
            "{plan:?}"
        );
    }

    #[test]
    fn a_stricter_protected_dacl_of_an_administrators_is_left_alone() {
        let admin_only = vec![
            allow(SYSTEM, FILE_ALL_ACCESS, OICI),
            allow(ADMINISTRATORS, FILE_ALL_ACCESS, OICI),
        ];
        assert_eq!(
            plan_root(&desc(ADMINISTRATORS, true, admin_only)),
            RootPlan::Custom
        );
        // Readers other than Users are no writers either.
        let mut readers = ours();
        readers.push(allow(INTERACTIVE, 0x8000_0000, OICI));
        assert_eq!(
            plan_root(&desc(ADMINISTRATORS, true, readers)),
            RootPlan::Custom
        );
    }

    #[test]
    fn anyone_else_who_may_write_is_a_reason() {
        for (sid, mask) in [
            (EVERYONE, 0x0000_0002),
            (USERS, 0x4000_0000),
            (USER, 0x1000_0000),
            (INTERACTIVE, 0x0004_0000),
            ("S-1-5-11", 0x0001_0000),
        ] {
            let mut aces = ours();
            aces.push(allow(sid, mask, OICI));
            let plan = plan_root(&desc(ADMINISTRATORS, true, aces));
            assert_eq!(
                plan,
                RootPlan::Fix {
                    owner: false,
                    why: vec![Problem::Writer(sid.into())]
                },
                "{sid} {mask:#x}"
            );
        }
    }

    #[test]
    fn a_deny_entry_grants_nothing() {
        let mut aces = ours();
        aces.push(deny(USER, FILE_ALL_ACCESS));
        assert_eq!(
            plan_root(&desc(ADMINISTRATORS, true, aces)),
            RootPlan::Custom
        );
    }

    #[test]
    fn no_dacl_at_all_lets_everyone_in() {
        let d = Descriptor {
            owner: Some(ADMINISTRATORS.into()),
            protected: true,
            dacl: None,
        };
        assert!(matches!(plan_root(&d), RootPlan::Fix { why, .. } if why == vec![Problem::NoDacl]));
        assert!(matches!(
            plan_entry(&d, false, 1),
            EntryPlan::Reset { owner: false, .. }
        ));
    }

    #[test]
    fn the_service_token_with_its_own_dacl_is_kept() {
        // D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU), created by the service.
        let token = desc(
            ADMINISTRATORS,
            true,
            vec![
                allow(SYSTEM, 0x1000_0000, 0),
                allow(ADMINISTRATORS, 0x1000_0000, 0),
                allow(INTERACTIVE, 0x8000_0000, 0),
            ],
        );
        assert_eq!(plan_entry(&token, false, 1), EntryPlan::Keep);
    }

    #[test]
    fn a_file_the_service_wrote_under_the_old_inheritance_is_kept() {
        // Files do not get the Users "create" entry (container inherit
        // only), and CREATOR OWNER became the Administrators.
        let file = desc(
            ADMINISTRATORS,
            false,
            vec![
                allow(SYSTEM, FILE_ALL_ACCESS, INHERITED),
                allow(ADMINISTRATORS, FILE_ALL_ACCESS, INHERITED),
                allow(USERS, READ_AND_EXECUTE, INHERITED),
            ],
        );
        assert_eq!(plan_entry(&file, false, 1), EntryPlan::Keep);
    }

    #[test]
    fn a_users_file_is_reset_and_taken_over() {
        let file = desc(
            USER,
            false,
            vec![
                allow(SYSTEM, FILE_ALL_ACCESS, INHERITED),
                allow(ADMINISTRATORS, FILE_ALL_ACCESS, INHERITED),
                allow(USER, FILE_ALL_ACCESS, INHERITED),
                allow(USERS, READ_AND_EXECUTE, INHERITED),
            ],
        );
        let plan = plan_entry(&file, false, 1);
        assert_eq!(
            plan,
            EntryPlan::Reset {
                owner: true,
                why: vec![
                    Problem::Owner(Some(USER.into())),
                    Problem::Writer(USER.into())
                ]
            }
        );
    }

    #[test]
    fn a_users_explicit_entry_on_a_service_file_is_reset() {
        let mut aces = vec![
            allow(SYSTEM, FILE_ALL_ACCESS, INHERITED),
            allow(ADMINISTRATORS, FILE_ALL_ACCESS, INHERITED),
        ];
        aces.insert(0, allow(EVERYONE, 0x0000_0002, 0));
        let plan = plan_entry(&desc(SYSTEM, false, aces), false, 1);
        assert!(
            matches!(plan, EntryPlan::Reset { owner: false, .. }),
            "{plan:?}"
        );
    }

    #[test]
    fn a_folder_the_service_made_under_the_old_inheritance_is_reset() {
        // It still lets Users create in it.
        let plan = plan_entry(
            &desc(ADMINISTRATORS, false, programdata_inherited()),
            false,
            1,
        );
        assert!(
            matches!(plan, EntryPlan::Reset { owner: false, .. }),
            "{plan:?}"
        );
    }

    #[test]
    fn links_go_unless_an_administrator_made_them() {
        let link = |owner: &str| desc(owner, false, programdata_inherited());
        assert_eq!(plan_entry(&link(USER), true, 1), EntryPlan::RemoveLink);
        assert_eq!(
            plan_entry(&link(ADMINISTRATORS), true, 1),
            EntryPlan::LeaveLink
        );
        assert_eq!(plan_entry(&link(SYSTEM), true, 1), EntryPlan::LeaveLink);
        let ownerless = Descriptor {
            owner: None,
            protected: false,
            dacl: Some(ours()),
        };
        assert_eq!(plan_entry(&ownerless, true, 1), EntryPlan::RemoveLink);
    }

    #[test]
    fn a_second_name_is_removed_rather_than_its_shared_descriptor_changed() {
        let users = desc(USER, false, vec![allow(USER, FILE_ALL_ACCESS, 0)]);
        assert!(matches!(
            plan_entry(&users, false, 2),
            EntryPlan::RemoveName { .. }
        ));
        let safe = desc(SYSTEM, true, vec![allow(SYSTEM, FILE_ALL_ACCESS, 0)]);
        assert_eq!(plan_entry(&safe, false, 2), EntryPlan::Keep);
    }

    #[test]
    fn the_key_directory_and_a_media_image_are_never_entered() {
        use own_backup_backup::seal::local::KEY_DIRECTORY_NAME;
        assert!(left_alone(&[KEY_DIRECTORY_NAME]));
        assert!(left_alone(&["Media", "WORK", "tree"]));
        assert!(!left_alone(&["media", "work"]));
        assert!(!left_alone(&["media", "out", "tree"]));
        assert!(!left_alone(&["Logs", KEY_DIRECTORY_NAME]));
        assert!(!left_alone(&["config.toml"]));
    }

    #[test]
    fn the_inherited_descriptors_are_the_roots_entries_marked_inherited() {
        // Each of SY, BA (FA) and BU (0x1200a9) of the root, with ID added;
        // folders keep OICI, files carry no inheritance flags.
        let entries = |sddl: &str| {
            sddl.split('(')
                .skip(1)
                .map(|ace| ace.trim_end_matches(')').to_string())
                .collect::<Vec<_>>()
        };
        let root = entries(ROOT_SDDL);
        assert!(ROOT_SDDL.starts_with("D:P"));
        assert_eq!(
            root,
            ["A;OICI;FA;;;SY", "A;OICI;FA;;;BA", "A;OICI;0x1200a9;;;BU"]
        );
        let as_dir: Vec<String> = root
            .iter()
            .map(|a| a.replace(";OICI;", ";OICIID;"))
            .collect();
        let as_file: Vec<String> = root.iter().map(|a| a.replace(";OICI;", ";ID;")).collect();
        assert_eq!(entries(DIR_SDDL), as_dir);
        assert_eq!(entries(FILE_SDDL), as_file);
        assert!(!DIR_SDDL.starts_with("D:P") && !FILE_SDDL.starts_with("D:P"));
        assert_eq!(READ_AND_EXECUTE, 0x1200a9);
    }
}
