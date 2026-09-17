//! An iSCSI target of our own, so a backup image can become a real disk
//! without a driver of ours.
//!
//! This is what replaces Arsenal Image Mounter. AIM is a signed kernel driver
//! derived from AGPL sources: it cannot be shipped with a PolyForm
//! Noncommercial product, and it has to be installed before anything works.
//! Windows already contains the other half of the same idea — the Microsoft
//! iSCSI initiator, `MSiSCSI`, a service present in every edition and in WinPE.
//! Give it a target on the loopback address and it produces
//! `\\.\PhysicalDriveN` for us, mounts the filesystem inside, and lets the FS
//! driver itself delete the excluded files. Nothing is installed, nothing is
//! signed, nothing is AGPL.
//!
//! The protocol served here is the small part of RFC 7143 that one local
//! initiator uses: login, text (SendTargets), SCSI command, data in and out,
//! NOP and logout. No digests, no authentication, no error recovery beyond
//! level 0, one connection per session — all of which is negotiated honestly
//! in [`session`], and refused rather than faked when the initiator asks for
//! something else.
//!
//! Only the listener is here; the wire is in [`session`] and the commands in
//! [`scsi`]. All three are cross-platform on purpose: the protocol is the part
//! worth testing, and it tests on the machine the code is written on. Windows
//! contributes only the initiator side ([`crate::iscsi_win`]).

pub mod scsi;
pub mod session;

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use own_backup_common::{OwnBackupError, Result};
use tracing::{info, warn};

use crate::block_source::{BlockSource, WritableBlockSource};

/// Naming authority for the targets we hand out. `iqn.<yyyy-mm>.<reversed
/// domain>:<name>` is the form RFC 7143 requires; the date is when the name was
/// first used, and it never changes afterwards.
pub const IQN_PREFIX: &str = "iqn.2026-08.dev.ownbackup:image.";

/// More than one session at a time is not a workload we have — but the
/// initiator opens a discovery session, drops it and opens the real one, and a
/// slow teardown can overlap the two.
const MAX_SESSIONS: u32 = 4;

/// An iSCSI name for this image. Lower case and stripped of everything the
/// standard does not allow (letters, digits, `-`, `.`, `:`), because a name the
/// initiator refuses to parse is a mount that fails for a reason nobody reads.
pub fn iqn_for(serial: &str) -> String {
    let tail: String = serial
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '.' => c,
            _ => '-',
        })
        .collect();
    let tail = tail.trim_matches('-').to_string();
    format!("{IQN_PREFIX}{}", if tail.is_empty() { "unnamed" } else { &tail })
}

/// A stable, unique name for one image file, for the target's IQN and for the
/// serial INQUIRY reports. The file name alone is not unique (two jobs can
/// write `full.qcow2` in different folders) and the full path is not a legal
/// iSCSI name, so the name carries a short digest of the path.
pub fn serial_for_image(path: &std::path::Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let digest = {
        // FNV-1a: a few lines, and all that is needed to tell two paths apart.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in path.to_string_lossy().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{:08x}", h as u32)
    };
    format!("{stem}-{digest}")
}

/// The single logical unit behind a target: one image, read-only or writable.
///
/// Read-only is not a flag on the caller's conscience — it is a different
/// backing, and a write reaching a read-only LUN is refused down in [`scsi`]
/// with a proper sense code rather than silently dropped.
pub struct Lun {
    backing: Backing,
    block_size: u32,
    /// What INQUIRY reports as the unit serial. Windows tells two mounted
    /// images apart by it, so it is the image's own name and not a constant.
    serial: String,
    /// Opcodes already refused once, so the same refusal is not written to the
    /// log hundreds of times. Whatever watches this machine's disks sends the
    /// same handful of commands in a burst about once a second for as long as
    /// an image is attached, and the second identical line says nothing the
    /// first did not.
    refused: [bool; 256],
}

enum Backing {
    Ro(Box<dyn BlockSource>),
    Rw(Box<dyn WritableBlockSource>),
}

impl Lun {
    /// 512 bytes: what every partition table, filesystem and tool on a backup
    /// image already assumes, and what the images are laid out in.
    pub const BLOCK_SIZE: u32 = 512;

    pub fn read_only(source: Box<dyn BlockSource>) -> Self {
        Self {
            backing: Backing::Ro(source),
            block_size: Self::BLOCK_SIZE,
            serial: "0001".into(),
            refused: [false; 256],
        }
    }

    pub fn read_write(source: Box<dyn WritableBlockSource>) -> Self {
        Self {
            backing: Backing::Rw(source),
            block_size: Self::BLOCK_SIZE,
            serial: "0001".into(),
            refused: [false; 256],
        }
    }

    /// True the FIRST time this opcode is refused, false every time after.
    pub(crate) fn first_refusal_of(&mut self, opcode: u8) -> bool {
        !std::mem::replace(&mut self.refused[opcode as usize], true)
    }

    /// The image's own serial, as INQUIRY reports it.
    pub fn with_serial(mut self, serial: &str) -> Self {
        // The field is fixed width on the wire and ASCII by definition.
        let clean: String =
            serial.chars().filter(|c| c.is_ascii_graphic()).take(32).collect();
        if !clean.is_empty() {
            self.serial = clean;
        }
        self
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self.backing, Backing::Ro(_))
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn size(&self) -> u64 {
        match &self.backing {
            Backing::Ro(s) => s.size(),
            Backing::Rw(s) => s.size(),
        }
    }

    /// Number of whole blocks. A trailing partial block is not addressable and
    /// is left out of the capacity we report.
    pub fn blocks(&self) -> u64 {
        self.size() / self.block_size as u64
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match &mut self.backing {
            Backing::Ro(s) => s.read_at(offset, buf),
            Backing::Rw(s) => s.read_at(offset, buf),
        }
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        match &mut self.backing {
            Backing::Ro(_) => {
                Err(OwnBackupError::Device("iSCSI: write to a read-only LUN".into()))
            }
            Backing::Rw(s) => s.write_at(offset, data),
        }
    }
}

/// Serve `lun` under `iqn` until `stop` is set.
///
/// Accept is polled rather than blocked on, so the caller can take the target
/// down between sessions — which is how every mount here ends: the volumes are
/// dismounted, the initiator logs out, and this returns.
pub fn serve_listener(
    listener: TcpListener,
    iqn: String,
    lun: Arc<Mutex<Lun>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let addr = listener
        .local_addr()
        .map_err(|e| OwnBackupError::Device(format!("iSCSI local_addr: {e}")))?;
    let size = lun.lock().map(|l| l.size()).unwrap_or(0);
    info!(%addr, target = %iqn, size, "iSCSI target listening");

    listener
        .set_nonblocking(true)
        .map_err(|e| OwnBackupError::Device(format!("iSCSI nonblocking: {e}")))?;

    let open = Arc::new(AtomicU32::new(0));
    let mut accepted: u64 = 0;

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // This hands out the contents of a backup. The initiator is the
                // kernel of THIS machine and reaches us over the address we
                // chose; anything arriving from elsewhere is not it.
                if !peer.ip().is_loopback() && peer.ip() != addr.ip() {
                    warn!(%peer, "iSCSI: refusing an initiator from another host");
                    continue;
                }
                if open.load(Ordering::SeqCst) >= MAX_SESSIONS {
                    warn!(%peer, "iSCSI: too many sessions, refusing");
                    continue;
                }
                accepted += 1;
                info!(%peer, accepted, "iSCSI initiator connected");
                stream.set_nodelay(true).ok();
                stream
                    .set_nonblocking(false)
                    .map_err(|e| OwnBackupError::Device(format!("iSCSI blocking: {e}")))?;

                let lun = lun.clone();
                let iqn = iqn.clone();
                let open_bg = open.clone();
                open.fetch_add(1, Ordering::SeqCst);
                let spawned = std::thread::Builder::new()
                    .name("iscsi-session".into())
                    .spawn(move || {
                        let mut stream = stream;
                        match session::Session::new(iqn, lun).run(&mut stream) {
                            Ok(()) => info!(%peer, "iSCSI initiator disconnected"),
                            Err(e) => warn!(%peer, error = %e, "iSCSI session ended with an error"),
                        }
                        open_bg.fetch_sub(1, Ordering::SeqCst);
                    });
                if let Err(e) = spawned {
                    open.fetch_sub(1, Ordering::SeqCst);
                    warn!(error = %e, "iSCSI session thread: not started");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(OwnBackupError::Device(format!("iSCSI accept: {e}"))),
        }
    }

    info!(%addr, accepted, still_open = open.load(Ordering::SeqCst), "iSCSI target stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_name_survives_whatever_the_image_is_called() {
        assert_eq!(
            iqn_for("DESKTOP-849SVK9_disk2_full_20260916-133400-f135"),
            "iqn.2026-08.dev.ownbackup:image.desktop-849svk9-disk2-full-20260916-133400-f135"
        );
        // Nothing the standard forbids may reach the wire.
        assert!(iqn_for("Копия C:\\ (1)").starts_with(IQN_PREFIX));
        assert!(iqn_for("Копия C:\\ (1)")[IQN_PREFIX.len()..]
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '.')));
        assert_eq!(iqn_for("***"), format!("{IQN_PREFIX}unnamed"));
    }

    #[test]
    fn a_read_only_lun_refuses_writes_instead_of_dropping_them() {
        let mut lun = Lun::read_only(Box::new(scsi::tests::MemSource::new(4096)));
        assert!(lun.is_read_only());
        assert_eq!(lun.blocks(), 8);
        let e = lun.write_at(0, &[1, 2, 3]).unwrap_err().to_string();
        assert!(e.contains("read-only"), "{e}");
    }
}
