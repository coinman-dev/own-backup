//! The Windows boot configuration store (BCD), read and edited offline — for
//! restoring Windows from Linux (PLAN §6, stage 3).
//!
//! A BCD store is a registry hive ([`crate::regf`]) at `\EFI\Microsoft\Boot\BCD`
//! on the ESP (UEFI) or `\Boot\BCD` on the active partition (BIOS):
//!
//! ```text
//! <root>\Objects\{object GUID}\Description      Type = REG_DWORD object type
//! <root>\Objects\{object GUID}\Elements\<type>  Element = the element's data
//! ```
//!
//! An element's type is a 32-bit number written as eight hex digits; bits
//! 24–27 say how its data is encoded. Format 1 is a *device*, and that is the
//! part a restore cares about: the boot manager's `device`, a loader's
//! `device` and `osdevice`, a resume object's `device` name partitions by the
//! identity of their disk and the partition itself:
//!
//! ```text
//! 0x00  GUID  additional options (usually zero)
//! 0x10  u32   device type: 6 = partition (5 = boot, 8 = locate, …)
//! 0x14  u32   flags
//! 0x18  u32   size of what follows 0x10 (0x48)
//! 0x1C  u32   reserved
//! 0x20  16 B  partition: GPT unique GUID | MBR byte offset (u64) + 8 zero
//! 0x30  u32   local device type (0 = local disk)
//! 0x34  u32   partition style: 0 = GPT, 1 = MBR
//! 0x38  16 B  disk: GPT disk GUID | MBR signature (u32) + 12 zero
//! 0x48  16 B  zero
//! ```
//!
//! (The layout matches the store Windows writes and ReactOS' boot library
//! headers; acceptance on a real Windows is listed in the report.)
//!
//! When a restore keeps the partition and disk GUIDs, the store is left alone
//! — that is the normal case and the safest. Only when they change (a new GPT
//! disk GUID, MBR signature, a moved partition) do the device elements need
//! [`BcdStore::retarget`]; everything else in the store comes through byte for
//! byte.

use uuid::Uuid;

use own_backup_common::{tr, OwnBackupError, Result};

use crate::regf::{Hive, Value, REG_BINARY};

/// `{bootmgr}`.
pub const BOOTMGR: &str = "{9dea862c-5cdd-4e70-acc1-f32b344d4795}";

/// Object types (`Description\Type`).
pub const OBJECT_BOOT_MANAGER: u32 = 0x1010_0002;
pub const OBJECT_OS_LOADER: u32 = 0x1020_0003;
pub const OBJECT_RESUME: u32 = 0x1020_0004;

/// Elements a restore looks at.
pub const ELEMENT_APPLICATION_DEVICE: u32 = 0x1100_0001;
pub const ELEMENT_APPLICATION_PATH: u32 = 0x1200_0002;
pub const ELEMENT_DESCRIPTION: u32 = 0x1200_0004;
pub const ELEMENT_OS_DEVICE: u32 = 0x2100_0001;
pub const ELEMENT_DEFAULT_OBJECT: u32 = 0x2300_0003;

const DEVICE_LEN: usize = 0x58;
const DEVICE_TYPE_PARTITION: u32 = 6;
const STYLE_GPT: u32 = 0;
const STYLE_MBR: u32 = 1;

fn bad(reason: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::ImageFormat(tr!("bcd.bad_store", reason = reason).into_owned())
}

/// Which partition a device element names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartitionRef {
    Gpt { disk: Uuid, partition: Uuid },
    Mbr { signature: u32, offset: u64 },
}

impl std::fmt::Display for PartitionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PartitionRef::Gpt { disk, partition } => {
                write!(f, "GPT disk {{{disk}}} partition {{{partition}}}")
            }
            PartitionRef::Mbr { signature, offset } => {
                write!(f, "MBR disk {signature:08X} partition at {offset}")
            }
        }
    }
}

/// The partition a device element's bytes name, if it is a partition device
/// we understand. Anything else — boot, locate, ramdisk, VHD devices — is
/// `None` and is never rewritten.
pub fn device_partition(data: &[u8]) -> Option<PartitionRef> {
    if data.len() < DEVICE_LEN {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    if u32_at(0x10) != DEVICE_TYPE_PARTITION || u32_at(0x30) != 0 {
        return None;
    }
    let guid_at = |o: usize| Uuid::from_bytes_le(data[o..o + 16].try_into().unwrap());
    match u32_at(0x34) {
        STYLE_GPT => Some(PartitionRef::Gpt {
            disk: guid_at(0x38),
            partition: guid_at(0x20),
        }),
        STYLE_MBR => Some(PartitionRef::Mbr {
            signature: u32_at(0x38),
            offset: u64::from_le_bytes(data[0x20..0x28].try_into().unwrap()),
        }),
        _ => None,
    }
}

/// Write `to` into a partition device element, keeping every other byte
/// (additional options, flags, whatever follows).
fn set_device_partition(data: &mut [u8], to: &PartitionRef) {
    data[0x20..0x30].fill(0);
    data[0x38..0x48].fill(0);
    match to {
        PartitionRef::Gpt { disk, partition } => {
            data[0x20..0x30].copy_from_slice(&partition.to_bytes_le());
            data[0x34..0x38].copy_from_slice(&STYLE_GPT.to_le_bytes());
            data[0x38..0x48].copy_from_slice(&disk.to_bytes_le());
        }
        PartitionRef::Mbr { signature, offset } => {
            data[0x20..0x28].copy_from_slice(&offset.to_le_bytes());
            data[0x34..0x38].copy_from_slice(&STYLE_MBR.to_le_bytes());
            data[0x38..0x3C].copy_from_slice(&signature.to_le_bytes());
        }
    }
}

/// A fresh partition device element naming `p`.
pub fn partition_device(p: &PartitionRef) -> Vec<u8> {
    let mut d = vec![0u8; DEVICE_LEN];
    d[0x10..0x14].copy_from_slice(&DEVICE_TYPE_PARTITION.to_le_bytes());
    d[0x18..0x1C].copy_from_slice(&((DEVICE_LEN - 0x10) as u32).to_le_bytes());
    set_device_partition(&mut d, p);
    d
}

/// One device element of one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceElement {
    pub element: u32,
    /// `None`: not a partition device (boot, locate, …).
    pub partition: Option<PartitionRef>,
}

/// One object, as far as a restore needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcdObject {
    pub id: String,
    pub kind: Option<u32>,
    pub description: Option<String>,
    pub path: Option<String>,
    pub devices: Vec<DeviceElement>,
}

/// One element [`BcdStore::retarget`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retargeted {
    pub object: String,
    pub element: u32,
}

fn element_type(name: &str) -> Option<u32> {
    (name.len() == 8)
        .then(|| u32::from_str_radix(name, 16).ok())
        .flatten()
}

fn is_device_element(t: u32) -> bool {
    (t >> 24) & 0xF == 1
}

/// A BCD store, parsed.
#[derive(Debug, Clone)]
pub struct BcdStore {
    hive: Hive,
}

impl BcdStore {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let hive = Hive::parse(bytes)?;
        if hive.root.subkey("Objects").is_none() {
            return Err(bad("no Objects key"));
        }
        Ok(Self { hive })
    }

    pub fn from_hive(hive: Hive) -> Result<Self> {
        if hive.root.subkey("Objects").is_none() {
            return Err(bad("no Objects key"));
        }
        Ok(Self { hive })
    }

    pub fn hive(&self) -> &Hive {
        &self.hive
    }

    pub fn objects(&self) -> Vec<BcdObject> {
        let Some(objects) = self.hive.root.subkey("Objects") else {
            return Vec::new();
        };
        objects
            .subkeys
            .iter()
            .map(|o| {
                let kind = o
                    .subkey("Description")
                    .and_then(|d| d.value("Type"))
                    .and_then(Value::as_dword);
                let elements = o.subkey("Elements");
                let string = |t: u32| {
                    elements
                        .and_then(|e| e.subkey(&format!("{t:08x}")))
                        .and_then(|k| k.value("Element"))
                        .and_then(Value::as_string)
                };
                let devices = elements
                    .map(|e| {
                        e.subkeys
                            .iter()
                            .filter_map(|k| {
                                let t = element_type(&k.name)?;
                                if !is_device_element(t) {
                                    return None;
                                }
                                let data = &k.value("Element")?.data;
                                Some(DeviceElement {
                                    element: t,
                                    partition: device_partition(data),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                BcdObject {
                    id: o.name.clone(),
                    kind,
                    description: string(ELEMENT_DESCRIPTION),
                    path: string(ELEMENT_APPLICATION_PATH),
                    devices,
                }
            })
            .collect()
    }

    /// Point every partition device element naming `from` at `to` instead.
    /// Returns what was changed; nothing else in the store is touched.
    pub fn retarget(&mut self, from: &PartitionRef, to: &PartitionRef) -> Vec<Retargeted> {
        let mut changed = Vec::new();
        let Some(objects) = self.hive.root.subkey_mut("Objects") else {
            return changed;
        };
        for o in &mut objects.subkeys {
            let id = o.name.clone();
            let Some(elements) = o.subkey_mut("Elements") else {
                continue;
            };
            for k in &mut elements.subkeys {
                let Some(t) = element_type(&k.name).filter(|t| is_device_element(*t)) else {
                    continue;
                };
                let Some(v) = k
                    .values
                    .iter_mut()
                    .find(|v| v.name.eq_ignore_ascii_case("Element"))
                else {
                    continue;
                };
                if v.kind != REG_BINARY || device_partition(&v.data).as_ref() != Some(from) {
                    continue;
                }
                set_device_partition(&mut v.data, to);
                changed.push(Retargeted {
                    object: id.clone(),
                    element: t,
                });
            }
        }
        changed
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.hive.to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regf::{Key, REG_MULTI_SZ};

    const ESP: &str = "c0ffee00-1111-2222-3333-444444444444";
    const WIN: &str = "c0ffee00-5555-6666-7777-888888888888";
    const DISK: &str = "d15c0000-aaaa-bbbb-cccc-dddddddddddd";
    const LOADER: &str = "{3e3d4c2b-1a09-11ef-8b5a-806e6f6e6963}";

    fn gpt(part: &str) -> PartitionRef {
        PartitionRef::Gpt {
            disk: Uuid::parse_str(DISK).unwrap(),
            partition: Uuid::parse_str(part).unwrap(),
        }
    }

    fn sd() -> Vec<u8> {
        let mut d = vec![1, 0, 0x04, 0x80];
        d.extend_from_slice(&20u32.to_le_bytes());
        d.extend_from_slice(&32u32.to_le_bytes());
        d.extend_from_slice(&[0; 8]);
        let system = [1u8, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        d.extend_from_slice(&system);
        d.extend_from_slice(&system);
        d
    }

    fn element(o: &mut Key, t: u32, v: Value) {
        o.add_subkey("Elements")
            .add_subkey(&format!("{t:08x}"))
            .set_value(v);
    }

    fn binary(data: Vec<u8>) -> Value {
        Value {
            name: "Element".into(),
            kind: REG_BINARY,
            data,
        }
    }

    fn text(s: &str) -> Value {
        Value::string("Element", s)
    }

    /// A store shaped like the one bcdboot writes on a UEFI machine.
    fn store() -> BcdStore {
        let mut h = Hive::new("NewStoreRoot", sd());
        h.root
            .add_subkey("Description")
            .set_value(Value::string("KeyName", "BCD00000000"));
        let objects = h.root.add_subkey("Objects");

        let bm = objects.add_subkey(BOOTMGR);
        bm.add_subkey("Description")
            .set_value(Value::dword("Type", OBJECT_BOOT_MANAGER));
        element(
            bm,
            ELEMENT_APPLICATION_DEVICE,
            binary(partition_device(&gpt(ESP))),
        );
        element(
            bm,
            ELEMENT_APPLICATION_PATH,
            text(r"\EFI\Microsoft\Boot\bootmgfw.efi"),
        );
        element(bm, ELEMENT_DESCRIPTION, text("Windows Boot Manager"));
        let mut list: Vec<u8> = LOADER.encode_utf16().flat_map(u16::to_le_bytes).collect();
        list.extend([0, 0, 0, 0]);
        element(
            bm,
            0x2400_0001,
            Value {
                name: "Element".into(),
                kind: REG_MULTI_SZ,
                data: list,
            },
        );

        let l = objects.add_subkey(LOADER);
        l.add_subkey("Description")
            .set_value(Value::dword("Type", OBJECT_OS_LOADER));
        element(
            l,
            ELEMENT_APPLICATION_DEVICE,
            binary(partition_device(&gpt(WIN))),
        );
        element(l, ELEMENT_OS_DEVICE, binary(partition_device(&gpt(WIN))));
        element(
            l,
            ELEMENT_APPLICATION_PATH,
            text(r"\Windows\system32\winload.efi"),
        );
        element(l, ELEMENT_DESCRIPTION, text("Windows 11"));

        // A device that is not a partition ("boot"): must never be rewritten.
        let mut boot = vec![0u8; DEVICE_LEN];
        boot[0x10] = 5;
        let r = objects.add_subkey("{0ce4991b-e6b3-4b16-b23c-5e0d9250e5d9}");
        element(r, ELEMENT_APPLICATION_DEVICE, binary(boot));
        BcdStore::from_hive(h).unwrap()
    }

    #[test]
    fn objects_show_their_devices_and_names() {
        let s = BcdStore::parse(&store().to_bytes().unwrap()).unwrap();
        let objects = s.objects();
        let bm = objects.iter().find(|o| o.id == BOOTMGR).unwrap();
        assert_eq!(bm.kind, Some(OBJECT_BOOT_MANAGER));
        assert_eq!(bm.description.as_deref(), Some("Windows Boot Manager"));
        assert_eq!(
            bm.devices,
            vec![DeviceElement {
                element: ELEMENT_APPLICATION_DEVICE,
                partition: Some(gpt(ESP))
            }]
        );
        let l = objects.iter().find(|o| o.id == LOADER).unwrap();
        assert_eq!(l.path.as_deref(), Some(r"\Windows\system32\winload.efi"));
        assert_eq!(l.devices.len(), 2);
        let other = objects.iter().find(|o| o.id.starts_with("{0ce4")).unwrap();
        assert_eq!(other.devices[0].partition, None);
    }

    #[test]
    fn retargeting_changes_exactly_the_matching_devices() {
        let mut s = store();
        let before = s.to_bytes().unwrap();
        let new_disk = Uuid::parse_str("11111111-0000-0000-0000-000000000000").unwrap();
        let new_win = PartitionRef::Gpt {
            disk: new_disk,
            partition: Uuid::parse_str(WIN).unwrap(),
        };
        let changed = s.retarget(&gpt(WIN), &new_win);
        assert_eq!(
            changed,
            vec![
                Retargeted {
                    object: LOADER.into(),
                    element: ELEMENT_APPLICATION_DEVICE
                },
                Retargeted {
                    object: LOADER.into(),
                    element: ELEMENT_OS_DEVICE
                },
            ]
        );
        let back = BcdStore::parse(&s.to_bytes().unwrap()).unwrap();
        let objects = back.objects();
        let l = objects.iter().find(|o| o.id == LOADER).unwrap();
        assert!(l.devices.iter().all(|d| d.partition == Some(new_win)));
        let bm = objects.iter().find(|o| o.id == BOOTMGR).unwrap();
        assert_eq!(
            bm.devices[0].partition,
            Some(gpt(ESP)),
            "the ESP was not asked for"
        );
        // Retargeting to the same identity is a no-op on the bytes.
        let mut same = store();
        same.retarget(&gpt(ESP), &gpt(ESP));
        assert_eq!(same.to_bytes().unwrap()[12..508], before[12..508]);
        assert_eq!(same.to_bytes().unwrap()[512..], before[512..]);
    }

    #[test]
    fn a_device_element_keeps_its_other_bytes() {
        let mut d = partition_device(&gpt(WIN));
        d[0..16].copy_from_slice(&[0xAB; 16]); // additional options
        d[0x14] = 0x02; // flags
        let mut e = d.clone();
        let to = PartitionRef::Mbr {
            signature: 0x1234_5678,
            offset: 1 << 20,
        };
        set_device_partition(&mut e, &to);
        assert_eq!(device_partition(&e), Some(to));
        assert_eq!(e[..0x20], d[..0x20]);
        assert_eq!(e[0x48..], d[0x48..]);
    }

    #[test]
    fn an_mbr_device_as_windows_writes_it_is_understood() {
        // A loader's device on a BIOS machine: partition at 101 MiB of the
        // disk with signature E9C14BB4, in the shape of published dumps of
        // Windows-written stores. To be confirmed against a store bcdboot
        // writes on the Windows stand (see the report).
        let hex = "00000000000000000000000000000000\
                   06000000000000004800000000000000\
                   00005006000000000000000000000000\
                   0000000001000000b44bc1e900000000\
                   00000000000000000000000000000000\
                   0000000000000000";
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes.len(), DEVICE_LEN);
        let p = PartitionRef::Mbr {
            signature: 0xE9C1_4BB4,
            offset: 0x0650_0000,
        };
        assert_eq!(device_partition(&bytes), Some(p));
        assert_eq!(partition_device(&p), bytes, "we write what Windows writes");
    }

    #[test]
    fn short_or_foreign_device_data_is_not_a_partition() {
        assert_eq!(device_partition(&[0u8; 20]), None);
        let mut d = partition_device(&gpt(ESP));
        d[0x34] = 7;
        assert_eq!(device_partition(&d), None);
        let mut d = partition_device(&gpt(ESP));
        d[0x30] = 3; // ramdisk, not a local disk
        assert_eq!(device_partition(&d), None);
    }

    #[test]
    fn a_hive_without_objects_is_not_a_store() {
        let h = Hive::new("R", sd());
        assert!(BcdStore::parse(&h.to_bytes().unwrap()).is_err());
    }
}
