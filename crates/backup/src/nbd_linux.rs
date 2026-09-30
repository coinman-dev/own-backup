//! In-process NBD block device for mounting backups on Linux (plan Q4).
//!
//! The service itself serves the disk blocks — no qemu-nbd, no nbd-client:
//! a `socketpair` is handed to the kernel NBD driver (`nbd.ko`, in-tree)
//! via ioctls, and a thread answers the kernel's read requests straight from
//! the image reader (qcow2 chain with decompression, or VHDX). The device
//! then looks like any disk (`/dev/nbdX`) and the volume inside is mounted
//! read-only with `mount -o ro,loop,offset=…`.
//!
//! Uses the classic ioctl attach (`NBD_SET_SOCK` + `NBD_DO_IT`) and the
//! old-style transmission protocol — stable kernel ABI since 2.6:
//! ```text
//! request:  magic 0x25609513 | type u32 | handle u64 | from u64 | len u32
//! reply:    magic 0x67446698 | error u32 | handle u64 [ + data for READ ]
//! ```
//! (all integers big-endian). Only READ/FLUSH/DISC are honoured — the device
//! is attached with `NBD_FLAG_READ_ONLY`, writes are refused with EPERM.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use own_backup_common::{OwnBackupError, Result};

// _IO(0xab, n) — the NBD ioctls carry no argument size bits.
const NBD_SET_SOCK: libc::c_ulong = 0xab00;
const NBD_SET_BLKSIZE: libc::c_ulong = 0xab01;
const NBD_SET_SIZE: libc::c_ulong = 0xab02;
const NBD_DO_IT: libc::c_ulong = 0xab03;
const NBD_CLEAR_SOCK: libc::c_ulong = 0xab04;
const NBD_CLEAR_QUE: libc::c_ulong = 0xab05;
const NBD_DISCONNECT: libc::c_ulong = 0xab08;
const NBD_SET_FLAGS: libc::c_ulong = 0xab0a;

const NBD_FLAG_HAS_FLAGS: libc::c_ulong = 1 << 0;
const NBD_FLAG_READ_ONLY: libc::c_ulong = 1 << 1;

const NBD_REQUEST_MAGIC: u32 = 0x2560_9513;
const NBD_REPLY_MAGIC: u32 = 0x6744_6698;

const NBD_CMD_READ: u16 = 0;
const NBD_CMD_WRITE: u16 = 1;
const NBD_CMD_DISC: u16 = 2;
const NBD_CMD_FLUSH: u16 = 3;

/// Defensive cap on a single request (the kernel caps around 32 MiB itself).
const MAX_REQUEST_LEN: u32 = 64 * 1024 * 1024;

/// What the NBD server needs from an image — the shared [`BlockSource`]
/// (same providers back the Windows iSCSI target).
pub use crate::block_source::BlockSource as NbdSource;

fn ioctl(file: &File, cmd: libc::c_ulong, arg: libc::c_ulong, what: &str) -> Result<()> {
    // SAFETY: `file` is a valid open descriptor; NBD ioctls take a plain
    // integer argument (or none).
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), cmd as _, arg) };
    if rc < 0 {
        return Err(OwnBackupError::Device(format!(
            "ioctl {what} on nbd device failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// A backup image attached as `/dev/nbdX`, served by our own threads.
/// MUST be detached with [`NbdAttachment::detach`]; the device dies with the
/// serving process, so this only lives inside the long-running service.
pub struct NbdAttachment {
    pub device: String,
    nbd_file: File,
    doit_thread: Option<JoinHandle<()>>,
    serve_thread: Option<JoinHandle<()>>,
}

impl NbdAttachment {
    /// Attach `source` read-only to the first free `/dev/nbdX`.
    pub fn attach(source: Box<dyn NbdSource>) -> Result<Self> {
        ensure_nbd_module()?;
        let (device, nbd_file) = find_free_device()?;
        let size = source.size();

        let (kernel_side, user_side) =
            UnixStream::pair().map_err(|e| OwnBackupError::Device(format!("socketpair: {e}")))?;

        ioctl(&nbd_file, NBD_SET_BLKSIZE, 512, "NBD_SET_BLKSIZE")?;
        ioctl(
            &nbd_file,
            NBD_SET_SIZE,
            size as libc::c_ulong,
            "NBD_SET_SIZE",
        )?;
        ioctl(
            &nbd_file,
            NBD_SET_FLAGS,
            NBD_FLAG_HAS_FLAGS | NBD_FLAG_READ_ONLY,
            "NBD_SET_FLAGS",
        )?;
        ioctl(
            &nbd_file,
            NBD_SET_SOCK,
            kernel_side.as_raw_fd() as libc::c_ulong,
            "NBD_SET_SOCK",
        )?;

        // NBD_DO_IT blocks for the whole lifetime of the attachment and
        // returns on disconnect — it needs its own thread and its own handle.
        let doit_file = nbd_file
            .try_clone()
            .map_err(|e| OwnBackupError::Device(format!("dup nbd fd: {e}")))?;
        let device_for_log = device.clone();
        let doit_thread = std::thread::Builder::new()
            .name("nbd-doit".into())
            .spawn(move || {
                // The kernel holds its own reference to the socket from
                // NBD_SET_SOCK; keep ours alive alongside for clarity.
                let _keep = kernel_side;
                // SAFETY: valid fd; blocks until NBD_DISCONNECT.
                let rc = unsafe { libc::ioctl(doit_file.as_raw_fd(), NBD_DO_IT as _, 0) };
                debug!(device = %device_for_log, rc, "NBD_DO_IT returned");
                let _ = unsafe { libc::ioctl(doit_file.as_raw_fd(), NBD_CLEAR_QUE as _, 0) };
                let _ = unsafe { libc::ioctl(doit_file.as_raw_fd(), NBD_CLEAR_SOCK as _, 0) };
            })
            .map_err(|e| OwnBackupError::Device(format!("spawn nbd-doit: {e}")))?;

        let serve_thread = std::thread::Builder::new()
            .name("nbd-serve".into())
            .spawn(move || {
                let mut stream = user_side;
                let mut source = source;
                if let Err(e) = serve_requests(&mut stream, source.as_mut()) {
                    warn!(error = %e, "NBD serve loop ended with an error");
                }
            })
            .map_err(|e| OwnBackupError::Device(format!("spawn nbd-serve: {e}")))?;

        // The device is usable once the kernel reports the size we set.
        let sys_size = format!("/sys/block/{}/size", device.trim_start_matches("/dev/"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let sectors: u64 = std::fs::read_to_string(&sys_size)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            if sectors * 512 >= size {
                break;
            }
            if Instant::now() > deadline {
                warn!(device = %device, "nbd device did not report its size in time");
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        info!(device = %device, size, "backup image attached as an NBD device");
        Ok(Self {
            device,
            nbd_file,
            doit_thread: Some(doit_thread),
            serve_thread: Some(serve_thread),
        })
    }

    /// Disconnect the device and join the serving threads.
    pub fn detach(mut self) -> Result<()> {
        ioctl(&self.nbd_file, NBD_DISCONNECT, 0, "NBD_DISCONNECT")?;
        if let Some(t) = self.doit_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.serve_thread.take() {
            let _ = t.join();
        }
        info!(device = %self.device, "NBD device detached");
        Ok(())
    }
}

impl Drop for NbdAttachment {
    fn drop(&mut self) {
        // Belt and suspenders for the error/unwind path; normal teardown goes
        // through detach(). A second DISCONNECT on a dead device just errors.
        if self.doit_thread.is_some() {
            let _ = unsafe { libc::ioctl(self.nbd_file.as_raw_fd(), NBD_DISCONNECT as _, 0) };
        }
    }
}

/// Load `nbd.ko` if `/dev/nbd0` is missing (in-tree module, present on every
/// mainstream kernel incl. WSL2). Best effort — the follow-up check reports.
pub(crate) fn ensure_nbd_module() -> Result<()> {
    if Path::new("/dev/nbd0").exists() {
        return Ok(());
    }
    let _ = std::process::Command::new("modprobe").arg("nbd").status();
    if !Path::new("/dev/nbd0").exists() {
        return Err(OwnBackupError::Device(
            "/dev/nbd0 not available — the kernel NBD module could not be loaded \
             (try `modprobe nbd` as root)"
                .into(),
        ));
    }
    Ok(())
}

/// A device is busy while `/sys/block/nbdX/pid` exists.
fn find_free_device() -> Result<(String, File)> {
    for i in 0..64 {
        let device = format!("/dev/nbd{i}");
        if !Path::new(&device).exists() {
            break;
        }
        if Path::new(&format!("/sys/block/nbd{i}/pid")).exists() {
            continue;
        }
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device)
        {
            Ok(f) => return Ok((device, f)),
            Err(e) => {
                debug!(device = %device, error = %e, "cannot open nbd device, trying next");
            }
        }
    }
    Err(OwnBackupError::Device(
        "no free /dev/nbdX device found (all busy, or no permission — root required)".into(),
    ))
}

/// Answer kernel NBD requests until DISC/EOF. Generic over the stream so the
/// protocol is unit-testable without a kernel device.
pub fn serve_requests<S: Read + Write>(stream: &mut S, source: &mut dyn NbdSource) -> Result<()> {
    let size = source.size();
    let mut header = [0u8; 28];
    loop {
        if let Err(e) = stream.read_exact(&mut header) {
            // EOF after disconnect is the normal way out.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(e.into());
        }
        let magic = u32::from_be_bytes(header[0..4].try_into().unwrap());
        if magic != NBD_REQUEST_MAGIC {
            return Err(OwnBackupError::Device(format!(
                "bad NBD request magic {magic:#x}"
            )));
        }
        // Upper 16 bits carry command flags (e.g. FUA) — irrelevant here.
        let cmd = u32::from_be_bytes(header[4..8].try_into().unwrap()) as u16;
        let handle: [u8; 8] = header[8..16].try_into().unwrap();
        let from = u64::from_be_bytes(header[16..24].try_into().unwrap());
        let len = u32::from_be_bytes(header[24..28].try_into().unwrap());

        match cmd {
            NBD_CMD_READ => {
                if len > MAX_REQUEST_LEN
                    || from.checked_add(len as u64).is_none_or(|end| end > size)
                {
                    reply(stream, libc::EINVAL as u32, &handle, &[])?;
                    continue;
                }
                let mut data = vec![0u8; len as usize];
                match source.read_at(from, &mut data) {
                    Ok(()) => reply(stream, 0, &handle, &data)?,
                    Err(e) => {
                        warn!(from, len, error = %e, "NBD read failed");
                        reply(stream, libc::EIO as u32, &handle, &[])?;
                    }
                }
            }
            NBD_CMD_WRITE => {
                // Read-only attach; still must drain the payload to stay in sync.
                let mut sink = vec![0u8; len.min(MAX_REQUEST_LEN) as usize];
                stream.read_exact(&mut sink)?;
                reply(stream, libc::EPERM as u32, &handle, &[])?;
            }
            NBD_CMD_FLUSH => reply(stream, 0, &handle, &[])?,
            NBD_CMD_DISC => {
                debug!("NBD disconnect request");
                return Ok(());
            }
            other => {
                debug!(cmd = other, "unsupported NBD command");
                reply(stream, libc::EINVAL as u32, &handle, &[])?;
            }
        }
    }
}

fn reply<S: Write>(stream: &mut S, error: u32, handle: &[u8; 8], data: &[u8]) -> Result<()> {
    let mut buf = Vec::with_capacity(16 + data.len());
    buf.extend_from_slice(&NBD_REPLY_MAGIC.to_be_bytes());
    buf.extend_from_slice(&error.to_be_bytes());
    buf.extend_from_slice(handle);
    buf.extend_from_slice(data);
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemSource(Vec<u8>);
    impl NbdSource for MemSource {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let off = offset as usize;
            buf.copy_from_slice(&self.0[off..off + buf.len()]);
            Ok(())
        }
    }

    fn request(cmd: u16, handle: u64, from: u64, len: u32) -> Vec<u8> {
        let mut r = Vec::with_capacity(28);
        r.extend_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        r.extend_from_slice(&(cmd as u32).to_be_bytes());
        r.extend_from_slice(&handle.to_be_bytes());
        r.extend_from_slice(&from.to_be_bytes());
        r.extend_from_slice(&len.to_be_bytes());
        r
    }

    fn read_reply(stream: &mut impl Read, data_len: usize) -> (u32, u64, Vec<u8>) {
        let mut head = [0u8; 16];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(
            u32::from_be_bytes(head[0..4].try_into().unwrap()),
            NBD_REPLY_MAGIC
        );
        let error = u32::from_be_bytes(head[4..8].try_into().unwrap());
        let handle = u64::from_be_bytes(head[8..16].try_into().unwrap());
        let mut data = vec![0u8; if error == 0 { data_len } else { 0 }];
        stream.read_exact(&mut data).unwrap();
        (error, handle, data)
    }

    /// Full protocol conversation over a socketpair — no kernel involved.
    #[test]
    fn serves_reads_and_refuses_writes() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let disk: Vec<u8> = (0..131072u32).map(|i| (i % 251) as u8).collect();
        let expected = disk.clone();
        let handle = std::thread::spawn(move || {
            let mut source = MemSource(disk);
            serve_requests(&mut server, &mut source).unwrap();
        });

        // Read 4 KiB from an odd offset.
        client
            .write_all(&request(NBD_CMD_READ, 7, 12345, 4096))
            .unwrap();
        let (err, h, data) = read_reply(&mut client, 4096);
        assert_eq!((err, h), (0, 7));
        assert_eq!(data, expected[12345..12345 + 4096]);

        // Reads beyond the end are rejected, not crashed on.
        client
            .write_all(&request(NBD_CMD_READ, 8, 131000, 4096))
            .unwrap();
        let (err, h, _) = read_reply(&mut client, 0);
        assert_eq!((err as i32, h), (libc::EINVAL, 8));

        // Writes are refused with EPERM (payload drained, stream stays in sync).
        let mut wr = request(NBD_CMD_WRITE, 9, 0, 512);
        wr.extend_from_slice(&[0xAA; 512]);
        client.write_all(&wr).unwrap();
        let (err, h, _) = read_reply(&mut client, 0);
        assert_eq!((err as i32, h), (libc::EPERM, 9));

        // Flush is a no-op success; DISC ends the loop cleanly.
        client.write_all(&request(NBD_CMD_FLUSH, 10, 0, 0)).unwrap();
        let (err, _, _) = read_reply(&mut client, 0);
        assert_eq!(err, 0);
        client.write_all(&request(NBD_CMD_DISC, 11, 0, 0)).unwrap();
        handle.join().unwrap();
    }
}
