//! Giving a restored copy identities of its own (PLAN §6, stage 3) — what
//! `identity_choice: change` does after the data is written and checked.
//!
//! `identity_check_linux` found what the copy would duplicate on this
//! machine; the person chose to change it. Each restored device is read again
//! (what is on the disk now, not what the copy said), and every identity this
//! machine already carries elsewhere is replaced by a fresh one with the
//! subsystem's own tool:
//!
//! | what      | how                                                              |
//! |-----------|------------------------------------------------------------------|
//! | ext2/3/4  | `e2fsck -f -p` (replays the journal), `tune2fs -U <new>`         |
//! | XFS       | mount `nouuid` and unmount (replays the log), `xfs_admin -U`     |
//! | btrfs     | `btrfstune -f -U <new>` — rewrites the fsid everywhere, so GRUB and kernels without `metadata_uuid` read it; single-device only |
//! | FAT       | `fatlabel -i <dev> <serial>`                                     |
//! | LUKS      | `cryptsetup luksUUID --uuid <new>`                               |
//! | LVM       | `vgimportclone --basevgname <vg>_restored` — new PV and VG UUIDs and a new name |
//! | md        | `mdadm --assemble --update=uuid --uuid=<new>` on the restored members, then stop |
//! | label     | the old label with `-r` (`-R` on FAT; `-r2`, `-r3`… while taken), shortened to fit: `tune2fs -L` (ext, 16 bytes, after the same `e2fsck`), `xfs_admin -L` (XFS, 12, after the same log replay), `btrfs filesystem label` (btrfs, 255, single-device only), `fatlabel` (FAT, 11, upper case) |
//!
//! What lies inside a container is a device too: a LUKS container whose
//! passphrase came with the restore is opened, a renamed volume group is
//! activated, an array is assembled — and the filesystems found there are
//! treated the same way, with one difference: inside a container whose own
//! identity clashed, everything is a copy of what the machine's original
//! container holds, so it is changed whether or not that original is open
//! right now (a stopped array, an inactive volume group and a closed LUKS
//! container show nothing of their contents to the machine, yet starting
//! them later would meet the copy). Those stacked devices are created under a udev hold
//! of their own (`udev_hold_linux::dm_name_rule` and the others): until their
//! identities are changed they would otherwise get the machine's own
//! `/dev/disk/by-uuid` links. They are closed again before the run ends, in
//! reverse order.
//!
//! Then, if the person asked for it, the restored system is made to name its
//! devices by the new identities (`identity_refs_linux`): every restored
//! filesystem is mounted and its fstab, crypttab, mdadm.conf, GRUB and
//! systemd-boot files rewritten. When its initramfs keeps a copy of what
//! changed, the restored system's own tool rebuilds it (`initramfs_linux`)
//! while the stacked devices are still open; a rebuild that fails is a
//! follow-up for the person, not a failed run — the data is restored.
//!
//! Never changed: a GPT PARTUUID — `root=PARTUUID=` may name it, and the
//! restore keeps the copy's partition table as it is (reported as kept). A
//! btrfs spanning several devices, and a container whose passphrase is not
//! known, are reported too, not guessed at.
//!
//! The order matters for the machine, not only for the copy: all of this runs
//! while the restore's udev hold still covers the target, so nothing on the
//! machine reacts to the clone before it has become a different device.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::Path;

use tracing::{info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use crate::identity_clash::{Identity, IdentityKind, KernelVersion};
use crate::identity_refs_linux::{fix_tree, Renamed};
use crate::initramfs_linux as initramfs;
use crate::linux_tools::{args, run_checked, run_ok, Tools};

/// A device the restore wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredDevice {
    /// Its node (`/dev/sdb2`, or the restored volume itself).
    pub node: String,
    /// The partition number on the target, when it is one.
    pub part_index: Option<u32>,
    /// The LUKS passphrase the restore re-encrypted it with, if it did.
    pub passphrase: Option<String>,
}

/// One identity changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub kind: IdentityKind,
    pub old: String,
    pub new: String,
    /// The device it was changed on.
    pub node: String,
    pub part_index: Option<u32>,
}

/// What a run changed, edited, left and could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub changed: Vec<Changed>,
    /// Files rewritten, as `<device>:<path in its filesystem>`.
    pub edited: Vec<String>,
    /// Things the person should know: what was kept and why, what the
    /// restored system still has to do itself.
    pub notes: Vec<String>,
    /// Set when a step failed; everything before it stays done and listed.
    pub failed: Option<String>,
}

/// A filesystem mounted for editing.
pub trait MountedFs {
    fn path(&self) -> &Path;
    fn unmount(self: Box<Self>) -> Result<()>;
}

/// What a restored fstab may name a filesystem by besides its UUID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsNames {
    pub label: Option<String>,
    pub partuuid: Option<String>,
}

/// What the run needs from the machine besides its programs.
pub trait Host {
    /// The identities the device at `node` carries now. A device that cannot
    /// be read is an error, not a device without identities.
    fn identities_on(&mut self, node: &str) -> Result<Vec<Identity>>;
    /// A fresh random UUID, lowercase and dashed.
    fn new_uuid(&mut self) -> String;
    /// Mount `node` read-write as `fstype` with `opts`.
    fn mount(&mut self, node: &str, fstype: &str, opts: &str) -> Result<Box<dyn MountedFs>>;
    /// Install a udev hold made of `rules`; it lasts as long as the guard.
    fn hold(&mut self, rules: Vec<String>) -> Result<Box<dyn std::any::Any>>;
    /// Wait until udev has processed what is queued.
    fn settle(&mut self);
    /// A kernel name for a new md array (`md126`) that nothing uses.
    fn free_md_name(&mut self) -> String;
    /// The running kernel, for the btrfs temporary-fsid rule.
    fn kernel(&self) -> Option<KernelVersion>;
    /// How many devices the btrfs on `node` spans (`num_devices` in its
    /// superblock); 1 when it cannot be read — btrfstune then says.
    fn btrfs_devices(&mut self, node: &str) -> u64;
    /// The label and GPT partition UUID of the device at `node`, read from
    /// the device itself — never from what this machine's links resolve.
    fn fs_names(&mut self, node: &str) -> FsNames;
    /// No device-mapper device on the machine is called `name`.
    fn dm_name_free(&mut self, name: &str) -> bool;
    /// Rebuild a restored system's initramfs (`initramfs_linux::rebuild`).
    fn rebuild_initramfs(&mut self, req: &initramfs::Request) -> initramfs::Rebuilt;
}

/// What the stacked devices are, to close them again.
enum Opened {
    Luks(String),
    Vg(String),
    Md(String),
}

/// A stacked device to close, recorded before the command that creates it
/// runs: a half-done activation or assembly is undone too. `confirmed` once
/// that command succeeded — a close that fails for one that never came up is
/// not worth a note.
struct OpenedDev {
    what: Opened,
    confirmed: bool,
}

/// A device to look at.
#[derive(Debug, Clone)]
struct Dev {
    node: String,
    part_index: Option<u32>,
    passphrase: Option<String>,
    /// Inside a restored container whose own identity clashed: what it
    /// holds is the machine's too (see the module documentation).
    in_clashing_container: bool,
    /// The LUKS mapping this run opened that it lies in, innermost.
    luks_map: Option<String>,
}

/// A filesystem seen on a restored device, for the editing pass.
struct FsSeen {
    node: String,
    kind: IdentityKind,
    key: String,
    /// btrfs spanning more than one device (`Identity::multi_device`).
    multi_device: bool,
    luks_map: Option<String>,
}

/// A filesystem mounted by the editing pass, for the rebuild's mount plan.
struct MountedSeen {
    node: String,
    fstype: String,
    key: String,
}

/// A restored system found by the editing pass.
struct SystemSeen {
    /// Its root filesystem, and how the editing pass mounted it.
    node: String,
    fstype: String,
    opts: String,
    subdir: std::path::PathBuf,
    config: initramfs::SystemConfig,
    luks_map: Option<String>,
}

struct Run<'a> {
    tools: &'a mut dyn Tools,
    host: &'a mut dyn Host,
    /// The machine's identities, without the restored devices.
    present: &'a BTreeMap<String, Vec<Identity>>,
    out: Outcome,
    opened: Vec<OpenedDev>,
    holds: Vec<Box<dyn std::any::Any>>,
    filesystems: Vec<FsSeen>,
    /// The LUKS mappings this run opened, with the container's UUID key.
    luks_maps: Vec<(String, String)>,
    /// Prefix of the LUKS mappings this run opens.
    dm_prefix: String,
    dm_held: bool,
    seq: u32,
    /// Nodes whose journal or log this run already replayed.
    prepared: Vec<String>,
    /// Labels of the restored filesystems, with the node carrying each:
    /// those of every restored device are read before the walk, those of
    /// the devices it opens as it meets them. A new label must be none of
    /// them, nor one already given.
    labels: Vec<(String, String)>,
    /// New labels given by this run.
    given: Vec<String>,
    /// What the restored devices carried when read before the walk; the
    /// walk takes each from here instead of reading it again.
    pre_read: BTreeMap<String, Vec<Identity>>,
}

/// Change, on `devices`, every identity `present` (the machine's devices,
/// without those the restore wrote) already carries; with `fix_references`,
/// edit the restored system to match. Stacked devices are closed before it
/// returns, whatever happened.
pub fn change_identities(
    tools: &mut dyn Tools,
    host: &mut dyn Host,
    present: &BTreeMap<String, Vec<Identity>>,
    devices: &[RestoredDevice],
    fix_references: bool,
) -> Outcome {
    let mut run = Run {
        tools,
        host,
        present,
        out: Outcome::default(),
        opened: Vec::new(),
        holds: Vec::new(),
        filesystems: Vec::new(),
        luks_maps: Vec::new(),
        dm_prefix: format!("obk-reid-{}-", std::process::id()),
        dm_held: false,
        seq: 0,
        prepared: Vec::new(),
        labels: Vec::new(),
        given: Vec::new(),
        pre_read: BTreeMap::new(),
    };
    // Every restored device's labels before anything is relabelled: a new
    // label must not be one a device further on still carries. One that
    // cannot be read fails in the walk.
    for d in devices {
        if let Ok(ids) = run.host.identities_on(&d.node) {
            run.saw_labels(&d.node, &ids);
            run.pre_read.insert(d.node.clone(), ids);
        }
    }
    let devs: Vec<Dev> = devices
        .iter()
        .map(|d| Dev {
            node: d.node.clone(),
            part_index: d.part_index,
            passphrase: d.passphrase.clone(),
            in_clashing_container: false,
            luks_map: None,
        })
        .collect();
    let mut result = run.walk(devs);
    if result.is_ok() && fix_references && !run.out.changed.is_empty() {
        result = run.fix_references();
    }
    run.duplicate_label_notes();
    run.close_all();
    if let Err(e) = result {
        warn!(error = %e, "changing the restored copy's identities failed");
        run.out.failed = Some(e.to_string());
    }
    drop(run.holds);
    run.out
}

/// Eight hex digits of a FAT serial, as `fatlabel -i` takes it, and as blkid
/// prints it.
fn fat_serial_from(uuid: &str) -> (String, String) {
    let hex: String = uuid
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect::<String>()
        .to_ascii_uppercase();
    let shown = format!("{}-{}", &hex[..4], &hex[4..8]);
    (hex, shown)
}

/// The longest label a filesystem of kind `fs` takes, in bytes, and whether
/// it is kept in upper case (FAT: DOS labels are, and `fatlabel` warns about
/// lower case).
fn label_limit(fs: IdentityKind) -> Option<(usize, bool)> {
    match fs {
        IdentityKind::Ext => Some((16, false)),
        IdentityKind::Xfs => Some((12, false)),
        IdentityKind::Btrfs => Some((255, false)),
        IdentityKind::Fat => Some((11, true)),
        _ => None,
    }
}

/// A new label for a filesystem of kind `fs` labelled `old`: still
/// recognisable — the old one with `-r` (`-R` on FAT), or `-r2`, `-r3`…
/// while `taken` says the name is in use — shortened at a character
/// boundary to fit the filesystem's limit. Never starts with `-` (the tools
/// would read it as an option). On FAT only characters a DOS label takes
/// are kept (others become `_`). `None` for a filesystem without labels.
fn fresh_label(fs: IdentityKind, old: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let (limit, upper) = label_limit(fs)?;
    let base: String = if upper {
        old.chars()
            .map(|c| {
                let c = c.to_ascii_uppercase();
                if c.is_ascii_alphanumeric() || " !#$%&'()-@^_`{}~".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    } else {
        old.to_string()
    };
    let base = base.trim_start_matches('-');
    for n in 1u32.. {
        let mut suffix = if n == 1 {
            "-r".to_string()
        } else {
            format!("-r{n}")
        };
        if upper {
            suffix.make_ascii_uppercase();
        }
        let mut cut = limit.saturating_sub(suffix.len()).min(base.len());
        while !base.is_char_boundary(cut) {
            cut -= 1;
        }
        let label = format!("{}{suffix}", base[..cut].trim_end());
        let label = label.trim_start_matches('-').to_string();
        if !label.is_empty() && !taken(&label) {
            return Some(label);
        }
    }
    None
}

/// mdadm's colon spelling of a UUID.
fn md_colons(uuid: &str) -> String {
    let hex: String = uuid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    hex.as_bytes()
        .chunks(8)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// The DTO slug of an identity kind (`identity_check_linux` uses the same).
pub fn kind_slug(kind: IdentityKind) -> &'static str {
    match kind {
        IdentityKind::Ext => "ext",
        IdentityKind::Xfs => "xfs",
        IdentityKind::Btrfs => "btrfs",
        IdentityKind::Luks => "luks",
        IdentityKind::LvmPv => "lvm_pv",
        IdentityKind::LvmVg => "lvm_vg",
        IdentityKind::LvmVgName => "lvm_vg_name",
        IdentityKind::Md => "md",
        IdentityKind::PartUuid => "partuuid",
        IdentityKind::Fat => "vfat",
        IdentityKind::Label => "label",
    }
}

impl Run<'_> {
    /// Does a device of the machine (other than the restored ones) carry it?
    fn clashes(&self, id: &Identity) -> bool {
        !id.key.is_empty()
            && self
                .present
                .values()
                .any(|ids| ids.iter().any(|p| p.kind == id.kind && p.key == id.key))
    }

    /// Must `id`, found on `dev`, change? When the machine carries it, or
    /// when `dev` is inside a container whose identity clashed.
    fn must_change(&self, dev: &Dev, id: &Identity) -> bool {
        self.clashes(id) || (dev.in_clashing_container && !id.key.is_empty())
    }

    fn record(&mut self, kind: IdentityKind, old: &str, new: &str, dev: &Dev) {
        info!(kind = kind_slug(kind), old, new, node = %dev.node, "identity changed");
        self.out.changed.push(Changed {
            kind,
            old: old.to_string(),
            new: new.to_string(),
            node: dev.node.clone(),
            part_index: dev.part_index,
        });
    }

    fn walk(&mut self, devs: Vec<Dev>) -> Result<()> {
        // Members of one array and PVs of one VG are changed together.
        let mut arrays: BTreeMap<String, (Identity, Vec<Dev>)> = BTreeMap::new();
        let mut groups: BTreeMap<String, (Vec<Identity>, Vec<Dev>)> = BTreeMap::new();
        for dev in devs {
            let ids = match self.pre_read.remove(&dev.node) {
                Some(ids) => ids,
                None => self.host.identities_on(&dev.node)?,
            };
            info!(node = %dev.node, identities = ?ids, "restored device read");
            if let Some(md) = ids.iter().find(|i| i.kind == IdentityKind::Md) {
                // A member's data may begin with a filesystem (v1.0, 0.90):
                // that one is the array's, changed through the array.
                if self.must_change(&dev, md) {
                    arrays
                        .entry(md.key.clone())
                        .or_insert_with(|| (md.clone(), Vec::new()))
                        .1
                        .push(dev);
                }
                continue;
            }
            let lvm: Vec<Identity> = ids
                .iter()
                .filter(|i| {
                    matches!(
                        i.kind,
                        IdentityKind::LvmPv | IdentityKind::LvmVg | IdentityKind::LvmVgName
                    )
                })
                .cloned()
                .collect();
            if !lvm.is_empty() {
                if lvm.iter().any(|i| self.must_change(&dev, i)) {
                    let vg = lvm
                        .iter()
                        .find(|i| i.kind == IdentityKind::LvmVg)
                        .or_else(|| lvm.iter().find(|i| i.kind == IdentityKind::LvmVgName))
                        .map(|i| i.key.clone())
                        .unwrap_or_else(|| dev.node.clone());
                    let entry = groups.entry(vg).or_insert_with(|| (Vec::new(), Vec::new()));
                    for i in &lvm {
                        if !entry.0.contains(i) {
                            entry.0.push(i.clone());
                        }
                    }
                    entry.1.push(dev);
                }
                continue;
            }
            if let Some(luks) = ids.iter().find(|i| i.kind == IdentityKind::Luks) {
                let clashing = self.must_change(&dev, luks);
                let mut key = luks.key.clone();
                if clashing {
                    let new = self.change_luks(&dev, luks)?;
                    key = Identity::new(IdentityKind::Luks, &new).key;
                }
                match &dev.passphrase {
                    Some(pw) => {
                        let inner = self.open_luks(&dev, pw, clashing, &key)?;
                        self.walk(vec![inner])?;
                    }
                    None if clashing => self.out.notes.push(
                        tr!(
                            "identity_change_linux.luks_inside_unchecked",
                            node = dev.node.as_str()
                        )
                        .into_owned(),
                    ),
                    None => {}
                }
                continue;
            }
            for id in ids.iter().filter(|i| {
                matches!(
                    i.kind,
                    IdentityKind::Ext | IdentityKind::Xfs | IdentityKind::Btrfs | IdentityKind::Fat
                )
            }) {
                let mut key = id.key.clone();
                if self.must_change(&dev, id) {
                    if let Some(new) = self.change_fs(&dev, id)? {
                        key = Identity::new(id.kind, &new).key;
                    }
                }
                self.filesystems.push(FsSeen {
                    node: dev.node.clone(),
                    kind: id.kind,
                    key,
                    multi_device: id.multi_device,
                    luks_map: dev.luks_map.clone(),
                });
            }
            // After the UUID: a btrfs is relabelled by its (now unique)
            // fsid, and the journal or log is replayed once for both.
            let labels: Vec<&Identity> = ids
                .iter()
                .filter(|i| i.kind == IdentityKind::Label)
                .collect();
            self.saw_labels(&dev.node, &ids);
            for label in labels {
                if self.must_change(&dev, label) {
                    self.change_label(&dev, label)?;
                }
            }
        }
        for (_, (md, members)) in arrays {
            let array = self.change_md(&md, &members)?;
            self.walk(vec![array])?;
        }
        for (_, (ids, pvs)) in groups {
            let lvs = self.change_vg(&ids, &pvs)?;
            self.walk(lvs)?;
        }
        Ok(())
    }

    /// Change one filesystem's identity; `None` when it is kept (and said why).
    fn change_fs(&mut self, dev: &Dev, id: &Identity) -> Result<Option<String>> {
        let node = dev.node.as_str();
        let new = match id.kind {
            IdentityKind::Ext => {
                let new = self.host.new_uuid();
                self.prepare(node, IdentityKind::Ext)?;
                run_ok(self.tools, "tune2fs", &args(["-U", &new, node]))?;
                new
            }
            IdentityKind::Xfs => {
                let new = self.host.new_uuid();
                self.prepare(node, IdentityKind::Xfs)?;
                run_ok(self.tools, "xfs_admin", &args(["-U", &new, node]))?;
                new
            }
            IdentityKind::Btrfs => {
                if id.multi_device || self.host.btrfs_devices(node) > 1 {
                    self.out.notes.push(
                        tr!("identity_change_linux.btrfs_multi_device", node = node).into_owned(),
                    );
                    return Ok(None);
                }
                let new = self.host.new_uuid();
                // Whatever the kernel scanned under the old fsid is stale
                // after the rewrite; forgetting a device it never saw fails,
                // which is fine.
                let _ = self
                    .tools
                    .run("btrfs", &args(["device", "scan", "--forget", node]), None);
                run_ok(self.tools, "btrfstune", &args(["-f", "-U", &new, node]))?;
                new
            }
            IdentityKind::Fat => {
                let (hex, shown) = fat_serial_from(&self.host.new_uuid());
                run_ok(self.tools, "fatlabel", &args(["-i", node, &hex]))?;
                shown
            }
            _ => return Ok(None),
        };
        self.record(id.kind, &id.shown, &new, dev);
        Ok(Some(new))
    }

    /// Make the filesystem on `node` fit for its superblock to change, once
    /// per node. ext: tune2fs changes a UUID only on a freshly checked
    /// filesystem, and the check replays the copy's journal — a journalled
    /// superblock replayed later would bring the old UUID or label back.
    /// XFS: xfs_admin refuses a dirty log, and a copy of a mounted XFS has
    /// one; only the kernel replays it (`nouuid`: the original may be
    /// mounted).
    fn prepare(&mut self, node: &str, kind: IdentityKind) -> Result<()> {
        if self.prepared.iter().any(|n| n == node) {
            return Ok(());
        }
        match kind {
            IdentityKind::Ext => {
                run_checked(self.tools, "e2fsck", &args(["-f", "-p", node]), None, |c| {
                    c < 4
                })?;
            }
            IdentityKind::Xfs => {
                let m = self.host.mount(node, "xfs", "rw,nouuid")?;
                m.unmount()?;
            }
            _ => return Ok(()),
        }
        self.prepared.push(node.to_string());
        Ok(())
    }

    /// Record the labels `ids` (read from `node`) carry.
    fn saw_labels(&mut self, node: &str, ids: &[Identity]) {
        for l in ids.iter().filter(|i| i.kind == IdentityKind::Label) {
            let seen = (l.shown.clone(), node.to_string());
            if !self.labels.contains(&seen) {
                self.labels.push(seen);
            }
        }
    }

    /// One label on more than one restored filesystem: a `LABEL=` in the
    /// restored system cannot tell them apart, and its references (when
    /// fixed) follow the first one's new label — said, not chosen silently.
    fn duplicate_label_notes(&mut self) {
        let mut done: Vec<&str> = Vec::new();
        for (label, _) in &self.labels {
            if done.contains(&label.as_str()) {
                continue;
            }
            done.push(label);
            let nodes: Vec<&str> = self
                .labels
                .iter()
                .filter(|(l, _)| l == label)
                .map(|(_, n)| n.as_str())
                .collect();
            if nodes.len() < 2 {
                continue;
            }
            let changed: Vec<&Changed> = self
                .out
                .changed
                .iter()
                .filter(|c| c.kind == IdentityKind::Label && c.old == *label)
                .collect();
            let note = match changed.first() {
                None => tr!(
                    "identity_change_linux.duplicate_label",
                    label = label.as_str(),
                    nodes = nodes.join(", ")
                ),
                Some(first) => tr!(
                    "identity_change_linux.duplicate_label_changed",
                    label = label.as_str(),
                    nodes = nodes.join(", "),
                    labels = changed
                        .iter()
                        .map(|c| format!("{} → {}", c.node, c.new))
                        .collect::<Vec<_>>()
                        .join(", "),
                    first = first.new.as_str()
                ),
            };
            warn!(%label, ?nodes, "one label on several restored filesystems");
            self.out.notes.push(note.into_owned());
        }
    }

    /// Give a filesystem's label a new value of its own (see
    /// [`fresh_label`]); a multi-device btrfs keeps it, and says so.
    fn change_label(&mut self, dev: &Dev, id: &Identity) -> Result<()> {
        let node = dev.node.as_str();
        let Some(fs) = id.fs else {
            return Ok(());
        };
        if fs == IdentityKind::Btrfs && (id.multi_device || self.host.btrfs_devices(node) > 1) {
            self.out.notes.push(
                tr!(
                    "identity_change_linux.btrfs_multi_device_label",
                    node = node,
                    label = id.shown.as_str()
                )
                .into_owned(),
            );
            return Ok(());
        }
        // Read through a lossy decoding: written back, it would not be the
        // label the system names.
        if id.shown.contains(char::REPLACEMENT_CHARACTER) {
            self.out.notes.push(
                tr!(
                    "identity_change_linux.label_not_utf8",
                    node = node,
                    label = id.shown.as_str()
                )
                .into_owned(),
            );
            return Ok(());
        }
        let taken: Vec<String> = self
            .present
            .values()
            .flatten()
            .filter(|i| i.kind == IdentityKind::Label)
            .map(|i| i.shown.clone())
            .chain(self.labels.iter().map(|(l, _)| l.clone()))
            .chain(self.given.iter().cloned())
            .collect();
        let Some(new) = fresh_label(fs, &id.shown, |l| {
            taken.iter().any(|t| t.eq_ignore_ascii_case(l))
        }) else {
            return Ok(());
        };
        match fs {
            IdentityKind::Ext => {
                self.prepare(node, fs)?;
                run_ok(self.tools, "tune2fs", &args(["-L", &new, node]))?;
            }
            IdentityKind::Xfs => {
                self.prepare(node, fs)?;
                run_ok(self.tools, "xfs_admin", &args(["-L", &new, node]))?;
            }
            IdentityKind::Btrfs => {
                run_ok(
                    self.tools,
                    "btrfs",
                    &args(["filesystem", "label", node, &new]),
                )?;
            }
            IdentityKind::Fat => {
                run_ok(self.tools, "fatlabel", &args([node, &new]))?;
            }
            _ => return Ok(()),
        }
        self.given.push(new.clone());
        self.record(IdentityKind::Label, &id.shown, &new, dev);
        Ok(())
    }

    /// The new UUID.
    fn change_luks(&mut self, dev: &Dev, id: &Identity) -> Result<String> {
        let new = self.host.new_uuid();
        run_ok(
            self.tools,
            "cryptsetup",
            &args(["luksUUID", "-q", "--uuid", &new, &dev.node]),
        )?;
        self.record(IdentityKind::Luks, &id.shown, &new, dev);
        Ok(new)
    }

    /// Open the container with the restore's passphrase, under a hold.
    /// `clashing`: the container's own identity clashed; `key`: its UUID's
    /// key now.
    fn open_luks(&mut self, dev: &Dev, passphrase: &str, clashing: bool, key: &str) -> Result<Dev> {
        if !self.dm_held {
            let guard = self
                .host
                .hold(vec![crate::udev_hold_linux::dm_name_rule(&self.dm_prefix)])?;
            self.holds.push(guard);
            self.dm_held = true;
        }
        self.seq += 1;
        let name = format!("{}{}", self.dm_prefix, self.seq);
        self.opening(Opened::Luks(name.clone()));
        run_checked(
            self.tools,
            "cryptsetup",
            &args(["open", "--type", "luks", "--key-file=-", &dev.node, &name]),
            Some(passphrase.as_bytes()),
            |c| c == 0,
        )?;
        self.opened_ok();
        self.luks_maps.push((name.clone(), key.to_string()));
        self.host.settle();
        Ok(Dev {
            node: format!("/dev/mapper/{name}"),
            part_index: dev.part_index,
            passphrase: None,
            in_clashing_container: clashing,
            luks_map: Some(name),
        })
    }

    /// Give the restored members of one array a new UUID by assembling them
    /// alone under a fresh name; the array stays assembled for what is
    /// inside it.
    fn change_md(&mut self, md: &Identity, members: &[Dev]) -> Result<Dev> {
        let name = self.host.free_md_name();
        let guard = self
            .host
            .hold(vec![crate::udev_hold_linux::md_rule(&name)])?;
        self.holds.push(guard);
        let new = md_colons(&self.host.new_uuid());
        let array = format!("/dev/{name}");
        let mut a = args([
            "--assemble",
            &array,
            "--run",
            "--update=uuid",
            &format!("--uuid={new}"),
        ]);
        a.extend(members.iter().map(|m| m.node.clone()));
        // Recorded first: mdadm can leave an inactive array holding the
        // members when it fails half-way.
        self.opening(Opened::Md(array.clone()));
        run_ok(self.tools, "mdadm", &a)?;
        self.opened_ok();
        self.host.settle();
        for m in members {
            self.record(IdentityKind::Md, &md.shown, &new, m);
        }
        Ok(Dev {
            node: array,
            part_index: members.first().and_then(|m| m.part_index),
            passphrase: None,
            in_clashing_container: true,
            luks_map: members.first().and_then(|m| m.luks_map.clone()),
        })
    }

    /// `vgimportclone` the restored PVs of one VG and activate it for what is
    /// inside its LVs.
    fn change_vg(&mut self, ids: &[Identity], pvs: &[Dev]) -> Result<Vec<Dev>> {
        // Without the old name (its metadata could not be read) nothing in
        // the restored system can be matched to it: the group still gets a
        // name of its own, but no reference to a VG name is edited.
        let old_name = ids
            .iter()
            .find(|i| i.kind == IdentityKind::LvmVgName)
            .map(|i| i.shown.clone());
        let stem = old_name.as_deref().unwrap_or("vg");
        let old_uuid = ids
            .iter()
            .find(|i| i.kind == IdentityKind::LvmVg)
            .map(|i| i.shown.clone());
        let taken = |name: &str| {
            self.present.values().any(|v| {
                v.iter()
                    .any(|i| i.kind == IdentityKind::LvmVgName && i.key == name)
            })
        };
        let mut base = format!("{stem}_restored");
        let mut n = 2;
        while taken(&base) {
            base = format!("{stem}_restored{n}");
            n += 1;
        }
        let mut a = args(["--basevgname", &base]);
        a.extend(pvs.iter().map(|p| p.node.clone()));
        let old_pvs: Vec<(Dev, Option<Identity>)> = pvs
            .iter()
            .map(|p| {
                let pv = self
                    .host
                    .identities_on(&p.node)?
                    .into_iter()
                    .find(|i| i.kind == IdentityKind::LvmPv);
                Ok((p.clone(), pv))
            })
            .collect::<Result<_>>()?;
        run_ok(self.tools, "vgimportclone", &a)?;

        // What vgimportclone actually wrote: the name (it may add a number)
        // and the new UUIDs, read back from the PVs.
        let mut new_name = base.clone();
        let mut new_uuid = None;
        for (dev, old_pv) in &old_pvs {
            let now = self.host.identities_on(&dev.node)?;
            if let Some(n) = now.iter().find(|i| i.kind == IdentityKind::LvmVgName) {
                new_name = n.shown.clone();
            }
            if let Some(u) = now.iter().find(|i| i.kind == IdentityKind::LvmVg) {
                new_uuid = Some(u.shown.clone());
            }
            if let (Some(old), Some(new)) =
                (old_pv, now.iter().find(|i| i.kind == IdentityKind::LvmPv))
            {
                self.record(IdentityKind::LvmPv, &old.shown, &new.shown, dev);
            }
        }
        let first = &pvs[0];
        match &old_name {
            Some(old) => self.record(IdentityKind::LvmVgName, old, &new_name, first),
            None => self.out.notes.push(
                tr!(
                    "identity_change_linux.vg_name_unknown",
                    vg = new_name.as_str()
                )
                .into_owned(),
            ),
        }
        if let (Some(old), Some(new)) = (&old_uuid, &new_uuid) {
            self.record(IdentityKind::LvmVg, old, new, first);
        }

        // Its LVs carry the source's filesystems: activated under a hold.
        if let Some(uuid) = &new_uuid {
            let guard = self
                .host
                .hold(vec![crate::udev_hold_linux::dm_lvm_vg_rule(uuid)])?;
            self.holds.push(guard);
        } else {
            self.out.notes.push(
                tr!(
                    "identity_change_linux.vg_not_activated",
                    vg = new_name.as_str()
                )
                .into_owned(),
            );
            return Ok(Vec::new());
        }
        // Recorded first: vgchange activates LV by LV and may stop half-way.
        self.opening(Opened::Vg(new_name.clone()));
        run_ok(self.tools, "vgchange", &args(["-ay", &new_name]))?;
        self.opened_ok();
        self.host.settle();
        let listed = run_ok(
            self.tools,
            "lvs",
            &args([
                "--noheadings",
                "--separator",
                "|",
                "-o",
                "lv_name,lv_active",
                &new_name,
            ]),
        )?;
        Ok(parse_lvs(&listed.stdout, &new_name)
            .into_iter()
            .map(|node| Dev {
                node,
                part_index: first.part_index,
                passphrase: None,
                in_clashing_container: true,
                luks_map: first.luks_map.clone(),
            })
            .collect())
    }

    /// The changes as `identity_refs_linux` takes them.
    fn renamed(&self) -> Vec<Renamed> {
        self.out
            .changed
            .iter()
            .map(|c| Renamed {
                kind: c.kind,
                old: c.old.clone(),
                new: c.new.clone(),
            })
            .collect()
    }

    /// Mount each filesystem seen and rewrite what names the old identities;
    /// then rebuild the initramfs of a restored system that needs it.
    fn fix_references(&mut self) -> Result<()> {
        let renamed = self.renamed();
        let mut bios_grub = false;
        let mut boot_config = false;
        let mut mounted: Vec<MountedSeen> = Vec::new();
        let mut systems: Vec<SystemSeen> = Vec::new();
        let filesystems = std::mem::take(&mut self.filesystems);
        for fs in &filesystems {
            let (fstype, opts, top_level) = match fs.kind {
                IdentityKind::Ext => ("ext4", "rw", false),
                IdentityKind::Xfs => ("xfs", "rw,nouuid", false),
                IdentityKind::Fat => ("vfat", "rw", false),
                IdentityKind::Btrfs => {
                    // A btrfs still carrying a machine's fsid is mounted only
                    // where the kernel gives it a temporary one: 6.7 and
                    // later, and a single-device filesystem only.
                    let still = self.clashes(&Identity::new(IdentityKind::Btrfs, &fs.key));
                    let temp_fsid =
                        !fs.multi_device && self.host.kernel().is_some_and(|k| k >= (6, 7));
                    if still && !temp_fsid {
                        self.out.notes.push(
                            tr!(
                                "identity_change_linux.btrfs_not_edited",
                                node = fs.node.as_str()
                            )
                            .into_owned(),
                        );
                        continue;
                    }
                    ("btrfs", "rw,subvolid=5", true)
                }
                _ => continue,
            };
            let m = self.host.mount(&fs.node, fstype, opts)?;
            let edited = fix_tree(m.path(), top_level, &renamed);
            // Read after the edits: what the system names is the new identities.
            let found = match &edited {
                Ok(_) => initramfs::find_systems(m.path(), top_level),
                Err(_) => Vec::new(),
            };
            m.unmount()?;
            let edited = edited.map_err(|e| {
                OwnBackupError::Device(
                    tr!(
                        "identity_change_linux.edit_failed",
                        node = fs.node.as_str(),
                        error = e
                    )
                    .into_owned(),
                )
            })?;
            bios_grub |= edited.bios_grub;
            boot_config |= edited.initramfs_config
                || edited
                    .files
                    .iter()
                    .any(|f| f.ends_with("grub.cfg") || f.contains("loader/entries/"));
            for flag in &edited.autorelabel {
                self.out.notes.push(
                    tr!(
                        "identity_change_linux.autorelabel",
                        node = fs.node.as_str(),
                        file = flag.as_str()
                    )
                    .into_owned(),
                );
            }
            if !edited.xattrs_lost.is_empty() {
                self.out.notes.push(
                    tr!(
                        "identity_change_linux.xattrs_lost",
                        node = fs.node.as_str(),
                        files = edited.xattrs_lost.join("; ")
                    )
                    .into_owned(),
                );
            }
            self.out
                .edited
                .extend(edited.files.iter().map(|f| format!("{}:{f}", fs.node)));
            mounted.push(MountedSeen {
                node: fs.node.clone(),
                fstype: fstype.to_string(),
                key: fs.key.clone(),
            });
            for (subdir, config) in found {
                systems.push(SystemSeen {
                    node: fs.node.clone(),
                    fstype: fstype.to_string(),
                    opts: opts.to_string(),
                    subdir,
                    config,
                    luks_map: fs.luks_map.clone(),
                });
            }
        }
        self.initramfs(systems, mounted, boot_config);
        if bios_grub {
            self.out
                .notes
                .push(tr!("identity_change_linux.reinstall_bios_grub").into_owned());
        }
        if self.out.edited.is_empty() {
            self.out
                .notes
                .push(tr!("identity_change_linux.nothing_to_edit").into_owned());
        }
        Ok(())
    }

    /// Rebuild the initramfs of each restored system whose image keeps a
    /// changed identity (`initramfs_linux::reasons`), with its own tool; say
    /// what came of it. Nothing here fails the run.
    fn initramfs(
        &mut self,
        systems: Vec<SystemSeen>,
        mut mounted: Vec<MountedSeen>,
        boot_config: bool,
    ) {
        let renamed = self.renamed();
        if systems.is_empty() {
            // A boot configuration was fixed, and a container identity an
            // image may keep changed, but no restored root could be looked
            // into (a LUKS container without its passphrase, a btrfs still
            // carrying the machine's fsid).
            let container = self.out.changed.iter().any(|c| {
                matches!(
                    c.kind,
                    IdentityKind::Luks | IdentityKind::Md | IdentityKind::LvmVgName
                )
            });
            if container && boot_config {
                self.out
                    .notes
                    .push(tr!("identity_change_linux.initramfs_root_unreachable").into_owned());
            }
            return;
        }
        for mut sys in systems {
            // A host-only dracut image records the root, /boot and the ESP:
            // did one of their UUIDs change?
            let candidates = self.candidates(&mounted);
            let boot_nodes: Vec<String> =
                initramfs::mount_plan(&sys.config.fstab, &sys.node, &candidates)
                    .map(|plan| plan.into_iter().map(|m| m.node).collect())
                    .unwrap_or_default();
            let recorded_changed = self.out.changed.iter().any(|c| {
                (c.node == sys.node || boot_nodes.contains(&c.node))
                    && matches!(
                        c.kind,
                        IdentityKind::Ext
                            | IdentityKind::Xfs
                            | IdentityKind::Btrfs
                            | IdentityKind::Fat
                    )
            });
            let why = initramfs::reasons(&renamed, recorded_changed, &sys.config);
            let system = sys.config.os.shown();
            if why.is_empty() {
                info!(%system, node = %sys.node, "the restored system's initramfs keeps none of the changed identities");
                continue;
            }
            info!(%system, node = %sys.node, reasons = ?why, "the restored system's initramfs is to be rebuilt");
            let Some((tool, program)) = sys.config.tool.clone() else {
                self.out.notes.push(
                    tr!(
                        "identity_change_linux.initramfs_unknown_system",
                        system = system.as_str()
                    )
                    .into_owned(),
                );
                continue;
            };
            let command = tool.command();
            if let Some(map) = sys.luks_map.clone() {
                if let Err(note) = self.luks_by_crypttab_name(
                    &map,
                    &sys.config.crypttab,
                    &mut sys.node,
                    &mut mounted,
                    &command,
                ) {
                    self.out.notes.push(note);
                    continue;
                }
            }
            let candidates = self.candidates(&mounted);
            let mounts = match initramfs::mount_plan(&sys.config.fstab, &sys.node, &candidates) {
                Ok(m) => m,
                Err(source) => {
                    self.out.notes.push(
                        tr!(
                            "identity_change_linux.initramfs_boot_not_restored",
                            system = system.as_str(),
                            source = source.as_str(),
                            command = command.as_str()
                        )
                        .into_owned(),
                    );
                    continue;
                }
            };
            let req = initramfs::Request {
                root_node: sys.node.clone(),
                root_fstype: sys.fstype.clone(),
                root_opts: sys.opts.clone(),
                subdir: sys.subdir.clone(),
                mounts,
                tool,
                program,
            };
            info!(?req, "rebuilding the restored system's initramfs");
            let note = match self.host.rebuild_initramfs(&req) {
                initramfs::Rebuilt::Done { images, output } => {
                    // cryptsetup's hook says so when it could not tie the
                    // root's container to a crypttab entry: that image would
                    // not unlock the system.
                    let crypt: Vec<&str> = output
                        .lines()
                        .map(str::trim)
                        .filter(|l| {
                            l.contains("cryptsetup: WARNING") || l.contains("cryptsetup: ERROR")
                        })
                        .collect();
                    if why.contains(&initramfs::Reason::Crypttab) && !crypt.is_empty() {
                        warn!(?crypt, "the rebuilt image may not unlock the restored root");
                        let warning = tr!("identity_change_linux.initramfs_crypt_warning");
                        tr!(
                            "identity_change_linux.initramfs_failed",
                            tool = tool.name(),
                            error = warning,
                            command = command.as_str(),
                            output = crypt.join(" / ")
                        )
                    } else {
                        info!(?images, "the restored system's initramfs was rebuilt");
                        tr!(
                            "identity_change_linux.initramfs_rebuilt",
                            tool = tool.name()
                        )
                    }
                }
                initramfs::Rebuilt::Failed { error, output } => {
                    warn!(%error, "rebuilding the restored system's initramfs failed");
                    let last = initramfs::tail(&output, 6);
                    tr!(
                        "identity_change_linux.initramfs_failed",
                        tool = tool.name(),
                        error = error.as_str(),
                        command = command.as_str(),
                        output = if last.is_empty() {
                            "—".to_string()
                        } else {
                            last
                        }
                    )
                }
            };
            self.out.notes.push(note.into_owned());
        }
    }

    /// The restored filesystems a rebuild may mount, with the names its
    /// fstab may use for them, read from the devices.
    fn candidates(&mut self, mounted: &[MountedSeen]) -> Vec<initramfs::Candidate> {
        mounted
            .iter()
            .map(|m| {
                let names = self.host.fs_names(&m.node);
                initramfs::Candidate {
                    node: m.node.clone(),
                    fstype: m.fstype.clone(),
                    uuid_key: m.key.clone(),
                    label: names.label,
                    partuuid: names.partuuid,
                }
            })
            .collect()
    }

    /// The restored root lies in LUKS mapping `map`, opened by this run under
    /// a temporary name. The system's tool ties the container it must unlock
    /// to crypttab by the mapping's name (cryptsetup's initramfs hook looks
    /// it up there), so for the rebuild the mapping takes the name its
    /// crypttab entry gives it, under a udev hold of its own; `root` and
    /// `mounted` follow the new node. A name the machine already uses is not
    /// taken: then no rebuild (`Err` is the note). A container crypttab does
    /// not list is unlocked some other way, and is left as it is.
    fn luks_by_crypttab_name(
        &mut self,
        map: &str,
        crypttab: &str,
        root: &mut String,
        mounted: &mut [MountedSeen],
        command: &str,
    ) -> std::result::Result<(), String> {
        let Some(key) = self
            .luks_maps
            .iter()
            .find(|(n, _)| n == map)
            .map(|(_, k)| k.clone())
        else {
            return Ok(());
        };
        let Some(name) = crypttab_name(crypttab, &key) else {
            return Ok(());
        };
        if name == map {
            return Ok(());
        }
        let usable = !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-.+".contains(c));
        if !usable || !self.host.dm_name_free(&name) {
            return Err(tr!(
                "identity_change_linux.initramfs_luks_name_taken",
                name = name.as_str(),
                command = command
            )
            .into_owned());
        }
        let renamed = (|| -> Result<()> {
            let guard = self
                .host
                .hold(vec![crate::udev_hold_linux::dm_exact_name_rule(&name)])?;
            self.holds.push(guard);
            run_ok(self.tools, "dmsetup", &args(["rename", map, &name]))?;
            Ok(())
        })();
        if let Err(e) = renamed {
            return Err(tr!(
                "identity_change_linux.initramfs_failed",
                tool = "dmsetup",
                error = e,
                command = command,
                output = "—"
            )
            .into_owned());
        }
        self.host.settle();
        info!(from = map, to = %name, "the restored root's LUKS mapping renamed after its crypttab entry");
        for o in &mut self.opened {
            if let Opened::Luks(n) = &mut o.what {
                if n == map {
                    *n = name.clone();
                }
            }
        }
        for (n, _) in &mut self.luks_maps {
            if n == map {
                *n = name.clone();
            }
        }
        let (old, new) = (format!("/dev/mapper/{map}"), format!("/dev/mapper/{name}"));
        if *root == old {
            root.clone_from(&new);
        }
        for m in mounted.iter_mut().filter(|m| m.node == old) {
            m.node.clone_from(&new);
        }
        Ok(())
    }

    /// Record a stacked device about to be created (see [`OpenedDev`]).
    fn opening(&mut self, what: Opened) {
        self.opened.push(OpenedDev {
            what,
            confirmed: false,
        });
    }

    /// The device recorded last came up.
    fn opened_ok(&mut self) {
        if let Some(o) = self.opened.last_mut() {
            o.confirmed = true;
        }
    }

    /// Close every stacked device, innermost first. A device that will not
    /// close is said, not raised: the restore itself is done.
    fn close_all(&mut self) {
        while let Some(o) = self.opened.pop() {
            let (prog, a, what) = match &o.what {
                Opened::Luks(name) => ("cryptsetup", args(["close", name]), name.clone()),
                Opened::Vg(vg) => ("vgchange", args(["-an", vg]), vg.clone()),
                Opened::Md(array) => ("mdadm", args(["--stop", array]), array.clone()),
            };
            let closed = run_ok(self.tools, prog, &a);
            if !o.confirmed {
                // Its command failed (that error is the run's): whatever it
                // left half-made is gone now, or there was nothing.
                if let Err(e) = closed {
                    info!(device = %what, error = %e, "nothing left of a failed activation, or it did not close");
                }
                continue;
            }
            if let Err(e) = closed {
                warn!(device = %what, error = %e, "a stacked device did not close");
                self.out.notes.push(
                    tr!(
                        "identity_change_linux.not_closed",
                        device = what.as_str(),
                        error = e
                    )
                    .into_owned(),
                );
            }
        }
        self.host.settle();
    }
}

/// Device nodes of the active LVs `lvs -o lv_name,lv_active` lists.
fn parse_lvs(out: &str, vg: &str) -> Vec<String> {
    let esc = |s: &str| s.replace('-', "--");
    out.lines()
        .filter_map(|l| {
            let mut it = l.trim().split('|');
            let name = it.next()?.trim();
            let active = it.next().unwrap_or("").trim();
            (!name.is_empty() && active == "active")
                .then(|| format!("/dev/mapper/{}-{}", esc(vg), esc(name)))
        })
        .collect()
}

/// The crypttab name of the container whose UUID has `key`
/// (`UUID=` or `/dev/disk/by-uuid/` source).
fn crypttab_name(crypttab: &str, key: &str) -> Option<String> {
    crypttab.lines().find_map(|l| {
        let l = l.trim();
        if l.starts_with('#') {
            return None;
        }
        let mut f = l.split_whitespace();
        let (name, source) = (f.next()?, f.next()?.trim_matches('"'));
        let uuid = source
            .strip_prefix("UUID=")
            .or_else(|| source.strip_prefix("/dev/disk/by-uuid/"))?
            .trim_matches('"');
        (Identity::new(IdentityKind::Luks, uuid).key == key).then(|| name.to_string())
    })
}

// --- the machine ------------------------------------------------------------

/// The real machine.
pub struct SystemHost;

struct RealMount(crate::linux_tools::TempMount);

impl MountedFs for RealMount {
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn unmount(self: Box<Self>) -> Result<()> {
        let RealMount(m) = *self;
        m.unmount()
    }
}

impl Host for SystemHost {
    fn identities_on(&mut self, node: &str) -> Result<Vec<Identity>> {
        crate::identity_check_linux::identities_on(node)
    }

    fn new_uuid(&mut self) -> String {
        uuid::Uuid::new_v4().hyphenated().to_string()
    }

    fn mount(&mut self, node: &str, fstype: &str, opts: &str) -> Result<Box<dyn MountedFs>> {
        Ok(Box::new(RealMount(crate::linux_tools::TempMount::mount(
            node,
            &[(fstype, opts)],
        )?)))
    }

    fn hold(&mut self, rules: Vec<String>) -> Result<Box<dyn std::any::Any>> {
        // A file per hold: the rules file is named after its tag and the
        // process, and one run holds a LUKS mapping, then the LVs inside it,
        // then an array — a shared name would let each replace the last.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Box::new(crate::udev_hold_linux::UdevHold::install_rules(
            &format!("reid{n}"),
            &rules,
        )?))
    }

    fn settle(&mut self) {
        let _ = std::process::Command::new("udevadm")
            .args(["settle", "--timeout=30"])
            .status();
    }

    fn free_md_name(&mut self) -> String {
        (100..127)
            .rev()
            .map(|n| format!("md{n}"))
            .find(|n| !Path::new("/sys/block").join(n).exists())
            .unwrap_or_else(|| "md99".into())
    }

    fn kernel(&self) -> Option<KernelVersion> {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .and_then(|r| crate::identity_clash::parse_kernel_version(&r))
    }

    fn btrfs_devices(&mut self, node: &str) -> u64 {
        let Ok(dev) = crate::linux_device::LinuxRawDevice::open_read(node) else {
            return 1;
        };
        let mut b = [0u8; 8];
        if dev.read_at(0x10000 + 0x88, &mut b).is_err() {
            return 1;
        }
        u64::from_le_bytes(b).max(1)
    }

    fn fs_names(&mut self, node: &str) -> FsNames {
        // `-p`: probe the device itself, not the cache or udev's links.
        let out = std::process::Command::new("blkid")
            .env("LC_ALL", "C")
            .args(["-p", "-o", "export", node])
            .stdin(std::process::Stdio::null())
            .output();
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let value = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.replace("\\ ", " "))
                .filter(|v| !v.is_empty())
        };
        FsNames {
            label: value("LABEL="),
            partuuid: value("PART_ENTRY_UUID="),
        }
    }

    fn dm_name_free(&mut self, name: &str) -> bool {
        let Ok(entries) = std::fs::read_dir("/sys/block") else {
            return false;
        };
        !entries.flatten().any(|e| {
            e.file_name().to_string_lossy().starts_with("dm-")
                && std::fs::read_to_string(e.path().join("dm/name")).is_ok_and(|n| n.trim() == name)
        }) && !Path::new("/dev/mapper").join(name).exists()
    }

    fn rebuild_initramfs(&mut self, req: &initramfs::Request) -> initramfs::Rebuilt {
        initramfs::rebuild(req)
    }
}

impl Outcome {
    /// The changes as the RPCs report them.
    pub fn dtos(&self) -> Vec<own_backup_common::engine_dto::IdentityChangeDto> {
        self.changed
            .iter()
            .map(|c| own_backup_common::engine_dto::IdentityChangeDto {
                kind: kind_slug(c.kind).to_string(),
                old: c.old.clone(),
                new: c.new.clone(),
                device: c.node.clone(),
                part_index: c.part_index,
            })
            .collect()
    }

    /// One line per change, for a summary or an error.
    pub fn describe(&self) -> String {
        self.changed
            .iter()
            .map(|c| format!("{} {} → {} ({})", kind_slug(c.kind), c.old, c.new, c.node))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The machine's identities without the `restored` devices (kernel names)
/// and everything stacked on them — what a changed identity must differ
/// from.
pub fn present_except(restored: &[String]) -> BTreeMap<String, Vec<Identity>> {
    let sys = Path::new(own_backup_common::linux_block::SYS_CLASS_BLOCK);
    let mut skip: Vec<String> = Vec::new();
    for name in restored {
        skip.extend(own_backup_common::linux_block::stack_above(sys, name));
        skip.push(name.clone());
    }
    let mut present = crate::identity_clash::host_identities(
        Path::new(own_backup_common::linux_block::UDEV_DATA),
        Path::new("/sys/dev/block"),
    );
    present.retain(|dev, _| !skip.contains(dev));
    present
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux_tools::fake::FakeTools;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const SRC: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    #[derive(Default)]
    struct FakeHost {
        ids: HashMap<String, Vec<Identity>>,
        uuids: u32,
        mounts: Vec<(String, String, String)>,
        holds: Vec<Vec<String>>,
        mount_dir: Option<PathBuf>,
        /// Per node, before `mount_dir`.
        mount_dirs: HashMap<String, PathBuf>,
        kernel: Option<KernelVersion>,
        names: HashMap<String, FsNames>,
        dm_taken: Vec<String>,
        rebuilds: Vec<initramfs::Request>,
        /// What a rebuild returns; a rewritten image when unset.
        rebuilt: Option<initramfs::Rebuilt>,
    }

    struct FakeMount(PathBuf);
    impl MountedFs for FakeMount {
        fn path(&self) -> &Path {
            &self.0
        }
        fn unmount(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    impl Host for FakeHost {
        fn identities_on(&mut self, node: &str) -> Result<Vec<Identity>> {
            Ok(self.ids.get(node).cloned().unwrap_or_default())
        }
        fn new_uuid(&mut self) -> String {
            self.uuids += 1;
            format!(
                "{:08x}-0000-4000-8000-000000000000",
                0xabc0_0000u32 + self.uuids
            )
        }
        fn mount(&mut self, node: &str, fstype: &str, opts: &str) -> Result<Box<dyn MountedFs>> {
            self.mounts.push((node.into(), fstype.into(), opts.into()));
            Ok(Box::new(FakeMount(
                self.mount_dirs
                    .get(node)
                    .or(self.mount_dir.as_ref())
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from("/nonexistent")),
            )))
        }
        fn hold(&mut self, rules: Vec<String>) -> Result<Box<dyn std::any::Any>> {
            self.holds.push(rules);
            Ok(Box::new(()))
        }
        fn settle(&mut self) {}
        fn free_md_name(&mut self) -> String {
            "md126".into()
        }
        fn kernel(&self) -> Option<KernelVersion> {
            self.kernel
        }
        fn btrfs_devices(&mut self, _node: &str) -> u64 {
            1
        }
        fn fs_names(&mut self, node: &str) -> FsNames {
            self.names.get(node).cloned().unwrap_or_default()
        }
        fn dm_name_free(&mut self, name: &str) -> bool {
            !self.dm_taken.iter().any(|n| n == name)
        }
        fn rebuild_initramfs(&mut self, req: &initramfs::Request) -> initramfs::Rebuilt {
            self.rebuilds.push(req.clone());
            self.rebuilt.clone().unwrap_or(initramfs::Rebuilt::Done {
                images: vec!["/boot/initrd.img-7.0.0-34-generic".into()],
                output: String::new(),
            })
        }
    }

    fn id(kind: IdentityKind, v: &str) -> Identity {
        Identity::new(kind, v)
    }

    fn machine(ids: Vec<Identity>) -> BTreeMap<String, Vec<Identity>> {
        let mut m = BTreeMap::new();
        m.insert("sda2".to_string(), ids);
        m
    }

    fn dev(node: &str, part: u32) -> RestoredDevice {
        RestoredDevice {
            node: node.into(),
            part_index: Some(part),
            passphrase: None,
        }
    }

    fn programs(t: &FakeTools) -> Vec<&str> {
        t.calls.iter().map(|(p, _, _)| p.as_str()).collect()
    }

    #[test]
    fn only_what_the_machine_already_has_is_changed() {
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb2".into(), vec![id(IdentityKind::Ext, SRC)]);
        host.ids.insert(
            "/dev/sdb3".into(),
            vec![id(
                IdentityKind::Xfs,
                "99999999-0000-4000-8000-000000000000",
            )],
        );
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![id(IdentityKind::Ext, SRC)]),
            &[dev("/dev/sdb2", 2), dev("/dev/sdb3", 3)],
            false,
        );
        assert_eq!(out.failed, None);
        assert_eq!(programs(&tools), vec!["e2fsck", "tune2fs"]);
        assert_eq!(out.changed.len(), 1);
        let c = &out.changed[0];
        assert_eq!((c.kind, c.old.as_str()), (IdentityKind::Ext, SRC));
        assert_eq!(tools.calls[1].1, args(["-U", &c.new, "/dev/sdb2"]));
        assert_eq!(c.part_index, Some(2));
    }

    #[test]
    fn each_filesystem_is_changed_with_its_own_tool() {
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb1".into(), vec![id(IdentityKind::Fat, "ABCD-1234")]);
        host.ids
            .insert("/dev/sdb2".into(), vec![id(IdentityKind::Xfs, SRC)]);
        host.ids
            .insert("/dev/sdb3".into(), vec![id(IdentityKind::Btrfs, SRC)]);
        let present = machine(vec![
            id(IdentityKind::Fat, "abcd-1234"),
            id(IdentityKind::Xfs, SRC),
            id(IdentityKind::Btrfs, SRC),
        ]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[
                dev("/dev/sdb1", 1),
                dev("/dev/sdb2", 2),
                dev("/dev/sdb3", 3),
            ],
            false,
        );
        assert_eq!(out.failed, None);
        assert_eq!(
            programs(&tools),
            vec!["fatlabel", "xfs_admin", "btrfs", "btrfstune"]
        );
        // The FAT serial is eight hex digits for fatlabel, dashed in the report.
        assert_eq!(tools.calls[0].1[0], "-i");
        assert_eq!(tools.calls[0].1[2].len(), 8);
        assert_eq!(out.changed[0].new.len(), 9);
        // XFS: the log is replayed by a nouuid mount first.
        assert_eq!(
            host.mounts[0],
            ("/dev/sdb2".into(), "xfs".into(), "rw,nouuid".into())
        );
        assert_eq!(tools.calls[3].1[..2], args(["-f", "-U"]));
    }

    fn label(fs: IdentityKind, l: &str) -> Identity {
        Identity::label(fs, l)
    }

    /// The cloud image's three labels and an XFS one, each changed with its
    /// filesystem's tool after its UUID (when that clashed too), the journal
    /// or log replayed once per filesystem.
    #[test]
    fn clashing_labels_get_new_ones_with_each_filesystems_tool() {
        let mut host = FakeHost::default();
        host.ids.insert(
            "/dev/sdb15".into(),
            vec![
                id(IdentityKind::Fat, "1111-2222"),
                label(IdentityKind::Fat, "UEFI"),
            ],
        );
        host.ids.insert(
            "/dev/sdb1".into(),
            vec![
                id(IdentityKind::Ext, SRC),
                label(IdentityKind::Ext, "cloudimg-rootfs"),
            ],
        );
        host.ids.insert(
            "/dev/sdb2".into(),
            vec![
                id(IdentityKind::Xfs, "99999999-0000-4000-8000-000000000000"),
                label(IdentityKind::Xfs, "data"),
            ],
        );
        host.ids.insert(
            "/dev/sdb16".into(),
            vec![
                id(IdentityKind::Btrfs, "88888888-0000-4000-8000-000000000000"),
                label(IdentityKind::Btrfs, "BOOT"),
            ],
        );
        // Only the root's UUID clashes; every label does (the machine's
        // side is udev's: labels without a filesystem kind).
        let present = machine(vec![
            id(IdentityKind::Ext, SRC),
            id(IdentityKind::Label, "cloudimg-rootfs"),
            id(IdentityKind::Label, "UEFI"),
            id(IdentityKind::Label, "data"),
            id(IdentityKind::Label, "BOOT"),
            // Taken already, in another case: the next number.
            id(IdentityKind::Label, "boot-R"),
        ]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[
                dev("/dev/sdb15", 15),
                dev("/dev/sdb1", 1),
                dev("/dev/sdb2", 2),
                dev("/dev/sdb16", 16),
            ],
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let calls: Vec<(String, Vec<String>)> = tools
            .calls
            .iter()
            .map(|(p, a, _)| (p.clone(), a.clone()))
            .collect();
        let new_uuid = out.changed[1].new.clone();
        assert_eq!(
            calls,
            vec![
                ("fatlabel".into(), args(["/dev/sdb15", "UEFI-R"])),
                ("e2fsck".into(), args(["-f", "-p", "/dev/sdb1"])),
                ("tune2fs".into(), args(["-U", &new_uuid, "/dev/sdb1"])),
                (
                    "tune2fs".into(),
                    args(["-L", "cloudimg-rootf-r", "/dev/sdb1"])
                ),
                ("xfs_admin".into(), args(["-L", "data-r", "/dev/sdb2"])),
                (
                    "btrfs".into(),
                    args(["filesystem", "label", "/dev/sdb16", "BOOT-r2"])
                ),
            ]
        );
        assert_eq!(
            host.mounts,
            vec![("/dev/sdb2".into(), "xfs".into(), "rw,nouuid".into())],
            "the XFS log is replayed before its label changes"
        );
        let labels: Vec<(&str, &str, &str)> = out
            .changed
            .iter()
            .filter(|c| c.kind == IdentityKind::Label)
            .map(|c| (c.node.as_str(), c.old.as_str(), c.new.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("/dev/sdb15", "UEFI", "UEFI-R"),
                ("/dev/sdb1", "cloudimg-rootfs", "cloudimg-rootf-r"),
                ("/dev/sdb2", "data", "data-r"),
                ("/dev/sdb16", "BOOT", "BOOT-r2"),
            ]
        );
        assert_eq!(out.dtos()[0].kind, "label");
    }

    /// Two restored filesystems with one label get two labels; a label the
    /// machine does not have stays; a multi-device btrfs keeps its label
    /// and says so.
    #[test]
    fn new_labels_are_unique_and_a_multi_device_btrfs_keeps_its_own() {
        let mut host = FakeHost::default();
        for n in ["/dev/sdb1", "/dev/sdb2"] {
            host.ids.insert(
                n.into(),
                vec![
                    id(IdentityKind::Fat, "1111-2222"),
                    label(IdentityKind::Fat, "ESP"),
                ],
            );
        }
        host.ids.insert(
            "/dev/sdb3".into(),
            vec![id(IdentityKind::Ext, SRC), label(IdentityKind::Ext, "mine")],
        );
        let multi = Identity {
            multi_device: true,
            ..label(IdentityKind::Btrfs, "pool")
        };
        host.ids.insert(
            "/dev/sdb4".into(),
            vec![Identity::btrfs_multi_device(BOOT), multi],
        );
        let present = machine(vec![
            id(IdentityKind::Label, "ESP"),
            id(IdentityKind::Label, "pool"),
        ]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[
                dev("/dev/sdb1", 1),
                dev("/dev/sdb2", 2),
                dev("/dev/sdb3", 3),
                dev("/dev/sdb4", 4),
            ],
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let news: Vec<&str> = out.changed.iter().map(|c| c.new.as_str()).collect();
        assert_eq!(news, vec!["ESP-R", "ESP-R2"]);
        assert_eq!(programs(&tools), vec!["fatlabel", "fatlabel"]);
        // Which one the references follow is said, not chosen silently.
        assert!(
            out.notes.iter().any(|n| n.contains("/dev/sdb1, /dev/sdb2")
                && n.contains("/dev/sdb2 → ESP-R2")
                && n.contains("«ESP-R»")),
            "{:?}",
            out.notes
        );
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("/dev/sdb4") && n.contains("pool")),
            "{:?}",
            out.notes
        );
    }

    /// A new label is none a restored device further on carries (read
    /// before the walk); a label read with replacements is kept and said.
    #[test]
    fn a_new_label_avoids_later_devices_and_a_non_utf8_one_is_kept() {
        let mut host = FakeHost::default();
        host.ids.insert(
            "/dev/sdb1".into(),
            vec![id(IdentityKind::Ext, SRC), label(IdentityKind::Ext, "data")],
        );
        host.ids.insert(
            "/dev/sdb2".into(),
            vec![
                id(IdentityKind::Ext, BOOT),
                label(IdentityKind::Ext, "data-r"),
            ],
        );
        host.ids.insert(
            "/dev/sdb3".into(),
            vec![
                id(IdentityKind::Fat, "1111-2222"),
                label(IdentityKind::Fat, "\u{FFFD}SP"),
            ],
        );
        let present = machine(vec![
            id(IdentityKind::Label, "data"),
            id(IdentityKind::Label, "\u{FFFD}SP"),
        ]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[
                dev("/dev/sdb1", 1),
                dev("/dev/sdb2", 2),
                dev("/dev/sdb3", 3),
            ],
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let news: Vec<&str> = out.changed.iter().map(|c| c.new.as_str()).collect();
        assert_eq!(news, vec!["data-r2"]);
        assert_eq!(programs(&tools), vec!["e2fsck", "tune2fs"]);
        assert!(
            out.notes.iter().any(|n| n.contains("/dev/sdb3")),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn a_new_label_fits_its_filesystem_and_stays_recognisable() {
        let free = |_: &str| false;
        assert_eq!(
            fresh_label(IdentityKind::Ext, "cloudimg-rootfs", free).as_deref(),
            Some("cloudimg-rootf-r")
        );
        assert_eq!(
            fresh_label(IdentityKind::Xfs, "abcdefghijkl", free).as_deref(),
            Some("abcdefghij-r")
        );
        assert_eq!(
            fresh_label(IdentityKind::Fat, "my.disk", free).as_deref(),
            Some("MY_DISK-R")
        );
        assert_eq!(
            fresh_label(IdentityKind::Fat, "EFI SYSTEM", free).as_deref(),
            Some("EFI SYSTE-R")
        );
        // Cut at a character, never inside one: 14 bytes would split the
        // seventh letter.
        let ru = fresh_label(IdentityKind::Ext, "aПриветМир", free).unwrap();
        assert_eq!(ru, "aПривет-r");
        assert!(ru.len() <= 16);
        // A tool would take a leading `-` for an option.
        assert_eq!(
            fresh_label(IdentityKind::Btrfs, "--x", free).as_deref(),
            Some("x-r")
        );
        assert_eq!(
            fresh_label(IdentityKind::Btrfs, "-", free).as_deref(),
            Some("r")
        );
        let long = "x".repeat(255);
        assert_eq!(
            fresh_label(IdentityKind::Btrfs, &long, free).unwrap().len(),
            255
        );
        let taken = |l: &str| ["a-r", "a-r2"].contains(&l);
        assert_eq!(
            fresh_label(IdentityKind::Ext, "a", taken).as_deref(),
            Some("a-r3")
        );
        assert_eq!(fresh_label(IdentityKind::Luks, "a", free), None);
    }

    /// The cloud image restored beside its source: root, /boot and ESP
    /// change labels, the restored fstab and GRUB follow, and the rebuild
    /// finds /boot by its new label.
    #[test]
    fn labels_and_the_references_to_them_change_together() {
        let dir = tempfile::tempdir().unwrap();
        let fstab = (
            "etc/fstab",
            "LABEL=cloudimg-rootfs / ext4 defaults 0 1\nLABEL=BOOT /boot ext4 defaults 0 2\n"
                .to_string(),
        );
        let resume = (
            "etc/initramfs-tools/conf.d/resume",
            "RESUME=none\n".to_string(),
        );
        let cmdline = (
            "etc/kernel/cmdline",
            "root=LABEL=cloudimg-rootfs ro\n".to_string(),
        );
        let mut host = restored_ubuntu(dir.path(), &[fstab, resume, cmdline]);
        std::fs::write(
            dir.path().join("boot/grub/grub.cfg"),
            "search --no-floppy --label --set=root BOOT\n\
             linux /vmlinuz root=LABEL=cloudimg-rootfs ro\n\
             chainloader /EFI/BOOT/BOOTX64.EFI\n",
        )
        .unwrap();
        host.ids
            .get_mut("/dev/sdb1")
            .unwrap()
            .push(label(IdentityKind::Ext, "cloudimg-rootfs"));
        host.ids
            .get_mut("/dev/sdb16")
            .unwrap()
            .push(label(IdentityKind::Ext, "BOOT"));
        // What blkid reads from /boot once its label changed.
        host.names.insert(
            "/dev/sdb16".into(),
            FsNames {
                label: Some("BOOT-r".into()),
                partuuid: None,
            },
        );
        let mut present = the_machine();
        present.get_mut("sda2").unwrap().extend([
            id(IdentityKind::Label, "cloudimg-rootfs"),
            id(IdentityKind::Label, "BOOT"),
        ]);
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &present,
            &[dev("/dev/sdb1", 1), dev("/dev/sdb16", 16)],
            true,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let root = dir.path().join("root");
        assert_eq!(
            std::fs::read_to_string(root.join("etc/fstab")).unwrap(),
            "LABEL=cloudimg-rootf-r / ext4 defaults 0 1\nLABEL=BOOT-r /boot ext4 defaults 0 2\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("etc/kernel/cmdline")).unwrap(),
            "root=LABEL=cloudimg-rootf-r ro\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("boot/grub/grub.cfg")).unwrap(),
            "search --no-floppy --label --set=root BOOT-r\n\
             linux /vmlinuz root=LABEL=cloudimg-rootf-r ro\n\
             chainloader /EFI/BOOT/BOOTX64.EFI\n"
        );
        // /etc/kernel/cmdline is copied into the image: rebuilt, with /boot
        // found by the label it has now.
        assert_eq!(host.rebuilds.len(), 1, "{:?}", out.notes);
        assert_eq!(host.rebuilds[0].mounts[0].node, "/dev/sdb16");
    }

    #[test]
    fn a_reencrypted_container_is_changed_then_opened_under_a_hold_and_closed() {
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb3".into(), vec![id(IdentityKind::Luks, SRC)]);
        let mapper = format!("/dev/mapper/obk-reid-{}-1", std::process::id());
        host.ids.insert(
            mapper.clone(),
            vec![id(
                IdentityKind::Btrfs,
                "22222222-0000-4000-8000-000000000000",
            )],
        );
        let present = machine(vec![
            id(IdentityKind::Luks, SRC),
            id(IdentityKind::Btrfs, "22222222-0000-4000-8000-000000000000"),
        ]);
        let mut tools = FakeTools::default();
        let devices = [RestoredDevice {
            passphrase: Some("secret".into()),
            ..dev("/dev/sdb3", 3)
        }];
        let out = change_identities(&mut tools, &mut host, &present, &devices, false);
        assert_eq!(out.failed, None, "{out:?}");
        assert_eq!(
            programs(&tools),
            vec![
                "cryptsetup",
                "cryptsetup",
                "btrfs",
                "btrfstune",
                "cryptsetup"
            ]
        );
        assert_eq!(
            tools.calls[1].2.as_deref(),
            Some(&b"secret"[..]),
            "passphrase on stdin"
        );
        assert_eq!(tools.calls[4].1[0], "close");
        assert!(host.holds[0][0].contains("DM_NAME"), "the mapping is held");
        assert_eq!(out.changed.len(), 2);
        assert_eq!(out.changed[1].node, mapper);
    }

    #[test]
    fn a_volume_group_is_imported_under_a_new_name_and_its_lvs_looked_into() {
        let vg_old = "aaaaaa-1111-2222-3333-4444-5555-bbbbbb";
        let vg_new = "cccccc-1111-2222-3333-4444-5555-dddddd";
        let mut host = FakeHost::default();
        host.ids.insert(
            "/dev/sdb3".into(),
            vec![
                id(
                    IdentityKind::LvmPv,
                    "pv0000-1111-2222-3333-4444-5555-pv0000",
                ),
                id(IdentityKind::LvmVg, vg_old),
                id(IdentityKind::LvmVgName, "ubuntu-vg"),
            ],
        );
        host.ids.insert(
            "/dev/mapper/ubuntu--vg_restored-ubuntu--lv".into(),
            vec![id(IdentityKind::Ext, SRC)],
        );
        let mut tools = FakeTools::default();
        tools
            .stdout
            .insert("lvs".into(), "  ubuntu-lv|active\n  swap|\n".into());
        let present = {
            let mut m = machine(vec![
                id(
                    IdentityKind::LvmPv,
                    "pv0000-1111-2222-3333-4444-5555-pv0000",
                ),
                id(IdentityKind::Ext, SRC),
            ]);
            m.insert(
                "dm-0".into(),
                vec![id(IdentityKind::LvmVgName, "ubuntu-vg")],
            );
            m
        };
        let mut run_host = host;
        // After vgimportclone: new PV/VG UUIDs and name.
        let after = vec![
            id(
                IdentityKind::LvmPv,
                "pv1111-1111-2222-3333-4444-5555-pv1111",
            ),
            id(IdentityKind::LvmVg, vg_new),
            id(IdentityKind::LvmVgName, "ubuntu-vg_restored"),
        ];
        // identities_on: 1st call (walk) and 2nd (old PV) return the old ids,
        // the 3rd the new ones — `after` swaps them in on the 2nd call.
        let old = run_host.ids.get("/dev/sdb3").cloned().unwrap();
        struct Seq<'a> {
            inner: &'a mut FakeHost,
            calls: u32,
            old: Vec<Identity>,
            after: Vec<Identity>,
        }
        impl Host for Seq<'_> {
            fn identities_on(&mut self, node: &str) -> Result<Vec<Identity>> {
                if node == "/dev/sdb3" {
                    self.calls += 1;
                    return Ok(if self.calls <= 2 {
                        self.old.clone()
                    } else {
                        self.after.clone()
                    });
                }
                self.inner.identities_on(node)
            }
            fn new_uuid(&mut self) -> String {
                self.inner.new_uuid()
            }
            fn mount(&mut self, n: &str, f: &str, o: &str) -> Result<Box<dyn MountedFs>> {
                self.inner.mount(n, f, o)
            }
            fn hold(&mut self, r: Vec<String>) -> Result<Box<dyn std::any::Any>> {
                self.inner.hold(r)
            }
            fn settle(&mut self) {}
            fn free_md_name(&mut self) -> String {
                self.inner.free_md_name()
            }
            fn kernel(&self) -> Option<KernelVersion> {
                None
            }
            fn btrfs_devices(&mut self, _node: &str) -> u64 {
                1
            }
            fn fs_names(&mut self, node: &str) -> FsNames {
                self.inner.fs_names(node)
            }
            fn dm_name_free(&mut self, name: &str) -> bool {
                self.inner.dm_name_free(name)
            }
            fn rebuild_initramfs(&mut self, req: &initramfs::Request) -> initramfs::Rebuilt {
                self.inner.rebuild_initramfs(req)
            }
        }
        let mut seq = Seq {
            inner: &mut run_host,
            calls: 0,
            old,
            after,
        };
        let out = change_identities(
            &mut tools,
            &mut seq,
            &present,
            &[dev("/dev/sdb3", 3)],
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        assert_eq!(
            programs(&tools),
            vec![
                "vgimportclone",
                "vgchange",
                "lvs",
                "e2fsck",
                "tune2fs",
                "vgchange"
            ]
        );
        assert_eq!(
            tools.calls[0].1,
            args(["--basevgname", "ubuntu-vg_restored", "/dev/sdb3"])
        );
        assert_eq!(tools.calls[5].1, args(["-an", "ubuntu-vg_restored"]));
        let kinds: Vec<IdentityKind> = out.changed.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                IdentityKind::LvmPv,
                IdentityKind::LvmVgName,
                IdentityKind::LvmVg,
                IdentityKind::Ext
            ]
        );
        assert!(run_host.holds[0][0].contains("LVM-cccccc11112222333344445555dddddd"));
    }

    #[test]
    fn restored_md_members_are_assembled_alone_with_a_new_uuid() {
        let md = "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d";
        let mut host = FakeHost::default();
        for n in ["/dev/sdb1", "/dev/sdc1"] {
            host.ids.insert(n.into(), vec![id(IdentityKind::Md, md)]);
        }
        let present = machine(vec![id(IdentityKind::Md, SRC)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[dev("/dev/sdb1", 1), dev("/dev/sdc1", 1)],
            false,
        );
        assert_eq!(out.failed, None);
        assert_eq!(programs(&tools), vec!["mdadm", "mdadm"]);
        let a = &tools.calls[0].1;
        assert_eq!(
            &a[..4],
            &args(["--assemble", "/dev/md126", "--run", "--update=uuid"])
        );
        assert!(a[4].starts_with("--uuid=") && a[4].matches(':').count() == 3);
        assert_eq!(&a[5..], &args(["/dev/sdb1", "/dev/sdc1"]));
        assert_eq!(tools.calls[1].1, args(["--stop", "/dev/md126"]));
        assert_eq!(out.changed.len(), 2, "both members");
        assert!(host.holds[0][0].contains("KERNEL==\"md126\""));
    }

    /// The array assembled from the restored members is read like any
    /// restored device, and its filesystem is changed even when the machine's
    /// own array is not assembled (its filesystem then shows nowhere): it is
    /// the same filesystem, and assembling the original later would meet it.
    #[test]
    fn the_filesystem_inside_a_restored_array_gets_a_new_uuid() {
        let md = "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d";
        let inner = "77777777-1111-4222-8333-444444444444";
        let mut host = FakeHost::default();
        for n in ["/dev/sdb3", "/dev/sdb4"] {
            host.ids.insert(n.into(), vec![id(IdentityKind::Md, md)]);
        }
        host.ids
            .insert("/dev/md126".into(), vec![id(IdentityKind::Ext, inner)]);
        // The machine: the original members only — no array, no ext4.
        let present = machine(vec![id(IdentityKind::Md, md)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[dev("/dev/sdb3", 3), dev("/dev/sdb4", 4)],
            false,
        );
        assert_eq!(out.failed, None);
        assert_eq!(
            programs(&tools),
            vec!["mdadm", "e2fsck", "tune2fs", "mdadm"],
            "assemble, change the ext4 inside, stop"
        );
        assert_eq!(tools.calls[2].1[2], "/dev/md126");
        let ext = out
            .changed
            .iter()
            .find(|c| c.kind == IdentityKind::Ext)
            .expect("the ext4 inside the array changed");
        assert_eq!((ext.node.as_str(), ext.part_index), ("/dev/md126", Some(3)));

        // A filesystem on a plain restored partition the machine does not
        // have is still left alone.
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb5".into(), vec![id(IdentityKind::Ext, inner)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[dev("/dev/sdb5", 5)],
            false,
        );
        assert_eq!((out.failed, out.changed.len()), (None, 0));
        assert!(tools.calls.is_empty());
    }

    /// A restored device that cannot be read fails the run instead of
    /// passing for one without identities.
    #[test]
    fn an_unreadable_restored_device_fails_the_run() {
        struct Broken;
        impl Host for Broken {
            fn identities_on(&mut self, node: &str) -> Result<Vec<Identity>> {
                Err(OwnBackupError::Device(format!("open({node}): EIO")))
            }
            fn new_uuid(&mut self) -> String {
                String::new()
            }
            fn mount(&mut self, _: &str, _: &str, _: &str) -> Result<Box<dyn MountedFs>> {
                unreachable!()
            }
            fn hold(&mut self, _: Vec<String>) -> Result<Box<dyn std::any::Any>> {
                Ok(Box::new(()))
            }
            fn settle(&mut self) {}
            fn free_md_name(&mut self) -> String {
                "md126".into()
            }
            fn kernel(&self) -> Option<KernelVersion> {
                None
            }
            fn btrfs_devices(&mut self, _node: &str) -> u64 {
                1
            }
            fn fs_names(&mut self, _node: &str) -> FsNames {
                FsNames::default()
            }
            fn dm_name_free(&mut self, _name: &str) -> bool {
                unreachable!()
            }
            fn rebuild_initramfs(&mut self, _req: &initramfs::Request) -> initramfs::Rebuilt {
                unreachable!()
            }
        }
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut Broken,
            &machine(Vec::new()),
            &[dev("/dev/sdb1", 1)],
            false,
        );
        assert!(out.failed.as_deref().is_some_and(|e| e.contains("EIO")));
    }

    #[test]
    fn a_failing_tool_stops_the_run_but_stacked_devices_still_close() {
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb3".into(), vec![id(IdentityKind::Luks, SRC)]);
        let mapper = format!("/dev/mapper/obk-reid-{}-1", std::process::id());
        host.ids.insert(mapper, vec![id(IdentityKind::Ext, SRC)]);
        let present = machine(vec![id(IdentityKind::Ext, SRC)]);
        let mut tools = FakeTools::default();
        tools.codes.insert("tune2fs".into(), 1);
        let out = change_identities(
            &mut tools,
            &mut host,
            &present,
            &[RestoredDevice {
                passphrase: Some("pw".into()),
                ..dev("/dev/sdb3", 3)
            }],
            true,
        );
        assert!(
            out.failed.as_deref().unwrap_or("").contains("tune2fs"),
            "{out:?}"
        );
        assert_eq!(programs(&tools).last(), Some(&"cryptsetup"));
        assert_eq!(tools.calls.last().unwrap().1[0], "close");
        assert!(host.mounts.is_empty(), "nothing is edited after a failure");
    }

    /// A failed assembly can leave an inactive array holding the members:
    /// it is stopped like a working one, without a note about a device that
    /// may never have existed.
    #[test]
    fn a_failed_assembly_is_still_stopped() {
        let md = "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d";
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb1".into(), vec![id(IdentityKind::Md, md)]);
        let mut tools = FakeTools::default();
        tools.codes.insert("mdadm".into(), 1);
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![id(IdentityKind::Md, md)]),
            &[dev("/dev/sdb1", 1)],
            false,
        );
        assert!(out.failed.as_deref().unwrap_or("").contains("mdadm"));
        assert_eq!(programs(&tools), vec!["mdadm", "mdadm"]);
        assert_eq!(tools.calls[1].1, args(["--stop", "/dev/md126"]));
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    /// A volume group whose name could not be read gets a new one, but no
    /// VG-name reference is edited (an invented old name would match
    /// whatever the restored system calls `vg`), and the person is told. A
    /// failed activation is undone.
    #[test]
    fn a_volume_group_without_a_readable_name_renames_no_references() {
        let vg = "aaaaaa-1111-2222-3333-4444-5555-bbbbbb";
        let pv = "pv0000-1111-2222-3333-4444-5555-pv0000";
        let mut host = FakeHost::default();
        host.ids.insert(
            "/dev/sdb3".into(),
            vec![id(IdentityKind::LvmPv, pv), id(IdentityKind::LvmVg, vg)],
        );
        let mut tools = FakeTools::default();
        tools.codes.insert("vgchange".into(), 5);
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![id(IdentityKind::LvmPv, pv)]),
            &[dev("/dev/sdb3", 3)],
            true,
        );
        assert_eq!(
            tools.calls[0].1,
            args(["--basevgname", "vg_restored", "/dev/sdb3"])
        );
        assert!(out
            .changed
            .iter()
            .all(|c| c.kind != IdentityKind::LvmVgName));
        assert!(
            out.notes.iter().any(|n| n.contains("vg_restored")),
            "{:?}",
            out.notes
        );
        // Activation failed: deactivated again all the same.
        assert!(out.failed.as_deref().unwrap_or("").contains("vgchange"));
        assert_eq!(
            programs(&tools),
            vec!["vgimportclone", "vgchange", "vgchange"]
        );
        assert_eq!(tools.calls[2].1, args(["-an", "vg_restored"]));
    }

    /// A btrfs spanning several devices keeps its fsid, and a copy still
    /// carrying the machine's is never mounted for editing — not even on a
    /// kernel with temporary fsids, which covers single-device ones only.
    #[test]
    fn a_multi_device_btrfs_copy_is_never_mounted_for_editing() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = FakeHost {
            mount_dir: Some(dir.path().to_path_buf()),
            kernel: Some((6, 8)),
            ..Default::default()
        };
        let ext = "77777777-1111-4222-8333-444444444444";
        host.ids
            .insert("/dev/sdb2".into(), vec![id(IdentityKind::Ext, ext)]);
        host.ids
            .insert("/dev/sdb3".into(), vec![Identity::btrfs_multi_device(SRC)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![
                id(IdentityKind::Ext, ext),
                id(IdentityKind::Btrfs, SRC),
            ]),
            &[dev("/dev/sdb2", 2), dev("/dev/sdb3", 3)],
            true,
        );
        assert_eq!(out.failed, None, "{out:?}");
        assert_eq!(programs(&tools), vec!["e2fsck", "tune2fs"]);
        assert_eq!(host.mounts.len(), 1, "{:?}", host.mounts);
        assert_eq!(host.mounts[0].0, "/dev/sdb2");
        assert!(out.notes.iter().filter(|n| n.contains("/dev/sdb3")).count() >= 2);
    }

    #[test]
    fn references_are_fixed_on_every_restored_filesystem_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/fstab"),
            format!("UUID={SRC} / ext4 defaults 0 1\n"),
        )
        .unwrap();
        let mut host = FakeHost {
            mount_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        host.ids
            .insert("/dev/sdb2".into(), vec![id(IdentityKind::Ext, SRC)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![id(IdentityKind::Ext, SRC)]),
            &[dev("/dev/sdb2", 2)],
            true,
        );
        assert_eq!(out.failed, None);
        assert_eq!(out.edited, vec!["/dev/sdb2:etc/fstab".to_string()]);
        let fstab = std::fs::read_to_string(dir.path().join("etc/fstab")).unwrap();
        assert!(fstab.contains(&out.changed[0].new), "{fstab}");
        assert_eq!(host.mounts[0].1, "ext4");
    }

    const BOOT: &str = "77777777-1111-4222-8333-444444444444";

    /// A restored Ubuntu: its root (`etc/os-release`, `etc/fstab`, the tool)
    /// and its /boot, as directories the fake mounts hand out by node.
    fn ubuntu(dir: &Path, root_uuid: &str, extra: &[(&str, String)]) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = dir.join("root");
        let boot = dir.join("boot");
        for d in ["etc", "usr/sbin", "usr/lib"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(boot.join("grub")).unwrap();
        std::fs::write(
            root.join("usr/lib/os-release"),
            "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nID_LIKE=debian\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../usr/lib/os-release", root.join("etc/os-release")).unwrap();
        std::fs::write(
            root.join("etc/fstab"),
            format!("UUID={root_uuid} / ext4 defaults 0 1\nLABEL=BOOT /boot ext4 defaults 0 2\n"),
        )
        .unwrap();
        let tool = root.join("usr/sbin/update-initramfs");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (rel, text) in extra {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let _ = std::fs::remove_file(&p);
            std::fs::write(p, text).unwrap();
        }
        (root, boot)
    }

    fn restored_ubuntu(dir: &Path, extra: &[(&str, String)]) -> FakeHost {
        let (root, boot) = ubuntu(dir, SRC, extra);
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb1".into(), vec![id(IdentityKind::Ext, SRC)]);
        host.ids
            .insert("/dev/sdb16".into(), vec![id(IdentityKind::Ext, BOOT)]);
        host.mount_dirs.insert("/dev/sdb1".into(), root);
        host.mount_dirs.insert("/dev/sdb16".into(), boot);
        host.names.insert(
            "/dev/sdb16".into(),
            FsNames {
                label: Some("BOOT".into()),
                partuuid: None,
            },
        );
        host
    }

    fn the_machine() -> BTreeMap<String, Vec<Identity>> {
        machine(vec![
            id(IdentityKind::Ext, SRC),
            id(IdentityKind::Ext, BOOT),
        ])
    }

    fn resume_on_root() -> (&'static str, String) {
        (
            "etc/initramfs-tools/conf.d/resume",
            format!("RESUME=UUID={SRC}\n"),
        )
    }

    /// The root's new UUID is the resume device the image copies: the
    /// system's own tool rebuilds it, inside the restored root with its /boot
    /// — resolved by the restored fstab's LABEL to the restored partition.
    #[test]
    fn an_image_keeping_a_changed_uuid_is_rebuilt_by_the_systems_own_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = restored_ubuntu(dir.path(), &[resume_on_root()]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &the_machine(),
            &[dev("/dev/sdb1", 1), dev("/dev/sdb16", 16)],
            true,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let new_root = &out
            .changed
            .iter()
            .find(|c| c.node == "/dev/sdb1")
            .unwrap()
            .new;
        let resume =
            std::fs::read_to_string(dir.path().join("root/etc/initramfs-tools/conf.d/resume"))
                .unwrap();
        assert!(resume.contains(new_root.as_str()), "fixed first: {resume}");
        assert_eq!(host.rebuilds.len(), 1, "{:?}", out.notes);
        let req = &host.rebuilds[0];
        assert_eq!(
            (
                req.root_node.as_str(),
                req.root_fstype.as_str(),
                req.root_opts.as_str()
            ),
            ("/dev/sdb1", "ext4", "rw")
        );
        assert_eq!(req.tool, initramfs::Tool::UpdateInitramfs);
        assert_eq!(req.program, "/usr/sbin/update-initramfs");
        assert_eq!(req.mounts.len(), 1);
        assert_eq!(
            (req.mounts[0].target.as_str(), req.mounts[0].node.as_str()),
            ("/boot", "/dev/sdb16")
        );
        assert!(
            out.notes.iter().any(|n| n.contains("(update-initramfs)")),
            "{:?}",
            out.notes
        );
    }

    /// dracut 109 and later build host-only images unless told otherwise, and
    /// those wait for /boot by its UUID: a /boot whose UUID changed alone
    /// makes the restored system's image stale, so it is rebuilt — the image
    /// stores the device command line, which the rebuild now records by
    /// by-uuid links (no longer a reason to leave it). An older dracut that
    /// is not configured host-only builds generic images: nothing to do.
    #[test]
    fn a_hostonly_dracut_image_is_rebuilt_when_boots_uuid_changed() {
        use std::os::unix::fs::PermissionsExt;
        let only_boot = || machine(vec![id(IdentityKind::Ext, BOOT)]);
        let devices = [dev("/dev/sdb1", 1), dev("/dev/sdb16", 16)];
        for (version, rebuilt) in [("110-11ubuntu0.1", true), ("108", false)] {
            let dir = tempfile::tempdir().unwrap();
            let dracut = (
                "usr/bin/dracut",
                format!("#!/bin/bash\nDRACUT_VERSION=\"{version}\"\n"),
            );
            let mut host = restored_ubuntu(dir.path(), &[dracut]);
            std::fs::set_permissions(
                dir.path().join("root/usr/bin/dracut"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            let out = change_identities(
                &mut FakeTools::default(),
                &mut host,
                &only_boot(),
                &devices,
                true,
            );
            assert_eq!(out.failed, None, "{out:?}");
            assert!(
                out.changed.iter().all(|c| c.node == "/dev/sdb16"),
                "{:?}",
                out.changed
            );
            assert_eq!(
                host.rebuilds.len(),
                usize::from(rebuilt),
                "dracut {version}: {:?}",
                out.notes
            );
        }
    }

    /// Without fixing references there is nothing new for an image to keep;
    /// with nothing of the changes in what the image copies there is no
    /// reason to rebuild; on an unknown system nothing is guessed.
    #[test]
    fn no_rebuild_without_fixed_references_a_reason_or_a_known_tool() {
        let devices = [dev("/dev/sdb1", 1), dev("/dev/sdb16", 16)];
        let dir = tempfile::tempdir().unwrap();
        let mut host = restored_ubuntu(dir.path(), &[resume_on_root()]);
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &the_machine(),
            &devices,
            false,
        );
        assert!(host.rebuilds.is_empty() && out.notes.is_empty(), "{out:?}");

        let dir = tempfile::tempdir().unwrap();
        let mut host = restored_ubuntu(dir.path(), &[]);
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &the_machine(),
            &devices,
            true,
        );
        assert!(host.rebuilds.is_empty(), "{out:?}");
        assert!(
            !out.notes.iter().any(|n| n.contains("initramfs")),
            "{:?}",
            out.notes
        );

        let dir = tempfile::tempdir().unwrap();
        let os = (
            "usr/lib/os-release",
            "ID=nixos\nPRETTY_NAME=\"NixOS 26.05\"\n".to_string(),
        );
        let mut host = restored_ubuntu(dir.path(), &[resume_on_root(), os]);
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &the_machine(),
            &devices,
            true,
        );
        assert!(host.rebuilds.is_empty());
        assert!(
            out.notes.iter().any(|n| n.contains("NixOS 26.05")),
            "{:?}",
            out.notes
        );
    }

    /// A failed rebuild leaves the run successful — the data is restored —
    /// and says what to run and what the tool said last.
    #[test]
    fn a_failed_rebuild_is_a_follow_up_with_the_command_not_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = restored_ubuntu(dir.path(), &[resume_on_root()]);
        host.rebuilt = Some(initramfs::Rebuilt::Failed {
            error: "exit code 1".into(),
            output: "update-initramfs: Generating /boot/initrd.img-7.0\nE: no space left\n".into(),
        });
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &the_machine(),
            &[dev("/dev/sdb1", 1), dev("/dev/sdb16", 16)],
            true,
        );
        assert_eq!(out.failed, None);
        let note = out
            .notes
            .iter()
            .find(|n| n.contains("update-initramfs -u -k all"))
            .unwrap_or_else(|| panic!("{:?}", out.notes));
        assert!(
            note.contains("E: no space left") && note.contains("exit code 1"),
            "{note}"
        );
    }

    /// A /boot the restored fstab names but the restore did not write: no
    /// rebuild into the root's empty directory.
    #[test]
    fn a_boot_partition_that_was_not_restored_stops_the_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = restored_ubuntu(dir.path(), &[resume_on_root()]);
        let out = change_identities(
            &mut FakeTools::default(),
            &mut host,
            &the_machine(),
            &[dev("/dev/sdb1", 1)],
            true,
        );
        assert!(host.rebuilds.is_empty());
        assert!(
            out.notes.iter().any(|n| n.contains("LABEL=BOOT")),
            "{:?}",
            out.notes
        );
    }

    /// The root inside a re-encrypted LUKS container: the container's new
    /// UUID is in crypttab, so the image must be rebuilt, and the mapping
    /// opened under a temporary name takes the crypttab name first (under a
    /// hold of its own) — that name is how the system's tool finds it — and
    /// is closed under it.
    #[test]
    fn a_root_in_luks_is_rebuilt_under_its_crypttab_name() {
        let inner = "22222222-0000-4000-8000-000000000000";
        let system = |dir: &Path| {
            let crypttab = ("etc/crypttab", format!("dm_crypt-0 UUID={SRC} none luks\n"));
            let fstab = ("etc/fstab", format!("UUID={inner} / ext4 defaults 0 1\n"));
            ubuntu(dir, inner, &[crypttab, fstab]).0
        };
        let mapper = format!("/dev/mapper/obk-reid-{}-1", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let root = system(dir.path());
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb3".into(), vec![id(IdentityKind::Luks, SRC)]);
        host.ids
            .insert(mapper.clone(), vec![id(IdentityKind::Ext, inner)]);
        host.mount_dirs.insert(mapper.clone(), root.clone());
        let devices = [RestoredDevice {
            passphrase: Some("pw".into()),
            ..dev("/dev/sdb3", 3)
        }];
        let present = machine(vec![id(IdentityKind::Luks, SRC)]);
        let mut tools = FakeTools::default();
        let out = change_identities(&mut tools, &mut host, &present, &devices, true);
        assert_eq!(out.failed, None, "{out:?}");
        let crypttab = std::fs::read_to_string(root.join("etc/crypttab")).unwrap();
        assert!(crypttab.contains(&out.changed[0].new), "{crypttab}");
        let rename = tools
            .calls
            .iter()
            .find(|(p, _, _)| p == "dmsetup")
            .expect("renamed");
        assert_eq!(
            rename.1,
            args([
                "rename",
                mapper.trim_start_matches("/dev/mapper/"),
                "dm_crypt-0"
            ])
        );
        assert!(host
            .holds
            .iter()
            .any(|h| h[0].contains("ENV{DM_NAME}==\"dm_crypt-0\"")));
        assert_eq!(host.rebuilds.len(), 1, "{:?}", out.notes);
        assert_eq!(host.rebuilds[0].root_node, "/dev/mapper/dm_crypt-0");
        assert!(host.rebuilds[0].mounts.is_empty());
        assert_eq!(tools.calls.last().unwrap().1, args(["close", "dm_crypt-0"]));

        // The name is the machine's own: no rename, no rebuild, a note.
        let dir = tempfile::tempdir().unwrap();
        host.mount_dirs.insert(mapper.clone(), system(dir.path()));
        host.rebuilds.clear();
        host.dm_taken.push("dm_crypt-0".into());
        let mut tools = FakeTools::default();
        let out = change_identities(&mut tools, &mut host, &present, &devices, true);
        assert!(host.rebuilds.is_empty());
        assert!(!tools.calls.iter().any(|(p, _, _)| p == "dmsetup"));
        assert!(
            out.notes.iter().any(|n| n.contains("dm_crypt-0")),
            "{:?}",
            out.notes
        );
        assert_eq!(
            tools.calls.last().unwrap().1,
            args(["close", mapper.trim_start_matches("/dev/mapper/")]),
            "closed under the temporary name"
        );
    }

    #[test]
    fn crypttab_names_its_container_by_uuid() {
        let key = Identity::new(IdentityKind::Luks, SRC).key;
        let text = format!(
            "# <target> <source>\nswap /dev/sda2 /dev/urandom swap\nluks-x \"UUID={}\" none\n",
            SRC.to_ascii_uppercase()
        );
        assert_eq!(crypttab_name(&text, &key), Some("luks-x".into()));
        let text = format!("root /dev/disk/by-uuid/{SRC} none luks\n");
        assert_eq!(crypttab_name(&text, &key), Some("root".into()));
        assert_eq!(crypttab_name("root /dev/sda3 none\n", &key), None);
    }

    #[test]
    fn a_partuuid_or_a_unique_identity_is_never_touched() {
        let mut host = FakeHost::default();
        host.ids
            .insert("/dev/sdb2".into(), vec![id(IdentityKind::Ext, SRC)]);
        let mut tools = FakeTools::default();
        let out = change_identities(
            &mut tools,
            &mut host,
            &machine(vec![id(IdentityKind::PartUuid, SRC)]),
            &[dev("/dev/sdb2", 2)],
            true,
        );
        assert!(tools.calls.is_empty());
        assert!(out.changed.is_empty() && out.edited.is_empty());
    }

    #[test]
    fn lvs_output_gives_the_active_volumes_mapper_nodes() {
        assert_eq!(
            parse_lvs("  root|active\n  swap|\n  my-data|active\n", "vg-1"),
            vec![
                "/dev/mapper/vg--1-root".to_string(),
                "/dev/mapper/vg--1-my--data".to_string()
            ]
        );
        assert_eq!(
            md_colons("1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"),
            "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d"
        );
        assert_eq!(
            fat_serial_from("0f0e0d0c-aaaa-4000-8000-000000000000"),
            ("0F0E0D0C".to_string(), "0F0E-0D0C".to_string())
        );
    }

    fn have(prog: &str) -> bool {
        std::process::Command::new("sh")
            .args([
                "-c",
                &format!("command -v {prog} || test -x /usr/sbin/{prog} || test -x /sbin/{prog}"),
            ])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn image(dir: &Path, name: &str, mib: u64) -> String {
        let p = dir.join(name);
        std::fs::File::create(&p)
            .unwrap()
            .set_len(mib << 20)
            .unwrap();
        p.to_str().unwrap().to_string()
    }

    fn sh(cmd: &str) {
        let ok = std::process::Command::new("sh")
            .args(["-c", cmd])
            .status()
            .unwrap()
            .success();
        assert!(ok, "{cmd}");
    }

    /// The machine as the change sees it: one device ("sda9") carrying what
    /// the restored files carry now, as the source would.
    fn source_of(nodes: &[&str]) -> BTreeMap<String, Vec<Identity>> {
        let mut ids = Vec::new();
        for n in nodes {
            ids.extend(SystemHost.identities_on(n).unwrap());
        }
        let mut m = BTreeMap::new();
        m.insert("sda9".to_string(), ids);
        m
    }

    /// e2fsprogs, btrfs-progs and dosfstools on partition-sized files: each
    /// copy reads back with the new identity the run reported. No root, no
    /// mount.
    #[test]
    fn real_tools_give_ext4_btrfs_and_fat_copies_new_identities() {
        if !(have("mkfs.ext4") && have("mkfs.btrfs") && have("btrfstune") && have("fatlabel")) {
            eprintln!("e2fsprogs/btrfs-progs/dosfstools not installed — skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ext = image(dir.path(), "ext.img", 32);
        let btr = image(dir.path(), "btrfs.img", 128);
        let fat = image(dir.path(), "fat.img", 64);
        sh(&format!("mkfs.ext4 -q -F {ext}"));
        sh(&format!("mkfs.btrfs -q -f {btr} >/dev/null"));
        sh(&format!("mkfs.vfat -F 32 {fat} >/dev/null"));
        let present = source_of(&[&ext, &btr, &fat]);
        let devices: Vec<RestoredDevice> = [&ext, &btr, &fat]
            .iter()
            .enumerate()
            .map(|(i, n)| dev(n, i as u32 + 1))
            .collect();
        let out = change_identities(
            &mut crate::linux_tools::SystemTools,
            &mut SystemHost,
            &present,
            &devices,
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        assert_eq!(out.changed.len(), 3, "{out:?}");
        for c in &out.changed {
            let now = SystemHost.identities_on(&c.node).unwrap();
            assert!(
                now.iter()
                    .any(|i| i.kind == c.kind && i.key == Identity::new(c.kind, &c.new).key),
                "{c:?} reads back as {now:?}"
            );
            assert_ne!(
                Identity::new(c.kind, &c.old).key,
                Identity::new(c.kind, &c.new).key
            );
        }
        let fsck = std::process::Command::new("e2fsck")
            .args(["-f", "-n", &ext])
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stdout)
        );
        let check = std::process::Command::new("btrfs")
            .args(["check", "--readonly", &btr])
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "{}",
            String::from_utf8_lossy(&check.stderr)
        );
    }

    fn blkid_label(path: &str) -> String {
        let out = std::process::Command::new("blkid")
            .args(["-p", "-o", "value", "-s", "LABEL", path])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Labels as the Ubuntu cloud image has them, on partition-sized files:
    /// what this module reads is what blkid reads (XFS too), and e2fsprogs,
    /// btrfs-progs and dosfstools give each copy the new label the run
    /// reported, the filesystems still clean. No root, no mount.
    #[test]
    fn real_tools_relabel_ext4_btrfs_and_fat_copies() {
        let tools = [
            "mkfs.ext4",
            "mkfs.btrfs",
            "btrfs",
            "mkfs.vfat",
            "fatlabel",
            "fsck.fat",
        ];
        if !tools.iter().all(|t| have(t)) || !have("blkid") {
            eprintln!("e2fsprogs/btrfs-progs/dosfstools/blkid not installed — skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ext = image(dir.path(), "ext.img", 32);
        let btr = image(dir.path(), "btrfs.img", 128);
        let fat = image(dir.path(), "fat.img", 64);
        sh(&format!("mkfs.ext4 -q -F -L cloudimg-rootfs {ext}"));
        sh(&format!("mkfs.btrfs -q -f -L BOOT {btr} >/dev/null"));
        sh(&format!("mkfs.vfat -F 32 -n UEFI {fat} >/dev/null"));
        let mut read = vec![ext.clone(), btr.clone(), fat.clone()];
        if have("mkfs.xfs") {
            let xfs = image(dir.path(), "xfs.img", 320);
            sh(&format!("mkfs.xfs -q -f -L twelve-bytes {xfs}"));
            read.push(xfs);
        }
        let label_on = |n: &str| {
            SystemHost
                .identities_on(n)
                .unwrap()
                .into_iter()
                .find(|i| i.kind == IdentityKind::Label)
                .map(|i| i.shown)
        };
        for n in &read {
            assert_eq!(label_on(n), Some(blkid_label(n)), "{n}");
        }
        let present = source_of(&[&ext, &btr, &fat]);
        let devices: Vec<RestoredDevice> = [&ext, &btr, &fat]
            .iter()
            .enumerate()
            .map(|(i, n)| dev(n, i as u32 + 1))
            .collect();
        let out = change_identities(
            &mut crate::linux_tools::SystemTools,
            &mut SystemHost,
            &present,
            &devices,
            false,
        );
        assert_eq!(out.failed, None, "{out:?}");
        let labels: Vec<(&str, &str)> = out
            .changed
            .iter()
            .filter(|c| c.kind == IdentityKind::Label)
            .map(|c| (c.old.as_str(), c.new.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("cloudimg-rootfs", "cloudimg-rootf-r"),
                ("BOOT", "BOOT-r"),
                ("UEFI", "UEFI-R")
            ]
        );
        for c in out.changed.iter().filter(|c| c.kind == IdentityKind::Label) {
            assert_eq!(blkid_label(&c.node), c.new, "{c:?}");
            assert_eq!(label_on(&c.node).as_deref(), Some(c.new.as_str()));
        }
        for (prog, a) in [
            ("e2fsck", vec!["-f", "-n", &ext]),
            ("btrfs", vec!["check", "--readonly", &btr]),
            ("fsck.fat", vec!["-n", &fat]),
        ] {
            let o = std::process::Command::new(prog).args(&a).output().unwrap();
            assert!(
                o.status.success(),
                "{prog}: {}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }

    /// As root (loop mounts): an XFS copy (its log replayed by a mount first)
    /// and an ext4 system whose fstab names both get new identities, and the
    /// fstab follows them.
    #[test]
    #[ignore = "root: mounts loop devices"]
    fn real_xfs_and_a_restored_fstab_follow_new_identities() {
        let dir = tempfile::tempdir().unwrap();
        let xfs = image(dir.path(), "xfs.img", 320);
        sh(&format!("mkfs.xfs -q -f {xfs}"));
        let xfs_uuid = SystemHost.identities_on(&xfs).unwrap()[0].shown.clone();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("etc")).unwrap();
        let root_uuid = "0a0b0c0d-1111-4222-8333-444455556666";
        std::fs::write(
            tree.join("etc/fstab"),
            format!(
                "UUID={root_uuid} / ext4 defaults 0 1\nUUID={xfs_uuid} /srv xfs defaults 0 2\n"
            ),
        )
        .unwrap();
        let ext = image(dir.path(), "root.img", 64);
        sh(&format!(
            "mkfs.ext4 -q -F -U {root_uuid} -d {} {ext}",
            tree.display()
        ));
        let present = source_of(&[&ext, &xfs]);
        let out = change_identities(
            &mut crate::linux_tools::SystemTools,
            &mut SystemHost,
            &present,
            &[dev(&ext, 2), dev(&xfs, 3)],
            true,
        );
        assert_eq!(out.failed, None, "{out:?}");
        assert_eq!(out.changed.len(), 2, "{out:?}");
        assert_eq!(out.edited, vec![format!("{ext}:etc/fstab")]);
        let fstab = std::process::Command::new("debugfs")
            .args(["-R", "cat /etc/fstab", &ext])
            .output()
            .unwrap();
        let fstab = String::from_utf8_lossy(&fstab.stdout);
        for c in &out.changed {
            assert!(fstab.contains(&format!("UUID={}", c.new)), "{fstab}");
        }
        assert!(
            !fstab.contains(root_uuid) && !fstab.contains(&xfs_uuid),
            "{fstab}"
        );
        let repair = std::process::Command::new("xfs_repair")
            .args(["-n", &xfs])
            .output()
            .unwrap();
        assert!(
            repair.status.success(),
            "{}",
            String::from_utf8_lossy(&repair.stderr)
        );
    }
}
