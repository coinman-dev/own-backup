//! What a UEFI boot entry is, in bytes.
//!
//! The pure half of [`crate::efivars_win`]: given the contents of a `Boot####`
//! firmware variable, say what it points at and what it is called. No firmware,
//! no Windows, so it can be tested anywhere — which matters more here than
//! usual, because the other half can only be exercised on a machine whose boot
//! configuration is the thing at risk.
//!
//! ## Why this is not just a string
//!
//! A boot entry does not name a file the way a shortcut does. It names a
//! *device path*: a chain of nodes that walks from the machine down to a file —
//! the disk, then the partition on it, then the path inside that partition's
//! filesystem. The partition is identified by its GUID and its position in
//! sectors, never by its drive letter or its number in the list, because the
//! firmware runs before anything that could assign either.
//!
//! That is also why a boot entry copied to another machine, or to a disk that
//! was restored into a different layout, points at nothing: the GUID in the
//! entry no longer matches any partition. The firmware does not complain. The
//! menu item simply appears and does not work.
//!
//! ## The layout (UEFI spec §3.1.3)
//!
//! ```text
//! u32    Attributes           bit 0 = LOAD_OPTION_ACTIVE
//! u16    FilePathListLength   length of the device path, in bytes
//! u16[]  Description          UTF-16, NUL-terminated — the name in the menu
//! u8[]   FilePathList         FilePathListLength bytes of device path nodes
//! u8[]   OptionalData         whatever is left: arguments for the loader
//! ```
//!
//! `FilePathListLength` is a LENGTH, not an offset: the description sits before
//! the device path and its size is only known by finding the terminating NUL.
//! Renaming an entry therefore moves everything after the name, which is why
//! this module round-trips through bytes instead of patching them in place.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use own_backup_common::{OwnBackupError, Result};

/// The entry is offered by the firmware's boot menu. Without this bit the entry
/// exists, keeps its place in the order, and is skipped — which is what
/// "disabled" means to a firmware.
pub const LOAD_OPTION_ACTIVE: u32 = 0x0000_0001;
/// The firmware does not show it, but may still boot it if asked by number.
pub const LOAD_OPTION_HIDDEN: u32 = 0x0000_0008;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::ImageFormat(format!("запись загрузки: {msg}"))
}

/// One `Boot####` variable, taken apart.
///
/// `file_path_list` is kept as raw bytes on purpose. We re-serialise entries we
/// were asked to rename, and a device path we did not fully understand must
/// come back out exactly as it went in — the firmware is the only thing that
/// has to make sense of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOption {
    pub attributes: u32,
    pub description: String,
    pub file_path_list: Vec<u8>,
    pub optional_data: Vec<u8>,
}

impl LoadOption {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 6 {
            return Err(bad(format!("всего {} байт, а заголовку нужно 6", bytes.len())));
        }
        let attributes = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let path_len = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as usize;

        // The description runs to the first UTF-16 NUL. Its length is not
        // recorded anywhere, so a missing terminator is unrecoverable rather
        // than merely odd.
        let rest = &bytes[6..];
        let mut end = None;
        let mut i = 0;
        while i + 1 < rest.len() {
            if rest[i] == 0 && rest[i + 1] == 0 {
                end = Some(i);
                break;
            }
            i += 2;
        }
        let Some(end) = end else {
            return Err(bad("название не заканчивается нулём"));
        };
        let units: Vec<u16> =
            rest[..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let description = String::from_utf16_lossy(&units);

        let after_name = end + 2;
        if after_name + path_len > rest.len() {
            return Err(bad(format!(
                "путь заявлен в {path_len} байт, а осталось {}",
                rest.len() - after_name
            )));
        }
        Ok(Self {
            attributes,
            description,
            file_path_list: rest[after_name..after_name + path_len].to_vec(),
            optional_data: rest[after_name + path_len..].to_vec(),
        })
    }

    /// Back to the bytes the firmware expects. Everything but the name and the
    /// flags travels through untouched.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.description.len() * 2 + self.file_path_list.len());
        out.extend_from_slice(&self.attributes.to_le_bytes());
        out.extend_from_slice(&(self.file_path_list.len() as u16).to_le_bytes());
        for u in self.description.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]); // the terminator the description needs
        out.extend_from_slice(&self.file_path_list);
        out.extend_from_slice(&self.optional_data);
        out
    }

    /// Shown in the firmware's menu.
    pub fn is_active(&self) -> bool {
        self.attributes & LOAD_OPTION_ACTIVE != 0
    }

    pub fn is_hidden(&self) -> bool {
        self.attributes & LOAD_OPTION_HIDDEN != 0
    }

    /// The device path, node by node. An unparsable tail stops the walk rather
    /// than throwing away the nodes already read: a half-understood entry is
    /// still worth showing, it just must not be edited.
    pub fn device_path(&self) -> Vec<DevicePathNode> {
        DevicePathNode::walk(&self.file_path_list)
    }

    /// The file this entry starts, as it is written inside the partition
    /// (`\EFI\acronis\bootx64.efi`). `None` for an entry that names a device
    /// and no file — a whole disk, a network card, a removable-media slot.
    pub fn file_path(&self) -> Option<String> {
        self.device_path().into_iter().find_map(|n| match n {
            DevicePathNode::FilePath(p) => Some(p),
            _ => None,
        })
    }

    /// Which partition holds that file, if the entry says so.
    pub fn hard_drive(&self) -> Option<HardDrive> {
        self.device_path().into_iter().find_map(|n| match n {
            DevicePathNode::HardDrive(h) => Some(h),
            _ => None,
        })
    }

    /// True when every node of the device path was understood. An entry that
    /// fails this is shown but not offered for editing: re-serialising bytes we
    /// could not read would be guessing with the machine's boot configuration.
    pub fn fully_understood(&self) -> bool {
        !self.device_path().iter().any(|n| matches!(n, DevicePathNode::Unreadable { .. }))
    }

    /// Does this entry boot off something a person plugs in — a flash drive, an
    /// external disk?
    ///
    /// Such an entry belongs to the stick, not to the machine: it exists while
    /// the stick is in, and firmware makes a new one the next time. Копируя
    /// меню загрузки, мы копируем машину, а не то, что в неё воткнули: запись
    /// про флешку на восстановленной машине указывает в пустоту.
    ///
    /// Told by the device path, not by the name: a stick with a bootable ESP
    /// has a perfectly ordinary file path (`\EFI\BOOT\BOOTX64.EFI`) and a
    /// description of its own choosing, and the only thing that always gives it
    /// away is the USB node the firmware put in front of it.
    pub fn is_removable_device(&self) -> bool {
        self.device_path().iter().any(|n| match n {
            DevicePathNode::Other { kind, subtype, .. } => {
                *kind == DevicePathNode::MESSAGING
                    && matches!(
                        *subtype,
                        DevicePathNode::MSG_USB
                            | DevicePathNode::MSG_USB_CLASS
                            | DevicePathNode::MSG_USB_WWID
                    )
            }
            _ => false,
        })
    }
}

/// One node of a device path. Anything we do not have a shape for is kept whole
/// under [`DevicePathNode::Other`] — it is still valid, just not ours to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePathNode {
    HardDrive(HardDrive),
    FilePath(String),
    End,
    Other { kind: u8, subtype: u8, data: Vec<u8> },
    /// The bytes ran out in the middle of a node, or a node claimed a length
    /// that cannot be true.
    Unreadable { at: usize, reason: String },
}

/// Media / Hard Drive node (type 4, subtype 1): which partition, named the way
/// firmware can find it before any operating system runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardDrive {
    pub partition_number: u32,
    pub start_lba: u64,
    pub size_lba: u64,
    pub signature: [u8; 16],
    /// 1 = MBR, 2 = GPT.
    pub format: u8,
    /// 0 = none, 1 = a 32-bit MBR signature, 2 = a GPT partition GUID.
    pub signature_type: u8,
}

impl HardDrive {
    /// The partition's own GUID, when the signature is one. This is the join key
    /// to a real partition: it survives renumbering, reformatting of other
    /// partitions, and a different drive letter.
    pub fn partition_guid(&self) -> Option<String> {
        if self.signature_type != 2 {
            return None;
        }
        let s = &self.signature;
        // A GUID on the wire is mixed-endian: the first three fields are little-
        // endian, the last eight bytes are in order.
        Some(format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{}",
            s[3], s[2], s[1], s[0],
            s[5], s[4],
            s[7], s[6],
            s[8], s[9],
            s[10..16].iter().map(|b| format!("{b:02X}")).collect::<String>()
        ))
    }

    /// Describe a GPT partition the way a boot entry has to.
    ///
    /// `part_guid` comes from the partition table as text; the entry stores it
    /// as bytes, and the first three fields are byte-swapped on the way in.
    /// Getting that backwards produces a GUID that looks plausible in a hex
    /// dump and matches no partition on the machine.
    pub fn from_gpt(
        partition_number: u32,
        first_lba: u64,
        last_lba: u64,
        part_guid: &str,
    ) -> Option<Self> {
        Some(Self {
            partition_number,
            start_lba: first_lba,
            size_lba: last_lba.checked_sub(first_lba)?.checked_add(1)?,
            signature: guid_to_bytes(part_guid)?,
            format: 2,         // GPT
            signature_type: 2, // the signature is a partition GUID
        })
    }

    /// An MBR disk signature, for the entries that predate GPT.
    pub fn mbr_signature(&self) -> Option<u32> {
        (self.signature_type == 1)
            .then(|| u32::from_le_bytes(self.signature[0..4].try_into().unwrap()))
    }
}

impl DevicePathNode {
    const END: u8 = 0x7F;
    const MEDIA: u8 = 0x04;
    const MEDIA_HARD_DRIVE: u8 = 0x01;
    const MEDIA_FILE_PATH: u8 = 0x04;
    /// Type 3: how the device is reached — USB, SATA, NVMe, network.
    const MESSAGING: u8 = 0x03;
    const MSG_USB: u8 = 0x05;
    const MSG_USB_CLASS: u8 = 0x0F;
    const MSG_USB_WWID: u8 = 0x10;

    fn walk(bytes: &[u8]) -> Vec<Self> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            if at + 4 > bytes.len() {
                out.push(Self::Unreadable {
                    at,
                    reason: format!("на заголовок узла нужно 4 байта, осталось {}", bytes.len() - at),
                });
                break;
            }
            let kind = bytes[at];
            let subtype = bytes[at + 1];
            let len = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
            // A node shorter than its own header, or longer than what is left,
            // would send the walk backwards or off the end.
            if len < 4 || at + len > bytes.len() {
                out.push(Self::Unreadable { at, reason: format!("узел объявил длину {len}") });
                break;
            }
            let body = &bytes[at + 4..at + len];
            out.push(match (kind, subtype) {
                (Self::END, _) => Self::End,
                (Self::MEDIA, Self::MEDIA_HARD_DRIVE) if body.len() >= 38 => {
                    Self::HardDrive(HardDrive {
                        partition_number: u32::from_le_bytes(body[0..4].try_into().unwrap()),
                        start_lba: u64::from_le_bytes(body[4..12].try_into().unwrap()),
                        size_lba: u64::from_le_bytes(body[12..20].try_into().unwrap()),
                        signature: body[20..36].try_into().unwrap(),
                        format: body[36],
                        signature_type: body[37],
                    })
                }
                (Self::MEDIA, Self::MEDIA_FILE_PATH) => {
                    let units: Vec<u16> = body
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .take_while(|&u| u != 0)
                        .collect();
                    Self::FilePath(String::from_utf16_lossy(&units))
                }
                _ => Self::Other { kind, subtype, data: body.to_vec() },
            });
            if kind == Self::END {
                break;
            }
            at += len;
        }
        out
    }
}

/// `{A5F05BA7-F4D3-4C23-A281-87C71B0950FA}` → the sixteen bytes a device path
/// holds. Braces optional, case ignored; the first three fields go in
/// little-endian and the last eight in order, which is the one detail worth
/// being careful about.
fn guid_to_bytes(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .bytes()
        .filter(|b| *b != b'-')
        .collect();
    if hex.len() != 32 || !hex.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = |i: usize| -> u8 {
        u8::from_str_radix(std::str::from_utf8(&hex[i * 2..i * 2 + 2]).unwrap(), 16).unwrap()
    };
    let mut out = [0u8; 16];
    // Data1 (4 bytes), Data2 (2), Data3 (2) — each reversed.
    out[0] = n(3);
    out[1] = n(2);
    out[2] = n(1);
    out[3] = n(0);
    out[4] = n(5);
    out[5] = n(4);
    out[6] = n(7);
    out[7] = n(6);
    // Data4 (8 bytes) — as written.
    for i in 8..16 {
        out[i] = n(i);
    }
    Some(out)
}

/// Build a device path that points at a file on a partition: the two nodes
/// every "boot this loader" entry is made of, plus the end marker.
///
/// The partition is described the way firmware finds it — GUID, first sector,
/// length — and never by drive letter or position in a list, because none of
/// those exist yet when this path is walked.
pub fn build_device_path(hd: &HardDrive, file: &str) -> Vec<u8> {
    let mut out = Vec::new();

    // Media / Hard Drive, 42 bytes including the header.
    out.extend_from_slice(&[0x04, 0x01, 42, 0x00]);
    out.extend_from_slice(&hd.partition_number.to_le_bytes());
    out.extend_from_slice(&hd.start_lba.to_le_bytes());
    out.extend_from_slice(&hd.size_lba.to_le_bytes());
    out.extend_from_slice(&hd.signature);
    out.push(hd.format);
    out.push(hd.signature_type);

    // Media / File Path: the name inside that partition, UTF-16 with its NUL.
    let name: Vec<u8> = file.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let node_len = name.len() + 2 + 4;
    out.extend_from_slice(&[0x04, 0x04]);
    out.extend_from_slice(&(node_len as u16).to_le_bytes());
    out.extend_from_slice(&name);
    out.extend_from_slice(&[0, 0]);

    // End Entire Device Path.
    out.extend_from_slice(&[0x7F, 0xFF, 0x04, 0x00]);
    out
}

/// A whole entry, ready to be written.
pub fn build_entry(description: &str, hd: &HardDrive, file: &str) -> LoadOption {
    LoadOption {
        attributes: LOAD_OPTION_ACTIVE,
        description: description.to_string(),
        file_path_list: build_device_path(hd, file),
        optional_data: Vec::new(),
    }
}

/// The lowest number no entry uses yet.
///
/// Firmware hands them out from zero upwards and tools expect the same, so a
/// new entry takes the first gap rather than the next number after the highest:
/// a store that has been edited for years otherwise creeps towards the 0xFFFF
/// ceiling for no reason.
pub fn free_entry_number(used: &[u16]) -> Option<u16> {
    (0u16..=0xFFFF).find(|n| !used.contains(n))
}

/// Move `what` to `to` inside the order, keeping everything else in sequence.
/// Out-of-range positions clamp instead of failing: "put it first" should mean
/// that whatever number was typed.
pub fn reorder(order: &[u16], what: u16, to: usize) -> Vec<u16> {
    let mut v: Vec<u16> = order.iter().copied().filter(|n| *n != what).collect();
    v.insert(to.min(v.len()), what);
    v
}

/// Do these two name the same file inside a partition?
///
/// The same loader is written `\EFI\ubuntu\shimx64.efi` by the tool that put it
/// there and `\EFI\UBUNTU\SHIMX64.EFI` by the firmware that holds the entry —
/// this machine has both spellings side by side (2026-09-18). FAT has no case
/// to speak of and UEFI says separators are backslashes, so a comparison that
/// respects either would report a loader as unnamed while its entry sits right
/// there.
pub fn same_efi_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .replace('/', "\\")
            .trim_start_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// The removable-media fallback path.
///
/// Every firmware tries `\EFI\BOOT\BOOTX64.EFI` by itself; that is what makes a
/// USB stick boot on a machine that has never seen it. No boot entry names it,
/// and a list that flags it as "a loader nobody points at" would be reporting
/// the one file on the partition that is working exactly as intended.
pub fn is_fallback_path(p: &str) -> bool {
    let n = p.trim().replace('/', "\\").trim_start_matches('\\').to_lowercase();
    n == "efi\\boot\\bootx64.efi" || n == "efi\\boot\\bootia32.efi" || n == "efi\\boot\\bootaa64.efi"
}

/// `Boot0007` → 7. The firmware names its entries in hexadecimal, four digits,
/// upper case; nothing else is a boot entry.
pub fn entry_number(var_name: &str) -> Option<u16> {
    let digits = var_name.strip_prefix("Boot")?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

/// 7 → `Boot0007`.
pub fn entry_name(n: u16) -> String {
    format!("Boot{n:04X}")
}

/// `BootOrder` is an array of entry numbers, little-endian, no header.
pub fn parse_boot_order(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

pub fn serialize_boot_order(order: &[u16]) -> Vec<u8> {
    order.iter().flat_map(|n| n.to_le_bytes()).collect()
}

/// A saved copy of the machine's boot configuration.
///
/// Why bytes and not a description. A restore has to put back exactly what was
/// there, including the parts we do not understand: firmware puts its own data
/// in `OptionalData`, device paths carry node types we have no shape for, and
/// an entry rebuilt from our reading of it would be our opinion, not the
/// original. So the file carries the variables verbatim and a human-readable
/// section beside them that a restore IGNORES — it is there for whoever opens
/// the file to see what they are about to put back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootBackup {
    pub format: String,
    pub version: u32,
    /// Which machine this came off. A boot entry names partitions by GUID, so a
    /// copy taken elsewhere restores menu items that point at nothing.
    pub machine: String,
    pub taken_at: String,
    /// What a restore writes back.
    pub variables: Vec<SavedVar>,
    /// What those bytes mean. Not read back.
    pub readable: Vec<SavedNote>,
}

/// One firmware variable, as it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedVar {
    pub name: String,
    /// The EFI attribute bits the variable was stored with. Restoring a boot
    /// variable without `NON_VOLATILE` would make it vanish at the next power
    /// cut, which looks exactly like a restore that did not work.
    pub attributes: u32,
    /// base64; the variables are a few hundred bytes each.
    pub data: String,
}

/// A line for the person reading the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedNote {
    pub name: String,
    pub description: String,
    pub file: Option<String>,
    pub partition_guid: Option<String>,
}

pub const BACKUP_FORMAT: &str = "own-backup-uefi-boot";
pub const BACKUP_VERSION: u32 = 1;

impl SavedVar {
    pub fn new(name: &str, attributes: u32, data: &[u8]) -> Self {
        Self { name: name.to_string(), attributes, data: B64.encode(data) }
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        B64.decode(&self.data).map_err(|e| bad(format!("{}: испорчены данные ({e})", self.name)))
    }
}

/// Is this a variable a boot backup is allowed to touch?
///
/// The firmware's variable namespace holds far more than boot entries — keys,
/// platform settings, whatever the vendor put there — and writing into it at
/// random is how a machine stops starting. A restore takes its list of names
/// from a FILE, so the file must not be able to name anything else. Whitelist,
/// never blacklist.
/// Is this saved variable an entry for something a person plugs in?
///
/// The same question as [`LoadOption::is_removable_device`], asked of a copy
/// rather than of firmware — because a copy taken before that rule existed
/// carries such entries, and putting them back would fill the restored
/// machine's menu with a flash drive that is not there. `BootOrder` and
/// `Timeout` are not entries and always answer no.
pub fn names_removable_device(v: &SavedVar) -> bool {
    if entry_number(&v.name).is_none() {
        return false;
    }
    v.bytes()
        .ok()
        .and_then(|b| LoadOption::parse(&b).ok())
        .is_some_and(|o| o.is_removable_device())
}

pub fn is_boot_variable(name: &str) -> bool {
    matches!(name, "BootOrder" | "BootNext" | "Timeout") || entry_number(name).is_some()
}

impl BootBackup {
    pub fn new(machine: &str, variables: Vec<SavedVar>, readable: Vec<SavedNote>) -> Self {
        Self {
            format: BACKUP_FORMAT.to_string(),
            version: BACKUP_VERSION,
            machine: machine.to_string(),
            taken_at: chrono::Utc::now().to_rfc3339(),
            variables,
            readable,
        }
    }

    /// Read a backup and refuse anything that is not one.
    ///
    /// Every check here is a way a restore could write the wrong bytes into
    /// firmware, so none of them is advisory.
    pub fn load(json: &str) -> Result<Self> {
        let me: Self = serde_json::from_str(json)
            .map_err(|e| bad(format!("это не копия записей загрузки: {e}")))?;
        if me.format != BACKUP_FORMAT {
            return Err(bad(format!("чужой формат файла: {}", me.format)));
        }
        if me.version > BACKUP_VERSION {
            return Err(bad(format!(
                "файл версии {}, а эта программа понимает до {BACKUP_VERSION}",
                me.version
            )));
        }
        if me.variables.is_empty() {
            return Err(bad("в копии нет ни одной переменной"));
        }
        for v in &me.variables {
            if !is_boot_variable(&v.name) {
                return Err(bad(format!(
                    "{}: это не переменная загрузки, восстанавливать её отсюда нельзя",
                    v.name
                )));
            }
            v.bytes()?; // испорченный base64 лучше найти до записи, а не во время
        }
        Ok(me)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| bad(format!("не записать копию: {e}")))
    }

    /// The boot entries in it, for showing what a restore would put back.
    pub fn entries(&self) -> Vec<(u16, LoadOption)> {
        let mut out = Vec::new();
        for v in &self.variables {
            let Some(n) = entry_number(&v.name) else { continue };
            if let Ok(bytes) = v.bytes() {
                if let Ok(opt) = LoadOption::parse(&bytes) {
                    out.push((n, opt));
                }
            }
        }
        out
    }

    pub fn order(&self) -> Option<Vec<u16>> {
        self.variables
            .iter()
            .find(|v| v.name == "BootOrder")
            .and_then(|v| v.bytes().ok())
            .map(|b| parse_boot_order(&b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a load option the way firmware does, so the parser is tested
    /// against the layout and not against its own idea of it.
    fn build(attrs: u32, name: &str, path: &[u8], optional: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&attrs.to_le_bytes());
        v.extend_from_slice(&(path.len() as u16).to_le_bytes());
        for u in name.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(path);
        v.extend_from_slice(optional);
        v
    }

    fn hard_drive_node(part: u32, guid: [u8; 16]) -> Vec<u8> {
        let mut n = vec![0x04, 0x01, 42, 0x00];
        n.extend_from_slice(&part.to_le_bytes());
        n.extend_from_slice(&2048u64.to_le_bytes()); // start
        n.extend_from_slice(&204800u64.to_le_bytes()); // size
        n.extend_from_slice(&guid);
        n.push(0x02); // GPT
        n.push(0x02); // signature is a GUID
        n
    }

    fn file_path_node(path: &str) -> Vec<u8> {
        let mut body: Vec<u8> = path.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        body.extend_from_slice(&[0, 0]);
        let mut n = vec![0x04, 0x04];
        n.extend_from_slice(&((body.len() + 4) as u16).to_le_bytes());
        n.extend_from_slice(&body);
        n
    }

    fn end_node() -> Vec<u8> {
        vec![0x7F, 0xFF, 0x04, 0x00]
    }

    /// Messaging / USB (type 3, subtype 5): parent port and interface, six
    /// bytes in all. This is what firmware puts in front of anything plugged
    /// into a USB socket.
    fn usb_node(port: u8, interface: u8) -> Vec<u8> {
        vec![0x03, 0x05, 0x06, 0x00, port, interface]
    }

    #[test]
    fn an_entry_that_boots_off_a_stick_is_recognised_by_its_path() {
        // A bootable flash drive looks exactly like a disk from the name and
        // the file down: an ESP, `\EFI\BOOT\BOOTX64.EFI`, any description its
        // maker felt like. The USB node is the one part that gives it away.
        let guid = [0u8; 16];
        let mut path = usb_node(0, 0);
        path.extend(hard_drive_node(1, guid));
        path.extend(file_path_node(r"\EFI\BOOT\BOOTX64.EFI"));
        path.extend(end_node());
        let stick = LoadOption::parse(&build(LOAD_OPTION_ACTIVE, "UEFI: SanDisk", &path, &[]))
            .unwrap();
        assert!(stick.is_removable_device());

        // The machine's own Windows, on an internal disk: no USB node anywhere.
        let mut own = hard_drive_node(2, guid);
        own.extend(file_path_node(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
        own.extend(end_node());
        let windows =
            LoadOption::parse(&build(LOAD_OPTION_ACTIVE, "Windows Boot Manager", &own, &[]))
                .unwrap();
        assert!(!windows.is_removable_device());
    }

    #[test]
    fn the_other_two_usb_node_shapes_count_too() {
        // Firmware writes a class or a WWID instead of a port when it wants the
        // entry to survive being moved to another socket.
        for subtype in [0x0F, 0x10] {
            let mut path = vec![0x03, subtype, 0x06, 0x00, 0, 0];
            path.extend(file_path_node(r"\EFI\BOOT\BOOTX64.EFI"));
            path.extend(end_node());
            let opt = LoadOption::parse(&build(LOAD_OPTION_ACTIVE, "UEFI: Generic", &path, &[]))
                .unwrap();
            assert!(opt.is_removable_device(), "subtype {subtype:#04x}");
        }
    }

    #[test]
    fn a_copy_made_before_the_rule_still_leaves_the_stick_out() {
        // Копия от 2026-09-19 несёт «UEFI: JetFlashTranscend 64GB 1100» — и
        // будет нести всегда, файл уже снят. Отбор на возврате и на показе
        // работает по тем же байтам, что и отбор при съёме.
        let guid = [0u8; 16];
        let mut path = usb_node(0, 0);
        path.extend(hard_drive_node(1, guid));
        path.extend(file_path_node(r"\EFI\BOOT\BOOTX64.EFI"));
        path.extend(end_node());
        let stick =
            SavedVar::new("Boot0010", 7, &build(LOAD_OPTION_ACTIVE, "UEFI: JetFlash", &path, &[]));
        assert!(names_removable_device(&stick));

        let mut own = hard_drive_node(2, guid);
        own.extend(file_path_node(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
        own.extend(end_node());
        let windows =
            SavedVar::new("Boot0000", 7, &build(LOAD_OPTION_ACTIVE, "Windows 11", &own, &[]));
        assert!(!names_removable_device(&windows));

        // Порядок и время ожидания — не записи меню, их это не касается.
        assert!(!names_removable_device(&SavedVar::new("BootOrder", 7, &[0x10, 0x00])));
        assert!(!names_removable_device(&SavedVar::new("Timeout", 7, &[0x05, 0x00])));
    }

    #[test]
    fn a_sata_or_nvme_node_is_not_a_stick() {
        // Type 3 as well — the same "how it is reached" family — so the check
        // must look at the subtype, not just at the node kind.
        const MSG_SATA: u8 = 0x12;
        let mut path = vec![0x03, MSG_SATA, 0x0A, 0x00, 0, 0, 0, 0, 0, 0];
        path.extend(file_path_node(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
        path.extend(end_node());
        let opt =
            LoadOption::parse(&build(LOAD_OPTION_ACTIVE, "Windows Boot Manager", &path, &[]))
                .unwrap();
        assert!(!opt.is_removable_device());
    }

    /// The everyday entry: a name, a partition, a file inside it.
    #[test]
    fn an_entry_says_which_partition_and_which_file() {
        let guid = [
            0xA7, 0x5B, 0xF0, 0xA5, 0xD3, 0xF4, 0x23, 0x4C,
            0xA2, 0x81, 0x87, 0xC7, 0x1B, 0x09, 0x50, 0xFA,
        ];
        let mut path = hard_drive_node(2, guid);
        path.extend(file_path_node(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
        path.extend(end_node());
        let raw = build(LOAD_OPTION_ACTIVE, "Windows Boot Manager", &path, &[]);

        let opt = LoadOption::parse(&raw).unwrap();
        assert_eq!(opt.description, "Windows Boot Manager");
        assert!(opt.is_active());
        assert_eq!(opt.file_path().as_deref(), Some(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
        let hd = opt.hard_drive().unwrap();
        assert_eq!(hd.partition_number, 2);
        assert_eq!(hd.partition_guid().as_deref(), Some("A5F05BA7-F4D3-4C23-A281-87C71B0950FA"));
        assert!(opt.fully_understood());
    }

    /// Renaming moves everything after the name, so the only safe way to do it
    /// is to rebuild the whole variable. What we did not touch must come back
    /// byte for byte.
    #[test]
    fn a_rename_keeps_every_byte_that_is_not_the_name() {
        let mut path = hard_drive_node(1, [7u8; 16]);
        path.extend(file_path_node(r"\EFI\acronis\bootx64.efi"));
        path.extend(end_node());
        let raw = build(LOAD_OPTION_ACTIVE, "Acronis", &path, b"argument bytes");

        let mut opt = LoadOption::parse(&raw).unwrap();
        assert_eq!(opt.to_bytes(), raw, "разбор и сборка обязаны совпадать");

        opt.description = "Acronis True Image — восстановление".into();
        let again = LoadOption::parse(&opt.to_bytes()).unwrap();
        assert_eq!(again.description, "Acronis True Image — восстановление");
        assert_eq!(again.file_path_list, opt.file_path_list);
        assert_eq!(again.optional_data, b"argument bytes");
        assert_eq!(again.attributes, LOAD_OPTION_ACTIVE);
        assert_eq!(again.file_path().as_deref(), Some(r"\EFI\acronis\bootx64.efi"));
    }

    /// An entry the firmware created for a device rather than a file: there is
    /// no path inside a partition to show, and that is not a fault.
    #[test]
    fn an_entry_without_a_file_is_still_an_entry() {
        let mut path = vec![0x03, 0x0B, 0x0C, 0x00]; // Messaging / MAC address
        path.extend_from_slice(&[0xAA; 8]);
        path.extend(end_node());
        let raw = build(LOAD_OPTION_ACTIVE, "UEFI: Network Device", &path, &[]);

        let opt = LoadOption::parse(&raw).unwrap();
        assert_eq!(opt.description, "UEFI: Network Device");
        assert_eq!(opt.file_path(), None);
        assert_eq!(opt.hard_drive(), None);
        assert!(opt.fully_understood(), "непонятый ≠ неизвестного вида");
        assert_eq!(opt.to_bytes(), raw);
    }

    /// A disabled entry keeps its place and its target; only the bit differs.
    #[test]
    fn a_disabled_entry_differs_from_an_enabled_one_by_one_bit() {
        let mut path = hard_drive_node(1, [1u8; 16]);
        path.extend(file_path_node(r"\EFI\ubuntu\shimx64.efi"));
        path.extend(end_node());
        let on = LoadOption::parse(&build(LOAD_OPTION_ACTIVE, "ubuntu", &path, &[])).unwrap();
        let off = LoadOption::parse(&build(0, "ubuntu", &path, &[])).unwrap();
        assert!(on.is_active());
        assert!(!off.is_active());
        assert_eq!(on.file_path(), off.file_path());
    }

    /// Bytes that cannot be a load option are refused, not guessed at. Each of
    /// these once looked like something worth "recovering" from.
    #[test]
    fn nonsense_is_refused_rather_than_repaired() {
        assert!(LoadOption::parse(&[]).is_err(), "пусто");
        assert!(LoadOption::parse(&[1, 2, 3]).is_err(), "короче заголовка");
        // A name with no terminator: its length is unknowable.
        let mut v = vec![1, 0, 0, 0, 0, 0];
        v.extend_from_slice(&"name".encode_utf16().flat_map(|u| u.to_le_bytes()).collect::<Vec<_>>());
        assert!(LoadOption::parse(&v).is_err(), "название без нуля");
        // A device path longer than the bytes that follow it.
        let mut v = vec![1, 0, 0, 0, 200, 0];
        v.extend_from_slice(&[0, 0]);
        assert!(LoadOption::parse(&v).is_err(), "путь длиннее остатка");
    }

    /// A device path that stops making sense mid-way: the nodes already read
    /// are kept, the entry is marked as not ours to edit, and nothing loops.
    #[test]
    fn a_broken_device_path_stops_the_walk_instead_of_looping() {
        // A node claiming length 0 would leave the cursor where it was.
        let mut path = file_path_node(r"\EFI\x.efi");
        path.extend_from_slice(&[0x04, 0x04, 0x00, 0x00]);
        let opt = LoadOption::parse(&build(1, "x", &path, &[])).unwrap();
        let nodes = opt.device_path();
        assert!(matches!(nodes[0], DevicePathNode::FilePath(_)), "первый узел прочитан");
        assert!(matches!(nodes.last().unwrap(), DevicePathNode::Unreadable { .. }));
        assert!(!opt.fully_understood(), "такую запись править нельзя");
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The two entries a real machine really had, byte for byte, read out of its
    /// firmware on 2026-09-18 (`own-backup-cli boot list --raw`).
    ///
    /// Synthetic bytes only prove the parser agrees with whoever wrote it. These
    /// prove it agrees with a firmware, and they carry the details nobody makes
    /// up: Windows trails its own arguments after the device path, and the entry
    /// `efibootmgr` wrote for ubuntu ends in a four-byte marker of its own. Both
    /// have to survive a round trip untouched, because renaming an entry means
    /// rebuilding the whole variable around them.
    #[test]
    fn two_entries_out_of_a_real_firmware_parse_and_come_back_whole() {
        const WINDOWS: &str = "010000007400570069006e0064006f0077007300200042006f006f00740020004d0061006e006100670065007200000004012a000100000000080000000000000020030000000000fc28311d0c794741b0e30767e8b3a4c00202040446005c004500460049005c004d004900430052004f0053004f00460054005c0042004f004f0054005c0042004f004f0054004d004700460057002e0045004600490000007fff040057494e444f5753000100000088000000780000004200430044004f0042004a004500430054003d007b00390064006500610038003600320063002d0035006300640064002d0034006500370030002d0061006300630031002d006600330032006200330034003400640034003700390035007d00000064000100000010000000040000007fff0400";
        const UBUNTU: &str = "0100000062007500620075006e0074007500000004012a00010000002200000000000000deff28000000000010f55e2493a18d45b01fd137387604550202040434005c004500460049005c005500420055004e00540055005c005300480049004d005800360034002e0045004600490000007fff04000000424f";

        let raw = unhex(WINDOWS);
        let w = LoadOption::parse(&raw).unwrap();
        assert_eq!(w.description, "Windows Boot Manager");
        assert!(w.is_active());
        assert_eq!(w.file_path().as_deref(), Some(r"\EFI\MICROSOFT\BOOT\BOOTMGFW.EFI"));
        let hd = w.hard_drive().unwrap();
        assert_eq!(hd.partition_number, 1);
        assert_eq!(hd.start_lba, 2048);
        assert_eq!(hd.size_lba, 204800, "100 МиБ раздела EFI, в секторах");
        assert_eq!(hd.partition_guid().as_deref(), Some("1D3128FC-790C-4147-B0E3-0767E8B3A4C0"));
        assert_eq!(hd.format, 2, "GPT");
        assert!(!w.optional_data.is_empty(), "Windows кладёт туда свой объект BCD");
        assert!(w.fully_understood());
        assert_eq!(w.to_bytes(), raw, "сборка обязана вернуть ровно те же байты");

        let raw = unhex(UBUNTU);
        let u = LoadOption::parse(&raw).unwrap();
        assert_eq!(u.description, "ubuntu");
        assert_eq!(u.file_path().as_deref(), Some(r"\EFI\UBUNTU\SHIMX64.EFI"));
        let hd = u.hard_drive().unwrap();
        assert_eq!(hd.start_lba, 34, "раздел начинается сразу за разметкой");
        assert_eq!(hd.partition_guid().as_deref(), Some("245EF510-A193-458D-B01F-D13738760455"));
        assert_eq!(u.optional_data, vec![0x00, 0x00, 0x42, 0x4F], "хвост от efibootmgr");
        assert_eq!(u.to_bytes(), raw);

        // И то, ради чего всё затевалось: переименовать, не тронув ничего
        // другого.
        let mut renamed = u.clone();
        renamed.description = "Ubuntu 24.04".into();
        let back = LoadOption::parse(&renamed.to_bytes()).unwrap();
        assert_eq!(back.description, "Ubuntu 24.04");
        assert_eq!(back.file_path_list, u.file_path_list, "путь к загрузчику не тронут");
        assert_eq!(back.optional_data, u.optional_data, "чужой хвост не тронут");
        assert_eq!(back.attributes, u.attributes);
    }

    /// A copy has to come back byte for byte, including the bytes we never
    /// looked at.
    #[test]
    fn a_backup_survives_a_round_trip() {
        let order = serialize_boot_order(&[0x000F, 0x0000]);
        let entry = unhex("0100000062007500620075006e0074007500000004012a00010000002200000000000000deff28000000000010f55e2493a18d45b01fd137387604550202040434005c004500460049005c005500420055004e00540055005c005300480049004d005800360034002e0045004600490000007fff04000000424f");
        let saved = BootBackup::new(
            "MODULATOR",
            vec![SavedVar::new("BootOrder", 7, &order), SavedVar::new("Boot000F", 7, &entry)],
            vec![SavedNote {
                name: "Boot000F".into(),
                description: "ubuntu".into(),
                file: Some(r"\EFI\UBUNTU\SHIMX64.EFI".into()),
                partition_guid: Some("245EF510-A193-458D-B01F-D13738760455".into()),
            }],
        );
        let back = BootBackup::load(&saved.to_json().unwrap()).unwrap();
        assert_eq!(back.machine, "MODULATOR");
        assert_eq!(back.order(), Some(vec![0x000F, 0x0000]));
        let entries = back.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 0x000F);
        assert_eq!(entries[0].1.description, "ubuntu");
        assert_eq!(entries[0].1.to_bytes(), entry, "байты записи не изменились");
        assert_eq!(back.variables[1].attributes, 7, "флаги переменной сохранены");
    }

    /// A restore takes the names of the variables it writes from a FILE. If the
    /// file can name anything outside the boot namespace, opening the wrong
    /// file overwrites firmware settings that have nothing to do with booting.
    #[test]
    fn a_copy_may_only_name_variables_that_belong_to_booting() {
        for ok in ["BootOrder", "BootNext", "Timeout", "Boot0000", "BootFFFF"] {
            assert!(is_boot_variable(ok), "{ok}");
        }
        for no in ["PK", "KEK", "db", "SecureBoot", "SetupMode", "BootCurrent", "Driver0000", ""] {
            assert!(!is_boot_variable(no), "{no}");
        }

        let mut bad_one = BootBackup::new("x", vec![SavedVar::new("PK", 7, b"\x01")], vec![]);
        let json = bad_one.to_json().unwrap();
        let err = BootBackup::load(&json).unwrap_err().to_string();
        assert!(err.contains("PK"), "{err}");

        // И то же самое для мусора вместо данных.
        bad_one.variables = vec![SavedVar { name: "Boot0000".into(), attributes: 7, data: "не base64!".into() }];
        let err = BootBackup::load(&bad_one.to_json().unwrap()).unwrap_err().to_string();
        assert!(err.contains("Boot0000"), "{err}");
    }

    /// Anything that is not one of our files is refused rather than half-read.
    #[test]
    fn a_file_that_is_not_a_boot_copy_is_refused() {
        assert!(BootBackup::load("{}").is_err(), "пустой объект");
        assert!(BootBackup::load("не json").is_err());
        let mut b = BootBackup::new("x", vec![SavedVar::new("BootOrder", 7, &[0, 0])], vec![]);
        b.format = "чужой".into();
        assert!(BootBackup::load(&b.to_json().unwrap()).is_err(), "чужой формат");
        let mut b = BootBackup::new("x", vec![SavedVar::new("BootOrder", 7, &[0, 0])], vec![]);
        b.version = BACKUP_VERSION + 1;
        assert!(BootBackup::load(&b.to_json().unwrap()).is_err(), "версия из будущего");
        let b = BootBackup::new("x", vec![], vec![]);
        assert!(BootBackup::load(&b.to_json().unwrap()).is_err(), "нечего восстанавливать");
    }

    /// An entry we build has to be indistinguishable from one the firmware
    /// wrote. The test walks it back through the same parser a real entry goes
    /// through, because "it looked right in the debugger" is not a shape a
    /// firmware will accept.
    #[test]
    fn an_entry_we_build_reads_back_as_the_entry_we_meant() {
        let hd = HardDrive {
            partition_number: 1,
            start_lba: 34,
            size_lba: 2686942,
            signature: [
                0x10, 0xF5, 0x5E, 0x24, 0x93, 0xA1, 0x8D, 0x45,
                0xB0, 0x1F, 0xD1, 0x37, 0x38, 0x76, 0x04, 0x55,
            ],
            format: 2,
            signature_type: 2,
        };
        let built = build_entry("Acronis True Image", &hd, r"\EFI\acronis\bootx64.efi");
        let back = LoadOption::parse(&built.to_bytes()).unwrap();

        assert_eq!(back.description, "Acronis True Image");
        assert!(back.is_active());
        assert_eq!(back.file_path().as_deref(), Some(r"\EFI\acronis\bootx64.efi"));
        assert_eq!(back.hard_drive().unwrap(), hd, "раздел описан ровно тем же");
        assert!(back.fully_understood());
        // Три узла и ничего лишнего: раздел, файл, конец.
        let nodes = back.device_path();
        assert_eq!(nodes.len(), 3, "{nodes:?}");
        assert!(matches!(nodes[2], DevicePathNode::End));
    }

    /// The node lengths are the part a firmware checks and a human never sees.
    /// A hard-drive node is 42 bytes by the spec; a file-path node is its name
    /// plus the terminator plus the header.
    #[test]
    fn the_nodes_we_write_have_the_lengths_the_spec_demands() {
        let hd = HardDrive {
            partition_number: 2,
            start_lba: 2048,
            size_lba: 204800,
            signature: [0x11; 16],
            format: 2,
            signature_type: 2,
        };
        let path = build_device_path(&hd, r"\EFI\x.efi");
        assert_eq!(&path[0..4], &[0x04, 0x01, 42, 0x00], "заголовок узла раздела");
        let file_at = 42;
        assert_eq!(&path[file_at..file_at + 2], &[0x04, 0x04]);
        let file_len = u16::from_le_bytes([path[file_at + 2], path[file_at + 3]]) as usize;
        // "\EFI\x.efi" — 10 знаков, плюс ноль, по два байта, плюс заголовок.
        assert_eq!(file_len, 10 * 2 + 2 + 4);
        assert_eq!(path.len(), 42 + file_len + 4, "и метка конца");
        assert_eq!(&path[path.len() - 4..], &[0x7F, 0xFF, 0x04, 0x00]);
    }

    /// A new entry takes the first free number, not the next one after the
    /// largest: a store edited for years would otherwise walk towards its
    /// ceiling for nothing.
    #[test]
    fn a_new_entry_takes_the_first_gap() {
        assert_eq!(free_entry_number(&[]), Some(0));
        assert_eq!(free_entry_number(&[0, 1, 2]), Some(3));
        assert_eq!(free_entry_number(&[0, 2, 3]), Some(1), "дырка посередине");
        assert_eq!(free_entry_number(&[1, 2]), Some(0));
        // Реальный случай этой машины: заняты 0x0000 и 0x000F.
        assert_eq!(free_entry_number(&[0x0000, 0x000F]), Some(1));
    }

    /// Moving an entry keeps everything else in sequence, and moving one that
    /// is not in the list yet puts it in.
    #[test]
    fn moving_an_entry_does_not_disturb_the_others() {
        let order = vec![0x0000, 0x000F, 0x0003];
        assert_eq!(reorder(&order, 0x0003, 0), vec![0x0003, 0x0000, 0x000F]);
        assert_eq!(reorder(&order, 0x0000, 2), vec![0x000F, 0x0003, 0x0000]);
        assert_eq!(reorder(&order, 0x0000, 99), vec![0x000F, 0x0003, 0x0000], "за конец — в конец");
        assert_eq!(reorder(&order, 0x0007, 1), vec![0x0000, 0x0007, 0x000F, 0x0003], "новой в списке не было");
        assert_eq!(reorder(&order, 0x0000, 0), order, "на своём месте — ничего не изменилось");
    }

    /// The GUID goes into the entry byte-swapped and comes out of it the same
    /// way. Round-tripping against the reader is the only check that catches a
    /// swap done in one direction only — which produces a GUID that reads fine
    /// and matches no partition on the machine.
    #[test]
    fn a_partition_guid_survives_the_trip_into_an_entry_and_back() {
        for text in [
            "245EF510-A193-458D-B01F-D13738760455",
            "{1d3128fc-790c-4147-b0e3-0767e8b3a4c0}",
        ] {
            let hd = HardDrive::from_gpt(1, 34, 2686975, text).unwrap();
            let back = hd.partition_guid().unwrap();
            assert_eq!(back, text.trim_matches(|c| c == '{' || c == '}').to_uppercase(), "{text}");
            assert_eq!(hd.size_lba, 2686975 - 34 + 1, "размер считается по последнему сектору");
            assert_eq!(hd.format, 2);
        }
        // Проверено против настоящей записи: байты из прошивки этой машины.
        let hd = HardDrive::from_gpt(1, 34, 2686975, "245EF510-A193-458D-B01F-D13738760455").unwrap();
        assert_eq!(
            hd.signature,
            [0x10, 0xF5, 0x5E, 0x24, 0x93, 0xA1, 0x8D, 0x45,
             0xB0, 0x1F, 0xD1, 0x37, 0x38, 0x76, 0x04, 0x55],
            "ровно те байты, что лежат в записи ubuntu"
        );
        assert_eq!(guid_to_bytes("не guid"), None);
        assert_eq!(guid_to_bytes(""), None);
        assert_eq!(guid_to_bytes("245EF510-A193-458D-B01F-D137387604"), None, "коротко");
    }

    /// The spelling difference this machine really has: the firmware holds the
    /// path in upper case, the tool that installed the loader wrote it in
    /// lower. Same file.
    #[test]
    fn the_same_loader_written_two_ways_is_the_same_loader() {
        assert!(same_efi_path(r"\EFI\UBUNTU\SHIMX64.EFI", r"\EFI\ubuntu\shimx64.efi"));
        assert!(same_efi_path(r"EFI\acronis\bootx64.efi", r"\EFI\acronis\bootx64.efi"));
        assert!(same_efi_path("/EFI/acronis/bootx64.efi", r"\EFI\acronis\bootx64.efi"));
        assert!(!same_efi_path(r"\EFI\acronis\bootx64.efi", r"\EFI\r-drive\bootx64.efi"));
        assert!(!same_efi_path(r"\EFI\ubuntu\shimx64.efi", r"\EFI\ubuntu\grubx64.efi"));
    }

    /// The fallback loader is not an orphan: nothing names it because every
    /// firmware looks for it by itself.
    #[test]
    fn the_removable_media_fallback_is_not_a_loader_nobody_points_at() {
        assert!(is_fallback_path(r"\EFI\BOOT\BOOTX64.EFI"));
        assert!(is_fallback_path(r"EFI\boot\bootx64.efi"));
        assert!(is_fallback_path(r"\EFI\Boot\bootia32.efi"));
        assert!(!is_fallback_path(r"\EFI\ubuntu\shimx64.efi"));
        assert!(!is_fallback_path(r"\EFI\BOOT\grubx64.efi"), "в той же папке, но не тот файл");
        assert!(!is_fallback_path(r"\EFI\Microsoft\Boot\bootmgfw.efi"));
    }

    #[test]
    fn entry_numbers_are_four_hex_digits_and_nothing_else() {
        assert_eq!(entry_number("Boot0000"), Some(0));
        assert_eq!(entry_number("Boot001F"), Some(0x1F));
        assert_eq!(entry_number("BootFFFF"), Some(0xFFFF));
        assert_eq!(entry_number("BootOrder"), None, "порядок — не запись");
        assert_eq!(entry_number("BootCurrent"), None);
        assert_eq!(entry_number("Boot001"), None, "три цифры — не наш случай");
        assert_eq!(entry_number("Driver0001"), None);
        assert_eq!(entry_name(0x1F), "Boot001F");
        assert_eq!(entry_name(0), "Boot0000");
    }

    #[test]
    fn the_boot_order_is_a_plain_list_of_numbers() {
        let bytes = [0x03, 0x00, 0x00, 0x00, 0x1F, 0x00];
        assert_eq!(parse_boot_order(&bytes), vec![3, 0, 0x1F]);
        assert_eq!(serialize_boot_order(&[3, 0, 0x1F]), bytes);
        assert_eq!(parse_boot_order(&[]), Vec::<u16>::new());
        // An odd trailing byte is not half an entry; it is ignored.
        assert_eq!(parse_boot_order(&[1, 0, 9]), vec![1]);
    }
}
