//! Identities a restore can duplicate on the same machine (PLAN §6, stage 3).
//!
//! A restored partition is a byte copy: its filesystem UUID, its LUKS UUID, its
//! LVM PV and VG UUIDs and its md array UUID are the source's. Restored onto a
//! second disk of the machine the source still lives on — or an image mounted
//! there — the kernel and its tools see two devices claiming to be one, and
//! each subsystem reacts differently:
//!
//! | what      | what goes wrong                                                    |
//! |-----------|--------------------------------------------------------------------|
//! | btrfs     | before kernel 6.7 a scanned clone joins the *mounted* filesystem as one of its devices; 6.7+ gives a single-device clone a temporary fsid on mount, never a multi-device one |
//! | LVM       | duplicate PVs: LVM picks one or refuses to activate; duplicate VG names clash outright |
//! | md        | incremental assembly can add the clone to the running array        |
//! | XFS       | the second mount is refused until mounted with `nouuid`            |
//! | ext2/3/4, FAT, LUKS, PARTUUID | both work, but `UUID=`/`PARTUUID=` in fstab, crypttab and the kernel command line resolve to whichever udev saw last |
//! | filesystem LABEL | the same for `LABEL=` and `/dev/disk/by-label` — one namespace for every filesystem type (the Ubuntu cloud image mounts `LABEL=cloudimg-rootfs`, `LABEL=BOOT`, `LABEL=UEFI`) |
//!
//! This module finds the duplicates — what the restored data carries, read
//! from its first bytes and its tail, against what the machine already has,
//! read from the udev database — and says how bad each one is and how it can
//! be changed. It changes nothing itself: re-identifying a filesystem also
//! means editing the restored system's fstab, crypttab and boot loader
//! configuration, and a PARTUUID of a Linux system is never changed silently
//! because `root=PARTUUID=` may name it.

use std::collections::BTreeMap;
use std::path::Path;

/// What kind of identity it is. Values are only compared within one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityKind {
    /// ext2/3/4 filesystem UUID.
    Ext,
    /// XFS filesystem UUID.
    Xfs,
    /// btrfs fsid.
    Btrfs,
    /// LUKS1/LUKS2 container UUID.
    Luks,
    /// LVM physical volume UUID.
    LvmPv,
    /// LVM volume group UUID.
    LvmVg,
    /// LVM volume group name — two VGs of one name clash even with distinct UUIDs.
    LvmVgName,
    /// md array UUID (the set every member carries).
    Md,
    /// GPT partition unique GUID.
    PartUuid,
    /// FAT volume serial (`XXXX-XXXX`) — an ESP's `UUID=` in fstab.
    Fat,
    /// Filesystem label (`LABEL=`, `/dev/disk/by-label`). One namespace for
    /// every filesystem type, so compared whatever carries it; which
    /// filesystem does is [`Identity::fs`]. Read from ext2/3/4, XFS, btrfs
    /// and FAT — the ones a restore can relabel.
    Label,
}

/// One identity, normalised for comparison and kept as shown for people.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    pub kind: IdentityKind,
    /// Comparison form: separators dropped; hexadecimal kinds lowercased. LVM
    /// UUIDs and names are case-sensitive and keep their case.
    pub key: String,
    /// As the subsystem's own tools print it.
    pub shown: String,
    /// btrfs only: the superblock says the filesystem spans more than one
    /// device. The kernel's temporary fsid (6.7+) covers single-device
    /// filesystems only, so a clone of such a member is always critical.
    /// Read from the data itself; the udev side never sets it.
    pub multi_device: bool,
    /// Labels only: the filesystem carrying it (`Ext`, `Xfs`, `Btrfs`,
    /// `Fat`), which decides how it can change and how long it may be. Read
    /// from the data; the udev side leaves it unset — a label of any
    /// filesystem takes the same `/dev/disk/by-label` name.
    pub fs: Option<IdentityKind>,
}

impl Identity {
    pub fn new(kind: IdentityKind, shown: &str) -> Self {
        let shown = shown.trim().to_string();
        let key = match kind {
            IdentityKind::LvmPv | IdentityKind::LvmVg => shown.replace('-', ""),
            // Names: case-sensitive, compared as written (by-label links are).
            IdentityKind::LvmVgName | IdentityKind::Label => shown.clone(),
            _ => shown
                .chars()
                .filter(|c| c.is_ascii_hexdigit())
                .collect::<String>()
                .to_ascii_lowercase(),
        };
        Self {
            kind,
            key,
            shown,
            multi_device: false,
            fs: None,
        }
    }

    /// The label of a filesystem of kind `fs`, as read from it.
    pub fn label(fs: IdentityKind, label: &str) -> Self {
        Self {
            fs: Some(fs),
            ..Self::new(IdentityKind::Label, label)
        }
    }

    /// A btrfs fsid whose superblock counts more than one device.
    pub fn btrfs_multi_device(shown: &str) -> Self {
        Self {
            multi_device: true,
            ..Self::new(IdentityKind::Btrfs, shown)
        }
    }
}

/// How serious a duplicate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Names (`UUID=`, `PARTUUID=`, `/dev/disk/by-uuid`) may resolve to either
    /// device; each works when addressed directly.
    Ambiguous,
    /// The second one cannot be mounted as is (XFS without `nouuid`).
    Mount,
    /// The kernel or a volume manager may merge them or pick the wrong one:
    /// data written to the clone could land on the original's set.
    Critical,
}

/// The command that gives the restored copy a new identity, when one exists.
/// Run against the RESTORED device, unmounted; the restored system's fstab,
/// crypttab and boot configuration then need the new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reidentify {
    /// `tune2fs -U random <dev>` (after `e2fsck -f`).
    Tune2fs,
    /// `xfs_admin -U generate <dev>`.
    XfsAdmin,
    /// `btrfstune -m <dev>` (metadata UUID, fast) or `-u` (rewrites all).
    Btrfstune,
    /// `cryptsetup luksUUID --uuid <new> <dev>`.
    CryptsetupLuksUuid,
    /// `vgimportclone --basevgname <new> <pvs…>` (new PV/VG UUIDs and name).
    Vgimportclone,
    /// `mdadm --assemble --update=uuid …` on the restored members only.
    MdadmUpdateUuid,
    /// `fatlabel -i <dev> <serial>`.
    Fatlabel,
    /// A label: `tune2fs -L <label> <dev>` (after `e2fsck -f`).
    LabelTune2fs,
    /// A label: `xfs_admin -L <label> <dev>`.
    LabelXfsAdmin,
    /// A label: `btrfs filesystem label <dev> <label>`; single-device only.
    LabelBtrfs,
    /// A label: `fatlabel <dev> <LABEL>`.
    LabelFatlabel,
    /// Not offered: changing it is the owner's decision (see module docs).
    None,
}

/// One duplicate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    pub identity: Identity,
    /// Kernel names of the devices already carrying it.
    pub already_on: Vec<String>,
    pub severity: Severity,
    pub reidentify: Reidentify,
    /// For XFS: mount the copy with `-o nouuid`; for ext4 read-only checks:
    /// `-o ro,noload` avoids replaying a journal into a copy being inspected.
    pub mount_hint: Option<&'static str>,
}

/// Kernel release as `(major, minor)`, for the btrfs temp-fsid rule.
pub type KernelVersion = (u32, u32);

/// Parse `uname -r` / `/proc/sys/kernel/osrelease` (`6.8.0-45-generic`).
pub fn parse_kernel_version(release: &str) -> Option<KernelVersion> {
    let mut it = release.trim().split(|c: char| !c.is_ascii_digit());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn assess(
    id: &Identity,
    kernel: Option<KernelVersion>,
) -> (Severity, Reidentify, Option<&'static str>) {
    match id.kind {
        IdentityKind::Ext => (Severity::Ambiguous, Reidentify::Tune2fs, Some("ro,noload")),
        IdentityKind::Xfs => (Severity::Mount, Reidentify::XfsAdmin, Some("nouuid")),
        IdentityKind::Btrfs => {
            // 6.7 added temp-fsid for single-device filesystems; before that a
            // clone is registered as another device of the same filesystem. An
            // unknown kernel is treated as the old one. A member of a
            // multi-device filesystem never gets a temporary fsid: its clone
            // joins the original's device set on any kernel.
            let temp_fsid = !id.multi_device && kernel.is_some_and(|k| k >= (6, 7));
            let sev = if temp_fsid {
                Severity::Ambiguous
            } else {
                Severity::Critical
            };
            // Re-identifying a multi-device btrfs needs every member present
            // at once; a restore does not offer it.
            let how = if id.multi_device {
                Reidentify::None
            } else {
                Reidentify::Btrfstune
            };
            (sev, how, None)
        }
        IdentityKind::Luks => (Severity::Ambiguous, Reidentify::CryptsetupLuksUuid, None),
        IdentityKind::LvmPv | IdentityKind::LvmVg | IdentityKind::LvmVgName => {
            (Severity::Critical, Reidentify::Vgimportclone, None)
        }
        IdentityKind::Md => (Severity::Critical, Reidentify::MdadmUpdateUuid, None),
        IdentityKind::PartUuid => (Severity::Ambiguous, Reidentify::None, None),
        IdentityKind::Fat => (Severity::Ambiguous, Reidentify::Fatlabel, None),
        IdentityKind::Label => {
            let how = match id.fs {
                Some(IdentityKind::Ext) => Reidentify::LabelTune2fs,
                Some(IdentityKind::Xfs) => Reidentify::LabelXfsAdmin,
                // Like its fsid, a multi-device btrfs's label is in every
                // member's superblock.
                Some(IdentityKind::Btrfs) if !id.multi_device => Reidentify::LabelBtrfs,
                Some(IdentityKind::Fat) => Reidentify::LabelFatlabel,
                _ => Reidentify::None,
            };
            (Severity::Ambiguous, how, None)
        }
    }
}

/// Every identity in `restored` that something in `present` already carries.
///
/// `present` maps kernel device names to their identities; devices listed in
/// `overwritten` (the restore target and its partitions, whose content is
/// about to be replaced) are not counted.
pub fn find_clashes(
    restored: &[Identity],
    present: &BTreeMap<String, Vec<Identity>>,
    overwritten: &[String],
    kernel: Option<KernelVersion>,
) -> Vec<Clash> {
    let mut out: Vec<Clash> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for id in restored {
        if id.key.is_empty() || !seen.insert((id.kind, id.key.clone())) {
            continue;
        }
        let already_on: Vec<String> = present
            .iter()
            .filter(|(dev, _)| !overwritten.contains(dev))
            .filter(|(_, ids)| ids.iter().any(|p| p.kind == id.kind && p.key == id.key))
            .map(|(dev, _)| dev.clone())
            .collect();
        if already_on.is_empty() {
            continue;
        }
        let (severity, reidentify, mount_hint) = assess(id, kernel);
        out.push(Clash {
            identity: id.clone(),
            already_on,
            severity,
            reidentify,
            mount_hint,
        });
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.identity.cmp(&b.identity))
    });
    out
}

// --- what the restored data carries ----------------------------------------

/// Reads `len` bytes at `offset` of the restored device or image; `None` past
/// the end or on error.
pub trait Probe {
    fn size(&self) -> u64;
    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>>;
}

impl Probe for &[u8] {
    fn size(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset).ok()?;
        self.get(start..start.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

/// The identities a partition's content carries — filesystem, LUKS, LVM, md.
/// A partition normally carries one family; an md member's data may begin
/// with a filesystem too (v1.0/0.90 keep the superblock at the end), and both
/// are reported.
pub fn identities_of(p: &mut dyn Probe) -> Vec<Identity> {
    let mut out = Vec::new();
    if let Some(head) = p.read(0, 68 * 1024).or_else(|| {
        let n = p.size().min(68 * 1024) as usize;
        p.read(0, n)
    }) {
        if let Some(u) = luks_uuid(&head) {
            out.push(Identity::new(IdentityKind::Luks, &u));
        }
        let label = |fs: IdentityKind, raw: Option<&[u8]>| {
            raw.and_then(label_text)
                .map(|l| Identity::label(fs, &l))
                .filter(|l| !l.key.is_empty())
        };
        if let Some(u) = ext_uuid(&head) {
            out.push(Identity::new(IdentityKind::Ext, &u));
            out.extend(label(IdentityKind::Ext, head.get(1024 + 120..1024 + 136)));
        }
        if let Some(u) = xfs_uuid(&head) {
            out.push(Identity::new(IdentityKind::Xfs, &u));
            out.extend(label(IdentityKind::Xfs, head.get(108..120)));
        }
        if let Some((u, devices)) = btrfs_fsid(&head) {
            out.push(if devices > 1 {
                Identity::btrfs_multi_device(&u)
            } else {
                Identity::new(IdentityKind::Btrfs, &u)
            });
            if let Some(l) = label(
                IdentityKind::Btrfs,
                head.get(65536 + 0x12b..65536 + 0x12b + 256),
            ) {
                out.push(Identity {
                    multi_device: devices > 1,
                    ..l
                });
            }
        }
        if let Some(u) = fat_serial(&head) {
            out.push(Identity::new(IdentityKind::Fat, &u));
            if let Some(l) = fat_label(p, &head) {
                out.push(Identity::label(IdentityKind::Fat, &l));
            }
        }
    }
    out.extend(lvm_identities(p));
    if let Some(u) = md_uuid(p) {
        out.push(Identity::new(IdentityKind::Md, &u));
    }
    out
}

fn dashed(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn nonzero(b: &[u8]) -> bool {
    b.iter().any(|&x| x != 0)
}

fn luks_uuid(head: &[u8]) -> Option<String> {
    if head.get(0..6)? != b"LUKS\xba\xbe" {
        return None;
    }
    let raw = head.get(168..208)?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = std::str::from_utf8(&raw[..end]).ok()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn ext_uuid(head: &[u8]) -> Option<String> {
    let sb = head.get(1024..1024 + 128)?;
    if u16::from_le_bytes([sb[56], sb[57]]) != 0xEF53 || !nonzero(&sb[104..120]) {
        return None;
    }
    Some(dashed(&sb[104..120]))
}

fn xfs_uuid(head: &[u8]) -> Option<String> {
    if head.get(0..4)? != b"XFSB" || !nonzero(head.get(32..48)?) {
        return None;
    }
    Some(dashed(&head[32..48]))
}

/// The btrfs fsid and the superblock's `num_devices` (offset 0x88).
fn btrfs_fsid(head: &[u8]) -> Option<(String, u64)> {
    let sb = head.get(65536..65536 + 0x90)?;
    if &sb[64..72] != b"_BHRfS_M" || !nonzero(&sb[32..48]) {
        return None;
    }
    let devices = u64::from_le_bytes(sb[0x88..0x90].try_into().unwrap());
    Some((dashed(&sb[32..48]), devices))
}

/// FAT12/16/32 volume serial, spelled as blkid and fstab spell it
/// (`ABCD-1234`). FAT32 keeps it at 0x43, FAT12/16 at 0x27; the boot sector
/// signature has to be there too.
fn fat_serial(head: &[u8]) -> Option<String> {
    if head.get(510..512)? != [0x55, 0xAA] {
        return None;
    }
    let at = if head.get(0x52..0x57)? == b"FAT32" {
        0x43
    } else if head.get(0x36..0x39)? == b"FAT" {
        0x27
    } else {
        return None;
    };
    let s = head.get(at..at + 4)?;
    nonzero(s).then(|| format!("{:02X}{:02X}-{:02X}{:02X}", s[3], s[2], s[1], s[0]))
}

/// A label field: up to its first NUL; empty is none.
fn label_text(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = String::from_utf8_lossy(&raw[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A FAT label as blkid reads it: the volume-label entry of the root
/// directory, else the boot sector's copy (0x47 on FAT32, 0x2B on FAT12/16);
/// `NO NAME` is none. `fatlabel` writes both.
fn fat_label(p: &mut dyn Probe, head: &[u8]) -> Option<String> {
    const NO_NAME: &[u8; 11] = b"NO NAME    ";
    let fat32 = head.get(0x52..0x57)? == b"FAT32";
    let in_root = fat_root_label(p, head, fat32);
    let boot = head.get(if fat32 { 0x47..0x52 } else { 0x2B..0x36 })?;
    let found = [in_root.as_deref(), Some(boot)]
        .into_iter()
        .flatten()
        .filter(|l| *l != &NO_NAME[..])
        .find_map(label_text);
    found
}

/// The 11 bytes of the root directory's volume-label entry, looked for in
/// the fixed root directory (FAT12/16) or the root's first cluster (FAT32).
fn fat_root_label(p: &mut dyn Probe, head: &[u8], fat32: bool) -> Option<Vec<u8>> {
    let u16_at = |o: usize| {
        head.get(o..o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u64)
    };
    let u32_at = |o: usize| {
        head.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as u64)
    };
    let bps = u16_at(0x0B)?;
    let spc = *head.get(0x0D)? as u64;
    let reserved = u16_at(0x0E)?;
    let fats = *head.get(0x10)? as u64;
    let root_entries = u16_at(0x11)?;
    let fat_size = match u16_at(0x16)? {
        0 => u32_at(0x24)?,
        n => n,
    };
    if ![512, 1024, 2048, 4096].contains(&bps) || spc == 0 || fats == 0 || fat_size == 0 {
        return None;
    }
    let fats_end = (reserved + fats * fat_size) * bps;
    let (at, len) = if fat32 {
        let cluster = u32_at(0x2C)?.checked_sub(2)?;
        (fats_end + cluster * spc * bps, spc * bps)
    } else {
        (fats_end, root_entries * 32)
    };
    let dir = p.read(at, len.min(64 * 1024) as usize)?;
    for e in dir.as_chunks::<32>().0 {
        match e[0] {
            0x00 => break,
            0xE5 => continue,
            _ => {}
        }
        let attr = e[11] & 0x3F;
        let cluster = e[20] as u16 | e[21] as u16 | e[26] as u16 | e[27] as u16;
        if attr != 0x0F && attr & 0x18 == 0x08 && cluster == 0 {
            return Some(e[..11].to_vec());
        }
    }
    None
}

/// LVM2 PV: `LABELONE` in one of the first four sectors, the PV UUID in the
/// pv_header behind it, and the VG UUID and name in the text metadata the
/// first metadata area points at.
fn lvm_identities(p: &mut dyn Probe) -> Vec<Identity> {
    let mut out = Vec::new();
    let Some(head) = p.read(0, 2048) else {
        return out;
    };
    let Some(label) = (0..4).map(|s| s * 512).find(|&o| {
        head.get(o..o + 8) == Some(b"LABELONE") && head.get(o + 24..o + 32) == Some(b"LVM2 001")
    }) else {
        return out;
    };
    let pvh = label + u32::from_le_bytes(head[label + 20..label + 24].try_into().unwrap()) as usize;
    let Some(uuid) = head.get(pvh..pvh + 32) else {
        return out;
    };
    let Ok(uuid) = std::str::from_utf8(uuid) else {
        return out;
    };
    out.push(Identity::new(IdentityKind::LvmPv, &lvm_dashed(uuid)));

    // disk_locn lists after the UUID and the device size: data areas, a zero
    // terminator, then metadata areas.
    let mut at = pvh + 32 + 8;
    let locn = |at: usize| -> Option<(u64, u64)> {
        let b = head.get(at..at + 16)?;
        Some((
            u64::from_le_bytes(b[..8].try_into().unwrap()),
            u64::from_le_bytes(b[8..].try_into().unwrap()),
        ))
    };
    while let Some((off, _)) = locn(at) {
        at += 16;
        if off == 0 {
            break;
        }
    }
    let Some((mda_off, mda_size)) = locn(at) else {
        return out;
    };
    if mda_off == 0 {
        return out;
    }
    if let Some((vg_name, vg_uuid)) = lvm_metadata(p, mda_off, mda_size) {
        out.push(Identity::new(IdentityKind::LvmVg, &vg_uuid));
        out.push(Identity::new(IdentityKind::LvmVgName, &vg_name));
    }
    out
}

/// LVM prints PV/VG UUIDs grouped 6-4-4-4-4-4-6.
fn lvm_dashed(raw: &str) -> String {
    if raw.len() != 32 || raw.contains('-') {
        return raw.to_string();
    }
    let groups = [6, 4, 4, 4, 4, 4, 6];
    let mut out = Vec::new();
    let mut at = 0;
    for g in groups {
        out.push(&raw[at..at + g]);
        at += g;
    }
    out.join("-")
}

const MDA_MAGIC: &[u8; 16] = b"\x20LVM2\x20x[5A%r0N*>";

fn lvm_metadata(p: &mut dyn Probe, mda_off: u64, mda_size: u64) -> Option<(String, String)> {
    let hdr = p.read(mda_off, 512)?;
    if hdr.get(4..20)? != MDA_MAGIC {
        return None;
    }
    // raw_locn[0]: offset (from the area start), size, checksum, flags.
    let off = u64::from_le_bytes(hdr.get(40..48)?.try_into().ok()?);
    let size = u64::from_le_bytes(hdr.get(48..56)?.try_into().ok()?);
    if off == 0 || size == 0 || size > 16 * 1024 * 1024 {
        return None;
    }
    // The text area is circular: past its end it continues right after the
    // 512-byte header.
    let first = size.min(mda_size.saturating_sub(off));
    let mut text = p.read(mda_off + off, first as usize)?;
    if first < size {
        text.extend(p.read(mda_off + 512, (size - first) as usize)?);
    }
    let text = String::from_utf8_lossy(&text);
    parse_vg_header(&text)
}

/// The first `name {` block of LVM text metadata and its `id = "…"`.
fn parse_vg_header(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines().map(str::trim);
    let name = lines
        .by_ref()
        .find(|l| l.ends_with('{') && !l.starts_with('#'))?
        .trim_end_matches('{')
        .trim()
        .to_string();
    let id = lines
        .find_map(|l| l.strip_prefix("id"))?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .trim_matches('"')
        .to_string();
    (!name.is_empty() && !id.is_empty()).then_some((name, id))
}

const MD_MAGIC: u32 = 0xa92b_4efc;

/// md array UUID from a v1.1 (offset 0), v1.2 (4 KiB), v1.0 (end − 8..12 KiB)
/// or v0.90 (last 64 KiB-aligned 64 KiB) superblock.
fn md_uuid(p: &mut dyn Probe) -> Option<String> {
    let sectors = p.size() / 512;
    let mut v1 = vec![0u64, 4096];
    if sectors > 16 {
        v1.push(((sectors - 16) & !7) * 512);
    }
    for off in v1 {
        let Some(sb) = p.read(off, 64) else {
            continue;
        };
        if u32::from_le_bytes(sb[0..4].try_into().unwrap()) == MD_MAGIC
            && u32::from_le_bytes(sb[4..8].try_into().unwrap()) == 1
            && nonzero(&sb[16..32])
        {
            return Some(md_colons(&sb[16..32]));
        }
    }
    let len = p.size();
    if len >= 128 * 1024 {
        let off = (len & !0xFFFF) - 0x10000;
        if let Some(sb) = p.read(off, 64) {
            if u32::from_le_bytes(sb[0..4].try_into().unwrap()) == MD_MAGIC
                && u32::from_le_bytes(sb[4..8].try_into().unwrap()) == 0
            {
                let word = |i: usize| sb[i * 4..i * 4 + 4].to_vec();
                let raw: Vec<u8> = [word(5), word(13), word(14), word(15)].concat();
                // 0.90 stores the four words in host order; mdadm prints them
                // as big-endian hex groups.
                let be: Vec<u8> = raw
                    .chunks(4)
                    .flat_map(|w| [w[3], w[2], w[1], w[0]])
                    .collect();
                if nonzero(&be) {
                    return Some(md_colons(&be));
                }
            }
        }
    }
    None
}

/// mdadm's spelling: four 8-hex groups joined by colons.
fn md_colons(b: &[u8]) -> String {
    b.chunks(4)
        .map(|c| c.iter().map(|x| format!("{x:02x}")).collect::<String>())
        .collect::<Vec<_>>()
        .join(":")
}

// --- what the machine already has ------------------------------------------

/// Identities the machine's devices carry, from the udev database
/// (`<run_udev>/data/b<major>:<minor>`, `E:ID_FS_TYPE=` / `E:ID_FS_UUID=`),
/// keyed by kernel name via `<sys_dev_block>/<major>:<minor>`. PARTUUIDs come
/// from `E:ID_PART_ENTRY_UUID=`, labels from `E:ID_FS_LABEL_ENC=`.
///
/// udev's blkid prints LVM PV UUIDs dashed and md UUIDs as a plain UUID; both
/// normalise to the same key as [`identities_of`] produces.
pub fn host_identities(
    run_udev_data: &Path,
    sys_dev_block: &Path,
) -> BTreeMap<String, Vec<Identity>> {
    let mut out: BTreeMap<String, Vec<Identity>> = BTreeMap::new();
    let Ok(dir) = std::fs::read_dir(run_udev_data) else {
        return out;
    };
    for e in dir.flatten() {
        let file = e.file_name().to_string_lossy().into_owned();
        let Some(numbers) = file.strip_prefix('b') else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let name = std::fs::read_link(sys_dev_block.join(numbers))
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| numbers.to_string());
        let prop = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix("E:")?.strip_prefix(k)?.strip_prefix('='))
                .map(str::to_string)
        };
        let mut ids = Vec::new();
        if let (Some(ty), Some(uuid)) = (prop("ID_FS_TYPE"), prop("ID_FS_UUID")) {
            let kind = match ty.as_str() {
                "ext2" | "ext3" | "ext4" => Some(IdentityKind::Ext),
                "xfs" => Some(IdentityKind::Xfs),
                "btrfs" => Some(IdentityKind::Btrfs),
                "crypto_LUKS" => Some(IdentityKind::Luks),
                "LVM2_member" => Some(IdentityKind::LvmPv),
                "linux_raid_member" => Some(IdentityKind::Md),
                "vfat" => Some(IdentityKind::Fat),
                _ => None,
            };
            if let Some(kind) = kind {
                ids.push(Identity::new(kind, &uuid));
            }
        }
        // Any filesystem's (or container's) label takes a by-label link.
        // `_ENC` is the label as written, `\xNN`-escaped; ID_FS_LABEL has
        // spaces and odd characters replaced.
        if let Some(label) = prop("ID_FS_LABEL_ENC")
            .map(|e| udev_unescape(&e))
            .or_else(|| prop("ID_FS_LABEL"))
            .filter(|l| !l.trim().is_empty())
        {
            ids.push(Identity::new(IdentityKind::Label, &label));
        }
        if let Some(vg) = prop("DM_VG_NAME") {
            ids.push(Identity::new(IdentityKind::LvmVgName, &vg));
        }
        if let Some(pu) = prop("ID_PART_ENTRY_UUID") {
            ids.push(Identity::new(IdentityKind::PartUuid, &pu));
        }
        if !ids.is_empty() {
            out.entry(name).or_default().extend(ids);
        }
    }
    out
}

/// udev's `\xNN` escapes undone.
pub fn udev_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'x') {
            if let Some(v) = s
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lines of an fstab, crypttab or boot loader configuration that name `id`
/// (`UUID=…`, `PARTUUID=…`, `/dev/disk/by-uuid/…`, GRUB's `search --fs-uuid`)
/// — what has to change along with a re-identified copy. Comment lines are
/// skipped.
pub fn lines_naming(text: &str, id: &Identity) -> Vec<(usize, String)> {
    let needle = id.shown.to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with('#'))
        .filter(|(_, l)| l.to_ascii_lowercase().contains(&needle))
        .map(|(i, l)| (i + 1, l.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: [u8; 16] = [
        0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x4a, 0x7b, 0x8c, 0x9d, 0x0e, 0x1f, 0x2a, 0x3b, 0x4c,
        0x5d,
    ];
    const TEXT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    fn disk(len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    fn ids(d: &[u8]) -> Vec<Identity> {
        let mut p: &[u8] = d;
        identities_of(&mut p)
    }

    #[test]
    fn filesystems_and_luks_are_recognised() {
        let mut d = disk(1 << 20);
        d[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
        d[1024 + 104..1024 + 120].copy_from_slice(&RAW);
        assert_eq!(ids(&d), vec![Identity::new(IdentityKind::Ext, TEXT)]);

        let mut d = disk(1 << 20);
        d[0..4].copy_from_slice(b"XFSB");
        d[32..48].copy_from_slice(&RAW);
        assert_eq!(ids(&d)[0].kind, IdentityKind::Xfs);

        let mut d = disk(1 << 20);
        d[65536 + 64..65536 + 72].copy_from_slice(b"_BHRfS_M");
        d[65536 + 32..65536 + 48].copy_from_slice(&RAW);
        assert_eq!(ids(&d), vec![Identity::new(IdentityKind::Btrfs, TEXT)]);
        d[65536 + 0x88..65536 + 0x90].copy_from_slice(&1u64.to_le_bytes());
        assert!(!ids(&d)[0].multi_device);
        d[65536 + 0x88..65536 + 0x90].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(ids(&d), vec![Identity::btrfs_multi_device(TEXT)]);

        let mut d = disk(1 << 20);
        d[0..6].copy_from_slice(b"LUKS\xba\xbe");
        d[168..168 + TEXT.len()].copy_from_slice(TEXT.as_bytes());
        assert_eq!(ids(&d), vec![Identity::new(IdentityKind::Luks, TEXT)]);

        let mut d = disk(1 << 20);
        d[0x52..0x57].copy_from_slice(b"FAT32");
        d[0x43..0x47].copy_from_slice(&[0x34, 0x12, 0xCD, 0xAB]);
        d[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert_eq!(ids(&d), vec![Identity::new(IdentityKind::Fat, "ABCD-1234")]);
        assert_eq!(
            ids(&d)[0].key,
            "abcd1234",
            "blkid's case and ours are one key"
        );

        assert!(ids(&disk(1 << 20)).is_empty());
        assert!(ids(&disk(100)).is_empty(), "a short device must not panic");
    }

    /// A PV laid out the way `pvcreate` + `vgcreate` leave it: label in
    /// sector 1, metadata area at 4 KiB, text at +512 within it.
    fn lvm_pv(pv: &str, vg: &str, vg_id: &str) -> Vec<u8> {
        let mut d = disk(2 << 20);
        let l = 512;
        d[l..l + 8].copy_from_slice(b"LABELONE");
        d[l + 8..l + 16].copy_from_slice(&1u64.to_le_bytes());
        d[l + 20..l + 24].copy_from_slice(&32u32.to_le_bytes());
        d[l + 24..l + 32].copy_from_slice(b"LVM2 001");
        let h = l + 32;
        d[h..h + 32].copy_from_slice(pv.as_bytes());
        d[h + 32..h + 40].copy_from_slice(&(2u64 << 20).to_le_bytes());
        // data area, terminator, metadata area.
        d[h + 40..h + 48].copy_from_slice(&(1u64 << 20).to_le_bytes());
        d[h + 72..h + 80].copy_from_slice(&4096u64.to_le_bytes());
        d[h + 80..h + 88].copy_from_slice(&((1u64 << 20) - 4096).to_le_bytes());
        let m = 4096;
        d[m + 4..m + 20].copy_from_slice(MDA_MAGIC);
        let text = format!(
            "{vg} {{\nid = \"{vg_id}\"\nseqno = 1\nformat = \"lvm2\"\n}}\n# Generated by LVM2\n"
        );
        d[m + 40..m + 48].copy_from_slice(&512u64.to_le_bytes());
        d[m + 48..m + 56].copy_from_slice(&(text.len() as u64).to_le_bytes());
        d[m + 512..m + 512 + text.len()].copy_from_slice(text.as_bytes());
        d
    }

    #[test]
    fn an_lvm_pv_gives_its_pv_and_vg_identities() {
        let d = lvm_pv(
            "AbCdEf0123456789AbCdEf0123456789",
            "vg0",
            "zXyW12-3456-7890-abcd-efgh-ijkl-mnopqr",
        );
        let got = ids(&d);
        assert_eq!(
            got,
            vec![
                Identity::new(
                    IdentityKind::LvmPv,
                    "AbCdEf-0123-4567-89Ab-CdEf-0123-456789"
                ),
                Identity::new(
                    IdentityKind::LvmVg,
                    "zXyW12-3456-7890-abcd-efgh-ijkl-mnopqr"
                ),
                Identity::new(IdentityKind::LvmVgName, "vg0"),
            ]
        );
        // udev's dashed form and the raw label form are one key, case kept.
        assert_eq!(got[0].key, "AbCdEf0123456789AbCdEf0123456789");
    }

    #[test]
    fn md_superblocks_at_every_location_give_the_array_uuid() {
        let put_v1 = |d: &mut Vec<u8>, off: usize| {
            d[off..off + 4].copy_from_slice(&MD_MAGIC.to_le_bytes());
            d[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes());
            d[off + 16..off + 32].copy_from_slice(&RAW);
        };
        let len = 4 << 20;
        let want = Identity::new(IdentityKind::Md, "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d");
        for off in [0usize, 4096, (((len / 512) - 16) & !7) * 512] {
            let mut d = disk(len);
            put_v1(&mut d, off);
            assert_eq!(ids(&d), vec![want.clone()], "v1 superblock at {off}");
        }
        // 0.90 at the end; the key matches udev's dashed UUID form too.
        let mut d = disk(len);
        let off = (len & !0xFFFF) - 0x10000;
        d[off..off + 4].copy_from_slice(&MD_MAGIC.to_le_bytes());
        for (w, bytes) in [
            (5usize, 0x1a2b3c4du32),
            (13, 0x5e6f4a7b),
            (14, 0x8c9d0e1f),
            (15, 0x2a3b4c5d),
        ] {
            d[off + w * 4..off + w * 4 + 4].copy_from_slice(&bytes.to_le_bytes());
        }
        assert_eq!(ids(&d), vec![want.clone()]);
        assert_eq!(want.key, Identity::new(IdentityKind::Md, TEXT).key);
    }

    fn present(entries: &[(&str, Identity)]) -> BTreeMap<String, Vec<Identity>> {
        let mut m: BTreeMap<String, Vec<Identity>> = BTreeMap::new();
        for (dev, id) in entries {
            m.entry(dev.to_string()).or_default().push(id.clone());
        }
        m
    }

    #[test]
    fn clashes_are_found_ranked_and_the_target_is_not_counted() {
        let ext = Identity::new(IdentityKind::Ext, TEXT);
        let pv = Identity::new(
            IdentityKind::LvmPv,
            "AbCdEf-0123-4567-89Ab-CdEf-0123-456789",
        );
        let xfs = Identity::new(IdentityKind::Xfs, "00000000-0000-0000-0000-000000000001");
        let host = present(&[
            ("sda2", ext.clone()),
            ("sdb1", ext.clone()),
            ("sda3", pv.clone()),
            (
                "sdc1",
                Identity::new(IdentityKind::Xfs, "ffffffff-0000-0000-0000-000000000001"),
            ),
        ]);
        let got = find_clashes(
            &[ext.clone(), pv.clone(), xfs],
            &host,
            &["sdb1".into()],
            Some((6, 8)),
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].identity, pv);
        assert_eq!(got[0].severity, Severity::Critical);
        assert_eq!(got[0].reidentify, Reidentify::Vgimportclone);
        assert_eq!(got[1].identity, ext);
        assert_eq!(got[1].already_on, vec!["sda2".to_string()]);
        assert_eq!(got[1].mount_hint, Some("ro,noload"));
    }

    #[test]
    fn a_same_uuid_of_another_kind_is_not_a_clash() {
        let host = present(&[("sda1", Identity::new(IdentityKind::Luks, TEXT))]);
        assert!(
            find_clashes(&[Identity::new(IdentityKind::Ext, TEXT)], &host, &[], None).is_empty()
        );
    }

    #[test]
    fn btrfs_is_critical_before_6_7_and_unknown_kernels_count_as_old() {
        let b = Identity::new(IdentityKind::Btrfs, TEXT);
        let host = present(&[("sda2", b.clone())]);
        let sev = |k| find_clashes(std::slice::from_ref(&b), &host, &[], k)[0].severity;
        assert_eq!(sev(Some((6, 6))), Severity::Critical);
        assert_eq!(sev(None), Severity::Critical);
        assert_eq!(sev(Some((6, 7))), Severity::Ambiguous);
        assert_eq!(sev(Some((7, 0))), Severity::Ambiguous);
    }

    #[test]
    fn a_multi_device_btrfs_clone_is_critical_on_every_kernel() {
        let copy = Identity::btrfs_multi_device(TEXT);
        // The machine's side comes from udev, which says nothing of devices.
        let host = present(&[("sda2", Identity::new(IdentityKind::Btrfs, TEXT))]);
        for k in [None, Some((6, 6)), Some((6, 7)), Some((7, 0))] {
            let got = find_clashes(std::slice::from_ref(&copy), &host, &[], k);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].severity, Severity::Critical, "{k:?}");
            assert_eq!(got[0].reidentify, Reidentify::None, "not offered");
        }
    }

    #[test]
    fn xfs_needs_nouuid_and_partuuid_is_never_changed_for_you() {
        let x = Identity::new(IdentityKind::Xfs, TEXT);
        let p = Identity::new(IdentityKind::PartUuid, TEXT);
        let host = present(&[("sda2", x.clone()), ("sda3", p.clone())]);
        let got = find_clashes(&[x, p], &host, &[], None);
        assert_eq!(got[0].mount_hint, Some("nouuid"));
        assert_eq!(got[0].severity, Severity::Mount);
        assert_eq!(got[1].reidentify, Reidentify::None);
    }

    #[test]
    fn kernel_versions_parse() {
        assert_eq!(parse_kernel_version("6.8.0-45-generic"), Some((6, 8)));
        assert_eq!(parse_kernel_version("7.0.1"), Some((7, 0)));
        assert_eq!(parse_kernel_version("garbage"), None);
    }

    #[cfg(unix)]
    #[test]
    fn host_identities_come_from_the_udev_database() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let sysdev = root.path().join("dev-block");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&sysdev).unwrap();
        std::fs::create_dir_all(root.path().join("devices/sda/sda2")).unwrap();
        std::os::unix::fs::symlink(root.path().join("devices/sda/sda2"), sysdev.join("8:2"))
            .unwrap();
        std::fs::write(
            data.join("b8:2"),
            format!("S:disk/by-uuid/{TEXT}\nE:ID_FS_TYPE=ext4\nE:ID_FS_UUID={TEXT}\nE:ID_PART_ENTRY_UUID=0bc5a2c4-01\n"),
        )
        .unwrap();
        std::fs::write(data.join("c4:1"), "E:ID_FS_UUID=x\n").unwrap();
        std::fs::write(
            data.join("b253:0"),
            "E:DM_VG_NAME=vg0\nE:ID_FS_TYPE=swap\nE:ID_FS_UUID=abc\n",
        )
        .unwrap();
        let host = host_identities(&data, &sysdev);
        assert_eq!(
            host["sda2"],
            vec![
                Identity::new(IdentityKind::Ext, TEXT),
                Identity::new(IdentityKind::PartUuid, "0bc5a2c4-01")
            ]
        );
        // No sysfs link: keyed by the numbers.
        assert_eq!(
            host["253:0"],
            vec![Identity::new(IdentityKind::LvmVgName, "vg0")]
        );
        assert!(!host.contains_key("4:1"));
    }

    #[test]
    fn configuration_lines_that_name_an_identity_are_listed() {
        let fstab = format!(
            "# UUID={TEXT} old\nUUID={} / ext4 defaults 0 1\n/dev/disk/by-uuid/{TEXT} /home ext4 defaults 0 2\n",
            TEXT.to_uppercase()
        );
        let id = Identity::new(IdentityKind::Ext, TEXT);
        let got = lines_naming(&fstab, &id);
        assert_eq!(got.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![2, 3]);
    }

    fn label_of(d: &[u8]) -> Option<Identity> {
        ids(d).into_iter().find(|i| i.kind == IdentityKind::Label)
    }

    #[test]
    fn labels_are_read_with_the_filesystem_that_carries_them() {
        let mut d = disk(1 << 20);
        d[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
        d[1024 + 104..1024 + 120].copy_from_slice(&RAW);
        d[1024 + 120..1024 + 135].copy_from_slice(b"cloudimg-rootfs");
        assert_eq!(
            ids(&d),
            vec![
                Identity::new(IdentityKind::Ext, TEXT),
                Identity::label(IdentityKind::Ext, "cloudimg-rootfs")
            ]
        );

        // XFS: 12 bytes, not NUL-terminated when full.
        let mut d = disk(1 << 20);
        d[0..4].copy_from_slice(b"XFSB");
        d[32..48].copy_from_slice(&RAW);
        d[108..120].copy_from_slice(b"twelve-bytes");
        assert_eq!(
            label_of(&d),
            Some(Identity::label(IdentityKind::Xfs, "twelve-bytes"))
        );

        // btrfs: a multi-device filesystem's label says so too.
        let mut d = disk(1 << 20);
        d[65536 + 64..65536 + 72].copy_from_slice(b"_BHRfS_M");
        d[65536 + 32..65536 + 48].copy_from_slice(&RAW);
        d[65536 + 0x12b..65536 + 0x12b + 4].copy_from_slice(b"data");
        let l = label_of(&d).unwrap();
        assert_eq!(
            (l.fs, l.shown.as_str(), l.multi_device),
            (Some(IdentityKind::Btrfs), "data", false)
        );
        d[65536 + 0x88..65536 + 0x90].copy_from_slice(&2u64.to_le_bytes());
        assert!(label_of(&d).unwrap().multi_device);

        // No label: none reported.
        let mut d = disk(1 << 20);
        d[0..4].copy_from_slice(b"XFSB");
        d[32..48].copy_from_slice(&RAW);
        assert_eq!(label_of(&d), None);
    }

    /// A FAT32 the way mkfs.fat leaves it: 512-byte sectors, 32 reserved,
    /// two FATs of `fat` sectors, the root in cluster 2.
    fn fat32(fat: u32, spc: u8) -> Vec<u8> {
        let mut d = disk(4 << 20);
        d[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        d[0x0D] = spc;
        d[0x0E..0x10].copy_from_slice(&32u16.to_le_bytes());
        d[0x10] = 2;
        d[0x24..0x28].copy_from_slice(&fat.to_le_bytes());
        d[0x2C..0x30].copy_from_slice(&2u32.to_le_bytes());
        d[0x43..0x47].copy_from_slice(&[0x34, 0x12, 0xCD, 0xAB]);
        d[0x47..0x52].copy_from_slice(b"NO NAME    ");
        d[0x52..0x57].copy_from_slice(b"FAT32");
        d[510..512].copy_from_slice(&[0x55, 0xAA]);
        d
    }

    fn dir_entry(name: &[u8; 11], attr: u8) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e
    }

    #[test]
    fn a_fat_label_is_the_root_directorys_then_the_boot_sectors() {
        let root = ((32 + 2 * 100) * 512) as usize;
        let mut d = fat32(100, 1);
        // A long-name entry and a deleted label come first.
        d[root..root + 32].copy_from_slice(&dir_entry(b"Ae\0f\0i\0\0\0\0\0", 0x0F));
        let mut gone = dir_entry(b"OLD        ", 0x08);
        gone[0] = 0xE5;
        d[root + 32..root + 64].copy_from_slice(&gone);
        d[root + 64..root + 96].copy_from_slice(&dir_entry(b"EFI        ", 0x10));
        d[root + 96..root + 128].copy_from_slice(&dir_entry(b"UEFI       ", 0x08));
        assert_eq!(
            label_of(&d),
            Some(Identity::label(IdentityKind::Fat, "UEFI"))
        );

        // Nothing in the root directory: the boot sector's, unless NO NAME.
        let mut d = fat32(100, 1);
        assert_eq!(label_of(&d), None);
        d[0x47..0x52].copy_from_slice(b"ESP        ");
        assert_eq!(
            label_of(&d),
            Some(Identity::label(IdentityKind::Fat, "ESP"))
        );

        // FAT16: the fixed root directory after the FATs.
        let mut d = disk(4 << 20);
        d[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        d[0x0D] = 4;
        d[0x0E..0x10].copy_from_slice(&4u16.to_le_bytes());
        d[0x10] = 2;
        d[0x11..0x13].copy_from_slice(&512u16.to_le_bytes());
        d[0x16..0x18].copy_from_slice(&20u16.to_le_bytes());
        d[0x27..0x2B].copy_from_slice(&[1, 2, 3, 4]);
        d[0x2B..0x36].copy_from_slice(b"NO NAME    ");
        d[0x36..0x3B].copy_from_slice(b"FAT16");
        d[510..512].copy_from_slice(&[0x55, 0xAA]);
        let root = (4 + 2 * 20) * 512;
        d[root..root + 32].copy_from_slice(&dir_entry(b"BOOT DISK  ", 0x08));
        assert_eq!(
            label_of(&d),
            Some(Identity::label(IdentityKind::Fat, "BOOT DISK"))
        );

        // A boot sector that makes no sense is no reason to panic.
        let mut d = fat32(0, 0);
        d[0x0B..0x0D].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(label_of(&d), None);
    }

    #[test]
    fn a_label_clashes_with_the_same_label_of_any_filesystem() {
        let host = present(&[
            (
                "sda1",
                Identity::new(IdentityKind::Label, "cloudimg-rootfs"),
            ),
            ("sda15", Identity::new(IdentityKind::Label, "UEFI")),
        ]);
        let got = find_clashes(
            &[
                Identity::label(IdentityKind::Btrfs, "cloudimg-rootfs"),
                Identity::label(IdentityKind::Fat, "UEFI"),
                // Labels are case-sensitive, as their by-label links are.
                Identity::label(IdentityKind::Ext, "uefi"),
            ],
            &host,
            &[],
            None,
        );
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.iter().all(|c| c.severity == Severity::Ambiguous));
        let of = |l: &str| got.iter().find(|c| c.identity.shown == l).unwrap();
        assert_eq!(of("cloudimg-rootfs").reidentify, Reidentify::LabelBtrfs);
        assert_eq!(of("cloudimg-rootfs").already_on, vec!["sda1".to_string()]);
        assert_eq!(of("UEFI").reidentify, Reidentify::LabelFatlabel);

        // How it changes depends on the filesystem; a multi-device btrfs
        // keeps it.
        let how = |l: Identity| find_clashes(&[l], &host, &[], None)[0].reidentify;
        assert_eq!(
            how(Identity::label(IdentityKind::Ext, "UEFI")),
            Reidentify::LabelTune2fs
        );
        assert_eq!(
            how(Identity::label(IdentityKind::Xfs, "UEFI")),
            Reidentify::LabelXfsAdmin
        );
        let multi = Identity {
            multi_device: true,
            ..Identity::label(IdentityKind::Btrfs, "UEFI")
        };
        assert_eq!(how(multi), Reidentify::None);
    }

    #[cfg(unix)]
    #[test]
    fn host_labels_come_from_the_udev_database_as_written() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let sysdev = root.path().join("dev-block");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&sysdev).unwrap();
        std::fs::write(
            data.join("b8:1"),
            "E:ID_FS_TYPE=ext4\nE:ID_FS_LABEL=my_disk\nE:ID_FS_LABEL_ENC=my\\x20disk\n",
        )
        .unwrap();
        std::fs::write(data.join("b8:2"), "E:ID_FS_TYPE=swap\nE:ID_FS_LABEL=swap\n").unwrap();
        std::fs::write(data.join("b8:3"), "E:ID_FS_TYPE=ext4\nE:ID_FS_LABEL=\n").unwrap();
        let host = host_identities(&data, &sysdev);
        assert_eq!(
            host["8:1"],
            vec![Identity::new(IdentityKind::Label, "my disk")]
        );
        assert_eq!(
            host["8:2"],
            vec![Identity::new(IdentityKind::Label, "swap")],
            "a by-label link whatever the type"
        );
        assert!(!host.contains_key("8:3"));
        assert_eq!(udev_unescape(r"a\x2fb\x20c\xzz"), r"a/b c\xzz");
    }
    /// The parsers against the real tools, compared with what blkid says.
    /// Needs root (loop devices for LVM and md) and mkfs.ext4, mkfs.xfs,
    /// mkfs.btrfs, cryptsetup, pvcreate/vgcreate, mdadm:
    /// `sudo cargo test -p own-backup-backup -- --ignored real_tools_agree`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn real_tools_agree_with_blkid() {
        use std::process::Command;
        let run = |args: &[&str]| {
            let out = Command::new(args[0]).args(&args[1..]).output().unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let blkid =
            |dev: &str| -> String { run(&["blkid", "-p", "-s", "UUID", "-o", "value", dev]) };
        let probe = |path: &str| -> Vec<Identity> {
            let data = std::fs::read(path).unwrap();
            let mut p: &[u8] = &data;
            identities_of(&mut p)
        };
        let dir = tempfile::tempdir().unwrap();
        let img = |name: &str, mib: u64| {
            let p = dir.path().join(name);
            std::fs::File::create(&p)
                .unwrap()
                .set_len(mib << 20)
                .unwrap();
            p.to_string_lossy().into_owned()
        };

        for (kind, mkfs, mib) in [
            (IdentityKind::Ext, vec!["mkfs.ext4", "-q", "-F"], 64),
            (IdentityKind::Xfs, vec!["mkfs.xfs", "-q", "-f"], 320),
            (IdentityKind::Btrfs, vec!["mkfs.btrfs", "-q", "-f"], 128),
        ] {
            let p = img("fs.img", mib);
            let mut args = mkfs.clone();
            args.push(&p);
            run(&args);
            assert_eq!(probe(&p), vec![Identity::new(kind, &blkid(&p))], "{mkfs:?}");
        }
        for ty in ["luks1", "luks2"] {
            let p = img("luks.img", 32);
            let out = Command::new("cryptsetup")
                .args([
                    "luksFormat",
                    "-q",
                    "--type",
                    ty,
                    "--pbkdf-force-iterations",
                    "1000",
                ])
                .args(if ty == "luks2" {
                    vec!["--pbkdf", "pbkdf2"]
                } else {
                    vec![]
                })
                .args([&p, "-"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    c.stdin.take().unwrap().write_all(b"pw")?;
                    c.wait()
                })
                .unwrap();
            assert!(out.success());
            assert_eq!(
                probe(&p),
                vec![Identity::new(IdentityKind::Luks, &blkid(&p))],
                "{ty}"
            );
        }

        let losetup = |p: &str| run(&["losetup", "-f", "--show", p]);
        // LVM: PV and VG identities as pvs/vgs print them.
        let p = img("pv.img", 64);
        let lo = losetup(&p);
        run(&["pvcreate", "-q", "-ff", "-y", &lo]);
        run(&["vgcreate", "-q", "ownbackuptestvg", &lo]);
        let pv = run(&["pvs", "--noheadings", "-o", "pv_uuid", &lo]);
        let vg = run(&["vgs", "--noheadings", "-o", "vg_uuid", "ownbackuptestvg"]);
        let got = probe(&lo);
        run(&["vgremove", "-q", "-f", "ownbackuptestvg"]);
        run(&["losetup", "-d", &lo]);
        assert_eq!(
            got,
            vec![
                Identity::new(IdentityKind::LvmPv, &pv),
                Identity::new(IdentityKind::LvmVg, &vg),
                Identity::new(IdentityKind::LvmVgName, "ownbackuptestvg"),
            ]
        );
        assert_eq!(
            got[0].key,
            Identity::new(IdentityKind::LvmPv, &blkid(&p)).key
        );

        // md: every superblock version, compared with mdadm's own UUID. Needs
        // the md driver; a kernel without it (some containers) skips this part.
        if !Path::new("/proc/mdstat").exists() {
            eprintln!("no md driver (/proc/mdstat) — md superblocks not checked");
            return;
        }
        for meta in ["1.2", "1.1", "1.0", "0.90"] {
            let a = losetup(&img("md-a.img", 64));
            let b = losetup(&img("md-b.img", 64));
            run(&[
                "mdadm",
                "--create",
                "/dev/md/ownbackuptest",
                "--run",
                "--level=1",
                "--raid-devices=2",
                "--metadata",
                meta,
                "--assume-clean",
                &a,
                &b,
            ]);
            let detail = run(&["mdadm", "--detail", "--export", "/dev/md/ownbackuptest"]);
            let uuid = detail
                .lines()
                .find_map(|l| l.strip_prefix("MD_UUID="))
                .unwrap()
                .to_string();
            run(&["mdadm", "--stop", "/dev/md/ownbackuptest"]);
            let got = probe(&a);
            let from_blkid = blkid(&a);
            run(&["losetup", "-d", &a]);
            run(&["losetup", "-d", &b]);
            assert_eq!(
                got,
                vec![Identity::new(IdentityKind::Md, &uuid)],
                "metadata {meta}"
            );
            assert_eq!(
                got[0].key,
                Identity::new(IdentityKind::Md, &from_blkid).key,
                "{meta}"
            );
        }
    }
}
