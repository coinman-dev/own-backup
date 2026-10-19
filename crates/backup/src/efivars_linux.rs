//! The machine's boot configuration through efivarfs — the Linux half of
//! [`crate::efivars`], beside [`crate::efivars_win`].
//!
//! Linux exposes every UEFI variable as a file in
//! `/sys/firmware/efi/efivars`, named `<Name>-<vendor GUID>`. A file's content
//! is the variable's 4-byte attribute word followed by its data, and the same
//! layout is what a write expects. Three things make that less simple than it
//! looks:
//!
//! - **One `write()` per variable.** efivarfs hands each write call to the
//!   firmware as a complete `SetVariable`, so the attributes and the data have
//!   to travel in a single buffer. A short write is an error, not progress.
//! - **Immutable files.** The kernel marks a variable immutable (`chattr +i`)
//!   unless it is on its list of variables safe to remove, so that a stray
//!   `rm -rf /sys` cannot brick a machine. `Boot####`, `BootOrder`, `BootNext`
//!   and `Timeout` are on that list, but some kernels and distributions are
//!   stricter; the flag is cleared just before a write or delete and only for
//!   the boot variables this module is allowed to touch (efibootmgr does the
//!   same).
//! - **No truncation.** efivarfs replaces a variable on write and does not
//!   implement truncation, so the file is opened without `O_TRUNC`. That only
//!   holds on efivarfs itself: the test root is an ordinary directory, where an
//!   untruncated rewrite would leave the tail of a longer old value behind, so
//!   there the file is truncated.
//!
//! Everything is rooted at a configurable directory: tests run the whole module
//! — reading the configuration, creating and deleting entries, exporting and
//! importing a copy — against a fake root, without firmware and without root.

#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use crate::efivars::{self, BootBackup, HardDrive, LoadOption, SavedNote, SavedVar};

/// The vendor GUID of the boot variables (EFI Global Variable), as efivarfs
/// spells it in file names.
pub const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

pub const EFI_VARIABLE_NON_VOLATILE: u32 = 0x01;
pub const EFI_VARIABLE_BOOTSERVICE_ACCESS: u32 = 0x02;
pub const EFI_VARIABLE_RUNTIME_ACCESS: u32 = 0x04;
/// Attributes a new boot variable is created with (see `efivars_win`).
pub const BOOT_VARIABLE_ATTRIBUTES: u32 =
    EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;

const EFIVARFS_MAGIC: i64 = 0xde5e_81e4;
// <linux/fs.h>: FS_IOC_GETFLAGS / FS_IOC_SETFLAGS take a long-sized pointer
// on 64-bit, but the kernel reads and writes an int.
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
const FS_IMMUTABLE_FL: libc::c_int = 0x10;

fn dev(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(msg.to_string())
}

/// One entry, with what the rest of the firmware's state says about it.
#[derive(Debug, Clone)]
pub struct BootEntry {
    pub number: u16,
    pub option: LoadOption,
    /// Position in `BootOrder`; `None` = no menu shows it.
    pub order_index: Option<usize>,
    pub is_current: bool,
    pub is_next: bool,
}

/// Everything read in one pass.
#[derive(Debug, Clone)]
pub struct BootConfig {
    /// Menu order first, then the entries no menu shows.
    pub entries: Vec<BootEntry>,
    pub order: Vec<u16>,
    pub current: Option<u16>,
    pub next: Option<u16>,
    pub timeout: Option<u16>,
    /// Numbers named by `BootOrder` with no (readable) variable behind them.
    pub dangling: Vec<u16>,
}

/// What restoring one variable would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Same,
    Differs,
    Absent,
}

/// What [`Efivarfs::apply_import`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imported {
    pub written: usize,
    /// Entries left out because they name a removable device.
    pub skipped_removable: usize,
    /// Entries left out because they name a partition this machine lacks.
    pub skipped_orphaned: Vec<String>,
}

/// The efivarfs directory of a system.
#[derive(Debug, Clone)]
pub struct Efivarfs {
    /// `/sys/firmware/efi`.
    firmware: PathBuf,
    /// `/sys/firmware/efi/efivars`.
    vars: PathBuf,
}

impl Default for Efivarfs {
    fn default() -> Self {
        Self::rooted(Path::new("/"))
    }
}

impl Efivarfs {
    /// The efivarfs of a system whose root is `root`.
    pub fn rooted(root: &Path) -> Self {
        let firmware = root.join("sys/firmware/efi");
        Self {
            vars: firmware.join("efivars"),
            firmware,
        }
    }

    /// Booted through UEFI: the kernel publishes `/sys/firmware/efi` only then.
    pub fn is_uefi(&self) -> bool {
        self.firmware.is_dir()
    }

    /// UEFI, and efivarfs is there to talk to it. A UEFI machine with efivarfs
    /// not mounted (some containers, `noefi`) is not a BIOS machine; it is one
    /// whose boot entries we cannot see, and the two are reported differently.
    fn ready(&self) -> Result<()> {
        if !self.is_uefi() {
            return Err(dev(tr!("efivars_win.legacy_bios")));
        }
        let mounted = std::fs::read_dir(&self.vars)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if !mounted {
            return Err(dev(tr!(
                "efivars_linux.not_mounted",
                path = self.vars.display()
            )));
        }
        Ok(())
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.vars.join(format!("{name}-{EFI_GLOBAL_GUID}"))
    }

    fn on_efivarfs(&self) -> bool {
        let Ok(c) = std::ffi::CString::new(self.vars.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        // SAFETY: zeroed statfs is a valid out-parameter; `c` is NUL-terminated.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return false;
        }
        st.f_type as i64 == EFIVARFS_MAGIC
    }

    /// Read a global variable: `(data, attributes)`, or `None` if absent.
    pub fn read_var_ex(&self, name: &str) -> Result<Option<(Vec<u8>, u32)>> {
        let path = self.path_of(name);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(dev(tr!(
                    "efivars_linux.read_failed",
                    name = name,
                    error = e
                )))
            }
        };
        if raw.len() < 4 {
            return Err(dev(tr!(
                "efivars_linux.read_failed",
                name = name,
                error = format!("{} bytes", raw.len())
            )));
        }
        let attributes = u32::from_le_bytes(raw[..4].try_into().unwrap());
        Ok(Some((raw[4..].to_vec(), attributes)))
    }

    pub fn read_var(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.read_var_ex(name)?.map(|(d, _)| d))
    }

    fn read_u16(&self, name: &str) -> Option<u16> {
        match self.read_var(name) {
            Ok(Some(b)) if b.len() >= 2 => Some(u16::from_le_bytes([b[0], b[1]])),
            _ => None,
        }
    }

    /// Clear the immutable flag the kernel may have set, so the variable can
    /// be rewritten or removed. Best effort: a filesystem without the flag
    /// (the test root on tmpfs) answers `ENOTTY`, and a real failure shows up
    /// in the write that follows.
    fn make_mutable(&self, path: &Path) {
        use std::os::unix::io::AsRawFd;
        let Ok(f) = std::fs::File::open(path) else {
            return;
        };
        let mut flags: libc::c_int = 0;
        // SAFETY: the kernel writes an int through the pointer.
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } != 0 {
            return;
        }
        if flags & FS_IMMUTABLE_FL == 0 {
            return;
        }
        flags &= !FS_IMMUTABLE_FL;
        // SAFETY: as above, reading the int.
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS, &flags) } != 0 {
            warn!(path = %path.display(), error = %std::io::Error::last_os_error(),
                "efivars: could not clear the immutable flag");
        } else {
            debug!(path = %path.display(), "efivars: immutable flag cleared");
        }
    }

    /// Write a boot variable. Only boot variables, never an empty value (that
    /// is how UEFI spells "delete" — [`Self::delete_var`] asks for it by name).
    pub fn write_var(&self, name: &str, data: &[u8], attributes: u32) -> Result<()> {
        if !efivars::is_boot_variable(name) {
            return Err(dev(tr!("efivars_win.write_not_boot_variable", name = name)));
        }
        if data.is_empty() {
            return Err(dev(tr!("efivars_win.empty_value", name = name)));
        }
        let path = self.path_of(name);
        if path.exists() {
            self.make_mutable(&path);
        }
        let mut buf = Vec::with_capacity(4 + data.len());
        buf.extend_from_slice(&attributes.to_le_bytes());
        buf.extend_from_slice(data);

        let mut opts = OpenOptions::new();
        opts.write(true).create(true).mode(0o644);
        if !self.on_efivarfs() {
            opts.truncate(true);
        }
        let failed = |e: &dyn std::fmt::Display| {
            dev(tr!(
                "efivars_win.write_failed",
                name = name,
                error = e.to_string()
            ))
        };
        let mut f = opts.open(&path).map_err(|e| failed(&e))?;
        let n = f.write(&buf).map_err(|e| failed(&e))?;
        if n != buf.len() {
            return Err(failed(&tr!(
                "efivars_linux.write_short",
                written = n,
                total = buf.len()
            )));
        }
        info!(
            variable = name,
            bytes = data.len(),
            attributes,
            "efivars: variable written"
        );
        Ok(())
    }

    /// Delete a boot variable.
    pub fn delete_var(&self, name: &str) -> Result<()> {
        if !efivars::is_boot_variable(name) {
            return Err(dev(tr!("efivars_win.not_boot_variable", name = name)));
        }
        let path = self.path_of(name);
        self.make_mutable(&path);
        std::fs::remove_file(&path).map_err(|e| {
            dev(tr!(
                "efivars_win.delete_failed",
                name = name,
                error = e.to_string()
            ))
        })?;
        info!(variable = name, "efivars: variable deleted");
        Ok(())
    }

    /// Every `Boot####` variable present, by number. efivarfs lists them all,
    /// so there is no need for the Windows probe of `Boot0000..Boot00FF`.
    pub fn entry_numbers(&self) -> Result<Vec<u16>> {
        let suffix = format!("-{EFI_GLOBAL_GUID}");
        let dir = std::fs::read_dir(&self.vars).map_err(|e| {
            dev(tr!(
                "efivars_linux.read_failed",
                name = self.vars.display(),
                error = e
            ))
        })?;
        let mut out: Vec<u16> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                efivars::entry_number(name.strip_suffix(&suffix)?)
            })
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    pub fn read_entry(&self, n: u16) -> Result<Option<LoadOption>> {
        let Some(bytes) = self.read_var(&efivars::entry_name(n))? else {
            return Ok(None);
        };
        Ok(Some(LoadOption::parse(&bytes)?))
    }

    /// Everything the firmware says about booting, in one pass.
    pub fn read_config(&self) -> Result<BootConfig> {
        self.ready()?;
        let order = self
            .read_var("BootOrder")?
            .map(|b| efivars::parse_boot_order(&b))
            .unwrap_or_default();
        let current = self.read_u16("BootCurrent");
        let next = self.read_u16("BootNext");
        let timeout = self.read_u16("Timeout");

        let mut numbers = self.entry_numbers()?;
        numbers.extend(order.iter().copied());
        numbers.sort_unstable();
        numbers.dedup();

        let mut entries = Vec::new();
        let mut dangling = Vec::new();
        for n in numbers {
            match self.read_entry(n) {
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
                Err(e) => {
                    info!(entry = %efivars::entry_name(n), error = %e, "efivars: entry not parsed");
                    if order.contains(&n) {
                        dangling.push(n);
                    }
                }
            }
        }
        entries.sort_by_key(|e| (e.order_index.unwrap_or(usize::MAX), e.number));
        info!(
            entries = entries.len(),
            in_menu = entries.iter().filter(|e| e.order_index.is_some()).count(),
            dangling = dangling.len(),
            "efivars: boot configuration read"
        );
        Ok(BootConfig {
            entries,
            order,
            current,
            next,
            timeout,
            dangling,
        })
    }

    /// A copy of the boot configuration, in the format `efivars_win` writes —
    /// a copy taken on either system restores on the other. Removable-device
    /// entries and `BootCurrent` are left out for the same reasons.
    pub fn export(&self, machine: &str) -> Result<BootBackup> {
        let cfg = self.read_config()?;
        let mut vars = Vec::new();
        let mut notes = Vec::new();
        for name in ["BootOrder", "BootNext", "Timeout"] {
            if let Some((data, attrs)) = self.read_var_ex(name)? {
                vars.push(SavedVar::new(name, attrs, &data));
            }
        }
        for e in &cfg.entries {
            if e.option.is_removable_device() {
                continue;
            }
            let name = efivars::entry_name(e.number);
            if let Some((data, attrs)) = self.read_var_ex(&name)? {
                vars.push(SavedVar::new(&name, attrs, &data));
            }
            notes.push(SavedNote {
                name,
                description: e.option.description.clone(),
                file: e.option.file_path(),
                partition_guid: e.option.hard_drive().and_then(|h| h.partition_guid()),
            });
        }
        Ok(BootBackup::new(machine, vars, notes))
    }

    /// Variable by variable, what a restore from `backup` would change.
    pub fn plan_import(&self, backup: &BootBackup) -> Result<Vec<(String, Change)>> {
        let mut out = Vec::new();
        for v in &backup.variables {
            if efivars::names_removable_device(v) {
                continue;
            }
            let want = v.bytes()?;
            let change = match self.read_var_ex(&v.name)? {
                Some((have, _)) if have == want => Change::Same,
                Some(_) => Change::Differs,
                None => Change::Absent,
            };
            out.push((v.name.clone(), change));
        }
        Ok(out)
    }

    /// Put a copy back, writing only what differs, `BootOrder` last.
    ///
    /// `partitions_here` is every GPT partition GUID this machine has
    /// (uppercase; see [`partition_guids_here`]). An entry naming a partition
    /// outside it is refused, as on Windows: firmware that cannot find the
    /// partition falls back to booting `bootx64.efi` off whatever device it
    /// sees. An empty set means "unknown" and the check is skipped.
    pub fn apply_import(
        &self,
        backup: &BootBackup,
        partitions_here: &std::collections::HashSet<String>,
    ) -> Result<Imported> {
        self.ready()?;
        let plan = self.plan_import(backup)?;
        let unchanged = |name: &str| plan.iter().any(|(n, c)| n == name && *c == Change::Same);
        let mut done = Imported::default();
        let mut order_last = None;
        for v in &backup.variables {
            if v.name == "BootOrder" {
                order_last = Some(v);
                continue;
            }
            if efivars::names_removable_device(v) {
                done.skipped_removable += 1;
                continue;
            }
            let guid = v
                .bytes()
                .ok()
                .and_then(|b| LoadOption::parse(&b).ok())
                .and_then(|o| o.hard_drive())
                .and_then(|h| h.partition_guid())
                .map(|g| g.to_uppercase());
            if let Some(guid) = guid {
                if !partitions_here.is_empty() && !partitions_here.contains(&guid) {
                    done.skipped_orphaned.push(format!("{} → {guid}", v.name));
                    continue;
                }
            }
            if unchanged(&v.name) {
                continue;
            }
            self.write_var(&v.name, &v.bytes()?, v.attributes)?;
            done.written += 1;
        }
        if let Some(v) = order_last {
            if !unchanged(&v.name) {
                self.write_var(&v.name, &v.bytes()?, v.attributes)?;
                done.written += 1;
            }
        }
        if !done.skipped_orphaned.is_empty() {
            warn!(
                entries = %done.skipped_orphaned.join("; "),
                "efivars: entries naming partitions this machine does not have were NOT written back"
            );
        }
        info!(
            written = done.written,
            skipped_removable = done.skipped_removable,
            skipped_orphaned = done.skipped_orphaned.len(),
            "efivars: boot configuration restored"
        );
        Ok(done)
    }

    /// Write one entry after checking that it reads back as meant.
    pub fn write_entry(&self, n: u16, option: &LoadOption) -> Result<()> {
        let bytes = option.to_bytes();
        let back = LoadOption::parse(&bytes)
            .map_err(|e| dev(tr!("efivars_win.readback_failed", error = e)))?;
        if &back != option {
            return Err(dev(tr!("efivars_win.readback_differs")));
        }
        let name = efivars::entry_name(n);
        let attributes = self
            .read_var_ex(&name)?
            .map(|(_, a)| a)
            .unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
        self.write_var(&name, &bytes, attributes)
    }

    /// Read one entry, refusing those whose device path we did not understand.
    fn entry_for_edit(&self, n: u16) -> Result<LoadOption> {
        let name = efivars::entry_name(n);
        let Some(option) = self.read_entry(n)? else {
            return Err(dev(tr!("efivars_win.no_such_entry", name = name)));
        };
        if !option.fully_understood() {
            return Err(dev(tr!("efivars_win.not_fully_understood", name = name)));
        }
        Ok(option)
    }

    pub fn set_description(&self, n: u16, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(dev(tr!("efivars_win.empty_name")));
        }
        let mut option = self.entry_for_edit(n)?;
        option.description = name.to_string();
        self.write_entry(n, &option)
    }

    pub fn set_active(&self, n: u16, active: bool) -> Result<()> {
        let mut option = self.entry_for_edit(n)?;
        if active {
            option.attributes |= efivars::LOAD_OPTION_ACTIVE;
        } else {
            option.attributes &= !efivars::LOAD_OPTION_ACTIVE;
        }
        self.write_entry(n, &option)
    }

    pub fn set_order(&self, order: &[u16]) -> Result<()> {
        let attributes = self
            .read_var_ex("BootOrder")?
            .map(|(_, a)| a)
            .unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
        self.write_var(
            "BootOrder",
            &efivars::serialize_boot_order(order),
            attributes,
        )
    }

    /// Remove an entry: out of `BootOrder` first, then the variable.
    pub fn delete_entry(&self, n: u16) -> Result<()> {
        let order = self
            .read_var("BootOrder")?
            .map(|b| efivars::parse_boot_order(&b))
            .unwrap_or_default();
        if order.contains(&n) {
            let kept: Vec<u16> = order.into_iter().filter(|x| *x != n).collect();
            self.set_order(&kept)?;
        }
        self.delete_var(&efivars::entry_name(n))
    }

    /// Add an entry for a loader already on a partition, at `position` in the
    /// menu (`None` = last). Returns its number.
    pub fn create_entry(
        &self,
        description: &str,
        hd: &HardDrive,
        file: &str,
        position: Option<usize>,
    ) -> Result<u16> {
        let cfg = self.read_config()?;
        let used: Vec<u16> = cfg.entries.iter().map(|e| e.number).collect();
        let n = efivars::free_entry_number(&used)
            .ok_or_else(|| dev(tr!("efivars_win.no_free_numbers")))?;
        let option = efivars::build_entry(description, hd, file);
        self.write_entry(n, &option)?;
        let at = position.unwrap_or(cfg.order.len());
        self.set_order(&efivars::reorder(&cfg.order, n, at))?;
        info!(entry = %efivars::entry_name(n), description, file, "efivars: entry created");
        Ok(n)
    }

    pub fn set_boot_next(&self, n: Option<u16>) -> Result<()> {
        match n {
            Some(n) => self.write_var("BootNext", &n.to_le_bytes(), BOOT_VARIABLE_ATTRIBUTES),
            None => match self.read_var("BootNext")? {
                Some(_) => self.delete_var("BootNext"),
                None => Ok(()),
            },
        }
    }

    pub fn set_timeout(&self, secs: u16) -> Result<()> {
        let attributes = self
            .read_var_ex("Timeout")?
            .map(|(_, a)| a)
            .unwrap_or(BOOT_VARIABLE_ATTRIBUTES);
        self.write_var("Timeout", &secs.to_le_bytes(), attributes)
    }
}

/// Every GPT partition GUID the kernel knows (`PARTUUID` of each partition in
/// `<sys>/class/block`), uppercase like the Windows side. Includes MBR
/// "PARTUUID"s (`xxxxxxxx-nn`), which never match a GPT entry and do no harm.
pub fn partition_guids_here(sys_class_block: &Path) -> std::collections::HashSet<String> {
    let Ok(dir) = std::fs::read_dir(sys_class_block) else {
        return Default::default();
    };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            own_backup_common::linux_block::partuuid(sys_class_block, &name)
        })
        .map(|g| g.to_uppercase())
        .collect()
}

// ---- one door for every change ---------------------------------------------

/// The copy of the boot configuration taken before a change, and whether it
/// will still be there after a restart.
#[derive(Debug, Clone)]
pub struct SafetyCopy {
    pub path: PathBuf,
    /// False on the boot medium, whose files live in RAM.
    pub durable: bool,
}

/// What someone asked to change. Partitions are named by their device, not
/// by a disk number: Linux has none that lasts.
#[derive(Debug, Clone)]
pub enum Action {
    Rename {
        entry: u16,
        name: String,
    },
    SetActive {
        entry: u16,
        on: bool,
        force: bool,
    },
    Move {
        entry: u16,
        to: usize,
    },
    Delete {
        entry: u16,
        force: bool,
    },
    /// A new entry for a loader on the partition `hd` describes.
    Create {
        name: String,
        hd: HardDrive,
        file: String,
        position: Option<usize>,
    },
    BootNext {
        entry: Option<u16>,
    },
    Timeout {
        secs: u16,
    },
}

/// What [`Efivarfs::apply_action`] did.
#[derive(Debug, Clone)]
pub struct Done {
    pub message: String,
    pub safety_copy: SafetyCopy,
    pub created: Option<u16>,
}

impl Efivarfs {
    /// Copy the whole boot configuration into `dir`, named by the second, in
    /// the format both systems read (`BootBackup`).
    pub fn safety_copy(&self, dir: &Path, durable: bool) -> Result<SafetyCopy> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!(
            "boot-{}.json",
            chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
        ));
        let machine = own_backup_common::media_mode::meaningful_machine_name().unwrap_or_default();
        std::fs::write(&path, self.export(&machine)?.to_json()?)?;
        info!(path = %path.display(), durable, "efivars: safety copy taken");
        Ok(SafetyCopy { path, durable })
    }

    /// Every change to the boot configuration goes through here, for the same
    /// two rules as on Windows (`efivars_win::apply_action`): a copy is taken
    /// before anything is written, and the entry this machine booted from is
    /// not removed or hidden unless someone says so out loud (`force`).
    pub fn apply_action(&self, action: Action, copy_dir: &Path, durable: bool) -> Result<Done> {
        if let Some((entry, force, deleting)) = match &action {
            Action::Delete { entry, force } => Some((*entry, *force, true)),
            Action::SetActive {
                entry,
                on: false,
                force,
            } => Some((*entry, *force, false)),
            _ => None,
        } {
            if self.read_config()?.current == Some(entry) && !force {
                let entry = format!("{entry:04X}");
                return Err(dev(if deleting {
                    tr!("efivars_win.current_entry_delete", entry = entry)
                } else {
                    tr!("efivars_win.current_entry_hide", entry = entry)
                }));
            }
        }
        let safety_copy = self
            .safety_copy(copy_dir, durable)
            .map_err(|e| dev(tr!("efivars_win.safety_copy_failed", error = e)))?;

        let mut created = None;
        let message = match action {
            Action::Rename { entry, name } => {
                let was = self
                    .read_entry(entry)?
                    .map(|o| o.description)
                    .unwrap_or_default();
                self.set_description(entry, &name)?;
                tr!(
                    "efivars_win.done_renamed",
                    entry = format!("{entry:04X}"),
                    was = was,
                    name = name
                )
                .into_owned()
            }
            Action::SetActive { entry, on, .. } => {
                self.set_active(entry, on)?;
                let entry = format!("{entry:04X}");
                if on {
                    tr!("efivars_win.done_shown", entry = entry).into_owned()
                } else {
                    tr!("efivars_win.done_hidden", entry = entry).into_owned()
                }
            }
            Action::Move { entry, to } => {
                let order = self.read_config()?.order;
                let new_order = efivars::reorder(&order, entry, to);
                self.set_order(&new_order)?;
                tr!(
                    "efivars_win.done_order",
                    order = new_order
                        .iter()
                        .map(|x| format!("{x:04X}"))
                        .collect::<Vec<_>>()
                        .join(" → ")
                )
                .into_owned()
            }
            Action::Delete { entry, .. } => {
                let was = self
                    .read_entry(entry)?
                    .map(|o| o.description)
                    .unwrap_or_default();
                self.delete_entry(entry)?;
                tr!(
                    "efivars_win.done_deleted",
                    entry = format!("{entry:04X}"),
                    was = was
                )
                .into_owned()
            }
            Action::Create {
                name,
                hd,
                file,
                position,
            } => {
                let file = file.replace('/', "\\");
                let file = if file.starts_with('\\') {
                    file
                } else {
                    format!("\\{file}")
                };
                let n = self.create_entry(&name, &hd, &file, position)?;
                created = Some(n);
                tr!(
                    "efivars_win.done_created",
                    entry = format!("{n:04X}"),
                    name = name,
                    file = file
                )
                .into_owned()
            }
            Action::BootNext { entry } => {
                self.set_boot_next(entry)?;
                match entry {
                    Some(n) => {
                        tr!("efivars_win.done_boot_next", entry = format!("{n:04X}")).into_owned()
                    }
                    None => tr!("efivars_win.done_boot_next_cleared").into_owned(),
                }
            }
            Action::Timeout { secs } => {
                self.set_timeout(secs)?;
                tr!("efivars_win.done_timeout", secs = secs).into_owned()
            }
        };
        Ok(Done {
            message,
            safety_copy,
            created,
        })
    }

    /// The entry that already starts `file` from the partition `hd` names,
    /// if there is one.
    pub fn entry_for(&self, hd: &HardDrive, file: &str) -> Result<Option<u16>> {
        let want = hd.partition_guid().map(|g| g.to_ascii_uppercase());
        Ok(self.read_config()?.entries.iter().find_map(|e| {
            let same_part = e
                .option
                .hard_drive()
                .and_then(|h| h.partition_guid())
                .map(|g| g.to_ascii_uppercase())
                == want;
            let same_file = e
                .option
                .file_path()
                .is_some_and(|f| efivars::same_efi_path(&f, file));
            (same_part && same_file).then_some(e.number)
        }))
    }
}

/// Where the safety copies go: the service's data directory on an installed
/// system, the medium's RAM elsewhere (and then said to be temporary).
pub fn safety_copy_dir(data_dir: &Path) -> (PathBuf, bool) {
    (
        data_dir.join("boot"),
        !own_backup_common::media_mode::is_media_mode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESP_GUID: &str = "11111111-2222-3333-4444-555555555555";

    fn fake() -> (tempfile::TempDir, Efivarfs) {
        let root = tempfile::tempdir().unwrap();
        let fs = Efivarfs::rooted(root.path());
        std::fs::create_dir_all(&fs.vars).unwrap();
        // efivarfs always carries some variables; an empty directory reads as
        // "not mounted".
        put(&fs, "PlatformLang", 7, b"en-US\0");
        (root, fs)
    }

    fn put(fs: &Efivarfs, name: &str, attrs: u32, data: &[u8]) {
        let mut raw = attrs.to_le_bytes().to_vec();
        raw.extend_from_slice(data);
        std::fs::write(fs.path_of(name), raw).unwrap();
    }

    fn hd() -> HardDrive {
        HardDrive::from_gpt(1, 2048, 204800, ESP_GUID).unwrap()
    }

    fn entry(fs: &Efivarfs, n: u16, name: &str) {
        let opt = efivars::build_entry(name, &hd(), r"\EFI\debian\shimx64.efi");
        put(fs, &efivars::entry_name(n), 7, &opt.to_bytes());
    }

    #[test]
    fn a_bios_machine_and_an_unmounted_efivarfs_are_told_apart() {
        let root = tempfile::tempdir().unwrap();
        let fs = Efivarfs::rooted(root.path());
        assert!(!fs.is_uefi());
        let bios = fs.read_config().unwrap_err().to_string();
        std::fs::create_dir_all(root.path().join("sys/firmware/efi/efivars")).unwrap();
        assert!(fs.is_uefi());
        let unmounted = fs.read_config().unwrap_err().to_string();
        assert_ne!(bios, unmounted);
    }

    #[test]
    fn the_configuration_reads_back_in_menu_order() {
        let (_root, fs) = fake();
        entry(&fs, 0, "debian");
        entry(&fs, 3, "Windows Boot Manager");
        entry(&fs, 5, "hidden");
        put(
            &fs,
            "BootOrder",
            7,
            &efivars::serialize_boot_order(&[3, 0, 9]),
        );
        put(&fs, "BootCurrent", 6, &0u16.to_le_bytes());
        put(&fs, "Timeout", 7, &5u16.to_le_bytes());
        let cfg = fs.read_config().unwrap();
        let numbers: Vec<u16> = cfg.entries.iter().map(|e| e.number).collect();
        assert_eq!(numbers, vec![3, 0, 5]);
        assert_eq!(cfg.current, Some(0));
        assert!(cfg.entries[1].is_current);
        assert_eq!(cfg.timeout, Some(5));
        assert_eq!(cfg.dangling, vec![9]);
        assert_eq!(cfg.entries[2].order_index, None);
    }

    #[test]
    fn a_variable_is_written_as_attributes_then_data_and_replaced_whole() {
        let (_root, fs) = fake();
        fs.write_var("Timeout", &[1, 2, 3, 4, 5, 6], 7).unwrap();
        fs.write_var("Timeout", &[9, 0], 7).unwrap();
        let raw = std::fs::read(fs.path_of("Timeout")).unwrap();
        assert_eq!(raw, vec![7, 0, 0, 0, 9, 0]);
        assert_eq!(fs.read_var_ex("Timeout").unwrap(), Some((vec![9, 0], 7)));
    }

    #[test]
    fn only_boot_variables_are_written_and_never_empty() {
        let (_root, fs) = fake();
        assert!(fs.write_var("PlatformLang", b"ru", 7).is_err());
        assert!(fs.write_var("db", b"x", 7).is_err());
        assert!(fs.write_var("BootOrder", &[], 7).is_err());
        assert!(fs.delete_var("PlatformLang").is_err());
        assert!(fs.path_of("PlatformLang").exists());
    }

    #[test]
    fn creating_an_entry_takes_a_free_number_and_a_place_in_the_menu() {
        let (_root, fs) = fake();
        entry(&fs, 0, "debian");
        entry(&fs, 1, "Windows Boot Manager");
        put(&fs, "BootOrder", 7, &efivars::serialize_boot_order(&[0, 1]));
        let n = fs
            .create_entry("Own Backup", &hd(), r"\EFI\BOOT\BOOTX64.EFI", Some(0))
            .unwrap();
        assert_eq!(n, 2);
        let cfg = fs.read_config().unwrap();
        assert_eq!(cfg.order, vec![2, 0, 1]);
        let made = fs.read_entry(2).unwrap().unwrap();
        assert_eq!(made.description, "Own Backup");
        assert_eq!(
            made.hard_drive()
                .unwrap()
                .partition_guid()
                .unwrap()
                .to_uppercase(),
            ESP_GUID.to_uppercase()
        );
        assert_eq!(
            fs.read_var_ex("Boot0002").unwrap().unwrap().1,
            BOOT_VARIABLE_ATTRIBUTES
        );
    }

    #[test]
    fn deleting_an_entry_takes_it_out_of_the_order_first() {
        let (_root, fs) = fake();
        entry(&fs, 0, "debian");
        entry(&fs, 1, "old");
        put(&fs, "BootOrder", 7, &efivars::serialize_boot_order(&[1, 0]));
        fs.delete_entry(1).unwrap();
        assert_eq!(fs.read_config().unwrap().order, vec![0]);
        assert!(!fs.path_of("Boot0001").exists());
    }

    #[test]
    fn rename_disable_next_and_timeout() {
        let (_root, fs) = fake();
        entry(&fs, 4, "debian");
        fs.set_description(4, "Debian 13").unwrap();
        fs.set_active(4, false).unwrap();
        let e = fs.read_entry(4).unwrap().unwrap();
        assert_eq!(e.description, "Debian 13");
        assert!(!e.is_active());
        assert!(fs.set_description(4, "  ").is_err());
        assert!(fs.set_description(7, "x").is_err());

        fs.set_boot_next(Some(4)).unwrap();
        assert_eq!(fs.read_config().unwrap().next, Some(4));
        fs.set_boot_next(None).unwrap();
        assert_eq!(fs.read_config().unwrap().next, None);
        fs.set_boot_next(None).unwrap();
        fs.set_timeout(3).unwrap();
        assert_eq!(fs.read_config().unwrap().timeout, Some(3));
    }

    #[test]
    fn a_copy_round_trips_and_only_differences_are_written() {
        let (_root, fs) = fake();
        entry(&fs, 0, "debian");
        entry(&fs, 1, "Windows Boot Manager");
        put(&fs, "BootOrder", 7, &efivars::serialize_boot_order(&[0, 1]));
        put(&fs, "BootCurrent", 6, &0u16.to_le_bytes());
        let copy = fs.export("host").unwrap();
        let copy = BootBackup::load(&copy.to_json().unwrap()).unwrap();
        assert!(copy.variables.iter().all(|v| v.name != "BootCurrent"));

        // Nothing changed: nothing is written.
        let here = [ESP_GUID.to_uppercase()].into_iter().collect();
        assert_eq!(fs.apply_import(&copy, &here).unwrap().written, 0);

        // Lose an entry and scramble the order; the copy brings both back.
        fs.delete_var("Boot0001").unwrap();
        fs.set_order(&[0]).unwrap();
        let done = fs.apply_import(&copy, &here).unwrap();
        assert_eq!(done.written, 2);
        assert_eq!(fs.read_config().unwrap().order, vec![0, 1]);
    }

    #[test]
    fn an_entry_for_a_partition_this_machine_lacks_is_not_written_back() {
        let (_root, fs) = fake();
        entry(&fs, 0, "debian");
        let copy = fs.export("host").unwrap();
        fs.delete_var("Boot0000").unwrap();
        let elsewhere = ["AAAAAAAA-0000-0000-0000-000000000000".to_string()]
            .into_iter()
            .collect();
        let done = fs.apply_import(&copy, &elsewhere).unwrap();
        assert_eq!(done.written, 0);
        assert_eq!(done.skipped_orphaned.len(), 1);
        assert!(!fs.path_of("Boot0000").exists());
        // Unknown (empty) set: written.
        let done = fs.apply_import(&copy, &Default::default()).unwrap();
        assert_eq!(done.written, 1);
    }

    #[test]
    fn partition_guids_come_from_sysfs_partuuids() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("sda1");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("uevent"),
            format!("MAJOR=8\nMINOR=1\nDEVNAME=sda1\nPARTUUID={ESP_GUID}\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("sda")).unwrap();
        std::fs::write(root.path().join("sda/uevent"), "DEVNAME=sda\n").unwrap();
        let got = partition_guids_here(root.path());
        assert_eq!(got.len(), 1);
        assert!(got.contains(&ESP_GUID.to_uppercase()));
    }

    #[test]
    fn every_change_takes_a_copy_first_and_spares_the_entry_booted_from() {
        let (root, fs) = fake();
        entry(&fs, 0, "debian");
        entry(&fs, 1, "old");
        put(&fs, "BootOrder", 7, &efivars::serialize_boot_order(&[0, 1]));
        put(&fs, "BootCurrent", 6, &0u16.to_le_bytes());
        let copies = root.path().join("copies");

        let refused = fs
            .apply_action(
                Action::Delete {
                    entry: 0,
                    force: false,
                },
                &copies,
                true,
            )
            .unwrap_err();
        assert!(refused.to_string().contains("0000"), "{refused}");
        assert!(fs.read_entry(0).unwrap().is_some());
        assert!(!copies.exists(), "nothing was done, so nothing was copied");

        let done = fs
            .apply_action(
                Action::Delete {
                    entry: 1,
                    force: false,
                },
                &copies,
                true,
            )
            .unwrap();
        assert!(done.safety_copy.path.is_file());
        let saved =
            BootBackup::load(&std::fs::read_to_string(&done.safety_copy.path).unwrap()).unwrap();
        assert!(saved.variables.iter().any(|v| v.name == "Boot0001"));
        assert!(fs.read_entry(1).unwrap().is_none());

        let created = fs
            .apply_action(
                Action::Create {
                    name: "Windows Boot Manager".into(),
                    hd: hd(),
                    file: "EFI/Microsoft/Boot/bootmgfw.efi".into(),
                    position: None,
                },
                &copies,
                true,
            )
            .unwrap()
            .created
            .unwrap();
        assert_eq!(
            fs.entry_for(&hd(), r"\EFI\Microsoft\Boot\bootmgfw.efi")
                .unwrap(),
            Some(created)
        );
        assert_eq!(fs.read_config().unwrap().order, vec![0, created]);
    }
}
