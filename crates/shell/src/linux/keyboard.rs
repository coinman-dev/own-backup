//! The keyboard layout for the strip's indicator, on a compositor that tells
//! no one (plan §6, stage 6, "taskbar" 4).
//!
//! A Wayland client learns the layout only from the keyboard events of its
//! own focused surface, and the strip never has the focus — so labwc keeps
//! the layout to itself, and there is no protocol to ask it. What the strip
//! can do instead is what labwc itself does: read the same keyboards and run
//! them through the same keymap. labwc puts every keyboard into one group
//! with one xkb state (the layout is global, `layoutScope` by default), and
//! builds its keymap from `XKB_DEFAULT_*` — which `kiosk-desktop` exports and
//! the strip, labwc's child, inherits. libxkbcommon given the same key
//! presses in the same keymap arrives at the same layout, with every option
//! (`grp:alt_shift_toggle`, or whatever /etc/default/keyboard says) handled
//! exactly as labwc handles it.
//!
//! The keys come from `/dev/input`, read beside the compositor and never
//! grabbed: the medium's session is root (own-backup-kiosk.service), and only
//! there is this switched on. A click on the indicator switches the layout
//! the way a person would — Alt+Shift on a keyboard of ours (`uinput`), which
//! labwc adds to its group like any other — so the compositor stays the one
//! that owns the layout, as on WinPE the window in front does.
//!
//! Where the mirror can lose track, it is put back: a key let go on another
//! console (Ctrl+Alt+F2 releases its keys there) leaves no held modifier
//! behind once this console is back, and a strip restarted by `shell-loop`
//! takes the layout it last saw — of the same labwc only (`LABWC_PID`).

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, CStr};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::log;

// --- libxkbcommon -----------------------------------------------------------------

#[repr(C)]
struct XkbContext {
    _opaque: [u8; 0],
}
#[repr(C)]
struct XkbKeymap {
    _opaque: [u8; 0],
}
#[repr(C)]
struct XkbStateRaw {
    _opaque: [u8; 0],
}

const XKB_KEY_UP: c_int = 0;
const XKB_KEY_DOWN: c_int = 1;
const XKB_STATE_LAYOUT_LOCKED: c_int = 1 << 6;
const XKB_STATE_LAYOUT_EFFECTIVE: c_int = 1 << 7;

#[link(name = "xkbcommon")]
extern "C" {
    fn xkb_context_new(flags: c_int) -> *mut XkbContext;
    fn xkb_context_unref(context: *mut XkbContext);
    fn xkb_keymap_new_from_names(
        context: *mut XkbContext,
        names: *const std::ffi::c_void,
        flags: c_int,
    ) -> *mut XkbKeymap;
    fn xkb_keymap_unref(keymap: *mut XkbKeymap);
    fn xkb_keymap_num_layouts(keymap: *mut XkbKeymap) -> u32;
    fn xkb_keymap_layout_get_name(keymap: *mut XkbKeymap, idx: u32) -> *const c_char;
    fn xkb_state_new(keymap: *mut XkbKeymap) -> *mut XkbStateRaw;
    fn xkb_state_unref(state: *mut XkbStateRaw);
    fn xkb_state_update_key(state: *mut XkbStateRaw, key: u32, direction: c_int) -> c_int;
    fn xkb_state_update_mask(
        state: *mut XkbStateRaw,
        depressed_mods: u32,
        latched_mods: u32,
        locked_mods: u32,
        depressed_layout: u32,
        latched_layout: u32,
        locked_layout: u32,
    ) -> c_int;
    fn xkb_state_serialize_layout(state: *mut XkbStateRaw, components: c_int) -> u32;
}

/// The keymap labwc builds and a state of our own in it.
pub struct Xkb {
    context: *mut XkbContext,
    keymap: *mut XkbKeymap,
    state: *mut XkbStateRaw,
}

impl Xkb {
    /// The keymap of `XKB_DEFAULT_*`, as labwc makes it (no names given:
    /// libxkbcommon reads the variables itself, exactly as for labwc).
    pub fn from_environment() -> Option<Self> {
        // SAFETY: plain constructors; every pointer is checked before use and
        // released in Drop.
        unsafe {
            let context = xkb_context_new(0);
            if context.is_null() {
                return None;
            }
            let keymap = xkb_keymap_new_from_names(context, std::ptr::null(), 0);
            if keymap.is_null() {
                xkb_context_unref(context);
                return None;
            }
            let state = xkb_state_new(keymap);
            if state.is_null() {
                xkb_keymap_unref(keymap);
                xkb_context_unref(context);
                return None;
            }
            Some(Self {
                context,
                keymap,
                state,
            })
        }
    }

    pub fn layouts(&self) -> u32 {
        // SAFETY: the keymap lives as long as self.
        unsafe { xkb_keymap_num_layouts(self.keymap) }
    }

    /// xkeyboard-config's name of a layout ("Russian"), for the log.
    pub fn layout_name(&self, index: u32) -> String {
        // SAFETY: the keymap lives as long as self; the string is the keymap's.
        unsafe {
            let p = xkb_keymap_layout_get_name(self.keymap, index);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    /// The layout in effect.
    pub fn layout(&self) -> u32 {
        // SAFETY: the state lives as long as self.
        unsafe { xkb_state_serialize_layout(self.state, XKB_STATE_LAYOUT_EFFECTIVE) }
    }

    /// An evdev key went down or up.
    pub fn key(&mut self, evdev_code: u16, down: bool) {
        // SAFETY: the state lives as long as self. Keycodes of xkb are evdev's
        // plus 8; one the keymap does not know (a mouse button) changes nothing.
        unsafe {
            xkb_state_update_key(
                self.state,
                u32::from(evdev_code) + 8,
                if down { XKB_KEY_DOWN } else { XKB_KEY_UP },
            );
        }
    }

    /// No key held, this layout locked: a fresh state that remembers only the
    /// layout — what is left once keys were let go where we could not see.
    pub fn reset_to(&mut self, layout: u32) {
        // SAFETY: the old state is replaced only by a valid new one.
        unsafe {
            let fresh = xkb_state_new(self.keymap);
            if fresh.is_null() {
                return;
            }
            xkb_state_unref(self.state);
            self.state = fresh;
            xkb_state_update_mask(self.state, 0, 0, 0, 0, 0, layout);
        }
    }

    /// The locked layout (not one latched for a key, nor held).
    pub fn locked(&self) -> u32 {
        // SAFETY: the state lives as long as self.
        unsafe { xkb_state_serialize_layout(self.state, XKB_STATE_LAYOUT_LOCKED) }
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        // SAFETY: created in from_environment, released once.
        unsafe {
            xkb_state_unref(self.state);
            xkb_keymap_unref(self.keymap);
            xkb_context_unref(self.context);
        }
    }
}

// --- evdev and uinput ---------------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_MAX: usize = 0x2ff;

const IOC_WRITE: libc::c_ulong = 1;
const IOC_READ: libc::c_ulong = 2;

const fn ioc(dir: libc::c_ulong, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (dir << 30)
        | ((size as libc::c_ulong) << 16)
        | ((ty as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

/// EVIOCGBIT(EV_KEY, len): which keys a device has.
const fn eviocgbit_key(len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x20 + EV_KEY as u8, len)
}
/// EVIOCGNAME(len).
const fn eviocgname(len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x06, len)
}
const UI_SET_EVBIT: libc::c_ulong = ioc(IOC_WRITE, b'U', 100, std::mem::size_of::<c_int>());
const UI_SET_KEYBIT: libc::c_ulong = ioc(IOC_WRITE, b'U', 101, std::mem::size_of::<c_int>());
const UI_DEV_SETUP: libc::c_ulong = ioc(
    IOC_WRITE,
    b'U',
    3,
    std::mem::size_of::<libc::uinput_setup>(),
);
const UI_DEV_CREATE: libc::c_ulong = ioc(0, b'U', 1, 0);
const UI_DEV_DESTROY: libc::c_ulong = ioc(0, b'U', 2, 0);

/// What our own keyboard is called — and how it is told from the person's.
const SWITCH_NAME: &str = "Own Backup layout switch";

/// Is this a keyboard that can change the layout: one with Shift or Alt.
/// Mice, power buttons and lid switches are left alone.
fn is_keyboard(fd: RawFd) -> bool {
    let mut bits = [0u8; KEY_MAX / 8 + 1];
    // SAFETY: the buffer is as long as the request says.
    let n = unsafe { libc::ioctl(fd, eviocgbit_key(bits.len()), bits.as_mut_ptr()) };
    let has = |key: u16| bits[usize::from(key) / 8] & (1 << (key % 8)) != 0;
    n > 0 && (has(KEY_LEFTSHIFT) || has(KEY_LEFTALT))
}

fn device_name(fd: RawFd) -> String {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is as long as the request says.
    let n = unsafe { libc::ioctl(fd, eviocgname(name.len()), name.as_mut_ptr()) };
    if n <= 0 {
        return String::new();
    }
    CStr::from_bytes_until_nul(&name)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Key presses and releases from what `read` returned, in order; repeats
/// (value 2) change no state and are dropped.
pub fn key_events(bytes: &[u8]) -> Vec<(u16, bool)> {
    const SIZE: usize = std::mem::size_of::<libc::input_event>();
    (0..bytes.len() / SIZE)
        .map(|i| &bytes[i * SIZE..(i + 1) * SIZE])
        .filter_map(|chunk| {
            // SAFETY: a whole input_event's bytes, read unaligned.
            let ev: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
            (ev.type_ == EV_KEY && (ev.value == 0 || ev.value == 1))
                .then_some((ev.code, ev.value == 1))
        })
        .collect()
}

/// A keyboard of ours that says Alt+Shift when the indicator is clicked.
pub struct Switch {
    file: File,
}

impl Switch {
    pub fn create() -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .map_err(|e| format!("/dev/uinput: {e}"))?;
        let fd = file.as_raw_fd();
        let ioctl_int = |req: libc::c_ulong, value: c_int| -> Result<(), String> {
            // SAFETY: these requests take an int by value.
            if unsafe { libc::ioctl(fd, req, value) } < 0 {
                return Err(format!("uinput: {}", std::io::Error::last_os_error()));
            }
            Ok(())
        };
        ioctl_int(UI_SET_EVBIT, c_int::from(EV_KEY))?;
        // The keys of a keyboard's first row too, not only the two it
        // presses: udev calls a device with them ID_INPUT_KEYBOARD, and that
        // is what makes the compositor add it to its keyboards.
        for key in 1..=88 {
            ioctl_int(UI_SET_KEYBIT, key)?;
        }
        // SAFETY: zeroed is a valid uinput_setup; the name fits with its NUL.
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id.bustype = 0x06; // BUS_VIRTUAL
        setup.id.vendor = 0x4f42; // "OB"
        setup.id.product = 0x0001;
        for (dst, src) in setup.name.iter_mut().zip(SWITCH_NAME.bytes()) {
            *dst = src as c_char;
        }
        // SAFETY: the request takes a pointer to a uinput_setup.
        if unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup as *const libc::uinput_setup) } < 0
            // SAFETY: no argument.
            || unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0
        {
            return Err(format!("uinput: {}", std::io::Error::last_os_error()));
        }
        Ok(Self { file })
    }

    /// Alt down, Shift down, Shift up, Alt up — each followed by a report.
    pub fn press(&mut self) -> std::io::Result<()> {
        let mut bytes = Vec::new();
        for (code, down) in SWITCH_KEYS {
            bytes.extend_from_slice(&event_bytes(EV_KEY, code, i32::from(down)));
            bytes.extend_from_slice(&event_bytes(EV_SYN, SYN_REPORT, 0));
        }
        self.file.write_all(&bytes)
    }
}

impl Drop for Switch {
    fn drop(&mut self) {
        // SAFETY: no argument.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

/// What the switch presses, in order.
pub const SWITCH_KEYS: [(u16, bool); 4] = [
    (KEY_LEFTALT, true),
    (KEY_LEFTSHIFT, true),
    (KEY_LEFTSHIFT, false),
    (KEY_LEFTALT, false),
];

fn event_bytes(type_: u16, code: u16, value: i32) -> Vec<u8> {
    // SAFETY: zeroed is a valid input_event (the kernel stamps the time).
    let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
    ev.type_ = type_;
    ev.code = code;
    ev.value = value;
    // SAFETY: input_event is plain data.
    unsafe {
        std::slice::from_raw_parts(
            (&ev as *const libc::input_event).cast::<u8>(),
            std::mem::size_of::<libc::input_event>(),
        )
    }
    .to_vec()
}

// --- the mirror ---------------------------------------------------------------------

/// The keyboards being read, by device node.
pub struct Mirror {
    pub xkb: Xkb,
    /// `XKB_DEFAULT_LAYOUT`, split: the codes the indicator names.
    pub codes: Vec<String>,
    devices: HashMap<PathBuf, File>,
    /// Nodes that are not keyboards, with the inode they had: looked at again
    /// only when the node is a new one.
    ignored: HashMap<PathBuf, u64>,
    /// Our console (`XDG_VTNR`), when known.
    vt: Option<String>,
    /// Another console was in front since the last look.
    away: bool,
    pub switch: Option<Switch>,
    labwc: String,
}

impl Mirror {
    pub fn start() -> Result<Self, String> {
        let xkb = Xkb::from_environment().ok_or("no keymap from XKB_DEFAULT_*")?;
        let codes = crate::layout::xkb_layout_codes(
            &std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_default(),
        );
        let names: Vec<String> = (0..xkb.layouts()).map(|i| xkb.layout_name(i)).collect();
        log(&format!(
            "keyboard: layouts {} ({})",
            codes.join(","),
            names.join(", ")
        ));
        let switch = match Switch::create() {
            Ok(s) => Some(s),
            Err(e) => {
                log(&format!("keyboard: no switching by click: {e}"));
                None
            }
        };
        let mut m = Self {
            xkb,
            codes,
            devices: HashMap::new(),
            ignored: HashMap::new(),
            vt: std::env::var("XDG_VTNR").ok().filter(|v| !v.is_empty()),
            away: false,
            switch,
            labwc: std::env::var("LABWC_PID").unwrap_or_default(),
        };
        if let Some(saved) = read_saved(&m.labwc) {
            if saved < m.xkb.layouts() {
                m.xkb.reset_to(saved);
                log(&format!(
                    "keyboard: layout {saved} as this labwc last had it"
                ));
            }
        }
        Ok(m)
    }

    /// Keyboards that appeared since the last look; their descriptors, for
    /// the caller to watch.
    pub fn scan(&mut self) -> Vec<(PathBuf, RawFd)> {
        let mut added = Vec::new();
        let Ok(dir) = std::fs::read_dir("/dev/input") else {
            return added;
        };
        let mut present = HashSet::new();
        for entry in dir.flatten() {
            let path = entry.path();
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"));
            if !is_event {
                continue;
            }
            present.insert(path.clone());
            if self.devices.contains_key(&path) {
                continue;
            }
            let ino = std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0);
            if self.ignored.get(&path) == Some(&ino) {
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
            else {
                self.ignored.insert(path, ino);
                continue;
            };
            let fd = file.as_raw_fd();
            // Our own switch is fed in directly when it presses; read here
            // as well, its keys would count twice.
            if !is_keyboard(fd) || device_name(fd) == SWITCH_NAME {
                self.ignored.insert(path, ino);
                continue;
            }
            log(&format!(
                "keyboard: reading {} ({})",
                path.display(),
                device_name(fd)
            ));
            added.push((path.clone(), fd));
            self.devices.insert(path, file);
        }
        self.devices.retain(|p, _| present.contains(p));
        self.ignored.retain(|p, _| present.contains(p));
        added
    }

    /// Read what a keyboard has. `Err` when it is gone: stop watching it.
    pub fn read(&mut self, path: &Path) -> Result<(), ()> {
        let Some(file) = self.devices.get_mut(path) else {
            return Err(());
        };
        let mut buf = [0u8; 64 * std::mem::size_of::<libc::input_event>()];
        let mut events = Vec::new();
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => events.extend(key_events(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log(&format!("keyboard: {} gone: {e}", path.display()));
                    self.devices.remove(path);
                    return Err(());
                }
            }
        }
        if !self.ours_in_front() {
            // labwc does not see these keys; neither do we.
            return Ok(());
        }
        for (code, down) in events {
            self.xkb.key(code, down);
        }
        Ok(())
    }

    /// Is our console the one in front? Coming back to it after another one
    /// lets go of whatever the mirror still thinks is held.
    pub fn ours_in_front(&mut self) -> bool {
        let Some(vt) = &self.vt else {
            return true;
        };
        let active = std::fs::read_to_string("/sys/class/tty/tty0/active").unwrap_or_default();
        let front = active.trim() == format!("tty{vt}");
        if !front {
            self.away = true;
        } else if std::mem::take(&mut self.away) {
            let locked = self.xkb.locked();
            self.xkb.reset_to(locked);
            log(&format!(
                "keyboard: back on tty{vt}, keys let go, layout {locked}"
            ));
        }
        front
    }

    /// The click on the indicator: Alt+Shift through our own keyboard, fed
    /// to the mirror as labwc gets it.
    pub fn switch_layout(&mut self) {
        let Some(switch) = self.switch.as_mut() else {
            log("keyboard: no switch to press");
            return;
        };
        match switch.press() {
            Ok(()) => {
                for (code, down) in SWITCH_KEYS {
                    self.xkb.key(code, down);
                }
            }
            Err(e) => log(&format!("keyboard: the switch did not press: {e}")),
        }
    }

    /// Remember the layout for a strip that starts again under this labwc,
    /// and say it for the medium's self-test — with the indicator's middle in
    /// screen pixels, where the stand clicks it.
    pub fn save(&self, label: &str, at: (i32, i32)) {
        let Some(path) = saved_path() else {
            return;
        };
        let text = format!(
            "{} {} {}\nat {} {}\n",
            self.labwc,
            self.xkb.locked(),
            label,
            at.0,
            at.1
        );
        let tmp = path.with_extension("layout.tmp");
        if std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn saved_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| Path::new(&d).join("own-backup-shell.layout"))
}

fn read_saved(labwc: &str) -> Option<u32> {
    let text = std::fs::read_to_string(saved_path()?).ok()?;
    crate::layout::saved_layout(&text, labwc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(type_: u16, code: u16, value: i32) -> Vec<u8> {
        event_bytes(type_, code, value)
    }

    #[test]
    fn only_presses_and_releases_of_keys_are_taken() {
        let mut bytes = Vec::new();
        bytes.extend(event(EV_KEY, KEY_LEFTALT, 1));
        bytes.extend(event(EV_SYN, SYN_REPORT, 0));
        bytes.extend(event(EV_KEY, KEY_LEFTALT, 2)); // a repeat
        bytes.extend(event(0x02, 0, 5)); // a mouse moving
        bytes.extend(event(EV_KEY, KEY_LEFTALT, 0));
        assert_eq!(
            key_events(&bytes),
            [(KEY_LEFTALT, true), (KEY_LEFTALT, false)]
        );
        assert!(key_events(&bytes[..10]).is_empty(), "half an event is none");
    }

    #[test]
    fn the_ioctl_numbers_are_the_kernels() {
        // From <linux/uinput.h> and <linux/input.h> on x86_64.
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(eviocgbit_key(96), 0x8060_4521);
        assert_eq!(eviocgname(256), 0x8100_4506);
    }

    /// The whole idea, against the real libxkbcommon and xkeyboard-config:
    /// the medium's keymap, Alt+Shift as a person presses it — and as the
    /// indicator's switch presses it.
    #[test]
    fn alt_shift_moves_the_mirror_to_the_next_layout_and_back() {
        if !Path::new("/usr/share/X11/xkb/rules/evdev").exists() {
            eprintln!("no xkeyboard-config here; skipped");
            return;
        }
        std::env::set_var("XKB_DEFAULT_LAYOUT", "us,ru");
        std::env::set_var("XKB_DEFAULT_OPTIONS", "grp:alt_shift_toggle");
        let mut xkb = Xkb::from_environment().expect("keymap");
        assert_eq!(xkb.layouts(), 2);
        assert_eq!(xkb.layout(), 0);
        for (code, down) in SWITCH_KEYS {
            xkb.key(code, down);
        }
        assert_eq!(xkb.layout(), 1, "Alt+Shift: Russian");
        // Shift first, then Alt: the same toggle.
        for (code, down) in [
            (KEY_LEFTSHIFT, true),
            (KEY_LEFTALT, true),
            (KEY_LEFTALT, false),
            (KEY_LEFTSHIFT, false),
        ] {
            xkb.key(code, down);
        }
        assert_eq!(xkb.layout(), 0, "and back");
        // Typing changes nothing; neither does a mouse button.
        xkb.key(30, true);
        xkb.key(30, false);
        xkb.key(0x110, true);
        xkb.key(0x110, false);
        assert_eq!(xkb.layout(), 0);
        // Alt held where we could not see it let go: a reset forgets it and
        // keeps the layout.
        for (code, down) in SWITCH_KEYS {
            xkb.key(code, down);
        }
        xkb.key(KEY_LEFTALT, true);
        xkb.reset_to(xkb.locked());
        assert_eq!(xkb.layout(), 1);
        xkb.key(KEY_LEFTSHIFT, true);
        xkb.key(KEY_LEFTSHIFT, false);
        assert_eq!(
            xkb.layout(),
            1,
            "Shift alone after the reset switches nothing"
        );
    }
}
