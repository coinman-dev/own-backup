//! The files of a Linux partition, read from Windows without mounting it.
//!
//! Windows has no driver for ext4, LVM or LUKS, and handing the partition to
//! WSL takes the whole disk away from Windows — with every other volume on it.
//! This reads the partition the way the backup engine already reads Linux
//! volumes from Windows: in userspace, read-only, through the layers it is
//! made of —
//!
//! * a LUKS container, opened with its passphrase ([`crate::luks`], the data
//!   decrypted on the fly by [`DecryptingVolume`]);
//! * an LVM physical volume, its logical volumes found from the volume
//!   group's own metadata ([`lvm_pv::logical_volumes`]) — the usual layout of
//!   an encrypted Ubuntu: `vgubuntu-root` and `vgubuntu-swap_1` inside LUKS;
//! * an ext2/3/4 filesystem, read by `ext4-view`, which also replays the
//!   journal in memory: a system that went down without unmounting shows what
//!   it had written, and nothing is ever written back;
//! * or btrfs ([`btrfs`]), its subvolumes as folders — what an Ubuntu
//!   installed on btrfs inside LUKS turned out to be (27.10).
//!
//! What a person sees of it is the Total Commander plugin (`crates/tc-plugin`),
//! which asks the service for it ([`Partition`] lives in a session there).

mod btrfs;
pub mod names;

use std::cell::RefCell;
use std::rc::Rc;

use ext4_view::{Ext4, Ext4Read};
use serde::Serialize;

use own_backup_common::lvm_pv::{self, PvSegment};
use own_backup_common::{OwnBackupError, Result};

use crate::aligned_volume::AlignedVolume;
use crate::decrypt::DecryptingVolume;
use crate::fat_map::RawVolume;

/// Enough of a volume's head to tell what it is: LUKS at 0, the LVM label in
/// the first four sectors, the ext superblock at 1024, the swap signature at
/// the end of the first page — and, to say what is there when it is none of
/// those, the btrfs superblock at 64 KiB and the first ZFS uberblock at
/// 128 KiB.
pub const PROBE_LEN: usize = 132 * 1024;

/// What a volume holds, as far as reading files from it is concerned. Only
/// LUKS, LVM and ext are read; the others are recognised to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    Luks,
    Lvm,
    Ext,
    Swap,
    Btrfs,
    Xfs,
    Zfs,
    /// Zeros: never written, or decrypted with a key that is not its own.
    Empty,
    Other,
}

/// What the head of a volume says it is.
pub fn identify(head: &[u8]) -> Content {
    if head.starts_with(b"LUKS\xba\xbe") {
        Content::Luks
    } else if lvm_pv::parse_label(head).is_some() {
        Content::Lvm
    } else if head.get(1080..1082) == Some(&[0x53, 0xEF][..]) {
        Content::Ext
    } else if matches!(
        head.get(4086..4096),
        Some(b"SWAPSPACE2") | Some(b"SWAP-SPACE")
    ) {
        Content::Swap
    } else if head.get(0x10040..0x10048) == Some(b"_BHRfS_M") {
        Content::Btrfs
    } else if head.starts_with(b"XFSB") {
        Content::Xfs
    } else if head.get(128 * 1024..128 * 1024 + 8) == Some(&0x00ba_b10c_u64.to_le_bytes()[..]) {
        Content::Zfs
    } else if !head.is_empty() && head.iter().all(|&b| b == 0) {
        Content::Empty
    } else {
        Content::Other
    }
}

/// Why a partition did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// A LUKS container, and no passphrase was given.
    NeedsPassword,
    /// The passphrase opens none of its keyslots.
    WrongPassword,
    /// Anything else, in words for the log and the person.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NeedsPassword => f.write_str("the partition is encrypted"),
            OpenError::WrongPassword => f.write_str("wrong passphrase"),
            OpenError::Failed(why) => f.write_str(why),
        }
    }
}

/// An opened partition: the filesystems in it.
pub struct Partition {
    pub encrypted: bool,
    pub roots: Vec<Root>,
}

/// One filesystem of a partition: the partition itself, or one of its
/// logical volumes (`vgubuntu-root`).
pub struct Root {
    /// Empty for a filesystem that is the partition itself.
    pub name: String,
    pub size: u64,
    /// The filesystem, or why it cannot be read.
    pub fs: std::result::Result<Fs, String>,
}

/// What the plugin is told about a [`Root`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootInfo {
    pub name: String,
    pub size: u64,
    pub label: String,
    /// Why it cannot be read; `None` when it can.
    pub error: Option<String>,
}

impl Partition {
    pub fn info(&self) -> Vec<RootInfo> {
        self.roots
            .iter()
            .map(|r| RootInfo {
                name: r.name.clone(),
                size: r.size,
                label: r.fs.as_ref().map(|f| f.label()).unwrap_or_default(),
                error: r.fs.as_ref().err().cloned(),
            })
            .collect()
    }
}

/// Open `vol` — a partition as read from the disk — with `passphrase` if it
/// is a LUKS container.
pub fn open(
    vol: Box<dyn RawVolume + Send>,
    passphrase: Option<&[u8]>,
) -> std::result::Result<Partition, OpenError> {
    let failed = |e: OwnBackupError| OpenError::Failed(e.to_string());
    // Raw devices on Windows take sector-aligned reads only; ext4 reads
    // inodes of 256 bytes wherever they lie.
    let mut vol: Box<dyn RawVolume + Send> = Box::new(AlignedVolume::new(vol));
    let mut encrypted = false;
    if identify(&head(&mut *vol).map_err(failed)?) == Content::Luks {
        let Some(pass) = passphrase else {
            return Err(OpenError::NeedsPassword);
        };
        let params = match crate::luks::open_luks(&mut *vol, pass) {
            Ok(p) => p,
            Err(e) if crate::luks::is_wrong_passphrase(&e) => return Err(OpenError::WrongPassword),
            Err(e) => return Err(failed(e)),
        };
        tracing::info!(
            data_offset = params.data_offset,
            sector = params.sector_size,
            "linux browse: LUKS opened"
        );
        vol = Box::new(DecryptingVolume::new(vol, params));
        encrypted = true;
    }

    let size = vol.len();
    let base: Shared = Rc::new(RefCell::new(vol));
    let content = identify(&head(&mut **base.borrow_mut()).map_err(failed)?);
    let roots = match content {
        Content::Lvm => {
            let layout = read_layout(&mut **base.borrow_mut()).map_err(OpenError::Failed)?;
            let mut roots = Vec::new();
            for lv in layout.volumes {
                let name = format!("{}-{}", layout.vg, lv.name);
                let fs = match lv.unreadable {
                    Some(why) => Err(why),
                    None => {
                        let mut view: Box<dyn RawVolume> =
                            Box::new(LvView::new(base.clone(), lv.segments, lv.size));
                        match head(&mut *view).map(|h| identify(&h)) {
                            Ok(Content::Ext) => load_ext(view),
                            Ok(Content::Btrfs) => load_btrfs(view),
                            // Swap is not a place to look for files.
                            Ok(Content::Swap) => continue,
                            Ok(other) => Err(not_ext(other)),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                };
                roots.push(Root {
                    name,
                    size: lv.size,
                    fs,
                });
            }
            roots
        }
        Content::Ext => vec![Root {
            name: String::new(),
            size,
            fs: load_ext(Box::new(SharedView(base))),
        }],
        Content::Btrfs => vec![Root {
            name: String::new(),
            size,
            fs: load_btrfs(Box::new(SharedView(base))),
        }],
        other => {
            let head = head(&mut **base.borrow_mut()).unwrap_or_default();
            tracing::warn!(
                content = ?other,
                first_bytes = %head.iter().take(16).map(|b| format!("{b:02x}")).collect::<String>(),
                encrypted,
                "linux browse: nothing readable in the partition"
            );
            return Err(OpenError::Failed(not_ext(other)));
        }
    };
    Ok(Partition { encrypted, roots })
}

fn not_ext(c: Content) -> String {
    use own_backup_common::tr;
    match c {
        Content::Swap => tr!("linux_browse.swap"),
        Content::Luks => tr!("linux_browse.luks_in_luks"),
        Content::Lvm => tr!("linux_browse.lvm_in_lvm"),
        Content::Xfs => tr!("linux_browse.unsupported", what = "XFS"),
        Content::Zfs => tr!("linux_browse.unsupported", what = "ZFS"),
        Content::Empty => tr!("linux_browse.empty"),
        Content::Ext | Content::Btrfs | Content::Other => tr!("linux_browse.unknown"),
    }
    .into_owned()
}

fn head(vol: &mut dyn RawVolume) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; PROBE_LEN.min(vol.len() as usize)];
    vol.read_at(0, &mut buf)?;
    Ok(buf)
}

/// The logical volumes of the PV `vol` is, from its metadata areas.
fn read_layout(vol: &mut dyn RawVolume) -> std::result::Result<lvm_pv::VolumeLayout, String> {
    let mut head = vec![0u8; lvm_pv::LABEL_SCAN_BYTES];
    vol.read_at(0, &mut head).map_err(|e| e.to_string())?;
    let label = lvm_pv::parse_label(&head).ok_or("no LVM label")?;
    let mut areas = Vec::new();
    for &(offset, size) in &label.metadata_areas {
        if size == 0 || size > lvm_pv::MAX_METADATA_AREA || offset + size > vol.len() {
            continue;
        }
        let mut area = vec![0u8; size as usize];
        vol.read_at(offset, &mut area).map_err(|e| e.to_string())?;
        areas.push((offset, area));
    }
    let refs: Vec<(u64, &[u8])> = areas.iter().map(|(o, a)| (*o, a.as_slice())).collect();
    lvm_pv::logical_volumes_of_areas(&refs, &label.uuid)
}

fn load_ext(vol: Box<dyn RawVolume>) -> std::result::Result<Fs, String> {
    Ext4::load(Box::new(Reader(vol)))
        .map(|ext| Fs::Ext(Box::new(ExtFs { ext, open: None })))
        .map_err(|e| format!("the ext filesystem cannot be read: {e}"))
}

fn load_btrfs(vol: Box<dyn RawVolume>) -> std::result::Result<Fs, String> {
    btrfs::Btrfs::load(vol)
        .map(|b| Fs::Btrfs(Box::new(b)))
        .map_err(|e| format!("the btrfs filesystem cannot be read: {e}"))
}

// --- the partitions of this machine ---------------------------------------------

/// A partition that holds what this module reads: an ext filesystem, an LVM
/// physical volume, or a LUKS container (which may hold either).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Source {
    /// `<disk device>#<partition index>`: what [`open`] is asked for.
    pub id: String,
    /// The whole disk (`\\.\PhysicalDrive3`, `/dev/nvme0n1`).
    pub disk: String,
    /// Windows' number of the disk; `None` on Linux.
    pub disk_number: Option<u32>,
    pub model: String,
    pub index: u32,
    pub offset: u64,
    pub size: u64,
    pub content: Content,
    /// The filesystem's label, or where it was last mounted (`/`, `/home`).
    pub label: String,
}

/// A partition `size` bytes long at `offset` of the whole disk `disk`, read
/// in place: nothing is mounted, attached or taken offline, so every other
/// volume of that disk stays as it is.
pub fn open_partition(disk: &str, offset: u64, size: u64) -> Result<Box<dyn RawVolume + Send>> {
    #[cfg(target_os = "windows")]
    let device: Box<dyn RawVolume + Send> =
        Box::new(crate::win_device::WinRawDevice::open_read(disk)?);
    #[cfg(target_os = "linux")]
    let device: Box<dyn RawVolume + Send> =
        Box::new(crate::linux_device::LinuxRawDevice::open_read(disk)?);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let device: Box<dyn RawVolume + Send> = {
        let _ = disk;
        return Err(OwnBackupError::Device(
            "no raw disks on this platform".into(),
        ));
    };
    Ok(Box::new(crate::volume_window::VolumeWindow::new(
        device, offset, size,
    )?))
}

/// The partitions of `disks` worth offering. Those Windows has a letter for
/// are not looked at: they are Windows' own, and reading them raw beside it
/// is pointless. The others are recognised by their first bytes, whatever
/// their partition type says — an Ubuntu installer marks a LUKS container as
/// "Linux filesystem".
pub fn find_sources(disks: &[own_backup_common::disk::DiskInfo]) -> Vec<Source> {
    let mut out = Vec::new();
    for disk in disks {
        let number = disk
            .device_id
            .rsplit("PhysicalDrive")
            .next()
            .and_then(|n| n.parse::<u32>().ok());
        for part in &disk.partitions {
            if part.volume_letter.is_some() || part.size_bytes < (1 << 20) {
                continue;
            }
            let content = open_partition(&disk.device_id, part.offset_bytes, part.size_bytes)
                .and_then(|mut v| head(&mut *v))
                .map(|h| identify(&h))
                .unwrap_or(Content::Other);
            if !matches!(
                content,
                Content::Ext | Content::Btrfs | Content::Lvm | Content::Luks
            ) {
                continue;
            }
            let label = source_label(part, content);
            out.push(Source {
                id: format!("{}#{}", disk.device_id, part.index),
                disk: disk.device_id.clone(),
                disk_number: number,
                model: disk.model.clone(),
                index: part.index,
                offset: part.offset_bytes,
                size: part.size_bytes,
                content,
                label,
            });
        }
    }
    out
}

/// What a partition is called besides its place and type: its label, or
/// where it was last mounted. Not for a LUKS container: the inventory gives
/// those a made-up label, «LUKS (зашифровано)», and the name already says
/// LUKS — it read «LUKS · LUKS (зашифровано)» (owner, 28.10).
fn source_label(part: &own_backup_common::disk::PartitionInfo, content: Content) -> String {
    if content == Content::Luks {
        return String::new();
    }
    if !part.volume_label.is_empty() {
        part.volume_label.clone()
    } else {
        part.linux_last_mounted.clone()
    }
}

/// An image file offered as a partition (development builds of the service:
/// how the plugin is tried without a Linux disk). `None` when it holds
/// nothing this module reads.
pub fn image_source(path: &str) -> Option<Source> {
    let size = std::fs::metadata(path).ok()?.len();
    let content = open_partition(path, 0, size)
        .and_then(|mut v| head(&mut *v))
        .map(|h| identify(&h))
        .ok()?;
    if !matches!(
        content,
        Content::Ext | Content::Btrfs | Content::Lvm | Content::Luks
    ) {
        return None;
    }
    Some(Source {
        id: format!("{path}#0"),
        disk: path.to_string(),
        disk_number: None,
        model: "image".into(),
        index: 0,
        offset: 0,
        size,
        content,
        label: String::new(),
    })
}

// --- volumes inside a volume ----------------------------------------------------

type Shared = Rc<RefCell<Box<dyn RawVolume + Send>>>;

/// The whole of a shared volume.
struct SharedView(Shared);

impl RawVolume for SharedView {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.borrow_mut().read_at(offset, buf)
    }
    fn len(&self) -> u64 {
        self.0.borrow().len()
    }
}

/// A logical volume: its segments, in its own order, on the shared PV.
struct LvView {
    base: Shared,
    segments: Vec<PvSegment>,
    size: u64,
}

impl LvView {
    fn new(base: Shared, segments: Vec<PvSegment>, size: u64) -> Self {
        Self {
            base,
            segments,
            size,
        }
    }
}

impl RawVolume for LvView {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let seg = self
                .segments
                .iter()
                .find(|s| at >= s.lv_offset && at < s.lv_offset + s.length)
                .ok_or_else(|| {
                    OwnBackupError::Device(format!("logical volume: {at} is outside its segments"))
                })?;
            let within = at - seg.lv_offset;
            let n = ((seg.length - within) as usize).min(buf.len() - done);
            self.base
                .borrow_mut()
                .read_at(seg.pv_offset + within, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }
    fn len(&self) -> u64 {
        self.size
    }
}

/// What `ext4-view` reads through.
struct Reader(Box<dyn RawVolume>);

impl Ext4Read for Reader {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        self.0
            .read_at(start_byte, dst)
            .map_err(|e| Box::new(std::io::Error::other(e.to_string())) as _)
    }
}

// --- the filesystem -------------------------------------------------------------

/// One entry of a directory, as the plugin shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Entry {
    /// [`names::to_display`] of the name.
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since 1970 (UTC) and the nanoseconds within.
    pub modified: i64,
    pub nanos: u32,
}

/// A readable filesystem: ext2/3/4 or btrfs.
pub enum Fs {
    // Boxed: both are hundreds of bytes, and the size of the larger.
    Ext(Box<ExtFs>),
    Btrfs(Box<btrfs::Btrfs>),
}

impl Fs {
    pub fn label(&self) -> String {
        match self {
            Fs::Ext(f) => f.label(),
            Fs::Btrfs(f) => f.label(),
        }
    }

    /// The entries of the directory at `path` (absolute Linux path bytes),
    /// symbolic links shown as what they point to; links to nowhere and
    /// special files (devices, sockets, pipes) left out.
    pub fn list(&mut self, path: &[u8]) -> std::result::Result<Vec<Entry>, String> {
        match self {
            Fs::Ext(f) => f.list(path),
            Fs::Btrfs(f) => f.list(path),
        }
    }

    /// The entry at `path`, links followed.
    pub fn stat(&mut self, path: &[u8]) -> std::result::Result<Entry, String> {
        match self {
            Fs::Ext(f) => f.stat(path),
            Fs::Btrfs(f) => f.stat(path),
        }
    }

    /// Up to `len` bytes of the file at `path` from `offset`; fewer at its end.
    pub fn read(
        &mut self,
        path: &[u8],
        offset: u64,
        len: usize,
    ) -> std::result::Result<Vec<u8>, String> {
        match self {
            Fs::Ext(f) => f.read(path, offset, len),
            Fs::Btrfs(f) => f.read(path, offset, len),
        }
    }
}

/// A readable ext filesystem.
pub struct ExtFs {
    ext: Ext4,
    /// The file last read, kept open: a copy reads one file in many pieces.
    open: Option<(Vec<u8>, ext4_view::File)>,
}

fn ext_error(e: ext4_view::Ext4Error) -> String {
    e.to_string()
}

impl ExtFs {
    pub fn label(&self) -> String {
        self.ext.label().to_str().unwrap_or_default().to_string()
    }

    /// The entries of the directory at `path` (absolute Linux path bytes),
    /// symbolic links shown as what they point to. Links that point nowhere
    /// or outside this filesystem, and special files (devices, sockets,
    /// pipes), have no files to show and are left out.
    pub fn list(&self, path: &[u8]) -> std::result::Result<Vec<Entry>, String> {
        let mut out = Vec::new();
        for entry in self.ext.read_dir(path).map_err(ext_error)? {
            let entry = entry.map_err(ext_error)?;
            let file_name = entry.file_name();
            let raw: &[u8] = file_name.as_ref();
            if raw == b"." || raw == b".." {
                continue;
            }
            let name = names::to_display(raw);
            // `metadata` of the path follows a symbolic link; of the entry it
            // would not.
            let Ok(meta) = self.ext.metadata(&entry.path()) else {
                continue;
            };
            let kind = meta.file_type();
            if !kind.is_dir() && !kind.is_regular_file() {
                continue;
            }
            let t = meta.modified();
            out.push(Entry {
                name,
                dir: kind.is_dir(),
                size: if kind.is_dir() { 0 } else { meta.len() },
                modified: t.seconds(),
                nanos: t.nanoseconds(),
            });
        }
        out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    /// The entry at `path`, links followed.
    pub fn stat(&self, path: &[u8]) -> std::result::Result<Entry, String> {
        let meta = self.ext.metadata(path).map_err(ext_error)?;
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or_default();
        let t = meta.modified();
        Ok(Entry {
            name: names::to_display(name),
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: t.seconds(),
            nanos: t.nanoseconds(),
        })
    }

    /// Up to `len` bytes of the file at `path` from `offset`; fewer at its end.
    pub fn read(
        &mut self,
        path: &[u8],
        offset: u64,
        len: usize,
    ) -> std::result::Result<Vec<u8>, String> {
        if self.open.as_ref().map(|(p, _)| p.as_slice()) != Some(path) {
            let file = self.ext.open(path).map_err(ext_error)?;
            self.open = Some((path.to_vec(), file));
        }
        let (_, file) = self.open.as_mut().expect("opened above");
        file.seek_to(offset.min(file.metadata().len()))
            .map_err(ext_error)?;
        let mut out = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = file.read_bytes(&mut out[got..]).map_err(ext_error)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        out.truncate(got);
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
