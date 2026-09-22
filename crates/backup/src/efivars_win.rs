//! Reading the machine's boot configuration out of firmware.
//!
//! The Windows half of [`crate::efivars`]. Boot entries are UEFI variables, and
//! Windows hands them over through `GetFirmwareEnvironmentVariableExW` once the
//! process holds `SeSystemEnvironmentPrivilege` — which an elevated process has
//! but does not have *enabled*, the same dance as [`crate::privileges_win`].
//!
//! **Read only.** Nothing here writes to firmware; the write half arrives with
//! stage E2 of `.ai/boot-entries-plan_2026-09-18.md`, and not before the backup
//! of these variables works, because until then a mistake has no way back.
//!
//! ## Why the list is not simply `BootOrder`
//!
//! `BootOrder` is the menu: the entries the firmware offers, in the order it
//! offers them. An entry that was switched off, or that lost its place when the
//! firmware rebuilt the list, still exists as a variable and is missing from
//! there. Those are exactly the ones worth finding — on the machine this was
//! written for, Acronis and R-Drive Image had their loaders sitting on the ESP
//! with no entry naming them at all (2026-09-18).
//!
//! So the list comes from enumerating variables, and `BootOrder` is applied to
//! it afterwards as an ordering. `NtEnumerateSystemEnvironmentValuesEx` is what
//! every tool of this kind uses for that; it is not documented, so a plain scan
//! of `Boot0000`..`Boot00FF` stands behind it. Which one answered is reported,
//! because "no entry for Acronis" and "we did not look properly" are the same
//! sentence on screen and very different facts.

#![cfg(target_os = "windows")]

use own_backup_common::{OwnBackupError, Result};
use tracing::{debug, info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_FUNCTION, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::efivars::{self, LoadOption};

/// Not in the `windows` crate at 0.58 -- it carries no binding for this call at
/// all -- so it is declared here. Plain kernel32, documented, present since
/// Windows 8.
#[link(name = "kernel32")]
extern "system" {
    fn GetFirmwareEnvironmentVariableExW(
        lpname: *const u16,
        lpguid: *const u16,
        pbuffer: *mut core::ffi::c_void,
        nsize: u32,
        pdwattributes: *mut u32,
    ) -> u32;

    fn SetFirmwareEnvironmentVariableExW(
        lpname: *const u16,
        lpguid: *const u16,
        pvalue: *const core::ffi::c_void,
        nsize: u32,
        dwattributes: u32,
    ) -> windows::Win32::Foundation::BOOL;
}

/// The attribute bits a boot variable is stored with: it survives a power cut,
/// and both the firmware and a running system may read it. Writing one back
/// without NON_VOLATILE would make it disappear at the next cold start, which
/// looks exactly like a restore that silently did nothing.
pub const EFI_VARIABLE_NON_VOLATILE: u32 = 0x01;
pub const EFI_VARIABLE_BOOTSERVICE_ACCESS: u32 = 0x02;
pub const EFI_VARIABLE_RUNTIME_ACCESS: u32 = 0x04;
pub const BOOT_VARIABLE_ATTRIBUTES: u32 =
    EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;

/// The namespace the boot variables live in (UEFI spec, EFI Global Variable).
/// Windows wants it as a string, braces and all.
const EFI_GLOBAL: &str = "{8be4df61-93ca-11d2-aa0d-00e098032b8c}";

/// UTF-16 with the NUL the call expects.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Room for one variable. A load option is a few hundred bytes; firmware
/// usually refuses anything past 64 KiB, so this cannot be too small in
/// practice and costs nothing when it is too large.
const VAR_MAX: usize = 64 * 1024;

fn dev(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(msg.to_string())
}

/// One entry, with what the rest of the firmware's state says about it.
#[derive(Debug, Clone)]
pub struct BootEntry {
    pub number: u16,
    pub option: LoadOption,
    /// Position in `BootOrder`. `None` = the firmware does not offer it; the
    /// entry exists and no menu shows it.
    pub order_index: Option<usize>,
    /// This machine booted from it just now.
    pub is_current: bool,
    /// It is set to be used once, on the next start only.
    pub is_next: bool,
}

impl BootEntry {
    /// The entry names a device rather than a file on a partition: network
    /// boot, a removable-media slot, "Hard Drive". Firmware creates these
    /// itself and recreates them when they are deleted, so they are shown and
    /// left alone.
    pub fn is_device_entry(&self) -> bool {
        self.option.file_path().is_none()
    }
}

/// Everything read in one pass.
#[derive(Debug, Clone)]
pub struct BootConfig {
    pub entries: Vec<BootEntry>,
    pub order: Vec<u16>,
    pub current: Option<u16>,
    pub next: Option<u16>,
    pub timeout: Option<u16>,
    /// How the entries were found, in words for the report.
    pub found_by: &'static str,
    /// Entry numbers named by `BootOrder` that have no variable behind them.
    /// A firmware that lists a boot option it cannot load is worth saying out
    /// loud rather than quietly dropping from the list.
    pub dangling: Vec<u16>,
}

/// Is this machine even booted through UEFI?
///
/// The documented probe: ask for a variable with an empty name. A UEFI machine
/// refuses the *name*; a legacy BIOS machine refuses the whole idea with
/// `ERROR_INVALID_FUNCTION`.
pub fn is_uefi() -> bool {
    let (name, guid) = (wide(""), wide(EFI_GLOBAL));
    let mut buf = [0u8; 8];
    // SAFETY: both strings are NUL-terminated and outlive the call; the buffer
    // and its length travel together. The empty name is the documented way to
    // ask this question.
    let _ = unsafe {
        GetFirmwareEnvironmentVariableExW(
            name.as_ptr(),
            guid.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: reading the calling thread's last error right after the call.
    unsafe { windows::Win32::Foundation::GetLastError() != ERROR_INVALID_FUNCTION }
}

/// Enable `SeSystemEnvironmentPrivilege`, without which every read below
/// returns `ERROR_PRIVILEGE_NOT_HELD` and nothing explains why.
///
/// `Ok(false)` means the token does not hold it at all — an unelevated process.
/// That is a fact for the caller to report, not an error to throw.
pub fn enable_privilege() -> Result<bool> {
    // SAFETY: the standard token-privilege sequence; the handle is closed on
    // every path out.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)
            .map_err(|e| dev(format!("не открыть маркер процесса: {e}")))?;

        let mut luid = LUID::default();
        if let Err(e) = LookupPrivilegeValueW(PCWSTR::null(), w!("SeSystemEnvironmentPrivilege"), &mut luid) {
            let _ = CloseHandle(token);
            return Err(dev(format!("нет такой привилегии: {e}")));
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let adjust = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        // The call reports success even when the privilege is not held; only
        // the last error tells them apart.
        let not_held = windows::Win32::Foundation::GetLastError() == ERROR_NOT_ALL_ASSIGNED;
        let _ = CloseHandle(token);
        adjust.map_err(|e| dev(format!("не включить привилегию: {e}")))?;
        Ok(!not_held)
    }
}

/// Take the privilege on first use.
///
/// It belongs to the PROCESS, not to the call, and every read and write here
/// needs it. Leaving that to callers cost the same bug twice on 2026-09-18:
/// `boot import` and then `boot rename` both failed with
/// ERROR_PRIVILEGE_NOT_HELD, while `boot list` worked — because that one goes
/// through `read_config`, which asks for the privilege first. A second
/// identical failure is a sign the rule is in the wrong place, so it moved
/// here, where nothing can go around it.
fn ensure_privilege() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // The outcome is reported by whoever needs to report it: a read that
        // then fails says so with the code the firmware gave.
        let _ = enable_privilege();
    });
}

/// One variable's bytes, or `None` when the firmware has no such variable.
pub fn read_var(name: &str) -> Result<Option<Vec<u8>>> {
    Ok(read_var_ex(name)?.map(|(data, _)| data))
}

/// The same, with the attribute bits the variable is stored under. A copy that
/// does not record them cannot put the variable back the way it was.
pub fn read_var_ex(name: &str) -> Result<Option<(Vec<u8>, u32)>> {
    ensure_privilege();
    let (name_w, guid) = (wide(name), wide(EFI_GLOBAL));
    let mut attrs: u32 = 0;
    let mut buf = vec![0u8; VAR_MAX];
    // SAFETY: both strings are NUL-terminated and outlive the call; the buffer
    // and its length travel together.
    let n = unsafe {
        GetFirmwareEnvironmentVariableExW(
            name_w.as_ptr(),
            guid.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut attrs,
        )
    };
    if n == 0 {
        // SAFETY: the error belongs to the call just made.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        use windows::Win32::Foundation::{ERROR_ENVVAR_NOT_FOUND, ERROR_FILE_NOT_FOUND};
        if err == ERROR_ENVVAR_NOT_FOUND || err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if err == ERROR_INVALID_FUNCTION {
            return Err(dev("эта машина загружается не через UEFI — записей загрузки нет"));
        }
        if err == windows::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD {
            return Err(dev(
                "нет привилегии SeSystemEnvironmentPrivilege — нужны права администратора",
            ));
        }
        return Err(dev(format!("не прочитать переменную {name}: {:?}", err)));
    }
    buf.truncate(n as usize);
    Ok(Some((buf, attrs)))
}

/// Put a variable back. **The only place in this program that writes to
/// firmware.**
///
/// The name is checked against [`efivars::is_boot_variable`] even though the
/// callers already did: a wrong name here does not produce an error message,
/// it produces a machine that behaves differently at the next power-on, and
/// the check costs nothing. Everything outside the boot namespace — platform
/// keys, vendor settings, secure-boot state — stays out of reach by
/// construction rather than by care.
pub fn write_var(name: &str, data: &[u8], attributes: u32) -> Result<()> {
    if !crate::efivars::is_boot_variable(name) {
        return Err(dev(format!("{name}: не переменная загрузки — записывать её отсюда нельзя")));
    }
    ensure_privilege();
    if data.is_empty() {
        // Zero length is how UEFI spells "delete". Not something a restore ever
        // wants by accident, so it is refused here and asked for by name when
        // the time comes.
        return Err(dev(format!("{name}: пустое значение стёрло бы переменную")));
    }
    let (name_w, guid) = (wide(name), wide(EFI_GLOBAL));
    // SAFETY: both strings are NUL-terminated and outlive the call; the data
    // pointer and its length travel together.
    let ok = unsafe {
        SetFirmwareEnvironmentVariableExW(
            name_w.as_ptr(),
            guid.as_ptr(),
            data.as_ptr() as *const _,
            data.len() as u32,
            attributes,
        )
    };
    if !ok.as_bool() {
        // SAFETY: the error belongs to the call just made.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        return Err(dev(format!("не записать {name}: {err:?}")));
    }
    info!(variable = name, bytes = data.len(), attributes, "efivars: variable written");
    Ok(())
}

/// What happened when we asked the firmware to accept a write.
#[derive(Debug, Clone)]
pub enum WriteProbe {
    /// Writes work: the value went in and came back.
    Works,
    /// The call succeeded and the value did not change. Some boards answer
    /// reads and drop writes; on those, everything past reading is pointless
    /// and has to say so rather than report success.
    Ignored,
    /// The call itself was refused, with its reason.
    Refused(String),
    /// Nothing safe to write to: the probe needs a variable that already
    /// exists, so that putting it back changes nothing.
    NotTested(String),
}

/// Will this firmware accept a write at all?
///
/// Asked by writing `Timeout` back with the value it already holds. Nothing
/// changes if it works, and if the board quietly ignores writes we learn it
/// here instead of after editing a boot entry that then does not move.
pub fn probe_write() -> Result<WriteProbe> {
    let Some((before, attrs)) = read_var_ex("Timeout")? else {
        return Ok(WriteProbe::NotTested(
            "у этой прошивки нет переменной Timeout, а заводить новую ради пробы нельзя".into(),
        ));
    };
    if let Err(e) = write_var("Timeout", &before, attrs) {
        return Ok(WriteProbe::Refused(e.to_string()));
    }
    match read_var_ex("Timeout")? {
        Some((after, _)) if after == before => Ok(WriteProbe::Works),
        Some(_) => Ok(WriteProbe::Ignored),
        None => Ok(WriteProbe::Ignored),
    }
}

fn read_u16_var(name: &str) -> Option<u16> {
    match read_var(name) {
        Ok(Some(b)) if b.len() >= 2 => Some(u16::from_le_bytes([b[0], b[1]])),
        _ => None,
    }
}

/// One boot entry by number.
pub fn read_entry(n: u16) -> Result<Option<LoadOption>> {
    let Some(bytes) = read_var(&efivars::entry_name(n))? else { return Ok(None) };
    Ok(Some(LoadOption::parse(&bytes)?))
}

/// Everything the firmware says about booting, in one pass.
pub fn read_config() -> Result<BootConfig> {
    if !is_uefi() {
        return Err(dev("эта машина загружается через обычный BIOS — записей UEFI у неё нет"));
    }
    if !enable_privilege()? {
        return Err(dev(
            "нет привилегии SeSystemEnvironmentPrivilege — запустите от имени администратора",
        ));
    }

    let order = read_var("BootOrder")?.map(|b| efivars::parse_boot_order(&b)).unwrap_or_default();
    let current = read_u16_var("BootCurrent");
    let next = read_u16_var("BootNext");
    let timeout = read_u16_var("Timeout");

    // Every entry that exists, then the order applied on top. Numbers named by
    // BootOrder are looked up too, so a dangling one is noticed instead of
    // silently missing.
    let (mut numbers, found_by) = match enumerate_names() {
        Ok(ns) if !ns.is_empty() => (ns, "перечисление переменных прошивки"),
        Ok(_) => (scan_numbers(), "перебор Boot0000..Boot00FF (перечисление вернуло пусто)"),
        Err(e) => {
            debug!(error = %e, "efivars: enumeration unavailable, scanning instead");
            (scan_numbers(), "перебор Boot0000..Boot00FF (перечисление недоступно)")
        }
    };
    for n in &order {
        if !numbers.contains(n) {
            numbers.push(*n);
        }
    }
    numbers.sort_unstable();
    numbers.dedup();

    let mut entries = Vec::new();
    let mut dangling = Vec::new();
    for n in numbers {
        match read_entry(n) {
            Ok(Some(option)) => entries.push(BootEntry {
                number: n,
                order_index: order.iter().position(|x| *x == n),
                is_current: current == Some(n),
                is_next: next == Some(n),
                option,
            }),
            Ok(None) => {
                if order.contains(&n) {
                    dangling.push(n);
                }
            }
            // A variable we cannot parse is still a variable; say so and keep
            // going rather than losing the whole list to one bad entry.
            Err(e) => {
                info!(entry = %efivars::entry_name(n), error = %e, "efivars: запись не разобрана");
                if order.contains(&n) {
                    dangling.push(n);
                }
            }
        }
    }
    // Menu order first, then the ones no menu shows.
    entries.sort_by_key(|e| (e.order_index.unwrap_or(usize::MAX), e.number));

    info!(
        entries = entries.len(),
        in_menu = entries.iter().filter(|e| e.order_index.is_some()).count(),
        dangling = dangling.len(),
        found_by,
        "efivars: boot configuration read"
    );
    Ok(BootConfig { entries, order, current, next, timeout, found_by, dangling })
}

/// `Boot0000`..`Boot00FF`, asked one by one.
///
/// Slow and blunt, and it finds everything that exists in practice: firmware
/// hands out entry numbers from zero upwards, and a machine with more than 256
/// of them is not a machine anyone has. Only used when proper enumeration is
/// not available.
fn scan_numbers() -> Vec<u16> {
    (0u16..=0xFF).filter(|n| matches!(read_var(&efivars::entry_name(*n)), Ok(Some(_)))).collect()
}

/// Every `Boot####` variable the firmware holds, by name.
///
/// `NtEnumerateSystemEnvironmentValuesEx` is undocumented but is the only way
/// to get a complete list, and it is what every tool in this area uses. Class 1
/// returns names and namespaces without values, which is all that is needed to
/// know what to ask for next.
fn enumerate_names() -> Result<Vec<u16>> {
    #[link(name = "ntdll")]
    extern "system" {
        fn NtEnumerateSystemEnvironmentValuesEx(
            information_class: u32,
            buffer: *mut core::ffi::c_void,
            length: *mut u32,
        ) -> i32;
    }
    const VARIABLE_NAMES: u32 = 1;
    const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;

    let mut len: u32 = 0;
    // SAFETY: the documented probing form — a null buffer with a zero length
    // asks only for the size.
    let status =
        unsafe { NtEnumerateSystemEnvironmentValuesEx(VARIABLE_NAMES, std::ptr::null_mut(), &mut len) };
    if status != STATUS_BUFFER_TOO_SMALL || len == 0 {
        return Err(dev(format!("перечисление переменных недоступно (0x{status:08X})")));
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: the buffer is as large as the call just asked for.
    let status = unsafe {
        NtEnumerateSystemEnvironmentValuesEx(
            VARIABLE_NAMES,
            buf.as_mut_ptr() as *mut _,
            &mut len,
        )
    };
    if status < 0 {
        return Err(dev(format!("перечисление переменных не удалось (0x{status:08X})")));
    }

    // VARIABLE_NAME: u32 NextEntryOffset, GUID VendorGuid (16 bytes), then the
    // name as UTF-16 to a NUL. NextEntryOffset == 0 ends the list.
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        if at + 20 > buf.len() {
            break;
        }
        let next = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        let name_at = at + 20;
        let units: Vec<u16> = buf[name_at..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        if let Some(n) = efivars::entry_number(&String::from_utf16_lossy(&units)) {
            out.push(n);
        }
        if next == 0 || at + next <= at {
            break;
        }
        at += next;
    }
    Ok(out)
}

// ---- копия и восстановление ------------------------------------------------

/// Everything a restore would need, read out of firmware.
///
/// `BootCurrent` is deliberately not in it: the firmware sets it at every start
/// to say which entry it used, and writing it back would be telling the machine
/// something about a boot that already happened.
pub fn export() -> Result<crate::efivars::BootBackup> {
    use crate::efivars::{SavedNote, SavedVar};

    let cfg = read_config()?;
    let mut vars = Vec::new();
    let mut notes = Vec::new();

    for name in ["BootOrder", "BootNext", "Timeout"] {
        if let Some((data, attrs)) = read_var_ex(name)? {
            vars.push(SavedVar::new(name, attrs, &data));
        }
    }
    let mut removable = 0usize;
    for e in &cfg.entries {
        // A flash drive's entry belongs to the flash drive. It exists while the
        // stick is in, firmware makes a new one next time, and restored onto
        // another machine it points at nothing — so the copy of the machine's
        // menu does not carry it, and nothing shows it later.
        if e.option.is_removable_device() {
            removable += 1;
            debug!(
                entry = %crate::efivars::entry_name(e.number),
                name = %e.option.description,
                "efivars: removable-device entry left out of the copy"
            );
            continue;
        }
        let name = crate::efivars::entry_name(e.number);
        // The bytes as the firmware holds them, not as we would rebuild them.
        if let Some((data, attrs)) = read_var_ex(&name)? {
            vars.push(SavedVar::new(&name, attrs, &data));
        }
        notes.push(SavedNote {
            name,
            description: e.option.description.clone(),
            file: e.option.file_path(),
            partition_guid: e.option.hard_drive().and_then(|h| h.partition_guid()),
        });
    }

    let machine = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "неизвестно".into());
    info!(
        variables = vars.len(),
        entries = notes.len(),
        removable,
        "efivars: boot configuration copied"
    );
    Ok(crate::efivars::BootBackup::new(&machine, vars, notes))
}

/// What restoring one variable would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The firmware already holds exactly these bytes.
    Same,
    /// It holds something else.
    Differs,
    /// It holds nothing under that name.
    Absent,
}

/// Variable by variable, what a restore from this copy would change. Read only:
/// this is what gets shown before anybody is asked to agree to it.
pub fn plan_import(backup: &crate::efivars::BootBackup) -> Result<Vec<(String, Change)>> {
    // Even reading needs the privilege, and it is per PROCESS, not per call:
    // showing what a restore would do failed with ERROR_PRIVILEGE_NOT_HELD
    // until this line existed (2026-09-18), while the very same read inside
    // `read_config` worked — because that one enables it first.
    if !enable_privilege()? {
        return Err(dev("нет привилегии SeSystemEnvironmentPrivilege — нужны права администратора"));
    }
    let mut out = Vec::new();
    for v in &backup.variables {
        // Not offered, because it will not be written: see `apply_import`.
        if crate::efivars::names_removable_device(v) {
            continue;
        }
        let want = v.bytes()?;
        let change = match read_var_ex(&v.name)? {
            Some((have, _)) if have == want => Change::Same,
            Some(_) => Change::Differs,
            None => Change::Absent,
        };
        out.push((v.name.clone(), change));
    }
    Ok(out)
}

/// Put the copy back. Returns how many variables were actually written.
///
/// Only what differs is written. A firmware's variable store has a limited
/// number of write cycles and a limited size, and rewriting a dozen identical
/// variables to report a bigger number would be spending both for nothing.
/// The partition this entry points at, when it points at one by GUID.
///
/// `None` for anything else — an entry that names a device some other way, or
/// no device at all (`BootOrder`, `Timeout`). Those are written back as before:
/// the check below only refuses what it can positively show is missing.
fn entry_partition_guid(v: &crate::efivars::SavedVar) -> Option<String> {
    let bytes = v.bytes().ok()?;
    crate::efivars::LoadOption::parse(&bytes)
        .ok()?
        .hard_drive()?
        .partition_guid()
        .map(|g| g.to_uppercase())
}

/// Every GPT partition GUID this machine currently has.
///
/// An EMPTY set means "could not be determined", and the caller treats it as
/// such — writing nothing back because no disk answered would be worse than the
/// problem being solved. Disk numbers are probed rather than enumerated: this
/// runs on boot media too, where an inventory service may not be there at all.
fn partition_guids_on_this_machine() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for disk in 0u32..32 {
        let Ok(layout) = crate::gpt_reader::read_disk_layout(disk) else {
            continue;
        };
        for p in &layout.partitions {
            if !p.part_guid.is_empty() {
                out.insert(p.part_guid.to_uppercase());
            }
        }
    }
    if out.is_empty() {
        warn!("efivars: no GPT partition could be read on this machine — orphan check skipped");
    }
    out
}

pub fn apply_import(backup: &crate::efivars::BootBackup) -> Result<usize> {
    if !enable_privilege()? {
        return Err(dev("нет привилегии SeSystemEnvironmentPrivilege — нужны права администратора"));
    }
    let plan = plan_import(backup)?;
    let here = partition_guids_on_this_machine();
    let mut written = 0;
    let mut orphaned: Vec<String> = Vec::new();
    // BootOrder last: until the entries it names exist, it points at nothing.
    // The firmware tolerates that, but a machine interrupted mid-restore should
    // be left with entries and a stale order rather than an order and no
    // entries.
    let mut order_last: Option<&crate::efivars::SavedVar> = None;
    for v in &backup.variables {
        if v.name == "BootOrder" {
            order_last = Some(v);
            continue;
        }
        // A copy taken before flash drives were left out still carries them;
        // writing one back would put a stick that is not in this machine into
        // its boot menu. `BootOrder` may still name it — firmware skips a
        // number with no entry behind it, and rewriting the order to match
        // would be editing what the user asked to have back.
        if crate::efivars::names_removable_device(v) {
            debug!(variable = %v.name, "efivars: removable-device entry not written back");
            continue;
        }
        // An entry names its partition by GUID. On a machine that does not have
        // that partition the firmware does NOT simply show a dead menu item: it
        // tries to boot it, fails to find the partition, and then looks for a
        // file of the same name on any device it can see. The names there are
        // `bootx64.efi`, so the machine starts booting off whatever stick
        // happens to be plugged in (seen 2026-09-20: five orphaned entries
        // walked the boot past the chosen device). Warning about it in text was
        // not enough — this refuses to write it.
        if let Some(guid) = entry_partition_guid(v) {
            if !here.is_empty() && !here.contains(&guid) {
                orphaned.push(format!("{} → раздел {guid}", v.name));
                continue;
            }
        }
        if plan.iter().any(|(n, c)| n == &v.name && *c == Change::Same) {
            continue;
        }
        write_var(&v.name, &v.bytes()?, v.attributes)?;
        written += 1;
    }
    if let Some(v) = order_last {
        if !plan.iter().any(|(n, c)| n == &v.name && *c == Change::Same) {
            write_var(&v.name, &v.bytes()?, v.attributes)?;
            written += 1;
        }
    }
    let offered =
        backup.variables.iter().filter(|v| !crate::efivars::names_removable_device(v)).count();
    if !orphaned.is_empty() {
        warn!(
            skipped = orphaned.len(),
            entries = %orphaned.join("; "),
            "efivars: entries naming partitions this machine does not have were NOT written back"
        );
    }
    info!(
        written,
        of = offered,
        skipped_removable = backup.variables.len() - offered,
        skipped_orphaned = orphaned.len(),
        "efivars: boot configuration restored"
    );
    Ok(written)
}

// ---- правка --------------------------------------------------------------

/// Delete a variable. UEFI spells that "write it with no data", which is also
/// why [`write_var`] refuses an empty value: the difference between saving an
/// entry and erasing it must never be a length that happened to come out zero.
pub fn delete_var(name: &str) -> Result<()> {
    if !crate::efivars::is_boot_variable(name) {
        return Err(dev(format!("{name}: не переменная загрузки")));
    }
    ensure_privilege();
    let (name_w, guid) = (wide(name), wide(EFI_GLOBAL));
    // SAFETY: both strings are NUL-terminated and outlive the call; a null
    // value with zero length is the documented way to remove a variable.
    let ok = unsafe {
        SetFirmwareEnvironmentVariableExW(
            name_w.as_ptr(),
            guid.as_ptr(),
            std::ptr::null(),
            0,
            BOOT_VARIABLE_ATTRIBUTES,
        )
    };
    if !ok.as_bool() {
        // SAFETY: the error belongs to the call just made.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        return Err(dev(format!("не удалить {name}: {err:?}")));
    }
    info!(variable = name, "efivars: variable deleted");
    Ok(())
}

/// The copy taken before a change, and whether it will still be there
/// afterwards.
#[derive(Debug, Clone)]
pub struct SafetyCopy {
    pub path: std::path::PathBuf,
    /// The file is on storage that survives a restart.
    ///
    /// On the boot medium it may not be. WinPE runs from a RAM disk, and a copy
    /// written there dies in the very restart it exists to survive — which is
    /// worth saying out loud rather than discovering afterwards.
    pub durable: bool,
}

/// Where the automatic copy before a change is kept.
///
/// On the boot medium, the writable part of the medium itself, beside the logs
/// that already go there. `OWN_BACKUP_LOG_MIRROR` is set by the medium's start
/// script only after it has proved it can write there, so its presence is the
/// answer to "is there anywhere durable" and not a guess.
pub fn safety_copy_dir() -> (std::path::PathBuf, bool) {
    if let Ok(mirror) = std::env::var(own_backup_common::log_mirror::LOG_MIRROR_ENV) {
        if !mirror.trim().is_empty() {
            let dir = std::path::Path::new(&mirror);
            // The mirror is `<medium>\OwnBackup\logs`; copies of the boot
            // configuration are not logs, so they go beside it.
            let base = dir.parent().unwrap_or(dir);
            return (base.join("boot"), true);
        }
    }
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    // In WinPE `ProgramData` is on the RAM disk: the directory exists, the file
    // will be written, and none of it outlives the restart.
    let durable = !own_backup_common::media_mode::is_windows_pe();
    (std::path::Path::new(&base).join("OwnBackup").join("boot"), durable)
}

/// Copy the whole boot configuration to a file named by the second.
///
/// Rule one of the plan: taken before every change, without asking and without
/// a way to decline. A change to firmware has no undo of its own, and the file
/// costs a few kilobytes.
///
/// A copy that will not survive a restart is still taken. Refusing to act
/// without durable storage would block exactly the case this whole section
/// exists for — a machine that does not start, being repaired from a read-only
/// medium. The caller is told, and decides.
pub fn safety_copy() -> Result<SafetyCopy> {
    let (dir, durable) = safety_copy_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("boot-{}.json", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    std::fs::write(&path, export()?.to_json()?)?;
    info!(path = %path.display(), durable, "efivars: safety copy taken");
    Ok(SafetyCopy { path, durable })
}

/// Write one entry, having first checked that what we assembled reads back as
/// what we meant.
///
/// Rule three of the plan. The bytes go to firmware, and a firmware does not
/// validate them for us: an entry with a length field one byte out is accepted,
/// stored, and then either ignored or — on some boards — makes the whole menu
/// unreadable. Parsing our own output before writing costs nothing and is the
/// only check available.
pub fn write_entry(n: u16, option: &crate::efivars::LoadOption) -> Result<()> {
    let bytes = option.to_bytes();
    let back = crate::efivars::LoadOption::parse(&bytes)
        .map_err(|e| dev(format!("собранная запись не читается обратно: {e} — не пишем")))?;
    if &back != option {
        return Err(dev("собранная запись читается иначе, чем задумана — не пишем"));
    }
    let attributes = read_var_ex(&crate::efivars::entry_name(n))?
        .map(|(_, a)| a)
        .unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
    write_var(&crate::efivars::entry_name(n), &bytes, attributes)
}

/// Read one entry, refusing the ones we are not entitled to rewrite.
///
/// Rule four: an entry whose device path we did not fully understand is shown
/// and left alone. Re-serialising bytes we could not read would be replacing
/// the firmware's data with our opinion of it.
fn entry_for_edit(n: u16) -> Result<crate::efivars::LoadOption> {
    let name = crate::efivars::entry_name(n);
    let Some(option) = read_entry(n)? else {
        return Err(dev(format!("записи {name} нет")));
    };
    if !option.fully_understood() {
        return Err(dev(format!(
            "{name}: её путь к устройству разобран не полностью — такую запись мы не переписываем"
        )));
    }
    Ok(option)
}

/// Give an entry another name. Everything else about it travels through
/// untouched.
pub fn set_description(n: u16, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(dev("пустое название — в меню его будет не отличить от соседей"));
    }
    let mut option = entry_for_edit(n)?;
    option.description = name.to_string();
    write_entry(n, &option)
}

/// Offer the entry in the menu, or stop offering it. The entry keeps its place
/// in the order either way — that is what a firmware means by "disabled".
pub fn set_active(n: u16, active: bool) -> Result<()> {
    let mut option = entry_for_edit(n)?;
    if active {
        option.attributes |= crate::efivars::LOAD_OPTION_ACTIVE;
    } else {
        option.attributes &= !crate::efivars::LOAD_OPTION_ACTIVE;
    }
    write_entry(n, &option)
}

/// Replace the menu order.
pub fn set_order(order: &[u16]) -> Result<()> {
    let attributes =
        read_var_ex("BootOrder")?.map(|(_, a)| a).unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
    write_var("BootOrder", &crate::efivars::serialize_boot_order(order), attributes)
}

/// Take an entry out of the menu and out of the firmware.
///
/// The order is rewritten first: a number left in `BootOrder` with no variable
/// behind it is an entry the firmware offers and cannot load.
pub fn delete_entry(n: u16) -> Result<()> {
    let order = read_var("BootOrder")?.map(|b| crate::efivars::parse_boot_order(&b)).unwrap_or_default();
    if order.contains(&n) {
        let kept: Vec<u16> = order.into_iter().filter(|x| *x != n).collect();
        set_order(&kept)?;
    }
    delete_var(&crate::efivars::entry_name(n))
}

/// Add an entry for a loader that is already on a partition, and put it in the
/// menu at `position` (`None` = last).
///
/// Returns the number it was given.
pub fn create_entry(
    description: &str,
    hd: &crate::efivars::HardDrive,
    file: &str,
    position: Option<usize>,
) -> Result<u16> {
    let cfg = read_config()?;
    let used: Vec<u16> = cfg.entries.iter().map(|e| e.number).collect();
    let n = crate::efivars::free_entry_number(&used)
        .ok_or_else(|| dev("в прошивке не осталось свободных номеров записей"))?;

    let option = crate::efivars::build_entry(description, hd, file);
    write_entry(n, &option)?;

    let at = position.unwrap_or(cfg.order.len());
    set_order(&crate::efivars::reorder(&cfg.order, n, at))?;
    info!(entry = %crate::efivars::entry_name(n), description, file, "efivars: entry created");
    Ok(n)
}

/// Boot this entry once, at the next start only. The firmware clears the
/// variable itself after using it.
pub fn set_boot_next(n: Option<u16>) -> Result<()> {
    match n {
        Some(n) => write_var("BootNext", &n.to_le_bytes(), BOOT_VARIABLE_ATTRIBUTES),
        None => match read_var("BootNext")? {
            Some(_) => delete_var("BootNext"),
            None => Ok(()),
        },
    }
}

/// How long the firmware shows its menu.
pub fn set_timeout(secs: u16) -> Result<()> {
    let attributes = read_var_ex("Timeout")?.map(|(_, a)| a).unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
    write_var("Timeout", &secs.to_le_bytes(), attributes)
}

// ---- одна дверь для всякой правки ------------------------------------------

/// What someone asked to change.
#[derive(Debug, Clone)]
pub enum Action {
    Rename { entry: u16, name: String },
    SetActive { entry: u16, on: bool, force: bool },
    Move { entry: u16, to: usize },
    Delete { entry: u16, force: bool },
    Create { name: String, disk: u32, partition: u32, file: String, position: Option<usize> },
    BootNext { entry: Option<u16> },
    Timeout { secs: u16 },
}

/// What it did.
#[derive(Debug, Clone)]
pub struct Done {
    pub message: String,
    pub safety_copy: SafetyCopy,
    /// The number a newly created entry was given.
    pub created: Option<u16>,
}

/// Every change to the boot configuration goes through here.
///
/// Not because it is tidy — because the two rules that matter cannot be left to
/// callers. The copy is taken before anything is written, and the entry this
/// machine booted from is not removed or hidden without someone saying so out
/// loud. A second caller (the interface, beside the command line) would
/// otherwise have to remember both, and the cost of forgetting is a machine
/// that does not start.
pub fn apply_action(action: Action) -> Result<Done> {
    // Rule two, before anything else happens.
    if let Some((entry, force, verb)) = match &action {
        Action::Delete { entry, force } => Some((*entry, *force, "Удалить")),
        Action::SetActive { entry, on: false, force } => Some((*entry, *force, "Убрать из меню")),
        _ => None,
    } {
        if read_config()?.current == Some(entry) && !force {
            return Err(dev(format!(
                "запись {entry:04X} — та, с которой эта машина загружена сейчас. {verb} её без \
                 другой рабочей записи в меню означает машину, которая не стартует"
            )));
        }
    }

    // Rule one. A change that could not be undone is not made.
    let safety_copy = safety_copy().map_err(|e| {
        dev(format!("не удалось сохранить копию записей загрузки ({e}) — без неё менять их нельзя"))
    })?;

    let mut created = None;
    let message = match action {
        Action::Rename { entry, name } => {
            if name.trim().is_empty() {
                return Err(dev("пустое название — в меню его будет не отличить от соседей"));
            }
            let was = read_entry(entry)?.map(|o| o.description).unwrap_or_default();
            set_description(entry, &name)?;
            format!("{entry:04X}: «{was}» → «{name}»")
        }
        Action::SetActive { entry, on, .. } => {
            set_active(entry, on)?;
            format!("{entry:04X}: {}", if on { "показывается в меню" } else { "убрана из меню" })
        }
        Action::Move { entry, to } => {
            let order = read_config()?.order;
            let new_order = crate::efivars::reorder(&order, entry, to);
            set_order(&new_order)?;
            format!(
                "Порядок: {}",
                new_order.iter().map(|x| format!("{x:04X}")).collect::<Vec<_>>().join(" → ")
            )
        }
        Action::Delete { entry, .. } => {
            let was = read_entry(entry)?.map(|o| o.description).unwrap_or_default();
            delete_entry(entry)?;
            format!("Запись {entry:04X} «{was}» удалена. Сам загрузчик остался на разделе.")
        }
        Action::Create { name, disk, partition, file, position } => {
            let layout = crate::gpt_reader::read_disk_layout(disk)?;
            let part = layout
                .partitions
                .iter()
                .find(|p| p.index == partition)
                .ok_or_else(|| dev(format!("на диске {disk} нет раздела {partition}")))?;
            let hd = crate::efivars::HardDrive::from_gpt(
                part.index,
                part.first_lba,
                part.last_lba,
                &part.part_guid,
            )
            .ok_or_else(|| dev(format!("не разобрать GUID раздела: {}", part.part_guid)))?;
            // Firmware spells paths with backslashes and from the root.
            let file = file.replace('/', "\\");
            let file = if file.starts_with('\\') { file } else { format!("\\{file}") };
            let n = create_entry(&name, &hd, &file, position)?;
            created = Some(n);
            format!("Создана запись {n:04X} «{name}» → {file}")
        }
        Action::BootNext { entry } => {
            set_boot_next(entry)?;
            match entry {
                Some(n) => format!("Следующая загрузка — с записи {n:04X}, один раз"),
                None => "Однократная загрузка убрана".to_string(),
            }
        }
        Action::Timeout { secs } => {
            set_timeout(secs)?;
            format!("Меню платы будет показываться {secs} с")
        }
    };
    Ok(Done { message, safety_copy, created })
}
