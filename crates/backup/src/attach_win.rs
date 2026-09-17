//! Showing a backup image to Windows as a real disk.
//!
//! One seam, one road: an iSCSI target of ours ([`crate::iscsi`]) plus the
//! initiator Windows already has ([`crate::iscsi_win`]). Everything that used
//! to need Arsenal Image Mounter goes through here — the read-only mount of a
//! qcow2 backup, and the read-write mount the exclusion pass uses to let the
//! filesystem driver delete files from a finished image.
//!
//! Two things are done here rather than left to callers, because forgetting
//! either has cost a whole debugging session before:
//!
//! * **The size is checked before anything writes.** The LUN is exactly
//!   `virtual_size` bytes, so a disk of another size is not our image and must
//!   not be touched. A resolution bug once pointed a zeroing pass at a live
//!   host disk (2026-07-08); this is the check that would have stopped it.
//! * **Teardown is ordered and unconditional.** Volumes are force-dismounted,
//!   then the session logs out, then the target stops. Out of order, Windows
//!   keeps handles open and the detach waits them out.
//!
//! The address is not assumed either. Loopback is tried first; if the initiator
//! cannot reach us there — it is a kernel component and has its own idea of
//! routing — the machine's own address is tried next.

#![cfg(target_os = "windows")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use own_backup_common::{OwnBackupError, Result};
use tracing::{info, warn};

use crate::block_source::{BlockSource, WritableBlockSource};
use crate::iscsi::{self, Lun};
use crate::iscsi_win::{self, AttachedTarget};
use crate::win_device::WinRawDevice;

fn dev(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::Device(msg.into())
}

/// An image currently presented to Windows as `\\.\PhysicalDriveN`.
///
/// Dropping it tears everything down; [`Attached::detach`] does the same and
/// lets the caller see it happen in order.
pub struct Attached {
    pub physical: String,
    pub disk_number: u32,
    target: Option<AttachedTarget>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Attached {
    /// Dismount, log out, stop serving — in that order.
    pub fn detach(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        // Anything Windows mounted from this disk has to let go first, or the
        // logout leaves a device nobody can remove.
        if self.disk_number != u32::MAX {
            for volume in crate::volume_win::all_volume_roots_on_disk(self.disk_number) {
                let dismounted = crate::volume_win::dismount_volume_forced(&volume);
                info!(device = %volume, dismounted, "attached image: volume force-dismounted");
            }
        }
        if let Some(target) = self.target.take() {
            target.logout();
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Serve `source` read-write and attach it. Writes go wherever the source puts
/// them — for the exclusion pass, a scratch overlay beside the image.
pub fn attach_rw(
    source: Box<dyn WritableBlockSource>,
    virtual_size: u64,
    serial: &str,
) -> Result<Attached> {
    attach(Lun::read_write(source).with_serial(serial), virtual_size, serial, true)
}

/// Serve `source` read-only and attach it. Windows sees a write-protected disk,
/// so nothing it does can reach the backup.
pub fn attach_ro(
    source: Box<dyn BlockSource>,
    virtual_size: u64,
    serial: &str,
) -> Result<Attached> {
    attach(Lun::read_only(source).with_serial(serial), virtual_size, serial, false)
}

fn attach(lun: Lun, virtual_size: u64, serial: &str, read_write: bool) -> Result<Attached> {
    info!(
        backend = "iscsi",
        serial,
        virtual_size,
        read_write,
        "attaching the image"
    );
    let iqn = iscsi::iqn_for(serial);
    let lun = Arc::new(Mutex::new(lun));

    let mut last: Option<OwnBackupError> = None;
    for address in candidate_addresses() {
        match attach_at(address, &iqn, lun.clone(), virtual_size, read_write) {
            Ok(attached) => return Ok(attached),
            Err(e) => {
                warn!(%address, error = %e, "iSCSI: this address did not work — trying the next");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| dev("iSCSI: no local address could be used for the target")))
}

fn attach_at(
    address: IpAddr,
    iqn: &str,
    lun: Arc<Mutex<Lun>>,
    virtual_size: u64,
    read_write: bool,
) -> Result<Attached> {
    let listener = TcpListener::bind(SocketAddr::new(address, 0))
        .map_err(|e| dev(format!("iSCSI bind {address}: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| dev(format!("iSCSI local_addr: {e}")))?
        .port();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let iqn_bg = iqn.to_string();
    let lun_bg = lun.clone();
    let server = std::thread::Builder::new()
        .name("iscsi-target".into())
        .spawn(move || {
            if let Err(e) = iscsi::serve_listener(listener, iqn_bg, lun_bg, stop_bg) {
                warn!(error = %e, "iSCSI target ended with an error");
            }
        })
        .map_err(|e| dev(format!("spawn iSCSI target: {e}")))?;

    // From here on the target is running, so every exit has to stop it.
    let attached = (|| -> Result<Attached> {
        let target = iscsi_win::login(&address.to_string(), port, iqn)?;
        let disk_number = target.disk_number;
        let physical = format!(r"\\.\PhysicalDrive{disk_number}");

        // The size check, before anything is written through this handle.
        let device = open_write_retry(&physical)?;
        if device.length() != virtual_size {
            return Err(dev(format!(
                "{physical} is {} bytes, expected {virtual_size} — refusing to touch a disk \
                 that is not the attached image",
                device.length()
            )));
        }
        let _ = device.update_properties();
        if read_write {
            // Offline or read-only at the disk level would stop the filesystem
            // driver before it started.
            if let Err(e) = device.set_online_this_session() {
                warn!(error = %e, "could not bring the iSCSI disk online — it may show offline in Disk Management");
            }
        }
        drop(device);

        info!(physical = %physical, disk = disk_number, read_write, "image attached and online");
        Ok(Attached {
            physical,
            disk_number,
            target: Some(target),
            stop: stop.clone(),
            server: None, // filled in below, once the value exists
        })
    })();

    match attached {
        Ok(mut a) => {
            a.server = Some(server);
            Ok(a)
        }
        Err(e) => {
            stop.store(true, Ordering::SeqCst);
            let _ = server.join();
            Err(e)
        }
    }
}

/// Where to put the target. Loopback first — nothing leaves the machine — and
/// the machine's own address second, for an initiator that would not come to
/// `127.0.0.1`.
fn candidate_addresses() -> Vec<IpAddr> {
    let mut out = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    if let Some(own) = own_address() {
        if own != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            out.push(own);
        }
    }
    out
}

/// This machine's own address, asked of the routing table rather than of DNS:
/// a UDP socket that never sends anything still has to choose a source address.
fn own_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// The disk appears a moment after the session does; opening it for write is
/// what proves PnP has finished with it.
fn open_write_retry(path: &str) -> Result<WinRawDevice> {
    let mut last: Option<OwnBackupError> = None;
    for attempt in 0..20 {
        match WinRawDevice::open_write(path) {
            Ok(device) => return Ok(device),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 + 50 * attempt));
            }
        }
    }
    Err(last.unwrap_or_else(|| dev(format!("cannot open {path}"))))
}
