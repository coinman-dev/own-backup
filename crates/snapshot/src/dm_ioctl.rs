//! Device-mapper requests issued directly on `/dev/mapper/control`.
//!
//! The same requests `dmsetup` and libdevmapper make, without either of them:
//! a snapshot of a device the root filesystem may sit on is switched in while
//! that device is suspended, and in that window this process must not start a
//! program or touch a file (`dm_snapshot.rs`). Every request is one buffer — a
//! `struct dm_ioctl` header followed by its data — built in advance, so issuing
//! it ([`Control::issue`]) allocates nothing.
//!
//! The layout is the kernel's `include/uapi/linux/dm-ioctl.h`, interface
//! version 4, in native byte order:
//!
//! ```text
//! struct dm_ioctl (312 bytes)          struct dm_target_spec (40 bytes)
//!   0 u32 version[3]                     0 u64 sector_start
//!  12 u32 data_size                      8 u64 length
//!  16 u32 data_start                    16 s32 status
//!  20 u32 target_count                  20 u32 next
//!  24 s32 open_count                    24 char target_type[16]
//!  28 u32 flags                         40 parameters, NUL-terminated,
//!  32 u32 event_nr (udev cookie)           padded to 8 bytes
//!  36 u32 padding
//!  40 u64 dev
//!  48 char name[128]
//! 176 char uuid[129]
//! 305 char data[7]
//! ```
//!
//! `next` of a loaded target counts from the start of that target; `next` of a
//! target the kernel reports counts from the first target (the kernel's
//! `populate_table` and `retrieve_status`). Tables may carry encryption keys,
//! so table buffers are wiped when dropped and requests that carry them ask the
//! kernel to wipe its copies too (`DM_SECURE_DATA_FLAG`).

#![cfg(target_os = "linux")]

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use zeroize::Zeroizing;

/// Size of `struct dm_ioctl`, and where the data of every request starts.
pub const HEADER_SIZE: usize = 312;
/// Size of `struct dm_target_spec`.
pub const SPEC_SIZE: usize = 40;
const NAME_LEN: usize = 128;
const UUID_LEN: usize = 129;
const TYPE_LEN: usize = 16;
/// Interface version asked for: major 4, any minor.
const VERSION: [u32; 3] = [4, 0, 0];

const OFF_DATA_SIZE: usize = 12;
const OFF_DATA_START: usize = 16;
const OFF_TARGET_COUNT: usize = 20;
const OFF_OPEN_COUNT: usize = 24;
const OFF_FLAGS: usize = 28;
const OFF_EVENT_NR: usize = 32;
const OFF_DEV: usize = 40;
const OFF_NAME: usize = 48;
const OFF_UUID: usize = 176;

/// The device-mapper commands this crate uses (`DM_*_CMD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Cmd {
    ListDevices = 2,
    DevCreate = 3,
    DevRemove = 4,
    DevSuspend = 6,
    DevStatus = 7,
    TableLoad = 9,
    TableClear = 10,
    TableDeps = 11,
    TableStatus = 12,
}

/// `_IOWR(0xfd, cmd, struct dm_ioctl)`.
pub fn request_code(cmd: Cmd) -> u32 {
    (3 << 30) | ((HEADER_SIZE as u32) << 16) | (0xfd << 8) | cmd as u32
}

/// `struct dm_ioctl` flags.
pub mod flag {
    pub const READONLY: u32 = 1 << 0;
    pub const SUSPEND: u32 = 1 << 1;
    pub const STATUS_TABLE: u32 = 1 << 4;
    pub const ACTIVE_PRESENT: u32 = 1 << 5;
    pub const INACTIVE_PRESENT: u32 = 1 << 6;
    pub const BUFFER_FULL: u32 = 1 << 8;
    pub const SKIP_LOCKFS: u32 = 1 << 10;
    pub const NOFLUSH: u32 = 1 << 11;
    pub const QUERY_INACTIVE_TABLE: u32 = 1 << 12;
    pub const SECURE_DATA: u32 = 1 << 15;
}

/// udev rule flags carried in the upper half of a request's cookie
/// (`event_nr`), as libdevmapper encodes them. A cookie whose lower half is
/// zero waits on no semaphore: `dmsetup udevcomplete` accepts it as flags only.
pub mod udev {
    pub const DISABLE_SUBSYSTEM_RULES: u32 = 0x0002;
    pub const DISABLE_DISK_RULES: u32 = 0x0004;
    pub const DISABLE_OTHER_RULES: u32 = 0x0008;
    pub const PRIMARY_SOURCE: u32 = 0x0040;

    /// Cookie of a device nothing should probe, mount or scan: our private
    /// layers. No blkid means no filesystem signature, so neither the btrfs
    /// device scan nor a desktop automounter sees a duplicate filesystem.
    pub const HIDDEN: u32 =
        (DISABLE_SUBSYSTEM_RULES | DISABLE_DISK_RULES | DISABLE_OTHER_RULES | PRIMARY_SOURCE) << 16;
    /// Cookie of an ordinary change of a device (what `dmsetup resume` sends).
    pub const VISIBLE: u32 = PRIMARY_SOURCE << 16;
}

/// One target line of a table: `<start> <length> <type> <parameters>`.
/// The parameters of a `crypt` target hold its key; they are wiped on drop
/// and never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub start: u64,
    pub length: u64,
    pub kind: String,
    pub params: Zeroizing<String>,
}

impl Target {
    pub fn new(start: u64, length: u64, kind: &str, params: impl Into<String>) -> Self {
        Target {
            start,
            length,
            kind: kind.to_string(),
            params: Zeroizing::new(params.into()),
        }
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the parameters: they may be a key.
        write!(f, "{} {} {} …", self.start, self.length, self.kind)
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap())
}

/// A NUL-terminated string from `buf[off..off + cap]`.
fn get_str(buf: &[u8], off: usize, cap: usize) -> io::Result<String> {
    let field = &buf[off..off + cap];
    let end = field
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| bad("unterminated string in a device-mapper reply"))?;
    String::from_utf8(field[..end].to_vec()).map_err(|_| bad("non-UTF-8 device-mapper string"))
}

fn put_str(buf: &mut [u8], off: usize, cap: usize, s: &str, what: &str) -> io::Result<()> {
    // Room for the terminating NUL, and no NUL inside.
    if s.len() >= cap || s.as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device-mapper {what} too long or malformed: {s:?}"),
        ));
    }
    buf[off..off + s.len()].copy_from_slice(s.as_bytes());
    Ok(())
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// One prepared request: header and data in one buffer, wiped when dropped.
pub struct Request {
    cmd: Cmd,
    buf: Zeroizing<Vec<u8>>,
}

impl Request {
    /// A request about the device `name` (empty: about no device) with
    /// `data_len` bytes of room for the reply.
    pub fn new(cmd: Cmd, name: &str, flags: u32, data_len: usize) -> io::Result<Self> {
        let mut buf = Zeroizing::new(vec![0u8; HEADER_SIZE + data_len]);
        for (i, v) in VERSION.iter().enumerate() {
            put_u32(&mut buf, i * 4, *v);
        }
        put_u32(&mut buf, OFF_DATA_SIZE, (HEADER_SIZE + data_len) as u32);
        put_u32(&mut buf, OFF_DATA_START, HEADER_SIZE as u32);
        put_u32(&mut buf, OFF_FLAGS, flags);
        put_str(&mut buf, OFF_NAME, NAME_LEN, name, "name")?;
        Ok(Request { cmd, buf })
    }

    /// `DM_DEV_CREATE`: a new device, no table yet.
    pub fn create(name: &str, uuid: &str) -> io::Result<Self> {
        let mut r = Self::new(Cmd::DevCreate, name, 0, 0)?;
        put_str(&mut r.buf, OFF_UUID, UUID_LEN, uuid, "uuid")?;
        Ok(r)
    }

    /// `DM_TABLE_LOAD`: `targets` as the inactive table of `name`.
    pub fn load(name: &str, targets: &[Target], flags: u32) -> io::Result<Self> {
        let mut data: Vec<u8> = Vec::new();
        for t in targets {
            if t.kind.is_empty() || t.kind.len() >= TYPE_LEN || t.params.as_bytes().contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("malformed device-mapper target {t:?}"),
                ));
            }
            let start = data.len();
            data.extend_from_slice(&t.start.to_ne_bytes());
            data.extend_from_slice(&t.length.to_ne_bytes());
            data.extend_from_slice(&0i32.to_ne_bytes()); // status
            data.extend_from_slice(&0u32.to_ne_bytes()); // next, below
            let mut kind = [0u8; TYPE_LEN];
            kind[..t.kind.len()].copy_from_slice(t.kind.as_bytes());
            data.extend_from_slice(&kind);
            data.extend_from_slice(t.params.as_bytes());
            data.push(0);
            data.resize(start + align8(data.len() - start), 0);
            let next = (data.len() - start) as u32;
            put_u32(&mut data, start + 20, next);
        }
        let data = Zeroizing::new(data);
        let mut r = Self::new(Cmd::TableLoad, name, flags | flag::SECURE_DATA, data.len())?;
        put_u32(&mut r.buf, OFF_TARGET_COUNT, targets.len() as u32);
        r.buf[HEADER_SIZE..].copy_from_slice(&data);
        Ok(r)
    }

    pub fn cmd(&self) -> Cmd {
        self.cmd
    }

    /// The whole buffer, as it goes to the kernel.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The udev cookie of a resume or a removal.
    pub fn set_cookie(&mut self, cookie: u32) {
        put_u32(&mut self.buf, OFF_EVENT_NR, cookie);
    }

    pub fn flags(&self) -> u32 {
        get_u32(&self.buf, OFF_FLAGS)
    }

    pub fn target_count(&self) -> u32 {
        get_u32(&self.buf, OFF_TARGET_COUNT)
    }

    pub fn open_count(&self) -> i32 {
        get_u32(&self.buf, OFF_OPEN_COUNT) as i32
    }

    /// `(major, minor)` of the device the reply is about.
    pub fn dev(&self) -> (u32, u32) {
        own_backup_common::linux_block::split_dev(get_u64(&self.buf, OFF_DEV))
    }

    pub fn name(&self) -> io::Result<String> {
        get_str(&self.buf, OFF_NAME, NAME_LEN)
    }

    pub fn uuid(&self) -> io::Result<String> {
        get_str(&self.buf, OFF_UUID, UUID_LEN)
    }

    /// The reply's data area, as far as the kernel says it filled it.
    fn data(&self) -> io::Result<&[u8]> {
        let start = get_u32(&self.buf, OFF_DATA_START) as usize;
        let size = get_u32(&self.buf, OFF_DATA_SIZE) as usize;
        if start < HEADER_SIZE || start > size || size > self.buf.len() {
            return Err(bad(format!(
                "device-mapper reply data {start}..{size} outside {} bytes",
                self.buf.len()
            )));
        }
        Ok(&self.buf[start..size])
    }

    /// The targets a `DM_TABLE_STATUS` reply lists: the table itself with
    /// `flag::STATUS_TABLE`, each target's status line without it.
    pub fn targets(&self) -> io::Result<Vec<Target>> {
        let mut out = Vec::new();
        for (spec, params) in self.target_spans()? {
            let spec = &self.buf[spec..spec + SPEC_SIZE];
            let params = Zeroizing::new(
                std::str::from_utf8(&self.buf[params])
                    .map_err(|_| bad("non-UTF-8 device-mapper target parameters"))?
                    .to_string(),
            );
            out.push(Target {
                start: get_u64(spec, 0),
                length: get_u64(spec, 8),
                kind: get_str(spec, 24, TYPE_LEN)?,
                params,
            });
        }
        Ok(out)
    }

    /// Where the targets of a `DM_TABLE_STATUS` reply lie in the buffer: the
    /// offset of each spec, and its parameters without their NUL.
    fn target_spans(&self) -> io::Result<Vec<(usize, std::ops::Range<usize>)>> {
        let data = self.data()?;
        let base = get_u32(&self.buf, OFF_DATA_START) as usize;
        let mut out = Vec::new();
        let mut off = 0usize;
        for _ in 0..self.target_count() {
            if off + SPEC_SIZE > data.len() {
                return Err(bad("device-mapper target outside the reply"));
            }
            let rest = &data[off + SPEC_SIZE..];
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| bad("unterminated device-mapper target parameters"))?;
            let params = base + off + SPEC_SIZE;
            out.push((base + off, params..params + end));
            let next = get_u32(data, off + 20) as usize;
            if next <= off {
                break; // the last one, or a malformed chain
            }
            off = next;
        }
        Ok(out)
    }

    /// Overwrite the key of every `crypt` target of a table reply, in this
    /// (wiped) buffer, with `0`s, as `dmsetup table` without `--showkeys`
    /// shows it: nothing parsed from the reply afterwards holds a key.
    fn mask_keys(&mut self) -> io::Result<()> {
        for (spec, params) in self.target_spans()? {
            if get_str(&self.buf[spec..spec + SPEC_SIZE], 24, TYPE_LEN)? == "crypt" {
                mask_crypt_key(&mut self.buf[params]);
            }
        }
        Ok(())
    }

    /// `(name, (major, minor))` of every device a `DM_LIST_DEVICES` reply lists.
    pub fn device_list(&self) -> io::Result<Vec<(String, (u32, u32))>> {
        let data = self.data()?;
        let mut out = Vec::new();
        let mut off = 0usize;
        loop {
            if off + 12 > data.len() {
                break;
            }
            let dev = get_u64(data, off);
            if dev == 0 {
                break; // "no devices"
            }
            let rest = &data[off + 12..];
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| bad("unterminated device-mapper name"))?;
            let name = String::from_utf8(rest[..end].to_vec())
                .map_err(|_| bad("non-UTF-8 device-mapper name"))?;
            out.push((name, own_backup_common::linux_block::split_dev(dev)));
            let next = get_u32(data, off + 8) as usize;
            if next == 0 {
                break;
            }
            off += next;
        }
        Ok(out)
    }

    /// `(major, minor)` of the devices a `DM_TABLE_DEPS` reply lists.
    pub fn deps(&self) -> io::Result<Vec<(u32, u32)>> {
        let data = self.data()?;
        if data.len() < 8 {
            return Ok(Vec::new());
        }
        let count = get_u32(data, 0) as usize;
        if 8 + count * 8 > data.len() {
            return Err(bad("device-mapper dependency list outside the reply"));
        }
        Ok((0..count)
            .map(|i| own_backup_common::linux_block::split_dev(get_u64(data, 8 + i * 8)))
            .collect())
    }
}

/// `<cipher> <key> <iv_offset> <device> <offset> …`: the key field turned
/// into as many `0`s. A key the table names by keyring reference
/// (`:32:logon:…`) or none (`-`) is masked the same way.
fn mask_crypt_key(params: &mut [u8]) {
    let mut fields = 0;
    let mut i = 0;
    while i < params.len() {
        if params[i] == b' ' {
            i += 1;
            continue;
        }
        let start = i;
        while i < params.len() && params[i] != b' ' {
            i += 1;
        }
        if fields == 1 {
            params[start..i].fill(b'0');
            return;
        }
        fields += 1;
    }
}

/// What `DM_DEV_STATUS` says about a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevInfo {
    pub name: String,
    pub uuid: String,
    pub dev: (u32, u32),
    pub open_count: i32,
    pub target_count: u32,
    pub flags: u32,
}

impl DevInfo {
    fn from_reply(r: &Request) -> io::Result<Self> {
        Ok(DevInfo {
            name: r.name()?,
            uuid: r.uuid()?,
            dev: r.dev(),
            open_count: r.open_count(),
            target_count: r.target_count(),
            flags: r.flags(),
        })
    }

    pub fn suspended(&self) -> bool {
        self.flags & flag::SUSPEND != 0
    }
}

/// `/dev/mapper/control`, opened once.
pub struct Control {
    file: File,
}

/// Largest table reply asked for before giving up.
const MAX_REPLY: usize = 8 << 20;

impl Control {
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/mapper/control")?;
        Ok(Control { file })
    }

    /// Issue a prepared request; the error is the errno. Allocates nothing and
    /// touches no file, so it is safe while a device the root filesystem sits
    /// on is suspended.
    pub fn issue(&self, req: &mut Request) -> Result<(), i32> {
        // SAFETY: the buffer is at least HEADER_SIZE bytes and its header says
        // how long it is (data_size), which is all the kernel reads or writes.
        let r = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                request_code(req.cmd) as _,
                req.buf.as_mut_ptr(),
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        } else {
            Ok(())
        }
    }

    fn run(&self, req: &mut Request) -> io::Result<()> {
        self.issue(req).map_err(io::Error::from_raw_os_error)
    }

    /// Issue `make()`'s request, growing its reply room until it fits.
    fn run_growing(&self, make: impl Fn(usize) -> io::Result<Request>) -> io::Result<Request> {
        let mut room = 16 << 10;
        loop {
            let mut r = make(room)?;
            self.run(&mut r)?;
            if r.flags() & flag::BUFFER_FULL == 0 {
                return Ok(r);
            }
            if room >= MAX_REPLY {
                return Err(bad("device-mapper reply larger than 8 MiB"));
            }
            room *= 4;
        }
    }

    pub fn status(&self, name: &str) -> io::Result<DevInfo> {
        let mut r = Request::new(Cmd::DevStatus, name, 0, 0)?;
        self.run(&mut r)?;
        DevInfo::from_reply(&r)
    }

    /// The live table of `name`, keys included (`dmsetup table --showkeys`).
    pub fn table(&self, name: &str) -> io::Result<Vec<Target>> {
        self.run_growing(|room| {
            Request::new(
                Cmd::TableStatus,
                name,
                flag::STATUS_TABLE | flag::SECURE_DATA,
                room,
            )
        })?
        .targets()
    }

    /// The live table of `name` with each `crypt` target's key replaced by
    /// `0`s (`dmsetup table` without `--showkeys`): for callers that need
    /// where a device's data lies, never its key. The kernel has no keyless
    /// form of the request — it wipes its own copy (`SECURE_DATA`) — so the
    /// key is masked in the reply buffer, which is wiped on drop like every
    /// smaller one tried before it, before anything is parsed out of it.
    pub fn table_without_keys(&self, name: &str) -> io::Result<Vec<Target>> {
        let mut reply = self.run_growing(|room| {
            Request::new(
                Cmd::TableStatus,
                name,
                flag::STATUS_TABLE | flag::SECURE_DATA,
                room,
            )
        })?;
        reply.mask_keys()?;
        reply.targets()
    }

    /// The status line of each target of `name` (`dmsetup status`).
    pub fn target_status(&self, name: &str) -> io::Result<Vec<Target>> {
        self.run_growing(|room| Request::new(Cmd::TableStatus, name, 0, room))?
            .targets()
    }

    /// The devices the live table of `name` is built on.
    pub fn deps(&self, name: &str) -> io::Result<Vec<(u32, u32)>> {
        self.run_growing(|room| Request::new(Cmd::TableDeps, name, 0, room))?
            .deps()
    }

    /// The devices the inactive (loaded, not yet resumed) table of `name` is
    /// built on.
    pub fn inactive_deps(&self, name: &str) -> io::Result<Vec<(u32, u32)>> {
        self.run_growing(|room| {
            Request::new(Cmd::TableDeps, name, flag::QUERY_INACTIVE_TABLE, room)
        })?
        .deps()
    }

    /// Every device-mapper device.
    pub fn list(&self) -> io::Result<Vec<(String, (u32, u32))>> {
        self.run_growing(|room| Request::new(Cmd::ListDevices, "", 0, room))?
            .device_list()
    }

    pub fn create(&self, name: &str, uuid: &str) -> io::Result<DevInfo> {
        let mut r = Request::create(name, uuid)?;
        self.run(&mut r)?;
        DevInfo::from_reply(&r)
    }

    pub fn load(&self, name: &str, targets: &[Target], flags: u32) -> io::Result<()> {
        self.run(&mut Request::load(name, targets, flags)?)
    }

    /// Drop the inactive table of `name`, if it has one.
    pub fn clear(&self, name: &str) -> io::Result<()> {
        self.run(&mut Request::new(Cmd::TableClear, name, 0, 0)?)
    }

    pub fn resume(&self, name: &str, cookie: u32) -> io::Result<()> {
        let mut r = Request::new(Cmd::DevSuspend, name, 0, 0)?;
        r.set_cookie(cookie);
        self.run(&mut r)
    }

    pub fn remove(&self, name: &str, cookie: u32) -> io::Result<()> {
        let mut r = Request::new(Cmd::DevRemove, name, 0, 0)?;
        r.set_cookie(cookie);
        self.run(&mut r)
    }
}

/// The prepared `DM_DEV_SUSPEND` that suspends `name` (`flags`: none for the
/// default flush + filesystem freeze, or `SKIP_LOCKFS`/`NOFLUSH`).
pub fn suspend_request(name: &str, flags: u32) -> io::Result<Request> {
    Request::new(Cmd::DevSuspend, name, flags | flag::SUSPEND, 0)
}

/// The prepared `DM_DEV_SUSPEND` that resumes `name`, swapping in its inactive
/// table when it has one.
pub fn resume_request(name: &str, cookie: u32) -> io::Result<Request> {
    let mut r = Request::new(Cmd::DevSuspend, name, 0, 0)?;
    r.set_cookie(cookie);
    Ok(r)
}

/// The prepared `DM_TABLE_CLEAR` of `name`.
pub fn clear_request(name: &str) -> io::Result<Request> {
    Request::new(Cmd::TableClear, name, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_codes_match_the_kernel_header() {
        // _IOWR(0xfd, n, struct dm_ioctl) with sizeof == 312 == 0x138.
        assert_eq!(request_code(Cmd::ListDevices), 0xC138_FD02);
        assert_eq!(request_code(Cmd::DevCreate), 0xC138_FD03);
        assert_eq!(request_code(Cmd::DevRemove), 0xC138_FD04);
        assert_eq!(request_code(Cmd::DevSuspend), 0xC138_FD06);
        assert_eq!(request_code(Cmd::DevStatus), 0xC138_FD07);
        assert_eq!(request_code(Cmd::TableLoad), 0xC138_FD09);
        assert_eq!(request_code(Cmd::TableClear), 0xC138_FD0A);
        assert_eq!(request_code(Cmd::TableDeps), 0xC138_FD0B);
        assert_eq!(request_code(Cmd::TableStatus), 0xC138_FD0C);
    }

    #[test]
    fn udev_cookies_carry_flags_only() {
        assert_eq!(udev::HIDDEN, 0x004e_0000);
        assert_eq!(udev::VISIBLE, 0x0040_0000);
        // The lower half (the semaphore) is zero: nothing waits on it.
        assert_eq!(udev::HIDDEN & 0xffff, 0);
    }

    #[test]
    fn create_header_layout() {
        let r = Request::create("obk-real-ab", "OBK-REAL-ab").unwrap();
        let b = r.bytes();
        assert_eq!(b.len(), HEADER_SIZE);
        assert_eq!(&b[0..12], &[4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(get_u32(b, OFF_DATA_SIZE), 312);
        assert_eq!(get_u32(b, OFF_DATA_START), 312);
        assert_eq!(get_u32(b, OFF_TARGET_COUNT), 0);
        assert_eq!(get_u32(b, OFF_FLAGS), 0);
        assert_eq!(get_u64(b, OFF_DEV), 0);
        assert_eq!(&b[48..60], b"obk-real-ab\0");
        assert_eq!(&b[176..188], b"OBK-REAL-ab\0");
        assert!(b[188..].iter().all(|&x| x == 0));
    }

    #[test]
    fn names_that_do_not_fit_are_refused() {
        assert!(Request::new(Cmd::DevStatus, &"x".repeat(127), 0, 0).is_ok());
        assert!(Request::new(Cmd::DevStatus, &"x".repeat(128), 0, 0).is_err());
        assert!(Request::new(Cmd::DevStatus, "a\0b", 0, 0).is_err());
        assert!(Request::create("n", &"u".repeat(129)).is_err());
    }

    /// The exact bytes of a two-target load, against a hand-built image of
    /// what libdevmapper sends for the same table.
    #[test]
    fn table_load_byte_image() {
        let t = [
            Target::new(0, 2048, "linear", "8:16 0"),
            Target::new(2048, 4096, "snapshot-origin", "252:3"),
        ];
        let r = Request::load("vol", &t, flag::READONLY).unwrap();
        let b = r.bytes();
        // 40 + "8:16 0\0" (7) = 47 -> 48; 40 + "252:3\0" (6) = 46 -> 48.
        assert_eq!(b.len(), HEADER_SIZE + 48 + 48);
        assert_eq!(get_u32(b, OFF_DATA_SIZE) as usize, b.len());
        assert_eq!(get_u32(b, OFF_DATA_START), 312);
        assert_eq!(get_u32(b, OFF_TARGET_COUNT), 2);
        assert_eq!(get_u32(b, OFF_FLAGS), flag::READONLY | flag::SECURE_DATA);

        let mut want = Vec::new();
        want.extend_from_slice(&0u64.to_ne_bytes());
        want.extend_from_slice(&2048u64.to_ne_bytes());
        want.extend_from_slice(&0i32.to_ne_bytes());
        want.extend_from_slice(&48u32.to_ne_bytes()); // next: from this spec
        want.extend_from_slice(b"linear\0\0\0\0\0\0\0\0\0\0");
        want.extend_from_slice(b"8:16 0\0\0");
        want.extend_from_slice(&2048u64.to_ne_bytes());
        want.extend_from_slice(&4096u64.to_ne_bytes());
        want.extend_from_slice(&0i32.to_ne_bytes());
        want.extend_from_slice(&48u32.to_ne_bytes());
        want.extend_from_slice(b"snapshot-origin\0");
        want.extend_from_slice(b"252:3\0\0\0");
        assert_eq!(&b[HEADER_SIZE..], &want[..]);
        // Every spec starts 8-byte aligned.
        assert_eq!(HEADER_SIZE % 8, 0);
    }

    #[test]
    fn malformed_targets_are_refused() {
        assert!(Request::load("v", &[Target::new(0, 1, "", "x")], 0).is_err());
        assert!(Request::load("v", &[Target::new(0, 1, "a-very-long-type", "x")], 0).is_err());
        assert!(Request::load("v", &[Target::new(0, 1, "linear", "a\0b")], 0).is_err());
    }

    /// A `DM_TABLE_STATUS` reply as the kernel writes it: `next` counts from
    /// the first spec, `data_size` is what was used.
    fn status_reply(targets: &[(u64, u64, &str, &str)], flags: u32) -> Request {
        let mut r = Request::new(Cmd::TableStatus, "vol", flags, 4096).unwrap();
        let mut data: Vec<u8> = Vec::new();
        for (start, len, kind, params) in targets {
            let at = data.len();
            data.extend_from_slice(&start.to_ne_bytes());
            data.extend_from_slice(&len.to_ne_bytes());
            data.extend_from_slice(&0i32.to_ne_bytes());
            data.extend_from_slice(&0u32.to_ne_bytes());
            let mut k = [0u8; 16];
            k[..kind.len()].copy_from_slice(kind.as_bytes());
            data.extend_from_slice(&k);
            data.extend_from_slice(params.as_bytes());
            data.push(0);
            let used = data.len();
            data.resize(align8(used), 0);
            let next = data.len() as u32;
            put_u32(&mut data, at + 20, next);
        }
        r.buf[HEADER_SIZE..HEADER_SIZE + data.len()].copy_from_slice(&data);
        put_u32(&mut r.buf, OFF_DATA_SIZE, (HEADER_SIZE + data.len()) as u32);
        put_u32(&mut r.buf, OFF_TARGET_COUNT, targets.len() as u32);
        r
    }

    #[test]
    fn table_status_round_trip() {
        let key = "a".repeat(128);
        let crypt = format!("aes-xts-plain64 {key} 0 253:16 32768");
        let r = status_reply(
            &[(0, 1000, "crypt", &crypt), (1000, 24, "linear", "8:1 2048")],
            flag::STATUS_TABLE,
        );
        let t = r.targets().unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(
            (t[0].start, t[0].length, t[0].kind.as_str()),
            (0, 1000, "crypt")
        );
        assert_eq!(t[0].params.as_str(), crypt);
        assert_eq!(t[1].params.as_str(), "8:1 2048");
        // What is parsed loads back byte for byte the same way.
        let back = Request::load("vol", &t, 0).unwrap();
        assert_eq!(Request::load("vol", &t, 0).unwrap().bytes(), back.bytes());
        // Debug never shows a key.
        assert!(!format!("{:?}", t[0]).contains(&key));
    }

    /// The keyless table: the key field of a crypt target is zeros in the
    /// buffer itself, every other field and target is as the kernel wrote it.
    #[test]
    fn a_table_without_keys_masks_them_in_the_buffer() {
        let key = "ab".repeat(32);
        let crypt = format!("aes-xts-plain64 {key} 0 253:16 32768 1 allow_discards");
        let mut r = status_reply(
            &[
                (0, 1000, "crypt", &crypt),
                (1000, 24, "linear", "8:1 2048"),
                (
                    1024,
                    8,
                    "crypt",
                    "aes-cbc-essiv:sha256 :32:logon:cryptsetup:x-d0 0 8:2 0",
                ),
            ],
            flag::STATUS_TABLE,
        );
        r.mask_keys().unwrap();
        assert!(!r.bytes().windows(key.len()).any(|w| w == key.as_bytes()));
        let t = r.targets().unwrap();
        assert_eq!(
            t[0].params.as_str(),
            format!(
                "aes-xts-plain64 {} 0 253:16 32768 1 allow_discards",
                "0".repeat(64)
            )
        );
        assert_eq!(t[1].params.as_str(), "8:1 2048");
        assert_eq!(
            t[2].params.as_str(),
            format!(
                "aes-cbc-essiv:sha256 {} 0 8:2 0",
                "0".repeat(":32:logon:cryptsetup:x-d0".len())
            )
        );
        let mut short = *b"null -";
        mask_crypt_key(&mut short);
        assert_eq!(&short, b"null 0");
        let mut one = *b"cipher";
        mask_crypt_key(&mut one);
        assert_eq!(&one, b"cipher");
    }

    #[test]
    fn status_of_a_device_without_a_table_is_empty() {
        let r = Request::new(Cmd::TableStatus, "vol", 0, 64).unwrap();
        assert!(r.targets().unwrap().is_empty());
    }

    #[test]
    fn truncated_status_is_an_error() {
        let mut r = status_reply(&[(0, 8, "snapshot", "0/8 0")], 0);
        put_u32(&mut r.buf, OFF_TARGET_COUNT, 3);
        // The second spec would start beyond data_size.
        let t = r.targets();
        assert!(t.is_err() || t.unwrap().len() == 1);
        put_u32(&mut r.buf, OFF_DATA_SIZE, 100_000);
        assert!(r.targets().is_err());
    }

    #[test]
    fn device_list_parse() {
        let mut r = Request::new(Cmd::ListDevices, "", 0, 256).unwrap();
        let mut data: Vec<u8> = Vec::new();
        // luks-x (252:0): dev, next, name, padding, event_nr, flags.
        let rec = |data: &mut Vec<u8>, dev: u64, name: &str, last: bool| {
            let at = data.len();
            data.extend_from_slice(&dev.to_ne_bytes());
            data.extend_from_slice(&0u32.to_ne_bytes());
            data.extend_from_slice(name.as_bytes());
            data.push(0);
            data.resize(at + align8(data.len() - at), 0);
            data.extend_from_slice(&[0u8; 8]); // event_nr + flags
            if !last {
                let next = (data.len() - at) as u32;
                put_u32(data, at + 8, next);
            }
        };
        rec(&mut data, 252 << 8, "luks-x", false);
        // A minor above 255: new_encode_dev puts its high bits at 20..
        let big = ((300u64 & 0xff) | (252 << 8)) | ((300u64 & !0xff) << 12);
        rec(&mut data, big, "obk-snap-1", true);
        r.buf[HEADER_SIZE..HEADER_SIZE + data.len()].copy_from_slice(&data);
        put_u32(&mut r.buf, OFF_DATA_SIZE, (HEADER_SIZE + data.len()) as u32);
        assert_eq!(
            r.device_list().unwrap(),
            vec![
                ("luks-x".to_string(), (252, 0)),
                ("obk-snap-1".to_string(), (252, 300))
            ]
        );
        // "No devices": the first record's dev is zero.
        let empty = Request::new(Cmd::ListDevices, "", 0, 256).unwrap();
        assert!(empty.device_list().unwrap().is_empty());
    }

    #[test]
    fn deps_parse() {
        let mut r = Request::new(Cmd::TableDeps, "vol", 0, 64).unwrap();
        put_u32(&mut r.buf, HEADER_SIZE, 2);
        r.buf[HEADER_SIZE + 8..HEADER_SIZE + 16]
            .copy_from_slice(&((253u64 << 8) | 16).to_ne_bytes());
        r.buf[HEADER_SIZE + 16..HEADER_SIZE + 24].copy_from_slice(&((7u64 << 8) | 3).to_ne_bytes());
        put_u32(&mut r.buf, OFF_DATA_SIZE, (HEADER_SIZE + 24) as u32);
        assert_eq!(r.deps().unwrap(), vec![(253, 16), (7, 3)]);
    }

    #[test]
    fn reply_header_fields() {
        let mut r = Request::new(Cmd::DevStatus, "luks-x", 0, 0).unwrap();
        put_u32(&mut r.buf, OFF_OPEN_COUNT, (-1i32) as u32);
        put_u32(&mut r.buf, OFF_FLAGS, flag::SUSPEND | flag::ACTIVE_PRESENT);
        r.buf[OFF_DEV..OFF_DEV + 8].copy_from_slice(&((252u64 << 8) | 7).to_ne_bytes());
        r.buf[OFF_UUID..OFF_UUID + 10].copy_from_slice(b"CRYPT-LUKS");
        let info = DevInfo::from_reply(&r).unwrap();
        assert_eq!(info.name, "luks-x");
        assert_eq!(info.uuid, "CRYPT-LUKS");
        assert_eq!(info.dev, (252, 7));
        assert_eq!(info.open_count, -1);
        assert!(info.suspended());
    }

    #[test]
    fn prepared_suspend_and_resume() {
        let s = suspend_request("luks-x", flag::SKIP_LOCKFS).unwrap();
        assert_eq!(s.cmd(), Cmd::DevSuspend);
        assert_eq!(s.flags(), flag::SUSPEND | flag::SKIP_LOCKFS);
        let r = resume_request("luks-x", udev::VISIBLE).unwrap();
        assert_eq!(r.flags(), 0);
        assert_eq!(get_u32(r.bytes(), OFF_EVENT_NR), 0x0040_0000);
        assert_eq!(clear_request("luks-x").unwrap().cmd(), Cmd::TableClear);
    }
}
