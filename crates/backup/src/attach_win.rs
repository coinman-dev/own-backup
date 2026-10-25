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
use std::time::Duration;
use zeroize::Zeroizing;

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
        // Runs twice for every `detach()` — once there, once in `Drop` — so the
        // disk number is TAKEN, not read. After the logout that number belongs
        // to nobody, and the next disk Windows numbers gets it: the second run
        // used to force-dismount and offline whatever disk that was (seen
        // 2026-09-28 as 11 s of retries on a `\\.\PhysicalDrive5` already gone).
        let disk_number = std::mem::replace(&mut self.disk_number, u32::MAX);
        // Anything Windows mounted from this disk has to let go first, or the
        // logout leaves a device nobody can remove.
        if disk_number != u32::MAX {
            let_go_of_disk(disk_number, &self.physical);
        }
        if let Some(target) = self.target.take() {
            // The target stops below either way: this may be the end of a
            // backup, which cannot wait forever. What it leaves behind is an
            // orphaned session, and the next login to this image — or the next
            // service start — logs it out.
            if !target.logout() {
                warn!(
                    physical = %self.physical,
                    "attached image: the initiator kept the session — it is left for the next \
                     login to this image to clear"
                );
            }
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

/// Take down what Windows built on an attached image's disk, so the initiator
/// can log the session out: every volume force-dismounted, then the disk
/// offline for this session (its read-only flag and persisted attributes
/// untouched).
///
/// ONLY for a disk known to be the image. Force-dismounting throws away cached
/// writes — right for an image, whose volumes are Windows' guesses, and data
/// loss on a real disk. Callers pass a disk the session named as a disk and
/// that is exactly the image's size.
pub(crate) fn let_go_of_disk(disk_number: u32, physical: &str) {
    for volume in crate::volume_win::all_volume_roots_on_disk(disk_number) {
        let dismounted = crate::volume_win::dismount_volume_forced(&volume);
        info!(device = %volume, dismounted, "attached image: volume force-dismounted");
    }
    // A dismounted volume can still keep the initiator's device busy while
    // PnP tears its stack down. Offline the disk for this session before
    // LogoutIScsiTarget.
    match open_write_retry(physical) {
        Ok(device) => {
            if let Err(e) = device.set_offline_this_session() {
                warn!(physical = %physical, error = %e,
                      "attached image: could not offline disk before iSCSI logout");
            }
            let _ = device.update_properties();
        }
        Err(e) => warn!(physical = %physical, error = %e,
                        "attached image: could not open disk before iSCSI logout"),
    }
}

/// Give up on a session that was logged in but will not be used. Best effort,
/// like every teardown: the error that brought us here is the one to report.
fn log_out_unused(target: AttachedTarget, physical: &str) {
    if !target.logout() {
        warn!(
            physical = %physical,
            "iSCSI: the initiator kept a session we are abandoning — the next login to this \
             image clears it"
        );
    }
}

/// How long the end of a backup waits for its image to be attached, and later
/// detached, before it goes on without (normally 4 s each). The calls inside
/// block in the kernel and cannot be interrupted: a device PnP is stuck on
/// held the whole machine's PnP for hours (2026-10-24), and the backup with it.
pub const ATTACH_LIMIT: Duration = Duration::from_secs(5 * 60);
pub const DETACH_LIMIT: Duration = Duration::from_secs(5 * 60);

/// [`attach_rw`] with a deadline, for the exclusion pass at the end of a
/// backup. On time it is exactly `attach_rw`. Past `limit` the caller gets an
/// error and goes on as after any failed attach (the image keeps zero-filled
/// entries, reported as such); the attach goes on in its thread, and if the
/// session comes up after all, that thread takes it down — volumes, logout,
/// target, in that order and once — since nobody else will.
pub fn attach_rw_within(
    source: Box<dyn WritableBlockSource>,
    virtual_size: u64,
    serial: &str,
    limit: Duration,
) -> Result<Attached> {
    let serial_bg = serial.to_string();
    crate::deadline::run_within(
        "iscsi-attach",
        limit,
        move || attach_rw(source, virtual_size, &serial_bg),
        |late| {
            if let Ok(late) = late {
                warn!(physical = %late.physical,
                      "attached image: came up after the backup stopped waiting — taking it down");
                late.detach();
            }
        },
    )
    .unwrap_or_else(|_| {
        warn!(
            serial,
            limit_s = limit.as_secs(),
            "attached image: Windows did not attach the image in time — going on without it"
        );
        Err(dev(format!(
            "Windows did not attach the image within {} min (the iSCSI login or the disk's \
             arrival did not finish; a stuck device may be holding Plug and Play)",
            limit.as_secs() / 60
        )))
    })
}

/// [`Attached::detach`] with a deadline. `false` when the teardown did not end
/// in time: it goes on in its thread, and until it ends Windows may still write
/// through the session into whatever the source writes to — the caller must
/// not use that (a scratch overlay) as the image's new content.
pub fn detach_within(attached: Attached, limit: Duration) -> bool {
    let physical = attached.physical.clone();
    let done =
        crate::deadline::run_within("iscsi-detach", limit, move || attached.detach(), |()| {});
    if done.is_err() {
        warn!(physical = %physical, limit_s = limit.as_secs(),
              "attached image: detaching did not finish in time — its writes are not used");
    }
    done.is_ok()
}

/// Serve `source` read-write and attach it. Writes go wherever the source puts
/// them — for the exclusion pass, a scratch overlay beside the image.
pub fn attach_rw(
    source: Box<dyn WritableBlockSource>,
    virtual_size: u64,
    serial: &str,
) -> Result<Attached> {
    attach(
        Lun::read_write(source).with_serial(serial),
        virtual_size,
        serial,
        true,
    )
}

/// Serve `source` read-only and attach it. Windows sees a write-protected disk,
/// so nothing it does can reach the backup.
pub fn attach_ro(
    source: Box<dyn BlockSource>,
    virtual_size: u64,
    serial: &str,
) -> Result<Attached> {
    attach(
        Lun::read_only(source).with_serial(serial),
        virtual_size,
        serial,
        false,
    )
}

fn attach(lun: Lun, virtual_size: u64, serial: &str, read_write: bool) -> Result<Attached> {
    info!(
        backend = "iscsi",
        serial, virtual_size, read_write, "attaching the image"
    );
    let iqn = iscsi::iqn_for(serial);
    let lun = Arc::new(Mutex::new(lun));
    let mut raw_secret = [0u8; 16];
    getrandom::getrandom(&mut raw_secret)
        .map_err(|e| dev(format!("iSCSI CHAP secret generation failed: {e}")))?;
    let chap_secret = Arc::new(Zeroizing::new(raw_secret));

    let mut last: Option<OwnBackupError> = None;
    for address in candidate_addresses() {
        match attach_at(
            address,
            &iqn,
            lun.clone(),
            virtual_size,
            read_write,
            chap_secret.clone(),
        ) {
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
    chap_secret: Arc<Zeroizing<[u8; 16]>>,
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
    let chap_bg = chap_secret.clone();
    let server = std::thread::Builder::new()
        .name("iscsi-target".into())
        .spawn(move || {
            if let Err(e) = iscsi::serve_listener_chap(listener, iqn_bg, lun_bg, stop_bg, chap_bg) {
                warn!(error = %e, "iSCSI target ended with an error");
            }
        })
        .map_err(|e| dev(format!("spawn iSCSI target: {e}")))?;

    // From here on the target is running, so every exit has to stop it — and
    // once the login succeeded, every exit has to log out BEFORE it stops. A
    // target stopped under a live session leaves that session behind, and the
    // next address then found it and could not clear it (2026-09-25).
    let attached = (|| -> Result<Attached> {
        let target = iscsi_win::login(&address.to_string(), port, iqn, &chap_secret, virtual_size)?;
        let disk_number = target.disk_number;
        let physical = format!(r"\\.\PhysicalDrive{disk_number}");

        // The size check, before anything is written through this handle. The
        // login chose the disk by the same size, through a read handle; this is
        // the handle that writes, so it is asked again.
        let device = match open_write_retry(&physical) {
            Ok(device) => device,
            Err(e) => {
                // The disk is the image (the login measured it), Windows just
                // will not hand it over: let go of what it built, then log out.
                let_go_of_disk(disk_number, &physical);
                log_out_unused(target, &physical);
                return Err(e);
            }
        };
        if device.length() != virtual_size {
            let length = device.length();
            drop(device);
            // Not ours after all: log out without touching this disk.
            log_out_unused(target, &physical);
            return Err(dev(format!(
                "{physical} is {length} bytes, expected {virtual_size} — refusing to touch a \
                 disk that is not the attached image"
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
