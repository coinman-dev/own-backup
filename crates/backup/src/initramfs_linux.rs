//! Rebuilding a restored system's initramfs after the restore gave it new
//! identities (PLAN §6, owner decision 13) — the last step of
//! `identity_change_linux` when the restored system's references were fixed.
//!
//! An initramfs is built from the system's configuration and keeps copies of
//! parts of it: crypttab (cryptsetup's initramfs-tools hook, dracut in
//! host-only mode, Arch's `crypttab.initramfs`), mdadm.conf (or one Debian's
//! hook generates at build time from the arrays it sees), the resume device
//! (`initramfs-tools/conf.d/resume`), and the command line dracut's
//! `kernel_cmdline` or a unified image's `/etc/kernel/cmdline` embeds.
//! `identity_refs_linux` fixed those files; the image the restored system
//! boots still carries the old identities until its own tool builds it again.
//! When that is needed is [`reasons`].
//!
//! The rebuild runs the restored system's own tool inside it: its root (the
//! restored filesystem with `etc/os-release` and `etc/fstab`) is mounted under
//! a private directory, its `/boot` and ESP by the entries of its own fstab
//! resolved to the restored devices ([`mount_plan`]) — never by a `UUID=` or
//! `LABEL=` lookup on this machine, where the source's devices still carry
//! the same labels and PARTUUIDs — then `/proc`, `/sys` and `/dev`, and the
//! tool runs after `chroot(2)` in the child, so no binary of the restored
//! system ever runs against this machine's root and no binary of this
//! machine is taken for the restored system's. Everything is unmounted again,
//! in reverse order, on every path. A rebuild counts as done only when the
//! tool exited 0 AND an image under `/boot` was actually rewritten; a tool
//! that "succeeds" writing an image the boot loader does not load is not a
//! rebuild.
//!
//! The restored devices are under the restore's udev hold, which leaves them
//! without `/dev/disk` links; dracut then records the devices the boot waits
//! for (host-only images wait for /boot and the ESP) by the kernel names this
//! machine gave them — `/dev/sdb2`, which the restored machine does not have,
//! and its boot stops. So the filesystems the rebuild reads get their
//! `/dev/disk/by-uuid` links for its duration (their UUIDs are their own by
//! then; labels and PARTUUIDs are still the source's and get none:
//! `udev_hold_linux::UuidLinks`). And every image is checked after it was
//! built (`initramfs_image_linux`): one that names this machine's devices,
//! or that cannot be read, is not kept — the images are put back as they were
//! from copies taken before the tool ran (`<image>.obk-prev`), as they are
//! after any failure. A rebuild never leaves the system's images worse than
//! the restore found them.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use own_backup_common::tr;

use crate::identity_clash::{Identity, IdentityKind};
use crate::identity_refs_linux::Renamed;
use crate::initramfs_image_linux as image;

/// How long the system's tool may take: dracut on a slow disk with every
/// kernel takes minutes, a hang must not hold the restore forever.
pub const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The system's own initramfs tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Debian and Ubuntu, whether initramfs-tools or Debian's dracut
    /// adaptation sits behind it.
    UpdateInitramfs,
    Dracut,
    Mkinitcpio,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::UpdateInitramfs => "update-initramfs",
            Tool::Dracut => "dracut",
            Tool::Mkinitcpio => "mkinitcpio",
        }
    }

    /// Every installed kernel's image, rebuilt from the current configuration.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Tool::UpdateInitramfs => &["-u", "-k", "all"],
            Tool::Dracut => &["--regenerate-all", "--force"],
            Tool::Mkinitcpio => &["-P"],
        }
    }

    /// The command as a person types it.
    pub fn command(self) -> String {
        format!("{} {}", self.name(), self.args().join(" "))
    }
}

/// What `os-release` says about a system.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
    pub pretty_name: String,
}

impl OsRelease {
    /// A name to show: the pretty one, else the id.
    pub fn shown(&self) -> String {
        if self.pretty_name.is_empty() {
            self.id.clone()
        } else {
            self.pretty_name.clone()
        }
    }
}

/// `KEY=value` lines; values may be quoted.
pub fn parse_os_release(text: &str) -> OsRelease {
    let mut os = OsRelease::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        match key.trim() {
            "ID" => os.id = value.to_ascii_lowercase(),
            "ID_LIKE" => {
                os.id_like = value
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect()
            }
            "PRETTY_NAME" => os.pretty_name = value.to_string(),
            _ => {}
        }
    }
    os
}

/// The distribution family that decides the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Debian,
    RedHat,
    Suse,
    Arch,
    Unknown,
}

/// The family by `ID`, then by `ID_LIKE` in its order (Linux Mint:
/// `ID=linuxmint`, `ID_LIKE="ubuntu debian"`).
pub fn family(os: &OsRelease) -> Family {
    for id in std::iter::once(&os.id).chain(&os.id_like) {
        let f = match id.as_str() {
            "debian" | "ubuntu" => Family::Debian,
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => Family::RedHat,
            "suse" | "sles" | "opensuse" => Family::Suse,
            s if s.starts_with("opensuse") => Family::Suse,
            "arch" | "manjaro" | "endeavouros" => Family::Arch,
            _ => continue,
        };
        return f;
    }
    Family::Unknown
}

/// The tools a family uses, preferred first. An unknown system gets none: a
/// guessed tool on a system nobody has looked at is not "its own".
pub fn tools_of(family: Family) -> &'static [Tool] {
    match family {
        Family::Debian => &[Tool::UpdateInitramfs],
        Family::RedHat | Family::Suse => &[Tool::Dracut],
        Family::Arch => &[Tool::Mkinitcpio, Tool::Dracut],
        Family::Unknown => &[],
    }
}

/// The first tool of `family` that `installed` finds (its path inside the
/// system), with that path.
pub fn select_tool(
    family: Family,
    mut installed: impl FnMut(Tool) -> Option<String>,
) -> Option<(Tool, String)> {
    tools_of(family)
        .iter()
        .find_map(|t| installed(*t).map(|p| (*t, p)))
}

// --- reading the restored system ----------------------------------------------

/// `rel` under `root`, symbolic links followed the way they resolve inside
/// that system: an absolute target starts again at `root`, `..` never climbs
/// above it. `None` when a component is missing or the links loop. The path
/// returned contains no symbolic link.
pub fn resolve_in_root(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut todo: Vec<String> = rel.split('/').rev().map(str::to_string).collect();
    let mut at: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(c) = todo.pop() {
        match c.as_str() {
            "" | "." => continue,
            ".." => {
                at.pop();
                continue;
            }
            _ => {}
        }
        let mut p = root.to_path_buf();
        p.extend(&at);
        p.push(&c);
        let meta = std::fs::symlink_metadata(&p).ok()?;
        if meta.file_type().is_symlink() {
            links += 1;
            if links > 40 {
                return None;
            }
            let target = std::fs::read_link(&p).ok()?;
            let target = target.to_str()?;
            if target.starts_with('/') {
                at.clear();
            }
            todo.extend(target.split('/').rev().map(str::to_string));
        } else {
            at.push(c);
        }
    }
    let mut p = root.to_path_buf();
    p.extend(&at);
    Some(p)
}

/// A regular file of the system, read as text (at most 1 MiB).
fn read_text(root: &Path, rel: &str) -> Option<String> {
    let p = resolve_in_root(root, rel)?;
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if !meta.file_type().is_file() || meta.len() > 1 << 20 {
        return None;
    }
    std::fs::read_to_string(&p).ok()
}

/// The `*.conf` files of a directory of the system, by name.
fn conf_files(root: &Path, dir: &str) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Some(d) = resolve_in_root(root, dir) else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&d) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".conf") && e.file_type().is_ok_and(|t| t.is_file()) {
            out.insert(name, e.path());
        }
    }
    out
}

/// A program of the system: its path inside it, when one of the usual
/// directories has it as an executable file.
fn find_program(root: &Path, name: &str) -> Option<String> {
    ["usr/sbin", "usr/bin", "sbin", "bin"]
        .iter()
        .find(|dir| {
            resolve_in_root(root, &format!("{dir}/{name}"))
                .and_then(|p| std::fs::symlink_metadata(p).ok())
                .is_some_and(|m| m.file_type().is_file() && m.mode() & 0o111 != 0)
        })
        .map(|dir| format!("/{dir}/{name}"))
}

/// What the decision and the rebuild need of a restored system, read after
/// its files were fixed (so they name the NEW identities).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemConfig {
    pub os: OsRelease,
    pub fstab: String,
    /// `crypttab` and Arch's `crypttab.initramfs`.
    pub crypttab: String,
    /// Both places mdadm.conf lives.
    pub mdadm: String,
    /// initramfs-tools' resume configuration.
    pub resume: String,
    /// `/etc/kernel/cmdline` (unified kernel images embed it).
    pub kernel_cmdline: String,
    /// dracut's configuration in the order dracut reads it.
    pub dracut: String,
    pub dracut_installed: bool,
    /// Its version number (`110` of `110-11ubuntu0.1`), when it says.
    pub dracut_version: Option<u32>,
    /// The tool to run and its path inside the system.
    pub tool: Option<(Tool, String)>,
}

/// The system whose root is `root`, if it is one: `etc/os-release` and a
/// regular `etc/fstab`.
pub fn read_system(root: &Path) -> Option<SystemConfig> {
    let os = parse_os_release(&read_text(root, "etc/os-release")?);
    let fstab = read_text(root, "etc/fstab")?;
    let texts = |paths: &[&str]| -> String {
        paths
            .iter()
            .filter_map(|p| read_text(root, p))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // dracut: /etc/dracut.conf, then the drop-ins of both directories by
    // name, a file in /etc replacing the same name in /usr/lib.
    let mut dracut = texts(&["etc/dracut.conf"]);
    let mut dropins = conf_files(root, "usr/lib/dracut/dracut.conf.d");
    dropins.extend(conf_files(root, "etc/dracut.conf.d"));
    for p in dropins.values() {
        if let Ok(t) = std::fs::read_to_string(p) {
            dracut.push('\n');
            dracut.push_str(&t);
        }
    }
    let dracut_program = find_program(root, "dracut");
    let dracut_installed = dracut_program.is_some();
    let dracut_version = dracut_program
        .iter()
        .map(|p| p.trim_start_matches('/'))
        .chain(["usr/lib/dracut/dracut-version.sh"])
        .filter_map(|p| read_text(root, p))
        .find_map(|t| parse_dracut_version(&t));
    let tool = select_tool(family(&os), |t| find_program(root, t.name()));
    Some(SystemConfig {
        os,
        fstab,
        crypttab: texts(&["etc/crypttab", "etc/crypttab.initramfs"]),
        mdadm: texts(&["etc/mdadm/mdadm.conf", "etc/mdadm.conf"]),
        resume: texts(&[
            "etc/initramfs-tools/conf.d/resume",
            "etc/initramfs-tools/initramfs.conf",
        ]),
        kernel_cmdline: texts(&["etc/kernel/cmdline"]),
        dracut,
        dracut_installed,
        dracut_version,
        tool,
    })
}

/// The systems in a mounted filesystem: at its root, or — a btrfs mounted at
/// its top level — in a top-level subvolume (`@`). The directory is relative
/// to `mount`.
pub fn find_systems(mount: &Path, top_level: bool) -> Vec<(PathBuf, SystemConfig)> {
    let mut dirs = vec![PathBuf::new()];
    if top_level {
        if let Ok(entries) = std::fs::read_dir(mount) {
            let mut subs: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| PathBuf::from(e.file_name()))
                .collect();
            subs.sort();
            dirs.extend(subs);
        }
    }
    dirs.into_iter()
        .filter_map(|d| read_system(&mount.join(&d)).map(|s| (d, s)))
        .collect()
}

// --- when ------------------------------------------------------------------------

/// Why an image has to be built again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A LUKS container's new UUID is in crypttab: the image carries the
    /// entries it unlocks.
    Crypttab,
    /// An md array's UUID changed: the image carries mdadm.conf, or one
    /// generated from the arrays seen when it was built.
    Md,
    /// The renamed volume group is used by the system: resume and host-only
    /// images name its LVs.
    VolumeGroup,
    /// A new identity is in what the image copies verbatim: the resume
    /// configuration, `/etc/kernel/cmdline`, dracut's configuration.
    Embedded,
    /// dracut builds host-only images, which record the devices of the root,
    /// /boot and the ESP (the root on the command line they store, the others
    /// as devices the boot waits for), and one of their UUIDs or labels changed.
    Hostonly,
}

/// How a changed identity may be written in a file.
fn spellings(r: &Renamed) -> Vec<String> {
    let mut out = vec![r.new.to_ascii_lowercase()];
    if r.kind == IdentityKind::Md {
        let hex: String = r.new.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 32 {
            out.push(
                format!(
                    "{}-{}-{}-{}-{}",
                    &hex[0..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..32]
                )
                .to_ascii_lowercase(),
            );
        }
    }
    out
}

fn mentions(text: &str, r: &Renamed) -> bool {
    let lower = text.to_ascii_lowercase();
    !r.new.is_empty() && spellings(r).iter().any(|s| lower.contains(s.as_str()))
}

/// The ways a system names a volume group (as `identity_refs_linux` rewrites
/// them).
fn names_vg(text: &str, vg: &str) -> bool {
    let esc = vg.replace('-', "--");
    [
        format!("/dev/mapper/{esc}-"),
        format!("dm-name-{esc}-"),
        format!("/dev/{vg}/"),
        format!("rd.lvm.vg={vg}"),
        format!("rd.lvm.lv={vg}/"),
    ]
    .iter()
    .any(|n| text.contains(n.as_str()))
}

/// The last plain assignment of `key` in dracut's configuration, unquoted.
pub fn dracut_option(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches(['"', '\'']).trim().to_string())
        })
        .next_back()
}

/// The dracut release (dracut-ng 109) from which an unset `hostonly` means
/// yes; before it, no.
pub const DRACUT_HOSTONLY_BY_DEFAULT: u32 = 109;

/// `DRACUT_VERSION=` of dracut's script or its version file, as a number.
pub fn parse_dracut_version(text: &str) -> Option<u32> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let l = l.strip_prefix("readonly ").unwrap_or(l);
        let v = l.strip_prefix("DRACUT_VERSION=")?;
        let v = v.trim_matches(['"', '\'']);
        let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// Does this dracut build host-only images? Its configuration says so, or
/// — not set — its version does; a version it does not tell counts as
/// recent (the side of rebuilding).
pub fn dracut_hostonly(text: &str, version: Option<u32>) -> bool {
    match dracut_option(text, "hostonly").as_deref() {
        Some(v) => v == "yes",
        None => version.is_none_or(|v| v >= DRACUT_HOSTONLY_BY_DEFAULT),
    }
}

/// Why the restored system's image must be built again after `changed`
/// (its references already fixed); empty when it need not be.
/// `recorded_changed`: the UUID or label of its root, /boot or ESP changed — what a
/// host-only dracut image records.
///
/// Deliberately on the side of rebuilding: the tool makes the image the
/// system's next kernel update would make anyway, while an image left with
/// an old UUID stops the system at boot. Only identities the restored system
/// actually names count, so a data disk restored beside a system does not
/// rebuild it. Not counted: LVM PV and VG UUIDs (LVM finds those by
/// scanning), a PARTUUID (never changed).
pub fn reasons(changed: &[Renamed], recorded_changed: bool, sys: &SystemConfig) -> Vec<Reason> {
    let embedded = [
        sys.resume.as_str(),
        sys.kernel_cmdline.as_str(),
        sys.dracut.as_str(),
    ];
    let mut out = Vec::new();
    let mut add = |r: Reason| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    for r in changed {
        match r.kind {
            IdentityKind::Luks if mentions(&sys.crypttab, r) => add(Reason::Crypttab),
            IdentityKind::Md => add(Reason::Md),
            IdentityKind::LvmVgName => {
                let used = [sys.fstab.as_str(), sys.crypttab.as_str()]
                    .iter()
                    .chain(&embedded)
                    .any(|t| names_vg(t, &r.new));
                if used {
                    add(Reason::VolumeGroup);
                }
            }
            _ => {}
        }
        let uuid_like = matches!(
            r.kind,
            IdentityKind::Ext
                | IdentityKind::Xfs
                | IdentityKind::Btrfs
                | IdentityKind::Fat
                | IdentityKind::Luks
                | IdentityKind::Md
        );
        if uuid_like && embedded.iter().any(|t| mentions(t, r)) {
            add(Reason::Embedded);
        }
        // `RESUME=LABEL=…`, `root=LABEL=…` in /etc/kernel/cmdline or
        // dracut's kernel_cmdline: copied into the image as well.
        if r.kind == IdentityKind::Label
            && embedded
                .iter()
                .any(|t| crate::identity_refs_linux::names_label(t, &r.new))
        {
            add(Reason::Embedded);
        }
    }
    if recorded_changed && sys.dracut_installed && dracut_hostonly(&sys.dracut, sys.dracut_version)
    {
        add(Reason::Hostonly);
    }
    out
}

// --- where -----------------------------------------------------------------------

/// A restored filesystem that may be mounted for the rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub node: String,
    pub fstype: String,
    /// Its UUID as `Identity` keys it (hex digits, lowercase).
    pub uuid_key: String,
    pub label: Option<String>,
    pub partuuid: Option<String>,
}

/// One mount under the restored root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMount {
    /// Where, inside the restored system (`/boot`).
    pub target: String,
    pub node: String,
    pub fstype: String,
    pub opts: String,
}

/// The mounts the tools write images to, in mounting order; the bool: the
/// rebuild cannot go on without it.
const BOOT_MOUNTS: &[(&str, bool)] = &[("/boot", true), ("/boot/efi", false), ("/efi", false)];

/// fstab's octal escapes (`\040` for a space).
fn fstab_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            out.push(u8::try_from(v).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(s)
}

/// The one restored filesystem an fstab source names; `None` when it names
/// none of them, more than one, or a device only this machine could resolve.
fn resolve<'a>(spec: &str, candidates: &'a [Candidate]) -> Option<&'a Candidate> {
    let one = |f: &dyn Fn(&Candidate) -> bool| {
        let mut it = candidates.iter().filter(|c| f(c));
        let first = it.next()?;
        it.next().is_none().then_some(first)
    };
    if let Some(v) = spec
        .strip_prefix("UUID=")
        .or_else(|| spec.strip_prefix("/dev/disk/by-uuid/"))
    {
        let key = Identity::new(IdentityKind::Ext, unquote(v)).key;
        return (!key.is_empty())
            .then(|| one(&|c| c.uuid_key == key))
            .flatten();
    }
    if let Some(v) = spec
        .strip_prefix("LABEL=")
        .or_else(|| spec.strip_prefix("/dev/disk/by-label/"))
    {
        let v = unquote(v);
        return one(&|c| c.label.as_deref() == Some(v));
    }
    if let Some(v) = spec
        .strip_prefix("PARTUUID=")
        .or_else(|| spec.strip_prefix("/dev/disk/by-partuuid/"))
    {
        let v = unquote(v).to_ascii_lowercase();
        return one(&|c| c.partuuid.as_deref().map(str::to_ascii_lowercase) == Some(v.clone()));
    }
    let mapper = if let Some(n) = spec
        .strip_prefix("/dev/mapper/")
        .or_else(|| spec.strip_prefix("/dev/disk/by-id/dm-name-"))
    {
        Some(format!("/dev/mapper/{n}"))
    } else {
        // `/dev/<vg>/<lv>`
        let parts: Vec<&str> = spec.trim_start_matches("/dev/").split('/').collect();
        (spec.starts_with("/dev/") && parts.len() == 2 && parts[0] != "disk" && parts[0] != "md")
            .then(|| {
                format!(
                    "/dev/mapper/{}-{}",
                    parts[0].replace('-', "--"),
                    parts[1].replace('-', "--")
                )
            })
    };
    mapper.and_then(|m| one(&|c| c.node == m))
}

/// Mount options for a restored filesystem under the rebuild; `nosuid,nodev`
/// are added when it is mounted.
fn opts_for(fstype: &str, fstab_opts: &str) -> String {
    let mut o = vec!["rw".to_string()];
    match fstype {
        // The original may be mounted on this machine.
        "xfs" => o.push("nouuid".into()),
        "btrfs" => o.extend(
            fstab_opts
                .split(',')
                .filter(|x| x.starts_with("subvol=") || x.starts_with("subvolid="))
                .map(str::to_string),
        ),
        _ => {}
    }
    o.join(",")
}

/// The mounts the restored system's own `fstab` (already naming the new
/// identities) asks for under its root `root_node`, each resolved to one of
/// the restored filesystems `candidates`. `Err(source)` when its `/boot` is
/// not among them: images written into the root's empty `/boot` directory
/// would not be the ones it boots. An ESP that cannot be resolved is left
/// out — the images the tools build live in `/boot`.
pub fn mount_plan(
    fstab: &str,
    root_node: &str,
    candidates: &[Candidate],
) -> Result<Vec<PlannedMount>, String> {
    let mut entries: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in fstab.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let mut target = fstab_unescape(f[1]);
        while target.len() > 1 && target.ends_with('/') {
            target.pop();
        }
        let opts = f.get(3).copied().unwrap_or("defaults").to_string();
        entries.insert(target, (fstab_unescape(f[0]), opts));
    }
    let mut out = Vec::new();
    for (target, required) in BOOT_MOUNTS {
        let Some((spec, fstab_opts)) = entries.get(*target) else {
            continue;
        };
        let found = resolve(spec, candidates)
            .filter(|c| c.node != root_node || c.fstype == "btrfs")
            .filter(|_| !fstab_opts.split(',').any(|o| o == "bind"));
        match found {
            Some(c) => out.push(PlannedMount {
                target: target.to_string(),
                node: c.node.clone(),
                fstype: c.fstype.clone(),
                opts: opts_for(&c.fstype, fstab_opts),
            }),
            None if *required => return Err(spec.clone()),
            None => info!(
                target,
                spec, "not among the restored filesystems: not mounted for the rebuild"
            ),
        }
    }
    Ok(out)
}

// --- how -------------------------------------------------------------------------

/// Everything the rebuild does on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub root_node: String,
    pub root_fstype: String,
    pub root_opts: String,
    /// The system's directory inside the root filesystem (`@` for a btrfs
    /// subvolume; empty otherwise).
    pub subdir: PathBuf,
    pub mounts: Vec<PlannedMount>,
    pub tool: Tool,
    /// The tool's path inside the system.
    pub program: String,
}

/// How a rebuild ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuilt {
    /// The tool exited 0 and rewrote these images (paths inside the system).
    Done {
        images: Vec<String>,
        output: String,
    },
    Failed {
        error: String,
        output: String,
    },
}

/// The last lines of a tool's output, for a note.
pub fn tail(output: &str, lines: usize) -> String {
    let all: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join(" / ")
}

/// Mounts made for one rebuild, undone in reverse order by [`Self::release`]
/// or on drop — an early return or a panic included.
struct MountStack {
    base: PathBuf,
    mounted: Vec<PathBuf>,
}

impl MountStack {
    /// A fresh private directory (0700) under `/run/own-backup`.
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let parent = Path::new("/run/own-backup");
        std::fs::create_dir_all(parent)?;
        let mut n = 0;
        loop {
            let base = parent.join(format!("initramfs-{}-{n}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&base) {
                Ok(()) => {
                    return Ok(Self {
                        base,
                        mounted: Vec::new(),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        }
    }

    fn mount(&mut self, args: &[&str], target: &Path) -> Result<(), String> {
        let out = Command::new("mount")
            .env("LC_ALL", "C")
            .args(args)
            .arg(target)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("mount {}: {e}", target.display()))?;
        if !out.status.success() {
            return Err(format!(
                "mount {} {}: {}",
                args.join(" "),
                target.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        info!(?args, target = %target.display(), "mounted for the initramfs rebuild");
        self.mounted.push(target.to_path_buf());
        Ok(())
    }

    /// Unmount everything, innermost first; what will not unmount is
    /// detached lazily and named in the error.
    fn release(&mut self) -> Result<(), String> {
        let mut stuck = Vec::new();
        while let Some(m) = self.mounted.pop() {
            let ok = Command::new("umount")
                .arg(&m)
                .stdin(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                warn!(dir = %m.display(), "umount refused — detaching lazily");
                let _ = Command::new("umount").arg("-l").arg(&m).status();
                stuck.push(m.display().to_string());
            }
        }
        let _ = std::fs::remove_dir(self.base.join("root"));
        let _ = std::fs::remove_dir(&self.base);
        if stuck.is_empty() {
            Ok(())
        } else {
            Err(tr!("initramfs_linux.umount_failed", dirs = stuck.join(", ")).into_owned())
        }
    }
}

impl Drop for MountStack {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// `rel` under `root` as a directory reached without a symbolic link: a
/// mount on a path through a link could land outside the restored system.
fn real_dir(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        p.push(c);
        let ok = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_dir());
        if !ok {
            return Err(tr!("initramfs_linux.not_a_directory", dir = rel).into_owned());
        }
    }
    Ok(p)
}

/// What tells a rewritten image: modification time, inode, size.
type ImageStamp = (i64, i64, u64, u64);

/// The initramfs images under the system's `/boot` (not following links),
/// with their [`ImageStamp`]; the copies [`Backup`] takes are not images.
fn images(sysroot: &Path) -> BTreeMap<String, ImageStamp> {
    fn walk(dir: &Path, rel: &str, depth: u32, out: &mut BTreeMap<String, ImageStamp>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let r = format!("{rel}/{name}");
            if ft.is_dir() && depth < 4 {
                walk(&e.path(), &r, depth + 1, out);
            } else if ft.is_file() {
                let lower = name.to_ascii_lowercase();
                let image = lower.starts_with("initrd") || lower.starts_with("initramfs");
                if image && !lower.ends_with(PREV_SUFFIX) {
                    if let Ok(m) = e.metadata() {
                        out.insert(r, (m.mtime(), m.mtime_nsec(), m.ino(), m.size()));
                    }
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&sysroot.join("boot"), "/boot", 1, &mut out);
    out
}

/// What a copy of an image taken before the rebuild is called, beside it.
const PREV_SUFFIX: &str = ".obk-prev";

/// The images as they were before the tool ran: a copy of each beside it
/// (the same filesystem, so putting one back is a rename).
struct Backup {
    /// `(path inside the system, image, copy)`.
    copies: Vec<(String, PathBuf, PathBuf)>,
}

impl Backup {
    /// Copy every image in `images` (paths inside the system under
    /// `sysroot`). On an error nothing is left behind.
    fn take<'a>(
        sysroot: &Path,
        images: impl IntoIterator<Item = &'a String>,
    ) -> Result<Self, String> {
        let mut b = Backup { copies: Vec::new() };
        for rel in images {
            let img = sysroot.join(rel.trim_start_matches('/'));
            let mut name = img.file_name().unwrap_or_default().to_os_string();
            name.push(PREV_SUFFIX);
            let copy = img.with_file_name(name);
            let copied = std::fs::copy(&img, &copy)
                .and_then(|_| std::fs::File::open(&copy))
                .and_then(|f| f.sync_all());
            if let Err(e) = copied {
                let _ = std::fs::remove_file(&copy);
                b.discard();
                return Err(format!("{rel}: {e}"));
            }
            b.copies.push((rel.clone(), img, copy));
        }
        Ok(b)
    }

    /// The copy of the image at `rel` (a path inside the system).
    fn copy_of(&self, rel: &str) -> Option<&Path> {
        self.copies
            .iter()
            .find(|(r, _, _)| r == rel)
            .map(|(_, _, c)| c.as_path())
    }

    /// The rebuild is kept: the copies go.
    fn discard(self) {
        for (rel, _, copy) in &self.copies {
            if let Err(e) = std::fs::remove_file(copy) {
                warn!(image = %rel, error = %e, "the copy of the previous image could not be removed");
            }
        }
    }

    /// Every image as it was (`before`): each copy renamed back over what the
    /// tool wrote, an image the tool created removed. `Err` names what could
    /// not be put back; those copies stay.
    fn put_back(self, sysroot: &Path, before: &BTreeMap<String, ImageStamp>) -> Result<(), String> {
        let mut failed = Vec::new();
        for (rel, img, copy) in &self.copies {
            if let Err(e) = std::fs::rename(copy, img) {
                failed.push(format!("{rel}: {e}"));
            }
        }
        for rel in images(sysroot).keys().filter(|r| !before.contains_key(*r)) {
            if let Err(e) = std::fs::remove_file(sysroot.join(rel.trim_start_matches('/'))) {
                failed.push(format!("{rel}: {e}"));
            }
        }
        if failed.is_empty() {
            info!("the previous initramfs images were put back");
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }
}

/// Check the images the tool rewrote: each must be readable and name none of
/// this machine's devices that the image it replaced did not name already.
fn check_images(
    sysroot: &Path,
    rewritten: &[String],
    backup: &Backup,
    devices: &image::MachineDevices,
) -> Result<(), String> {
    let mut names: Vec<String> = Vec::new();
    for rel in rewritten {
        let path = sysroot.join(rel.trim_start_matches('/'));
        let new = image::read_image(&path).map_err(|e| {
            tr!(
                "initramfs_linux.unverifiable",
                image = rel.as_str(),
                error = e
            )
            .into_owned()
        })?;
        let previous = backup
            .copy_of(rel)
            .and_then(|c| match image::read_image(c) {
                Ok(entries) => Some(entries),
                Err(e) => {
                    warn!(image = %rel, error = %e, "the previous image could not be read: every name counts");
                    None
                }
            });
        let found = image::new_device_refs(&new, previous.as_deref(), devices);
        for (entry, device) in &found {
            warn!(image = %rel, entry = %entry, device = %device, "the rebuilt image names a device by this machine's name");
        }
        for (_, device) in found {
            if !names.contains(&device) {
                names.push(device);
            }
        }
        info!(image = %rel, files = new.len(), "rebuilt image checked");
    }
    if names.is_empty() {
        Ok(())
    } else {
        Err(tr!("initramfs_linux.device_names", names = names.join(", ")).into_owned())
    }
}

/// The restored devices under `nodes` — those and the ones they are built
/// on (sysfs `slaves`: the LUKS container under its mapping, md members) —
/// by kernel name.
fn devices_under(nodes: &[&str]) -> Vec<String> {
    let mut todo: Vec<String> = nodes
        .iter()
        .filter_map(|n| std::fs::canonicalize(n).ok())
        .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .collect();
    let mut out: Vec<String> = Vec::new();
    while let Some(k) = todo.pop() {
        if out.contains(&k) {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(format!("/sys/class/block/{k}/slaves")) {
            todo.extend(
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned()),
            );
        }
        out.push(k);
    }
    out
}

/// The `/dev/disk/by-uuid` name udev gives a device, from `blkid -p -o
/// export`: the UUID of a filesystem or a LUKS container.
pub fn by_uuid_name(export: &str) -> Option<String> {
    let value = |key: &str| {
        export
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    matches!(value("USAGE="), Some("filesystem" | "crypto"))
        .then(|| value("UUID="))
        .flatten()
        .map(str::to_string)
}

/// `(kernel name, UUID)` of the restored devices under `nodes` that get a
/// by-uuid link for the rebuild. A UUID another device's link already has is
/// not taken from it: that device keeps its link, and the check after the
/// rebuild sees what the tool recorded instead.
fn uuid_links(nodes: &[&str]) -> Vec<(String, String)> {
    devices_under(nodes)
        .into_iter()
        .filter_map(|k| {
            let out = Command::new("blkid")
                .env("LC_ALL", "C")
                .args(["-p", "-o", "export"])
                .arg(format!("/dev/{k}"))
                .stdin(Stdio::null())
                .output()
                .ok()?;
            let uuid = by_uuid_name(&String::from_utf8_lossy(&out.stdout))?;
            let link = Path::new("/dev/disk/by-uuid").join(&uuid);
            if let Ok(to) = std::fs::canonicalize(&link) {
                if to != Path::new("/dev").join(&k) {
                    warn!(device = %k, uuid, other = %to.display(), "another device has this by-uuid link: none for the rebuild");
                    return None;
                }
            }
            Some((k, uuid))
        })
        .collect()
}

/// What a finished child printed and how it ended.
struct ChildEnd {
    code: Option<i32>,
    output: String,
    timed_out: bool,
}

/// Run `program` (a path inside `sysroot`) with `args` after `chroot` into
/// `sysroot`, in a process group of its own, with an empty environment but
/// `PATH` and `LC_ALL=C`, an empty stdin, stdout and stderr together; killed
/// with its group after `timeout`.
fn run_in_root(
    sysroot: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<ChildEnd> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let root_c = std::ffi::CString::new(sysroot.as_os_str().as_bytes())?;
    let (mut reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("LC_ALL", "C")
        .env("HOME", "/root")
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0);
    // SAFETY: chroot and chdir are async-signal-safe; the closure allocates
    // nothing (the path was made before the fork).
    unsafe {
        cmd.pre_exec(move || {
            if libc::chroot(root_c.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Our copies of the pipe's write end go with the command: the reader sees
    // the end once the tool and its children have exited.
    drop(cmd);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = buf.lock().unwrap_or_else(|p| p.into_inner());
                b.extend_from_slice(&chunk[..n]);
                // Keep the last 256 KiB: the end says why.
                if b.len() > 512 << 10 {
                    let cut = b.len() - (256 << 10);
                    b.drain(..cut);
                }
            }
            let _ = done_tx.send(());
        });
    }
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            // SAFETY: plain kill(2) of the tool's own process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // A child that left the group may still hold the pipe: what was read by
    // then is enough.
    let _ = done_rx.recv_timeout(Duration::from_secs(10));
    let output =
        String::from_utf8_lossy(&buf.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
    Ok(ChildEnd {
        code: status.code(),
        output,
        timed_out,
    })
}

/// Mount the restored system as `req` says, run its tool inside it, check
/// what it built, unmount.
pub fn rebuild(req: &Request) -> Rebuilt {
    let nodes: Vec<&str> = std::iter::once(req.root_node.as_str())
        .chain(req.mounts.iter().map(|m| m.node.as_str()))
        .collect();
    let wanted = uuid_links(&nodes);
    // Removed (and the devices announced again) when the rebuild is over.
    let _links = match crate::udev_hold_linux::UuidLinks::install(&wanted) {
        Ok(l) => Some(l),
        Err(e) => {
            warn!(error = %e, "by-uuid links for the rebuild not installed; the check decides");
            None
        }
    };
    for (k, uuid) in &wanted {
        let to = std::fs::canonicalize(Path::new("/dev/disk/by-uuid").join(uuid)).ok();
        if to.as_deref() == Some(&Path::new("/dev").join(k)) {
            info!(device = %k, uuid, "by-uuid link for the rebuild");
        } else {
            warn!(device = %k, uuid, "udev made no by-uuid link; the check decides");
        }
    }
    let mut stack = match MountStack::new() {
        Ok(s) => s,
        Err(e) => {
            return Rebuilt::Failed {
                error: e.to_string(),
                output: String::new(),
            }
        }
    };
    let result = rebuild_in(&mut stack, req);
    let released = stack.release();
    match (result, released) {
        (Ok(done), Ok(())) => done,
        (Ok(Rebuilt::Done { output, .. }), Err(e)) => Rebuilt::Failed { error: e, output },
        (Ok(failed), Err(e)) => {
            warn!(error = %e, "the rebuild's mounts did not all unmount");
            failed
        }
        (Err(error), _) => Rebuilt::Failed {
            error,
            output: String::new(),
        },
    }
}

fn rebuild_in(stack: &mut MountStack, req: &Request) -> Result<Rebuilt, String> {
    let top = stack.base.join("root");
    std::fs::create_dir(&top).map_err(|e| format!("{}: {e}", top.display()))?;
    stack.mount(
        &[
            "-t",
            &req.root_fstype,
            "-o",
            &format!("{},nosuid,nodev", req.root_opts),
            &req.root_node,
        ],
        &top,
    )?;
    let sysroot = match req.subdir.as_os_str().is_empty() {
        true => top.clone(),
        false => real_dir(&top, &req.subdir.to_string_lossy())?,
    };
    for m in &req.mounts {
        let at = real_dir(&sysroot, &m.target)?;
        stack.mount(
            &[
                "-t",
                &m.fstype,
                "-o",
                &format!("{},nosuid,nodev", m.opts),
                &m.node,
            ],
            &at,
        )?;
    }
    // The kernel's views the tools read: a /proc and a read-only /sys of
    // their own, this machine's /dev read-only (the device nodes still
    // work). Not this machine's /run — it holds its control sockets
    // (systemd, D-Bus, this service); an empty tmpfs instead.
    let p = real_dir(&sysroot, "proc")?;
    stack.mount(&["-t", "proc", "-o", "nosuid,nodev,noexec", "proc"], &p)?;
    let p = real_dir(&sysroot, "sys")?;
    stack.mount(
        &["-t", "sysfs", "-o", "ro,nosuid,nodev,noexec", "sysfs"],
        &p,
    )?;
    let p = real_dir(&sysroot, "dev")?;
    stack.mount(&["-o", "bind,ro", "/dev"], &p)?;
    if let Ok(p) = real_dir(&sysroot, "run") {
        stack.mount(
            &[
                "-t",
                "tmpfs",
                "-o",
                "nosuid,nodev,mode=0755,size=64m",
                "tmpfs",
            ],
            &p,
        )?;
    }

    let before = images(&sysroot);
    let backup = Backup::take(&sysroot, before.keys())
        .map_err(|e| tr!("initramfs_linux.backup_failed", error = e).into_owned())?;
    let ran = run_tool(&sysroot, req, &before);
    Ok(finish(
        &sysroot,
        &before,
        backup,
        ran,
        &image::MachineDevices::read(),
    ))
}

/// Keep what the tool built only when it rewrote images and they pass
/// [`check_images`]; otherwise put the previous images back (`before`, from
/// `backup`) and say so in the error.
fn finish(
    sysroot: &Path,
    before: &BTreeMap<String, ImageStamp>,
    backup: Backup,
    ran: Rebuilt,
    devices: &image::MachineDevices,
) -> Rebuilt {
    let checked = match ran {
        Rebuilt::Done { images, output } => {
            match check_images(sysroot, &images, &backup, devices) {
                Ok(()) => Rebuilt::Done { images, output },
                Err(error) => Rebuilt::Failed { error, output },
            }
        }
        failed => failed,
    };
    match checked {
        Rebuilt::Done { images, output } => {
            backup.discard();
            info!(images = ?images, "initramfs rebuilt");
            Rebuilt::Done { images, output }
        }
        Rebuilt::Failed { error, output } => {
            let error = match backup.put_back(sysroot, before) {
                Ok(()) => tr!("initramfs_linux.previous_restored", error = error),
                Err(e) => tr!(
                    "initramfs_linux.previous_not_restored",
                    error = error,
                    restore_error = e
                ),
            };
            Rebuilt::Failed {
                error: error.into_owned(),
                output,
            }
        }
    }
}

/// Run the system's tool in `sysroot` and say whether it rewrote an image
/// (of those `before` lists).
fn run_tool(sysroot: &Path, req: &Request, before: &BTreeMap<String, ImageStamp>) -> Rebuilt {
    info!(program = %req.program, args = ?req.tool.args(), root = %sysroot.display(), "running the restored system's initramfs tool");
    let end = match run_in_root(sysroot, &req.program, req.tool.args(), TIMEOUT) {
        Ok(end) => end,
        Err(e) => {
            return Rebuilt::Failed {
                error: format!("{}: {e}", req.program),
                output: String::new(),
            }
        }
    };
    for line in end.output.lines() {
        info!(tool = req.tool.name(), "{line}");
    }
    if end.timed_out {
        return Rebuilt::Failed {
            error: tr!("initramfs_linux.timeout", minutes = TIMEOUT.as_secs() / 60).into_owned(),
            output: end.output,
        };
    }
    if end.code != Some(0) {
        return Rebuilt::Failed {
            error: tr!(
                "initramfs_linux.exit_code",
                code = end
                    .code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            )
            .into_owned(),
            output: end.output,
        };
    }
    let after = images(sysroot);
    let rewritten: Vec<String> = after
        .iter()
        .filter(|(p, v)| before.get(*p) != Some(v))
        .map(|(p, _)| p.clone())
        .collect();
    if rewritten.is_empty() {
        return Rebuilt::Failed {
            error: tr!("initramfs_linux.no_image_rewritten").into_owned(),
            output: end.output,
        };
    }
    Rebuilt::Done {
        images: rewritten,
        output: end.output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str =
        "PRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\nNAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";

    fn renamed(kind: IdentityKind, old: &str, new: &str) -> Renamed {
        Renamed {
            kind,
            old: old.into(),
            new: new.into(),
        }
    }

    #[test]
    fn os_release_decides_the_family_and_the_tool() {
        let os = parse_os_release(UBUNTU);
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.shown(), "Ubuntu 26.04.1 LTS");
        assert_eq!(family(&os), Family::Debian);
        let fam = |t: &str| family(&parse_os_release(t));
        assert_eq!(
            fam("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            Family::Debian
        );
        assert_eq!(fam("ID=fedora\n"), Family::RedHat);
        assert_eq!(
            fam("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
            Family::RedHat
        );
        assert_eq!(
            fam("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"),
            Family::Suse
        );
        assert_eq!(fam("ID=endeavouros\nID_LIKE=arch\n"), Family::Arch);
        assert_eq!(fam("ID=nixos\n"), Family::Unknown);
        assert_eq!(fam("ID='gentoo'\n"), Family::Unknown);
    }

    #[test]
    fn the_command_is_the_familys_own_and_must_be_installed() {
        let all = |t: Tool| Some(format!("/usr/bin/{}", t.name()));
        assert_eq!(
            select_tool(Family::Debian, all),
            Some((Tool::UpdateInitramfs, "/usr/bin/update-initramfs".into()))
        );
        assert_eq!(select_tool(Family::RedHat, all).unwrap().0, Tool::Dracut);
        assert_eq!(select_tool(Family::Arch, all).unwrap().0, Tool::Mkinitcpio);
        // Arch with dracut instead of mkinitcpio.
        let dracut_only = |t: Tool| (t == Tool::Dracut).then(|| "/usr/bin/dracut".to_string());
        assert_eq!(
            select_tool(Family::Arch, dracut_only).unwrap().0,
            Tool::Dracut
        );
        // A Debian without update-initramfs, an unknown system: nothing.
        assert_eq!(select_tool(Family::Debian, dracut_only), None);
        assert_eq!(select_tool(Family::Unknown, all), None);
        assert_eq!(
            Tool::UpdateInitramfs.command(),
            "update-initramfs -u -k all"
        );
        assert_eq!(Tool::Dracut.command(), "dracut --regenerate-all --force");
        assert_eq!(Tool::Mkinitcpio.command(), "mkinitcpio -P");
    }

    const NEW: &str = "99999999-8888-4777-8666-555555555555";
    const OLD: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    fn sys() -> SystemConfig {
        SystemConfig {
            os: parse_os_release(UBUNTU),
            fstab: format!("UUID={NEW} / ext4 defaults 0 1\n"),
            tool: Some((Tool::UpdateInitramfs, "/usr/sbin/update-initramfs".into())),
            ..Default::default()
        }
    }

    /// A label is in the image only where the image copies text naming it:
    /// the resume device, `/etc/kernel/cmdline`, dracut's command line.
    #[test]
    fn a_new_label_the_image_copies_needs_a_new_image() {
        let l = [renamed(
            IdentityKind::Label,
            "cloudimg-rootfs",
            "cloudimg-rootf-r",
        )];
        let fstab_only = SystemConfig {
            fstab: "LABEL=cloudimg-rootf-r / ext4 defaults 0 1\n".into(),
            ..sys()
        };
        assert!(reasons(&l, false, &fstab_only).is_empty());
        let resume = SystemConfig {
            resume: "RESUME=LABEL=cloudimg-rootf-r\n".into(),
            ..sys()
        };
        assert_eq!(reasons(&l, false, &resume), vec![Reason::Embedded]);
        let cmdline = SystemConfig {
            kernel_cmdline: "root=LABEL=\"cloudimg-rootf-r\" ro\n".into(),
            ..sys()
        };
        assert_eq!(reasons(&l, false, &cmdline), vec![Reason::Embedded]);
        // Another label that starts the same is not this one.
        let other = SystemConfig {
            kernel_cmdline: "root=LABEL=cloudimg-rootf-r2 ro\n".into(),
            ..sys()
        };
        assert!(reasons(&l, false, &other).is_empty());
    }

    #[test]
    fn which_changes_need_a_new_image() {
        // A root filesystem's UUID alone: the kernel command line comes from
        // the boot loader (already fixed), not from the image.
        let root = [renamed(IdentityKind::Ext, OLD, NEW)];
        assert!(reasons(&root, true, &sys()).is_empty());

        // ... unless the image copies it: a resume device, a unified image's
        // command line, dracut's kernel_cmdline.
        let mut s = sys();
        s.resume = format!("RESUME=UUID={NEW}\n");
        assert_eq!(reasons(&root, true, &s), vec![Reason::Embedded]);
        let mut s = sys();
        s.kernel_cmdline = format!("root=UUID={} rw\n", NEW.to_ascii_uppercase());
        assert_eq!(reasons(&root, true, &s), vec![Reason::Embedded]);
        let mut s = sys();
        s.dracut = format!("kernel_cmdline=\"resume=UUID={NEW}\"\n");
        assert_eq!(reasons(&root, true, &s), vec![Reason::Embedded]);

        // ... or dracut builds host-only images, which record the root,
        // /boot and the ESP.
        let mut s = sys();
        s.dracut = "hostonly=\"yes\"\nhostonly_cmdline=\"no\"\n".into();
        s.dracut_installed = true;
        s.dracut_version = Some(59);
        assert_eq!(reasons(&root, true, &s), vec![Reason::Hostonly]);
        assert!(reasons(&root, false, &s).is_empty(), "not the root's");
        s.dracut = "hostonly=\"yes\"\nhostonly=\"no\"\n".into();
        assert!(
            reasons(&root, true, &s).is_empty(),
            "the last assignment counts"
        );
        // Not set: dracut-ng 109 and later (Ubuntu 26.04 has 110) build
        // host-only images by default.
        s.dracut = String::new();
        assert!(reasons(&root, true, &s).is_empty(), "dracut 059: generic");
        s.dracut_version = Some(110);
        assert_eq!(reasons(&root, true, &s), vec![Reason::Hostonly]);
        s.dracut_version = None;
        assert_eq!(reasons(&root, true, &s), vec![Reason::Hostonly]);

        // LUKS: only a container the system unlocks from crypttab.
        let luks = [renamed(IdentityKind::Luks, OLD, NEW)];
        assert!(reasons(&luks, false, &sys()).is_empty(), "a data container");
        let mut s = sys();
        s.crypttab = format!("dm_crypt-0 UUID={NEW} none luks\n");
        assert_eq!(reasons(&luks, false, &s), vec![Reason::Crypttab]);

        // md: always — the image has mdadm.conf, or one generated when it
        // was built.
        let md = [renamed(
            IdentityKind::Md,
            "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d",
            "11111111:22222222:33333333:44444444",
        )];
        assert_eq!(reasons(&md, false, &sys()), vec![Reason::Md]);
        // Its dashed spelling in a resume line is found too.
        let mut s = sys();
        s.resume = "RESUME=UUID=11111111-2222-2222-3333-333344444444\n".into();
        assert_eq!(reasons(&md, false, &s), vec![Reason::Md, Reason::Embedded]);

        // A renamed volume group the system uses; LVM UUIDs never count.
        let vg = [
            renamed(IdentityKind::LvmVgName, "ubuntu-vg", "ubuntu-vg_restored"),
            renamed(IdentityKind::LvmVg, "aaaa", "bbbb"),
            renamed(IdentityKind::LvmPv, "cccc", "dddd"),
        ];
        assert!(
            reasons(&vg, false, &sys()).is_empty(),
            "not used by this system"
        );
        let mut s = sys();
        s.fstab = "/dev/mapper/ubuntu--vg_restored-ubuntu--lv / ext4 defaults 0 1\n".into();
        assert_eq!(reasons(&vg, false, &s), vec![Reason::VolumeGroup]);
        let mut s = sys();
        s.fstab = "/dev/mapper/ubuntu--vg_restored2-root / ext4 defaults 0 1\n".into();
        assert!(
            reasons(&vg, false, &s).is_empty(),
            "another group's name is not this one"
        );
    }

    #[test]
    fn dracut_hostonly_follows_its_configuration_then_its_version() {
        assert!(!dracut_hostonly("", Some(108)));
        assert!(dracut_hostonly("", Some(109)));
        assert!(dracut_hostonly("hostonly=yes\n", Some(59)));
        assert!(!dracut_hostonly("hostonly='no'\n", Some(110)));
        assert_eq!(
            dracut_option("add_dracutmodules+=\" crypt \"\n", "add_dracutmodules"),
            None
        );
        assert_eq!(
            parse_dracut_version("#!/bin/bash\nDRACUT_VERSION=\"110-11ubuntu0.1\"\n"),
            Some(110)
        );
        assert_eq!(parse_dracut_version("DRACUT_VERSION=059\n"), Some(59));
        assert_eq!(parse_dracut_version("echo $DRACUT_VERSION\n"), None);
    }

    fn cand(
        node: &str,
        fstype: &str,
        uuid: &str,
        label: Option<&str>,
        partuuid: Option<&str>,
    ) -> Candidate {
        Candidate {
            node: node.into(),
            fstype: fstype.into(),
            uuid_key: Identity::new(IdentityKind::Ext, uuid).key,
            label: label.map(str::to_string),
            partuuid: partuuid.map(str::to_string),
        }
    }

    /// The restored fstab names the NEW identities; each resolves to the
    /// restored node carrying it — and nothing on this machine is asked.
    #[test]
    fn the_boot_mounts_come_from_the_restored_fstab_and_the_restored_nodes() {
        let boot = "77777777-1111-4222-8333-444444444444";
        let fstab = format!(
            "# /etc/fstab\n\
             UUID={NEW} / ext4 errors=remount-ro 0 1\n\
             UUID={} /boot ext4 defaults 0 2\n\
             UUID=0F0E-0D0C  /boot/efi\tvfat umask=0077 0 1\n\
             /swap.img none swap sw 0 0\n",
            boot.to_ascii_uppercase()
        );
        let candidates = [
            cand("/dev/sdb1", "ext4", NEW, None, None),
            cand("/dev/sdb16", "ext4", boot, None, None),
            cand("/dev/sdb15", "vfat", "0F0E-0D0C", None, None),
        ];
        let plan = mount_plan(&fstab, "/dev/sdb1", &candidates).unwrap();
        assert_eq!(
            plan,
            vec![
                PlannedMount {
                    target: "/boot".into(),
                    node: "/dev/sdb16".into(),
                    fstype: "ext4".into(),
                    opts: "rw".into()
                },
                PlannedMount {
                    target: "/boot/efi".into(),
                    node: "/dev/sdb15".into(),
                    fstype: "vfat".into(),
                    opts: "rw".into()
                },
            ]
        );

        // The Ubuntu cloud image names them by LABEL: the labels of the
        // restored devices, read from them.
        let fstab = "LABEL=cloudimg-rootfs / ext4 defaults 0 1\nLABEL=BOOT /boot ext4 defaults 0 2\nLABEL=UEFI /boot/efi vfat umask=0077 0 1\n";
        let candidates = [
            cand("/dev/vdb1", "ext4", NEW, Some("cloudimg-rootfs"), None),
            cand("/dev/vdb16", "ext4", boot, Some("BOOT"), None),
            cand("/dev/vdb15", "vfat", "0F0E-0D0C", Some("UEFI"), None),
        ];
        let plan = mount_plan(fstab, "/dev/vdb1", &candidates).unwrap();
        let nodes: Vec<&str> = plan.iter().map(|m| m.node.as_str()).collect();
        assert_eq!(nodes, vec!["/dev/vdb16", "/dev/vdb15"]);

        // A /boot not among the restored filesystems: no rebuild into the
        // root's empty directory.
        let fstab =
            format!("UUID={NEW} / ext4 defaults 0 1\nPARTUUID=abcd-02 /boot ext4 defaults 0 2\n");
        assert_eq!(
            mount_plan(&fstab, "/dev/sdb1", &candidates[..1]),
            Err("PARTUUID=abcd-02".to_string())
        );
        // ... and an ESP that is not: left out.
        let fstab =
            format!("UUID={NEW} / ext4 defaults 0 1\nUUID=1234-ABCD /boot/efi vfat defaults 0 1\n");
        assert_eq!(
            mount_plan(&fstab, "/dev/sdb1", &candidates[..1]),
            Ok(Vec::new())
        );
        // Two restored filesystems with one label: ambiguous, not guessed.
        let twins = [
            cand("/dev/sdb2", "ext4", boot, Some("BOOT"), None),
            cand("/dev/sdc2", "ext4", NEW, Some("BOOT"), None),
        ];
        assert!(mount_plan("LABEL=BOOT /boot ext4 defaults 0 2\n", "/dev/sdb1", &twins).is_err());
    }

    #[test]
    fn lvm_btrfs_and_partuuid_sources_resolve_too() {
        let candidates = [
            Candidate {
                node: "/dev/mapper/vg_restored-root".into(),
                fstype: "btrfs".into(),
                uuid_key: Identity::new(IdentityKind::Btrfs, NEW).key,
                label: None,
                partuuid: None,
            },
            cand("/dev/sdb2", "xfs", OLD, None, Some("0a0b0c0d-02")),
        ];
        let fstab = "/dev/vg_restored/root / btrfs subvol=@ 0 0\n\
                     /dev/mapper/vg_restored-root /boot btrfs subvol=@boot,compress=zstd 0 0\n\
                     PARTUUID=0A0B0C0D-02 /efi xfs defaults 0 0\n";
        let plan = mount_plan(fstab, "/dev/mapper/vg_restored-root", &candidates).unwrap();
        assert_eq!(
            plan[0].opts, "rw,subvol=@boot",
            "the same btrfs, its own subvolume"
        );
        assert_eq!(plan[1].node, "/dev/sdb2");
        assert_eq!(plan[1].opts, "rw,nouuid");
        assert_eq!(fstab_unescape("/boot\\040x"), "/boot x");
    }

    #[test]
    fn links_resolve_inside_the_restored_system_never_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("usr/lib/os-release"), UBUNTU).unwrap();
        std::os::unix::fs::symlink("../usr/lib/os-release", root.join("etc/os-release")).unwrap();
        assert_eq!(
            resolve_in_root(&root, "etc/os-release"),
            Some(root.join("usr/lib/os-release"))
        );
        // An absolute link means the restored system's root, not this one's.
        std::fs::write(dir.path().join("host-fstab"), "host\n").unwrap();
        std::os::unix::fs::symlink(dir.path().join("host-fstab"), root.join("etc/fstab")).unwrap();
        assert_eq!(read_text(&root, "etc/fstab"), None);
        std::os::unix::fs::symlink("/../../../etc/hostname", root.join("etc/up")).unwrap();
        assert_eq!(
            resolve_in_root(&root, "etc/up"),
            None,
            "`..` stops at its root"
        );
        std::os::unix::fs::symlink("loop", root.join("etc/loop")).unwrap();
        assert_eq!(resolve_in_root(&root, "etc/loop"), None);
        // No fstab: not a system.
        assert_eq!(find_systems(&root, false), Vec::new());
        std::fs::remove_file(root.join("etc/fstab")).unwrap();
        std::fs::write(root.join("etc/fstab"), "UUID=x / ext4 defaults 0 1\n").unwrap();
        let found = find_systems(&root, false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.os.id, "ubuntu");
        assert_eq!(found[0].1.tool, None, "update-initramfs is not installed");
    }

    #[test]
    fn a_system_in_a_btrfs_subvolume_with_its_tool_and_dracut_config() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        let root = top.join("@");
        for d in [
            "etc/dracut.conf.d",
            "usr/lib/dracut/dracut.conf.d",
            "usr/bin",
            "usr/sbin",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink("usr/bin", root.join("bin")).unwrap();
        std::fs::write(
            root.join("etc/os-release"),
            "ID=fedora\nPRETTY_NAME=\"Fedora Linux 44\"\n",
        )
        .unwrap();
        std::fs::write(root.join("etc/fstab"), "UUID=x / btrfs subvol=@ 0 0\n").unwrap();
        std::fs::write(
            root.join("usr/lib/dracut/dracut.conf.d/01-dist.conf"),
            "hostonly=\"yes\"\nhostonly_cmdline=\"no\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("usr/lib/dracut/dracut.conf.d/90-x.conf"),
            "hostonly_cmdline=\"no\"\n",
        )
        .unwrap();
        // /etc replaces the same name in /usr/lib.
        std::fs::write(
            root.join("etc/dracut.conf.d/90-x.conf"),
            "hostonly_cmdline=\"yes\"\n",
        )
        .unwrap();
        let dracut = root.join("usr/bin/dracut");
        std::fs::write(&dracut, "#!/bin/bash\nDRACUT_VERSION=\"110\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dracut, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            find_systems(top, false).is_empty(),
            "the top level is not the system"
        );
        let found = find_systems(top, true);
        assert_eq!(found.len(), 1);
        let (sub, s) = &found[0];
        assert_eq!(sub, Path::new("@"));
        assert_eq!(s.tool, Some((Tool::Dracut, "/usr/bin/dracut".into())));
        assert_eq!(s.dracut_version, Some(110));
        assert!(s.dracut_installed && dracut_hostonly(&s.dracut, None));
        assert_eq!(
            dracut_option(&s.dracut, "hostonly_cmdline").as_deref(),
            Some("yes"),
            "the /etc drop-in wins"
        );
    }

    #[test]
    fn an_image_counts_as_rewritten_by_time_inode_or_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("boot/efi/abc/6.1")).unwrap();
        std::fs::write(root.join("boot/initrd.img-7.0.0-34-generic"), "old").unwrap();
        std::fs::write(root.join("boot/vmlinuz-7.0.0-34-generic"), "k").unwrap();
        std::fs::write(root.join("boot/efi/abc/6.1/initrd"), "x").unwrap();
        std::os::unix::fs::symlink("initrd.img-7.0.0-34-generic", root.join("boot/initrd.img"))
            .unwrap();
        let before = images(root);
        assert_eq!(
            before.keys().cloned().collect::<Vec<_>>(),
            vec![
                "/boot/efi/abc/6.1/initrd".to_string(),
                "/boot/initrd.img-7.0.0-34-generic".to_string()
            ]
        );
        let tmp = root.join("boot/.new");
        std::fs::write(&tmp, "a new image").unwrap();
        std::fs::rename(&tmp, root.join("boot/initrd.img-7.0.0-34-generic")).unwrap();
        let after = images(root);
        let changed: Vec<&String> = after
            .iter()
            .filter(|(p, v)| before.get(*p) != Some(v))
            .map(|(p, _)| p)
            .collect();
        assert_eq!(changed, vec!["/boot/initrd.img-7.0.0-34-generic"]);
    }

    /// The stand's failure, decided: a rebuilt image waiting for the kernel
    /// names the restored /boot and ESP had here is not kept — the previous
    /// image is put back — and one waiting for by-uuid links is. A failed
    /// tool's images are put back too, and an image it created goes.
    #[test]
    fn a_rebuilt_image_naming_this_machines_devices_is_replaced_by_the_previous_one() {
        use image::fixtures::waiting_for;
        let devices = image::MachineDevices {
            paths: ["/dev/vdb", "/dev/vdb13", "/dev/vdb15", "/dev/block/252:29"]
                .map(String::from)
                .to_vec(),
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("boot/efi")).unwrap();
        let img = root.join("boot/initrd.img-7.0.0-15-generic");
        let rel = "/boot/initrd.img-7.0.0-15-generic".to_string();
        let old = waiting_for(&["/dev/disk/by-uuid/1a2b"]);
        let by_uuid = waiting_for(&["/dev/disk/by-uuid/7777", "/dev/disk/by-uuid/0F0E-0D0C"]);
        let by_name = waiting_for(&["/dev/vdb13", "/dev/vdb15"]);
        let copies = |root: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(root.join("boot"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(PREV_SUFFIX))
                .collect();
            v.sort();
            v
        };
        let rebuild = |new: Option<&[u8]>, extra: Option<&str>, ok: bool| {
            std::fs::write(&img, &old).unwrap();
            let before = images(root);
            let backup = Backup::take(root, before.keys()).unwrap();
            assert_eq!(copies(root), vec!["initrd.img-7.0.0-15-generic.obk-prev"]);
            // The tool writes a new file and renames it over the image.
            let mut rewritten = Vec::new();
            if let Some(new) = new {
                let tmp = root.join("boot/.tmp");
                std::fs::write(&tmp, new).unwrap();
                std::fs::rename(&tmp, &img).unwrap();
                rewritten.push(rel.clone());
            }
            if let Some(extra) = extra {
                std::fs::write(root.join("boot").join(extra), &by_name).unwrap();
                rewritten.push(format!("/boot/{extra}"));
            }
            let ran = if ok {
                Rebuilt::Done {
                    images: rewritten,
                    output: "dracut: done".into(),
                }
            } else {
                Rebuilt::Failed {
                    error: "exit code 1".into(),
                    output: String::new(),
                }
            };
            finish(root, &before, backup, ran, &devices)
        };

        match rebuild(Some(&by_uuid), None, true) {
            Rebuilt::Done { images, .. } => assert_eq!(images, vec![rel.clone()]),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&img).unwrap(), by_uuid, "kept");
        assert!(copies(root).is_empty());

        match rebuild(Some(&by_name), None, true) {
            Rebuilt::Failed { error, output } => {
                assert!(
                    error.contains("/dev/vdb13") && error.contains("/dev/vdb15"),
                    "{error}"
                );
                assert!(
                    error.contains("прежние образы возвращены")
                        || error.contains("previous images were put back"),
                    "{error}"
                );
                assert_eq!(output, "dracut: done");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&img).unwrap(), old, "put back");
        assert!(copies(root).is_empty());

        // Unreadable: not kept either.
        match rebuild(Some(b"\x7fELF"), None, true) {
            Rebuilt::Failed { error, .. } => assert!(error.contains(&rel), "{error}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&img).unwrap(), old);

        // The tool failed half-way: the image it rewrote is put back, the
        // one it created is removed.
        match rebuild(Some(&by_uuid), Some("initrd.img-7.1.0-1-generic"), false) {
            Rebuilt::Failed { error, .. } => assert!(error.contains("exit code 1"), "{error}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&img).unwrap(), old);
        assert!(!root.join("boot/initrd.img-7.1.0-1-generic").exists());
        assert!(copies(root).is_empty());
        assert_eq!(images(root).len(), 1, "a copy is not an image");
    }

    #[test]
    fn the_by_uuid_name_is_a_filesystems_or_a_containers_uuid() {
        let export = |usage: &str, uuid: &str| {
            format!("DEVNAME=/dev/vdb15\nUUID={uuid}\nBLOCK_SIZE=512\nTYPE=vfat\nUSAGE={usage}\nPART_ENTRY_UUID=0a0b\n")
        };
        assert_eq!(
            by_uuid_name(&export("filesystem", "0F0E-0D0C")).as_deref(),
            Some("0F0E-0D0C")
        );
        assert_eq!(
            by_uuid_name(&export("crypto", "1a2b")).as_deref(),
            Some("1a2b")
        );
        assert_eq!(
            by_uuid_name(&export("raid", "1a2b")),
            None,
            "md members share theirs"
        );
        assert_eq!(by_uuid_name(&export("filesystem", "")), None);
        assert_eq!(by_uuid_name("PART_ENTRY_UUID=0a0b\n"), None);
    }

    #[test]
    fn a_tail_is_the_last_non_empty_lines() {
        assert_eq!(tail("a\n\nb\n c \nd\n", 3), "b / c / d");
        assert_eq!(tail("", 3), "");
    }

    /// As root, or in a user namespace (`unshare -r`): the program runs
    /// inside the root it is given, with the environment set here, its
    /// output (both streams) captured, and a hang killed with its group.
    #[test]
    #[ignore = "root or `unshare -r`: chroot"]
    fn the_tool_runs_inside_the_root_and_a_hang_is_killed() {
        let busybox = Path::new("/usr/bin/busybox");
        assert!(busybox.exists(), "needs a static busybox");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::copy(busybox, root.join("bin/busybox")).unwrap();
        std::fs::write(root.join("inside"), "yes\n").unwrap();
        let end = run_in_root(
            root,
            "/bin/busybox",
            &[
                "sh",
                "-c",
                "/bin/busybox cat /inside; echo \"$PATH $LC_ALL\" >&2; exit 7",
            ],
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(end.code, Some(7));
        assert!(!end.timed_out);
        assert!(end.output.contains("yes"), "{}", end.output);
        assert!(
            end.output.contains("/usr/sbin:/usr/bin:/sbin:/bin C"),
            "{}",
            end.output
        );
        let start = Instant::now();
        let end = run_in_root(
            root,
            "/bin/busybox",
            &[
                "sh",
                "-c",
                "/bin/busybox sleep 600 & /bin/busybox sleep 600",
            ],
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(end.timed_out && end.code.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the group was killed"
        );
    }

    /// As root: a tiny system (a static busybox and a fake tool) on loop
    /// devices — root and a separate /boot — is mounted, the tool runs inside
    /// it and rewrites the image on /boot with one that passes the check, and
    /// nothing stays mounted, no copy and no link rule is left.
    #[test]
    #[ignore = "root: loop devices, mount, chroot"]
    fn real_chroot_rebuild_writes_the_image_on_the_restored_boot() {
        let busybox = Path::new("/usr/bin/busybox");
        assert!(busybox.exists(), "needs a static busybox");
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        for d in [
            "etc", "usr/sbin", "bin", "boot", "proc", "sys", "dev", "run", "tmp",
        ] {
            std::fs::create_dir_all(tree.join(d)).unwrap();
        }
        std::fs::copy(busybox, tree.join("bin/busybox")).unwrap();
        std::os::unix::fs::symlink("busybox", tree.join("bin/sh")).unwrap();
        std::fs::write(tree.join("etc/os-release"), UBUNTU).unwrap();
        std::fs::write(
            tree.join("etc/fstab"),
            "LABEL=root / ext4 defaults 0 1\nLABEL=boot /boot ext4 defaults 0 2\n",
        )
        .unwrap();
        let tool = tree.join("usr/sbin/update-initramfs");
        std::fs::write(
            &tool,
            "#!/bin/sh\n/bin/busybox echo \"args: $*\"\n/bin/busybox test -e /proc/self/mounts || exit 3\n/bin/busybox cp /etc/new-initrd /boot/initrd.img-1.tmp && /bin/busybox mv /boot/initrd.img-1.tmp /boot/initrd.img-1\n",
        )
        .unwrap();
        std::fs::write(
            tree.join("etc/new-initrd"),
            image::fixtures::waiting_for(&["/dev/disk/by-uuid/1a2b"]),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let boot_tree = dir.path().join("boot");
        std::fs::create_dir_all(&boot_tree).unwrap();
        std::fs::write(
            boot_tree.join("initrd.img-1"),
            image::fixtures::waiting_for(&[]),
        )
        .unwrap();
        let img = |name: &str, src: &Path| {
            let p = dir.path().join(name);
            std::fs::File::create(&p)
                .unwrap()
                .set_len(64 << 20)
                .unwrap();
            let ok = Command::new("mkfs.ext4")
                .args(["-q", "-F", "-d"])
                .arg(src)
                .arg(&p)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            let out = Command::new("losetup")
                .args(["-f", "--show"])
                .arg(&p)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let root_dev = img("root.img", &tree);
        let boot_dev = img("boot.img", &boot_tree);
        let req = Request {
            root_node: root_dev.clone(),
            root_fstype: "ext4".into(),
            root_opts: "rw".into(),
            subdir: PathBuf::new(),
            mounts: vec![PlannedMount {
                target: "/boot".into(),
                node: boot_dev.clone(),
                fstype: "ext4".into(),
                opts: "rw".into(),
            }],
            tool: Tool::UpdateInitramfs,
            program: "/usr/sbin/update-initramfs".into(),
        };
        let result = rebuild(&req);
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let left: Vec<_> = std::fs::read_dir(crate::udev_hold_linux::RULES_DIR)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.contains("-links"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        let boot_mnt = dir.path().join("boot-mnt");
        std::fs::create_dir_all(&boot_mnt).unwrap();
        assert!(Command::new("mount")
            .args(["-o", "ro", &boot_dev])
            .arg(&boot_mnt)
            .status()
            .unwrap()
            .success());
        let copies = boot_mnt.join(format!("initrd.img-1{PREV_SUFFIX}")).exists();
        let _ = Command::new("umount").arg(&boot_mnt).status();
        assert!(!copies, "the copy of the previous image is gone");
        let _ = Command::new("losetup")
            .args(["-d", &root_dev, &boot_dev])
            .status();
        match result {
            Rebuilt::Done { images, output } => {
                assert_eq!(images, vec!["/boot/initrd.img-1".to_string()]);
                assert!(output.contains("args: -u -k all"), "{output}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!mounts.contains("/run/own-backup/initramfs-"), "{mounts}");
    }
}
