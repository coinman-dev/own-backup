//! Linux: hearing a disk arrive or leave and a volume being mounted, so the
//! window re-reads its disk list when something changed.
//!
//! On Windows the window polls a signature of the present drive letters
//! (`GetLogicalDrives`). Linux has no letters, the signature was always 0 and
//! the window never noticed a disk plugged in after it opened. Here the same
//! number is a generation that goes up on every change:
//!
//! - udev's broadcasts on the `NETLINK_KOBJECT_UEVENT` socket, for block
//!   devices. Two groups are heard: udevd's, sent after it has made the
//!   `/dev/disk/by-…` links the inventory names volumes by, and the kernel's
//!   own, which is all there is where no udevd runs (a container, a boot
//!   medium before udev). One change then arrives twice; the window re-reads
//!   once per poll either way. Any user may listen: it is how `udevadm
//!   monitor` and every desktop learn about disks;
//! - `/proc/self/mountinfo`, which the kernel marks with `POLLPRI` on every
//!   mount and unmount: plugging a drive in is a device event, the desktop
//!   mounting it a moment later is not.
//!
//! Where the socket cannot be had, the names in `/sys/class/block` are
//! compared every few seconds instead. Nothing here decides anything about a
//! disk; a change only makes the window ask the service again.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Once;
use std::time::Duration;

/// Netlink multicast groups of `NETLINK_KOBJECT_UEVENT`: 1 is the kernel's,
/// 2 is udevd's (`libudev`'s `UDEV_MONITOR_UDEV`).
const KERNEL_GROUP: u32 = 1;
const UDEV_GROUP: u32 = 2;
/// How often `/sys/class/block` is compared when there is no socket.
const FALLBACK_POLL: Duration = Duration::from_secs(3);

static GENERATION: AtomicU32 = AtomicU32::new(1);
static START: Once = Once::new();

/// The current generation; starts the watch on first use.
pub fn generation() -> u32 {
    START.call_once(start);
    GENERATION.load(Ordering::Relaxed)
}

fn bump(why: &str) {
    let now = GENERATION.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    tracing::debug!(why, generation = now, "device watch: something changed");
}

fn start() {
    match open_uevent_socket() {
        Ok(socket) => spawn("device-watch-udev", move || listen(socket)),
        Err(error) => {
            tracing::warn!(%error, "device watch: no uevent socket, comparing /sys/class/block instead");
            spawn("device-watch-sysfs", poll_sysfs);
        }
    }
    match std::fs::File::open("/proc/self/mountinfo") {
        Ok(file) => spawn("device-watch-mounts", move || watch_mounts(file)),
        Err(error) => tracing::warn!(%error, "device watch: mount table not watched"),
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    if let Err(error) = std::thread::Builder::new().name(name.into()).spawn(body) {
        tracing::warn!(%error, thread = name, "device watch: thread did not start");
    }
}

fn open_uevent_socket() -> std::io::Result<OwnedFd> {
    // SAFETY: plain socket calls; the descriptor is owned right after creation
    // and the address is a fully initialised `sockaddr_nl` of the size given.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let socket = OwnedFd::from_raw_fd(fd);
        let mut address: libc::sockaddr_nl = std::mem::zeroed();
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        address.nl_groups = KERNEL_GROUP | UDEV_GROUP;
        let bound = libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if bound < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(socket)
    }
}

fn listen(socket: OwnedFd) {
    tracing::info!("device watch: listening to udev for disks");
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: the buffer outlives the call and its length is passed with it.
        let got = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if got < 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The queue overflowed and events were dropped: whatever they
                // were, the list is worth reading again.
                Some(libc::ENOBUFS) => bump("uevents dropped"),
                _ => {
                    tracing::warn!(%error, "device watch: uevent socket failed");
                    std::thread::sleep(FALLBACK_POLL);
                }
            }
            continue;
        }
        if is_block_event(&buffer[..got as usize]) {
            bump("block device uevent");
        }
    }
}

/// Is this netlink message about a block device? Both shapes are read:
/// udevd's (`libudev\0`, a header, then `KEY=value\0…` at the offset the
/// header names) and the kernel's (`action@devpath\0KEY=value\0…`).
fn is_block_event(message: &[u8]) -> bool {
    let properties = if let Some(header) = message.strip_prefix(b"libudev\0") {
        // magic (big-endian 0xfeedcafe), header size, then the properties'
        // offset and length — those two in the sender's byte order, which on
        // one machine is ours.
        let word = |at: usize| -> Option<u32> {
            header
                .get(at..at + 4)
                .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        };
        if header.get(..4) != Some(&0xfeed_cafe_u32.to_be_bytes()[..]) {
            return false;
        }
        let (Some(offset), Some(length)) = (word(8), word(12)) else {
            return false;
        };
        let (offset, length) = (offset as usize, length as usize);
        match message.get(offset..offset.saturating_add(length)) {
            Some(properties) => properties,
            None => return false,
        }
    } else {
        message
    };
    properties
        .split(|&b| b == 0)
        .any(|field| field == b"SUBSYSTEM=block")
}

fn watch_mounts(file: std::fs::File) {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLPRI,
        revents: 0,
    };
    loop {
        // SAFETY: one live pollfd; the file stays open for as long as this
        // loop runs. The kernel re-arms the event itself on the next poll.
        let ready = unsafe { libc::poll(&mut poll, 1, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            tracing::warn!(%error, "device watch: mount table no longer watched");
            return;
        }
        if poll.revents & (libc::POLLPRI | libc::POLLERR) != 0 {
            bump("mount table changed");
        }
    }
}

/// The block devices the kernel has, by name — what the fallback compares.
fn block_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/block")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn poll_sysfs() {
    let mut last = block_names();
    loop {
        std::thread::sleep(FALLBACK_POLL);
        let now = block_names();
        if now != last {
            bump("block devices changed");
            last = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udev_message(properties: &[u8]) -> Vec<u8> {
        let header_size = 40u32;
        let mut m = b"libudev\0".to_vec();
        m.extend_from_slice(&0xfeed_cafe_u32.to_be_bytes());
        m.extend_from_slice(&header_size.to_ne_bytes());
        m.extend_from_slice(&header_size.to_ne_bytes());
        m.extend_from_slice(&(properties.len() as u32).to_ne_bytes());
        m.resize(header_size as usize, 0);
        m.extend_from_slice(properties);
        m
    }

    #[test]
    fn a_kernel_uevent_about_a_disk_is_heard() {
        let disk = b"add@/devices/pci0000:00/usb1/1-1/block/sdb\0ACTION=add\0\
DEVPATH=/devices/pci0000:00/usb1/1-1/block/sdb\0SUBSYSTEM=block\0DEVNAME=sdb\0";
        assert!(is_block_event(disk));
        let usb = b"add@/devices/pci0000:00/usb1/1-1\0ACTION=add\0SUBSYSTEM=usb\0";
        assert!(!is_block_event(usb));
    }

    #[test]
    fn a_udev_message_is_read_at_its_properties() {
        let block = udev_message(b"ACTION=add\0SUBSYSTEM=block\0DEVTYPE=partition\0");
        assert!(is_block_event(&block));
        let net = udev_message(b"ACTION=add\0SUBSYSTEM=net\0");
        assert!(!is_block_event(&net));
        // A header that lies about where its properties are.
        let mut broken = udev_message(b"SUBSYSTEM=block\0");
        broken[20..24].copy_from_slice(&9999u32.to_ne_bytes());
        assert!(!is_block_event(&broken));
        // Not udev's magic: nothing is believed.
        let mut foreign = udev_message(b"SUBSYSTEM=block\0");
        foreign[8] = 0;
        assert!(!is_block_event(&foreign));
    }

    #[test]
    fn a_field_must_be_the_whole_subsystem() {
        assert!(!is_block_event(b"x@/y\0SUBSYSTEM=blockish\0"));
        assert!(!is_block_event(b"x@/y\0NOT_SUBSYSTEM=block\0"));
    }
}
