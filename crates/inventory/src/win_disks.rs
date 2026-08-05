//! Disk inventory for Windows, read straight from the storage stack.
//!
//! Until 2026-07-30 this ran four batched PowerShell calls (`Get-CimInstance
//! Win32_DiskDrive`, `Get-Disk`, `Get-Partition`, `Get-Volume`). That cost ten
//! seconds per `ListDisks`, dropped the partition GUIDs on the floor —
//! `Get-Partition` does not expose them, which is why the restore designer
//! always saw them empty — and, decisively, made the product depend on
//! something WinPE does not have. The boot media needs the whole program, GUI
//! included, so PowerShell had to go (`.ai/no-powershell-plan_2026-07-30.md`).
//!
//! What replaces it is what PowerShell was asking on our behalf anyway:
//!
//! * which physical drives exist — the object manager's own list of DOS device
//!   names (`QueryDosDevice`), with a bounded probe as the fallback;
//! * what each one is — `IOCTL_STORAGE_QUERY_PROPERTY` (model, serial, bus,
//!   removable, sector sizes, spinning or not) and `IOCTL_DISK_GET_DRIVE_
//!   GEOMETRY_EX` (exact capacity — `Win32_DiskDrive.Size` was rounded down to
//!   a whole cylinder, which cost us a special case of its own);
//! * how it is carved up — `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`, which hands over
//!   the partition type GUID, the unique GUID, the offset and the length;
//! * what is inside — the volume list, matched to partitions **by offset on the
//!   disk** rather than by drive letter, so a letterless volume is just as
//!   visible as `C:`.
//!
//! Filesystems Windows does not recognise are still identified by reading their
//! superblock ([`crate::fs_probe`]) — that part never went through PowerShell.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use own_backup_common::disk::{DiskInfo, PartitionInfo};
use own_backup_common::types::{FileSystem, PartitionTable};
use own_backup_common::{OwnBackupError, Result};
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNamesForVolumeNameW,
    QueryDosDeviceW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::IO::DeviceIoControl;

use crate::win_parse::{
    ansi_at, guid_at, media_type_of, parse_filesystem, u32_at, u64_at, GPT_TYPE_EFI_SYSTEM,
};

/// Disk inventory backed by the Windows storage stack.
pub struct WindowsInventory {
    _private: (),
}

impl WindowsInventory {
    pub fn new() -> Result<Self> {
        Ok(WindowsInventory { _private: () })
    }
}

// ---------------------------------------------------------------------------
// Win32 plumbing
// ---------------------------------------------------------------------------

/// RAII wrapper closing a Win32 handle exactly once.
struct FileHandle(HANDLE);

impl Drop for FileHandle {
    fn drop(&mut self) {
        // SAFETY: handle owned by us, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a device for *querying only*.
///
/// Zero desired access is deliberate: every IOCTL this module sends is
/// `FILE_ANY_ACCESS`, and a handle that asks for no rights opens disks and
/// volumes an unprivileged caller could not otherwise touch, without
/// disturbing whatever else holds them.
fn open_query(path: &str) -> Option<FileHandle> {
    let wide = to_wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    handle.ok().map(FileHandle)
}

/// Send an IOCTL with no input buffer; returns the bytes written to `out`.
fn ioctl(handle: HANDLE, code: u32, out: &mut [u8]) -> Option<usize> {
    ioctl_in(handle, code, &[], out)
}

/// Send an IOCTL with an input buffer; returns the bytes written to `out`.
fn ioctl_in(handle: HANDLE, code: u32, input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut returned = 0u32;
    let in_ptr = if input.is_empty() { None } else { Some(input.as_ptr() as *const _) };
    // SAFETY: both buffers are valid for the sizes passed.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            in_ptr,
            input.len() as u32,
            Some(out.as_mut_ptr() as *mut _),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    ok.ok().map(|()| returned as usize)
}

// ---------------------------------------------------------------------------
// Physical drives
// ---------------------------------------------------------------------------

/// Every physical drive number the system currently has.
///
/// The object manager already keeps this list — `QueryDosDevice` with no name
/// returns every DOS device, `PhysicalDriveN` among them — so there is nothing
/// to enumerate, filter or guess. If that call fails (a tiny buffer, a locked
/// down session), fall back to probing a bounded range, which is what the
/// restore's collision scan does too.
fn physical_drive_numbers() -> Vec<u32> {
    const MAX_PROBE: u32 = 64;
    let mut buf = vec![0u16; 64 * 1024];
    loop {
        // SAFETY: NULL device name asks for the whole list; buffer sized below.
        let len = unsafe { QueryDosDeviceW(PCWSTR::null(), Some(buf.as_mut_slice())) } as usize;
        if len > 0 {
            let mut out: Vec<u32> = buf[..len]
                .split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| {
                    let name = String::from_utf16_lossy(s);
                    name.strip_prefix("PhysicalDrive")?.parse::<u32>().ok()
                })
                .collect();
            out.sort_unstable();
            out.dedup();
            if !out.is_empty() {
                return out;
            }
            break;
        }
        if buf.len() >= 1024 * 1024 {
            break;
        }
        buf = vec![0u16; buf.len() * 4];
    }
    warn!("QueryDosDevice gave no drive list — probing PhysicalDrive0..63 instead");
    (0..MAX_PROBE)
        .filter(|n| open_query(&format!(r"\\.\PhysicalDrive{n}")).is_some())
        .collect()
}

/// What the storage stack says about one drive.
struct DiskFacts {
    size_bytes: u64,
    sector_logical: u32,
    sector_physical: u32,
    model: String,
    serial: String,
    media_type: String,
}

/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`: exact capacity and logical sector size.
///
/// `DISK_GEOMETRY_EX` = `DISK_GEOMETRY` (24 bytes: Cylinders@0, MediaType@8,
/// TracksPerCylinder@12, SectorsPerTrack@16, BytesPerSector@20) followed by
/// `DiskSize`@24.
fn disk_geometry(handle: HANDLE) -> Option<(u64, u32)> {
    let mut out = [0u8; 64];
    let n = ioctl(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &mut out)?;
    if n < 32 {
        return None;
    }
    let sector = u32_at(&out, 20);
    let size = u64_at(&out, 24);
    Some((size, if sector == 0 { 512 } else { sector }))
}

/// `IOCTL_STORAGE_QUERY_PROPERTY` with a `STORAGE_PROPERTY_QUERY` input
/// (PropertyId@0, QueryType@4 — `PropertyStandardQuery` is 0).
fn storage_property(handle: HANDLE, property_id: u32, out: &mut [u8]) -> Option<usize> {
    let mut query = [0u8; 12];
    query[0..4].copy_from_slice(&property_id.to_le_bytes());
    // QueryType = PropertyStandardQuery (0), AdditionalParameters = 0.
    ioctl_in(handle, IOCTL_STORAGE_QUERY_PROPERTY, &query, out)
}

const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_ACCESS_ALIGNMENT_PROPERTY: u32 = 6;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;

fn disk_facts(handle: HANDLE) -> DiskFacts {
    let (size_bytes, sector_logical) = disk_geometry(handle).unwrap_or((0, 512));

    // STORAGE_DEVICE_DESCRIPTOR: Version@0, Size@4, DeviceType@8,
    // DeviceTypeModifier@9, RemovableMedia@10, CommandQueueing@11,
    // VendorIdOffset@12, ProductIdOffset@16, ProductRevisionOffset@20,
    // SerialNumberOffset@24, BusType@28, RawPropertiesLength@32.
    let mut desc = [0u8; 1024];
    let device = storage_property(handle, STORAGE_DEVICE_PROPERTY, &mut desc);
    let (model, serial, bus, removable) = match device {
        Some(n) if n >= 36 => {
            let vendor = ansi_at(&desc[..n], u32_at(&desc, 12));
            let product = ansi_at(&desc[..n], u32_at(&desc, 16));
            let serial = ansi_at(&desc[..n], u32_at(&desc, 24));
            let model = match (vendor.is_empty(), product.is_empty()) {
                (false, false) => format!("{vendor} {product}"),
                (true, false) => product,
                (false, true) => vendor,
                (true, true) => String::new(),
            };
            (model, serial, u32_at(&desc, 28), desc[10] != 0)
        }
        _ => (String::new(), String::new(), 0, false),
    };

    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version@0, Size@4, IncursSeekPenalty@8.
    let mut penalty = [0u8; 16];
    let spins = storage_property(handle, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY, &mut penalty);
    let seek_penalty = match spins {
        Some(n) if n >= 9 => Some(penalty[8] != 0),
        _ => None,
    };

    // STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR: BytesPerLogicalSector@16,
    // BytesPerPhysicalSector@20. The physical size is what a 4Kn/512e disk
    // wants writes aligned to; the logical one is what offsets are counted in.
    let mut align = [0u8; 32];
    let (sector_logical, sector_physical) =
        match storage_property(handle, STORAGE_ACCESS_ALIGNMENT_PROPERTY, &mut align) {
            Some(n) if n >= 24 => {
                let logical = u32_at(&align, 16);
                let physical = u32_at(&align, 20);
                (
                    if logical == 0 { sector_logical } else { logical },
                    if physical == 0 { sector_logical } else { physical },
                )
            }
            _ => (sector_logical, sector_logical),
        };

    DiskFacts {
        size_bytes,
        sector_logical,
        sector_physical,
        model,
        serial,
        media_type: media_type_of(bus, seek_penalty, removable),
    }
}

/// One partition as the partition manager describes it.
struct RawPartition {
    number: u32,
    offset: u64,
    length: u64,
    /// GPT partition type GUID (empty on MBR disks).
    type_guid: String,
    /// GPT unique partition GUID — the value `Get-Partition` never showed us,
    /// and the one the BCD and UEFI use to name a partition.
    part_guid: String,
    /// MBR active flag.
    bootable: bool,
}

/// `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` → (scheme, disk id, partitions).
///
/// `DRIVE_LAYOUT_INFORMATION_EX`: PartitionStyle@0, PartitionCount@4, then a
/// union (MBR: Signature@8, CheckSum@12; GPT: DiskId@8) padded to 48 bytes,
/// then `PARTITION_INFORMATION_EX[]` of 144 bytes each — the same layout
/// `disk_session_win` writes back.
fn drive_layout(handle: HANDLE) -> Option<(PartitionTable, String, Vec<RawPartition>)> {
    const HEADER: usize = 48;
    const ENTRY: usize = 144;
    const UNION: usize = 32;
    const STYLE_MBR: u32 = 0;
    const STYLE_GPT: u32 = 1;

    // 128 GPT entries is the standard maximum; ask for room for all of them.
    let mut out = vec![0u8; HEADER + ENTRY * 128];
    let n = ioctl(handle, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &mut out)?;
    if n < HEADER {
        return None;
    }
    let style = u32_at(&out, 0);
    let count = (u32_at(&out, 4) as usize).min((n - HEADER) / ENTRY);
    let (scheme, disk_id) = match style {
        STYLE_GPT => (PartitionTable::Gpt, guid_at(&out, 8)),
        STYLE_MBR => (PartitionTable::Mbr, format!("{:08X}", u32_at(&out, 8))),
        _ => return None, // RAW: no table at all
    };

    let mut parts = Vec::new();
    for i in 0..count {
        let b = HEADER + i * ENTRY;
        let entry = &out[b..b + ENTRY];
        let length = u64_at(entry, 16);
        if length == 0 {
            continue; // an unused MBR slot
        }
        let u = UNION;
        // PARTITION_INFORMATION_MBR: PartitionType@0, BootIndicator@1.
        let mbr_type = entry[u];
        let (type_guid, part_guid, bootable) = if style == STYLE_GPT {
            (guid_at(entry, u), guid_at(entry, u + 16), false)
        } else {
            (String::new(), String::new(), entry[u + 1] != 0)
        };
        if style == STYLE_GPT && type_guid.is_empty() {
            continue; // an empty GPT slot
        }
        if style == STYLE_MBR && mbr_type == 0 {
            continue;
        }
        parts.push(RawPartition {
            number: u32_at(entry, 24),
            offset: u64_at(entry, 8),
            length,
            type_guid,
            part_guid,
            bootable,
        });
    }
    parts.sort_by_key(|p| p.offset);
    Some((scheme, disk_id, parts))
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

/// What Windows knows about one mounted volume.
struct VolumeFacts {
    /// `\\?\Volume{GUID}\` — with the trailing backslash Windows reports.
    root: String,
    disk_number: u32,
    offset: u64,
    letter: Option<char>,
    filesystem: FileSystem,
    label: String,
    size_bytes: u64,
    free_bytes: u64,
}

/// Every `\\?\Volume{GUID}\` root the system knows about.
fn volume_roots() -> Vec<String> {
    let mut out = Vec::new();
    let mut name = [0u16; 512];
    // SAFETY: buffer valid for the call; the find handle is closed below.
    let Ok(find) = (unsafe { FindFirstVolumeW(&mut name) }) else {
        return out;
    };
    loop {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        out.push(String::from_utf16_lossy(&name[..len]));
        name = [0u16; 512];
        // SAFETY: valid find handle and buffer.
        if unsafe { FindNextVolumeW(find, &mut name) }.is_err() {
            break;
        }
    }
    // SAFETY: handle from a successful FindFirstVolumeW.
    let _ = unsafe { FindVolumeClose(find) };
    out
}

/// `(disk number, starting offset)` of a single-extent volume. Spanned and
/// striped volumes are skipped on purpose: a partition-shaped inventory has
/// nowhere to put them, and we never back one up as if it were a partition.
fn volume_extent(root: &str) -> Option<(u32, u64)> {
    let handle = open_query(root.trim_end_matches('\\'))?;
    // VOLUME_DISK_EXTENTS: NumberOfDiskExtents@0, then DISK_EXTENT[] at 8
    // (DiskNumber@0, StartingOffset@8, ExtentLength@16), 24 bytes each.
    let mut out = [0u8; 8 + 24 * 16];
    let n = ioctl(handle.0, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut out)?;
    if n < 8 + 24 || u32_at(&out, 0) != 1 {
        return None;
    }
    Some((u32_at(&out, 8), u64_at(&out, 16)))
}

/// Drive letter mapped to this volume, if it has one. Folder mount points are
/// ignored — a volume can have several names, and only a letter is one.
fn volume_letter(root: &str) -> Option<char> {
    let wide = to_wide(root); // keeps the trailing backslash the call requires
    let mut buf = vec![0u16; 512];
    let mut needed = 0u32;
    // SAFETY: `wide` outlives the call; buffer sized by its length.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(wide.as_ptr()),
            Some(buf.as_mut_slice()),
            &mut needed,
        )
    };
    if ok.is_err() {
        if needed as usize <= buf.len() {
            return None;
        }
        buf = vec![0u16; needed as usize];
        // SAFETY: as above, with the buffer Windows asked for.
        if unsafe {
            GetVolumePathNamesForVolumeNameW(
                PCWSTR(wide.as_ptr()),
                Some(buf.as_mut_slice()),
                &mut needed,
            )
        }
        .is_err()
        {
            return None;
        }
    }
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let path = String::from_utf16_lossy(s);
            let b = path.as_bytes();
            (b.len() == 3 && b[1] == b':' && b[2] == b'\\' && b[0].is_ascii_alphabetic())
                .then(|| (b[0] as char).to_ascii_uppercase())
        })
        .next()
}

/// Label + filesystem + size, for a volume Windows has mounted. A volume it
/// could not mount (a Linux filesystem, an encrypted container) answers with
/// nothing, and the caller reads the superblock instead.
fn volume_contents(root: &str) -> (FileSystem, String, u64, u64) {
    let wide = to_wide(root);
    let mut label = [0u16; 256];
    let mut fs_name = [0u16; 64];
    // SAFETY: `wide` outlives the call; both buffers are sized by their length.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    };
    let (filesystem, label) = if ok.is_ok() {
        let cut = |b: &[u16]| {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf16_lossy(&b[..n])
        };
        (parse_filesystem(&cut(&fs_name)), cut(&label))
    } else {
        (FileSystem::Unknown, String::new())
    };

    let (mut size, mut free) = (0u64, 0u64);
    // SAFETY: `wide` outlives the call; both out-pointers are live locals.
    let _ = unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), None, Some(&mut size), Some(&mut free))
    };
    (filesystem, label, size, free)
}

fn enumerate_volumes() -> Vec<VolumeFacts> {
    let mut out = Vec::new();
    for root in volume_roots() {
        let Some((disk_number, offset)) = volume_extent(&root) else { continue };
        let (filesystem, label, size_bytes, free_bytes) = volume_contents(&root);
        out.push(VolumeFacts {
            letter: volume_letter(&root),
            root,
            disk_number,
            offset,
            filesystem,
            label,
            size_bytes,
            free_bytes,
        });
    }
    out
}

/// The `\\?\Volume{GUID}\` root the running Windows lives on.
///
/// Replaces "drive letter C: and NTFS", which was a guess that happened to be
/// right on most machines. On boot media it answers with the RAM disk, so no
/// partition on any real disk is marked as running the system — which is
/// exactly right there: on WinPE nothing is.
/// Is a Windows installation present on this volume?
///
/// Asked of every volume, not just the running one, because a rescue medium
/// needs to know which partition holds the system it is there to copy — and
/// from the medium that system is not running, so no flag says so. The test is
/// the kernel file itself: present on every Windows since NT, and nowhere else.
///
/// The letter is preferred over the volume GUID path only because a wrong or
/// stale letter fails a stat, while a GUID path always resolves; either form is
/// accepted by the filesystem API.
fn volume_holds_os(volume_root: Option<&str>, letter: Option<char>) -> bool {
    let root = match (letter, volume_root) {
        (Some(l), _) => format!("{l}:\\"),
        (None, Some(v)) => v.to_string(),
        (None, None) => return false,
    };
    let kernel = std::path::Path::new(&root)
        .join("Windows")
        .join("System32")
        .join("ntoskrnl.exe");
    kernel.exists()
}

fn system_volume_root() -> Option<String> {
    let mut win = [0u16; 260];
    // SAFETY: buffer valid for the call.
    let n = unsafe { GetSystemWindowsDirectoryW(Some(&mut win)) } as usize;
    if n == 0 || n > win.len() {
        return None;
    }
    let dir = String::from_utf16_lossy(&win[..n]);
    let root = format!("{}\\", dir.split('\\').next()?); // "C:\Windows" → "C:\"
    let wide = to_wide(&root);
    let mut name = [0u16; 512];
    // SAFETY: `wide` outlives the call; buffer sized by its length.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut name) }.ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

// ---------------------------------------------------------------------------
// DiskInventory implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl super::traits::DiskInventory for WindowsInventory {
    async fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        let started = std::time::Instant::now();
        let numbers = physical_drive_numbers();
        let volumes = enumerate_volumes();
        let system_volume = system_volume_root();

        let mut disks = Vec::new();
        for number in numbers {
            let device_id = format!(r"\\.\PhysicalDrive{number}");
            let Some(handle) = open_query(&device_id) else {
                debug!(number, "physical drive did not open — skipped");
                continue;
            };
            let facts = disk_facts(handle.0);
            let (partition_table, disk_guid, raw_parts) = match drive_layout(handle.0) {
                Some(v) => v,
                // A disk with no readable table (uninitialised, or a card
                // reader with no card) is still a disk the user may restore
                // onto, so it is listed with no partitions rather than hidden.
                None => (PartitionTable::Gpt, String::new(), Vec::new()),
            };
            drop(handle);

            let mut partitions = Vec::new();
            for raw in &raw_parts {
                let volume = volumes
                    .iter()
                    .find(|v| v.disk_number == number && v.offset == raw.offset);

                // Trust Windows only when it recognised the filesystem; read
                // the superblock ourselves otherwise. Neither GPT type GUIDs
                // nor volume objects are reliable on their own — a Linux ext4
                // /boot can be typed as an EFI System Partition, and Windows
                // then still creates a volume for it and calls it RAW.
                let vol_fs = volume.map(|v| v.filesystem).unwrap_or(FileSystem::Unknown);
                let probed = if vol_fs == FileSystem::Unknown {
                    super::fs_probe::probe_partition_fs(number, raw.offset, raw.length)
                } else {
                    None
                };
                let filesystem = if vol_fs != FileSystem::Unknown {
                    vol_fs
                } else {
                    probed.as_ref().map(|p| p.fs).unwrap_or(FileSystem::Unknown)
                };
                let volume_label = match volume.map(|v| v.label.clone()).unwrap_or_default() {
                    l if !l.trim().is_empty() => l,
                    _ => probed.as_ref().and_then(|p| p.label.clone()).unwrap_or_default(),
                };
                // Used = size − free where Windows understands the filesystem;
                // a foreign volume reports meaningless numbers there, so for a
                // probed one we take what its superblock says.
                let used_bytes = match volume {
                    Some(v) if vol_fs != FileSystem::Unknown => {
                        v.size_bytes.saturating_sub(v.free_bytes)
                    }
                    _ => probed.as_ref().and_then(|p| p.used_bytes).unwrap_or(0),
                };
                let is_encrypted = probed.as_ref().map(|p| p.encrypted).unwrap_or(false);
                let letter = volume.and_then(|v| v.letter);

                // Openable device: the drive letter, else the volume GUID path,
                // else a physical-drive window (`part:<disk>:<off>:<len>`). A
                // partition with nothing recognisable in it gets no device and
                // cannot be backed up — unless it is an encrypted container,
                // which becomes readable once unlocked.
                let volume_device = if let Some(l) = letter {
                    format!("{l}:")
                } else if filesystem == FileSystem::Unknown && !is_encrypted {
                    String::new()
                } else if let Some(v) = volume {
                    v.root.trim_end_matches('\\').to_string()
                } else {
                    format!("part:{number}:{}:{}", raw.offset, raw.length)
                };

                let is_system = match (&system_volume, volume) {
                    (Some(sys), Some(v)) => sys.eq_ignore_ascii_case(&v.root),
                    _ => false,
                };
                let is_boot = raw.type_guid.eq_ignore_ascii_case(GPT_TYPE_EFI_SYSTEM)
                    || (partition_table == PartitionTable::Mbr && raw.bootable);
                let has_os = volume_holds_os(volume.map(|v| v.root.as_str()), letter);

                partitions.push(PartitionInfo {
                    index: raw.number,
                    offset_bytes: raw.offset,
                    size_bytes: raw.length,
                    used_bytes,
                    gpt_type_guid: raw.type_guid.clone(),
                    gpt_partition_guid: raw.part_guid.clone(),
                    filesystem,
                    volume_letter: letter,
                    volume_device,
                    volume_label,
                    fs_uuid: String::new(),
                    is_system,
                    is_boot,
                    has_os,
                    is_encrypted,
                    // Filled in by the service, which can reach the BitLocker
                    // parser — this crate cannot, the parser's crate depends
                    // on this one.
                    unlock_methods: Vec::new(),
                    unlockable: false,
                });
            }
            partitions.sort_by_key(|p| p.index);

            debug!(
                number,
                model = %facts.model,
                size = facts.size_bytes,
                partitions = partitions.len(),
                "disk enumerated"
            );
            disks.push(DiskInfo {
                device_id,
                model: facts.model,
                serial: facts.serial,
                size_bytes: facts.size_bytes,
                sector_size_logical: facts.sector_logical,
                sector_size_physical: facts.sector_physical,
                media_type: facts.media_type,
                partition_table,
                disk_guid: if disk_guid.is_empty() { format!("disk-{number}") } else { disk_guid },
                partitions,
            });
        }

        if disks.is_empty() {
            warn!("no physical drives enumerated");
        }
        info!(count = disks.len(), ms = started.elapsed().as_millis(), "disk enumeration complete");
        Ok(disks)
    }

    async fn get_disk_details(&self, device_id: &str) -> Result<DiskInfo> {
        // There are a handful of disks in a machine; enumerating all of them to
        // answer about one costs milliseconds now.
        let disks = self.enumerate_disks().await?;
        disks
            .into_iter()
            .find(|d| d.device_id.eq_ignore_ascii_case(device_id))
            .ok_or_else(|| OwnBackupError::Device(format!("Disk not found: {device_id}")))
    }
}

