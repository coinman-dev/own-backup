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
//! NOP and logout. No digests or error recovery beyond level 0, one connection
//! per session. Production mounts require a fresh one-way CHAP secret; discovery
//! sessions can list the target but cannot access its blocks.
//!
//! Only the listener is here; the wire is in [`session`] and the commands in
//! [`scsi`]. All three are cross-platform on purpose: the protocol is the part
//! worth testing, and it tests on the machine the code is written on. Windows
//! contributes only the initiator side ([`crate::iscsi_win`]); the one decision
//! it makes that could hit the wrong disk — which device of a session is ours —
//! is in [`initiator`], for the same reason.

pub mod initiator;
pub mod scsi;
pub mod session;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

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
    format!(
        "{IQN_PREFIX}{}",
        if tail.is_empty() { "unnamed" } else { &tail }
    )
}

/// A stable, unique name for one image file, for the target's IQN and for the
/// serial INQUIRY reports. The file name alone is not unique (two jobs can
/// write `full.qcow2` in different folders) and the full path is not a legal
/// iSCSI name, so the name carries a short digest of the path.
pub fn serial_for_image(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
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
        let clean: String = serial
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .take(32)
            .collect();
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
            Backing::Ro(_) => Err(OwnBackupError::Device(
                "iSCSI: write to a read-only LUN".into(),
            )),
            Backing::Rw(s) => s.write_at(offset, data),
        }
    }
}

/// How long a connection may take over a PDU it has begun, and over logging in.
/// Generous by the standards of a loopback socket; the point is only that it is
/// finite (audit AUD-14).
const PDU_DEADLINE: Duration = Duration::from_secs(30);

/// How long a stopping target waits for its sessions to notice.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Windows may keep a blocking recv alive after shutdown through another
/// socket handle. Poll cancellation without shortening the protocol deadline.
const SOCKET_POLL: Duration = Duration::from_millis(250);

struct SessionSocket {
    socket: TcpStream,
    stop: Arc<AtomicBool>,
    read_deadline: Arc<Mutex<Option<Instant>>>,
    write_deadline: Duration,
}

impl SessionSocket {
    fn check_stop(&self) -> io::Result<()> {
        if self.stop.load(Ordering::SeqCst) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "iSCSI target stopped",
            ))
        } else {
            Ok(())
        }
    }
}

impl Read for SessionSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check_stop()?;
            let deadline = *self
                .read_deadline
                .lock()
                .map_err(|_| io::Error::other("iSCSI deadline poisoned"))?;
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "iSCSI PDU deadline",
                ));
            }
            match self.socket.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
    }
}

impl Write for SessionSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = Instant::now() + self.write_deadline;
        loop {
            self.check_stop()?;
            if Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "iSCSI write deadline",
                ));
            }
            match self.socket.write(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_stop()?;
        self.socket.flush()
    }
}

/// One accepted connection: the thread serving it, and a second handle on its
/// socket so the connection can be broken from outside.
///
/// The second handle is the whole point. A session thread spends its life
/// blocked in `read`, and there is no portable way to interrupt that — but
/// shutdown wakes the read where supported, while SessionSocket also checks
/// cancellation between bounded waits on Windows. Without it, stopping left
/// every session running and the
/// image open behind them (audit AUD-14).
struct Accepted {
    thread: std::thread::JoinHandle<()>,
    socket: Option<TcpStream>,
    peer: SocketAddr,
}

impl Accepted {
    fn hang_up(&self) {
        if let Some(s) = &self.socket {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Serve `lun` under `iqn` until `stop` is set.
///
/// Accept is polled rather than blocked on, so the caller can take the target
/// down between sessions — which is how every mount here ends: the volumes are
/// dismounted, the initiator logs out, and this returns.
///
/// Returning also means every session it accepted has ended: the LUN — and the
/// image file under it — is not still open in some thread nobody is holding.
pub fn serve_listener(
    listener: TcpListener,
    iqn: String,
    lun: Arc<Mutex<Lun>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    serve_listener_paced(listener, iqn, lun, stop, None, PDU_DEADLINE, STOP_GRACE)
}

/// Production mount: only an initiator holding this ephemeral per-target CHAP
/// secret can enter a normal session and read or write image blocks.
pub fn serve_listener_chap(
    listener: TcpListener,
    iqn: String,
    lun: Arc<Mutex<Lun>>,
    stop: Arc<AtomicBool>,
    secret: Arc<Zeroizing<[u8; 16]>>,
) -> Result<()> {
    serve_listener_paced(
        listener,
        iqn,
        lun,
        stop,
        Some(secret),
        PDU_DEADLINE,
        STOP_GRACE,
    )
}

/// The same, with the two durations named — so a test does not have to wait
/// half a minute to watch a deadline work.
fn serve_listener_paced(
    listener: TcpListener,
    iqn: String,
    lun: Arc<Mutex<Lun>>,
    stop: Arc<AtomicBool>,
    chap_secret: Option<Arc<Zeroizing<[u8; 16]>>>,
    pdu_deadline: Duration,
    stop_grace: Duration,
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
    let mut sessions: Vec<Accepted> = Vec::new();

    while !stop.load(Ordering::SeqCst) {
        // Sessions that have ended keep nothing alive; dropping them here is
        // what stops this list growing for the life of a long mount.
        sessions.retain(|s| !s.thread.is_finished());

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
                // Short socket waits check cancellation; the session keeps a
                // separate deadline for the whole login/PDU, not for each poll.
                let poll = pdu_deadline.min(SOCKET_POLL);
                if let Err(e) = stream
                    .set_read_timeout(Some(poll))
                    .and_then(|()| stream.set_write_timeout(Some(poll)))
                {
                    warn!(%peer, error = %e, "iSCSI: cannot configure bounded socket waits");
                    continue;
                }

                // A second handle on the same socket, kept here. It is not for
                // reading or writing — only for breaking the connection when
                // the target stops, which is the only way to get a thread out
                // of a blocking read.
                let leash = stream.try_clone().ok();
                let deadline = Arc::new(Mutex::new(Some(Instant::now() + pdu_deadline)));

                let lun = lun.clone();
                let iqn = iqn.clone();
                let chap_secret = chap_secret.clone();
                let open_bg = open.clone();
                let stop_bg = stop.clone();
                open.fetch_add(1, Ordering::SeqCst);
                let spawned = std::thread::Builder::new()
                    .name("iscsi-session".into())
                    .spawn(move || {
                        let mut stream = SessionSocket {
                            socket: stream,
                            stop: stop_bg,
                            read_deadline: deadline.clone(),
                            write_deadline: pdu_deadline,
                        };
                        let mut sess = session::Session::new(iqn, lun);
                        if let Some(secret) = chap_secret {
                            sess = sess.with_chap(secret);
                        }
                        sess = sess.with_pace(move |pace| {
                            let end = match pace {
                                session::Pace::Idle => None,
                                session::Pace::InPdu => Some(Instant::now() + pdu_deadline),
                            };
                            if let Ok(mut clock) = deadline.lock() {
                                *clock = end;
                            }
                        });
                        match sess.run(&mut stream) {
                            Ok(()) => info!(%peer, "iSCSI initiator disconnected"),
                            Err(e) => warn!(%peer, error = %e, "iSCSI session ended with an error"),
                        }
                        open_bg.fetch_sub(1, Ordering::SeqCst);
                    });
                match spawned {
                    Ok(thread) => sessions.push(Accepted {
                        thread,
                        socket: leash,
                        peer,
                    }),
                    Err(e) => {
                        open.fetch_sub(1, Ordering::SeqCst);
                        warn!(error = %e, "iSCSI session thread: not started");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(OwnBackupError::Device(format!("iSCSI accept: {e}"))),
        }
    }

    // Stopping means stopping. Break every connection still open, then wait a
    // bounded time for the threads to notice; the image stays open behind any
    // that do not, so this is not a formality.
    for s in &sessions {
        if !s.thread.is_finished() {
            info!(peer = %s.peer, "iSCSI: hanging up on a session that is still open");
            s.hang_up();
        }
    }
    let deadline = Instant::now() + stop_grace;
    while Instant::now() < deadline && sessions.iter().any(|s| !s.thread.is_finished()) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stuck = 0usize;
    for s in sessions {
        if s.thread.is_finished() {
            let _ = s.thread.join();
        } else {
            // Waiting forever would hang whoever asked for the unmount, which
            // is worse than one thread outliving its target. It is named, so
            // it can be looked for.
            warn!(peer = %s.peer, "iSCSI: a session did not end in time and is left running");
            stuck += 1;
        }
    }

    info!(
        %addr,
        accepted,
        stuck,
        still_open = open.load(Ordering::SeqCst),
        "iSCSI target stopped"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

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

    // --- Остановка цели действительно останавливает её (аудит AUD-14) -------
    //
    // Потоки сессий никуда не записывались: stop проверялся только в цикле
    // accept, а сессия его не видела вовсе. Остановленная цель оставляла
    // потоки жить, а вместе с ними — открытый образ под ними.

    fn a_tiny_target() -> (TcpListener, Arc<Mutex<Lun>>, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let lun = Arc::new(Mutex::new(Lun::read_only(Box::new(
            scsi::tests::MemSource::new(4096),
        ))));
        (listener, lun, Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn session_socket_enforces_the_pdu_deadline_even_when_data_is_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(SOCKET_POLL)).unwrap();
        client.write_all(&[7]).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let deadline = Arc::new(Mutex::new(Some(Instant::now() - Duration::from_secs(1))));
        let mut paced = SessionSocket {
            socket,
            stop: stop.clone(),
            read_deadline: deadline.clone(),
            write_deadline: Duration::from_secs(30),
        };
        let mut byte = [0];
        assert_eq!(
            paced.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // An idle logged-in session has no PDU deadline, so the queued byte
        // remains readable; the polling interval did not become its lifetime.
        *deadline.lock().unwrap() = None;
        assert_eq!(paced.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [7]);
        stop.store(true, Ordering::SeqCst);
        assert_eq!(
            paced.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            paced.write(&[8]).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    /// Подключение, которое молчит, раньше держало слот сессии до конца жизни
    /// процесса. Четырёх таких хватало, чтобы цель перестала принимать
    /// настоящего инициатора.
    #[test]
    fn a_connection_that_never_logs_in_lets_go_of_its_slot() {
        let (listener, lun, stop) = a_tiny_target();
        let addr = listener.local_addr().unwrap();
        let stop_bg = stop.clone();
        let server = std::thread::spawn(move || {
            serve_listener_paced(
                listener,
                "iqn.test:one".into(),
                lun,
                stop_bg,
                None,
                Duration::from_millis(150), // дедлайн на PDU и на вход
                Duration::from_secs(2),
            )
        });

        // Подключаемся и не говорим ничего.
        let silent = TcpStream::connect(addr).unwrap();
        // Дедлайн должен сработать сам, без нашего участия.
        let gave_up = Instant::now() + Duration::from_secs(3);
        let mut buf = [0u8; 1];
        silent
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut closed = false;
        while Instant::now() < gave_up {
            match (&silent).read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        assert!(closed, "сессия обязана закончиться сама, а не ждать вечно");

        stop.store(true, Ordering::SeqCst);
        server.join().unwrap().unwrap();
    }

    /// Остановка обрывает принятые соединения и ДОЖИДАЕТСЯ потоков: пока
    /// serve_listener не вернулся, образ под LUN считается открытым.
    #[test]
    fn stopping_the_target_ends_the_sessions_it_accepted() {
        let (listener, lun, stop) = a_tiny_target();
        let addr = listener.local_addr().unwrap();
        let stop_bg = stop.clone();
        let watch = lun.clone();
        let (finished, completion) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let result = serve_listener_paced(
                listener,
                "iqn.test:one".into(),
                lun,
                stop_bg,
                None,
                Duration::from_secs(60), // долгий дедлайн: проверяем не его
                Duration::from_secs(30),
            );
            let _ = finished.send(result);
        });

        let client = TcpStream::connect(addr).unwrap();
        // Wait for an accepted session to own the LUN. A fixed sleep could
        // stop the listener before it accepted anything on a loaded runner.
        let accepted_deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&watch) < 3 && Instant::now() < accepted_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let accepted = Arc::strong_count(&watch) >= 3;

        stop.store(true, Ordering::SeqCst);
        // Completion must precede both the 30-second stop grace and the
        // 60-second read timeout, without a fragile three-second wall clock.
        completion
            .recv_timeout(Duration::from_secs(10))
            .expect("остановка ждёт льготный срок или дедлайн чтения")
            .unwrap();
        server.join().unwrap();
        assert!(accepted, "слушатель не принял тестовое соединение");

        // Никто больше не держит LUN — значит, и образ под ним закрыт.
        assert_eq!(Arc::strong_count(&watch), 1, "сессия всё ещё держит LUN");
        drop(client);
    }

    /// Обычный случай — инициатор ушёл сам — тоже должен закрываться без
    /// ожидания льготного срока.
    #[test]
    fn a_session_the_initiator_closed_needs_no_grace_period() {
        let (listener, lun, stop) = a_tiny_target();
        let addr = listener.local_addr().unwrap();
        let stop_bg = stop.clone();
        let server = std::thread::spawn(move || {
            serve_listener_paced(
                listener,
                "iqn.test:one".into(),
                lun,
                stop_bg,
                None,
                Duration::from_secs(60),
                Duration::from_secs(30), // заведомо больше, чем тест готов ждать
            )
        });

        let client = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(client); // инициатор отключился

        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::SeqCst);
        let started = Instant::now();
        server.join().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "завершившаяся сессия не должна заставлять ждать льготный срок"
        );
    }
}
