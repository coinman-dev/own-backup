//! The machine's power state around a backup on Linux — the counterpart of
//! `power_win`: stay awake while copying, wake for a scheduled copy, sleep or
//! turn off after one, and hear that the machine is shutting down.
//!
//! All of it goes through systemd-logind over D-Bus (`org.freedesktop.login1`),
//! the one component every desktop and `systemctl suspend` itself defer to:
//!
//! * staying awake is a `block` inhibitor lock on `sleep` — a file descriptor;
//!   the lock lasts exactly as long as the descriptor is open, so a killed
//!   engine cannot leave the machine unable to sleep;
//! * a shutdown is announced by `PrepareForShutdown`, and a `delay` lock on
//!   `shutdown` gives the engine the few seconds it needs to hear it before
//!   systemd starts stopping units;
//! * "sleep after the copy" is `Suspend`, "turn off" is `ScheduleShutdown`,
//!   which logind announces to everyone signed in and `shutdown -c` cancels —
//!   the same as Windows' own countdown;
//! * waking for a copy is a `CLOCK_REALTIME_ALARM` timer, which the kernel
//!   programs into the RTC while the machine sleeps. It needs `CAP_WAKE_ALARM`,
//!   which the engine has as root.
//!
//! Everything is best-effort the way `power_win` is: without logind (a
//! container, a minimal system) a copy still runs, it is only not protected
//! from sleep, and the log says so.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use own_backup_common::tr;
use tracing::{info, warn};

/// `org.freedesktop.login1.Manager`, the part of it the engine uses.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Login1Manager {
    /// Take an inhibitor lock; it is held while the returned descriptor is open.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<zbus::OwnedFd>;

    fn suspend(&self, interactive: bool) -> zbus::Result<()>;

    /// `usec` is CLOCK_REALTIME in microseconds.
    fn schedule_shutdown(&self, kind: &str, usec: u64) -> zbus::Result<()>;

    fn set_wall_message(&self, wall_message: &str, enable: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn session_new(
        &self,
        session_id: String,
        object_path: zbus::OwnedObjectPath,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn session_removed(
        &self,
        session_id: String,
        object_path: zbus::OwnedObjectPath,
    ) -> zbus::Result<()>;
}

/// Who the inhibitor locks name in `systemd-inhibit --list`.
const WHO: &str = "Own Backup";

type Manager = Login1ManagerProxy<'static>;

// zbus is async only (6.0 dropped its blocking API). Everything here is
// called from plain threads — and, for a moment, from Tokio tasks — so each
// call is one `zbus::block_on` on the calling thread, never one inside
// another: zbus panics on that. A connection that outlives its call is run
// by zbus's helper thread until the next one.

async fn manager() -> zbus::Result<Manager> {
    let connection = zbus::Connection::system().await?;
    Login1ManagerProxy::new(&connection).await
}

fn system_manager() -> zbus::Result<Manager> {
    zbus::block_on(manager())
}

/// An inhibitor lock: logind holds it while this descriptor is open.
fn inhibit(what: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd> {
    inhibit_with(&system_manager()?, what, why, mode)
}

fn inhibit_with(manager: &Manager, what: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd> {
    Ok(OwnedFd::from(zbus::block_on(
        manager.inhibit(what, WHO, why, mode),
    )?))
}

/// The machine does not go to sleep while this is alive — neither on its own
/// idle timer nor by a desktop's "suspend" (they defer to logind's locks).
/// The display may still turn off. A suspend asked for with
/// `--ignore-inhibitors` by an administrator still happens; that is theirs.
pub struct KeepAwake(#[allow(dead_code)] OwnedFd);

impl KeepAwake {
    /// `reason` is what `systemd-inhibit --list` shows while it is held.
    pub fn new(reason: &str) -> Option<Self> {
        match inhibit("sleep", reason, "block") {
            Ok(fd) => Some(Self(fd)),
            Err(e) => {
                warn!(error = %e, "could not hold the machine awake (logind) — it may sleep");
                None
            }
        }
    }
}

/// The calling thread reads and writes at background priority while this is
/// alive — the counterpart of Windows' `THREAD_MODE_BACKGROUND_BEGIN`: the
/// lowest best-effort I/O priority (`ioprio_set`, class BE level 7, what
/// `ionice -c2 -n7` sets) and, for root, the lowest CPU priority (`nice` 19).
/// Both are per-thread on Linux. Not the idle I/O class: under a disk that is
/// never idle it would starve the copy until the engine watch gave up on it.
///
/// Must be dropped on the thread that made it; the blocking pool reuses
/// threads, and one left at background priority would slow whatever it runs
/// next. That is also why `nice` is touched only as root: an unprivileged
/// process cannot raise it back.
pub struct BackgroundIo {
    tid: libc::pid_t,
    ioprio: Option<libc::c_long>,
    nice: Option<libc::c_int>,
    _not_send: std::marker::PhantomData<*const ()>,
}

const IOPRIO_WHO_PROCESS: libc::c_long = 1;
const IOPRIO_CLASS_BE: libc::c_long = 2;
const IOPRIO_CLASS_SHIFT: libc::c_long = 13;

impl BackgroundIo {
    pub fn enter() -> Option<Self> {
        // SAFETY: plain syscalls on the calling thread's own id.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
        let old_io = unsafe {
            libc::syscall(
                libc::SYS_ioprio_get,
                IOPRIO_WHO_PROCESS,
                tid as libc::c_long,
            )
        };
        let ioprio = if old_io >= 0 {
            let lowest = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7;
            let set = unsafe {
                libc::syscall(
                    libc::SYS_ioprio_set,
                    IOPRIO_WHO_PROCESS,
                    tid as libc::c_long,
                    lowest,
                )
            };
            if set == 0 {
                Some(old_io)
            } else {
                warn!(error = %std::io::Error::last_os_error(),
                    "could not lower the copy's I/O priority");
                None
            }
        } else {
            warn!(error = %std::io::Error::last_os_error(),
                "could not read the copy's I/O priority");
            None
        };
        // SAFETY: plain calls; getpriority may legitimately return -1, so the
        // error is told by errno, cleared first.
        let nice = if unsafe { libc::geteuid() } == 0 {
            unsafe { *libc::__errno_location() = 0 };
            let old = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) };
            let read = std::io::Error::last_os_error().raw_os_error() == Some(0) || old != -1;
            if read && unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19) } == 0
            {
                Some(old)
            } else {
                warn!(error = %std::io::Error::last_os_error(),
                    "could not lower the copy's CPU priority");
                None
            }
        } else {
            None
        };
        if ioprio.is_none() && nice.is_none() {
            return None;
        }
        info!(
            io = ioprio.is_some(),
            cpu = nice.is_some(),
            "copy runs at background priority"
        );
        Some(Self {
            tid,
            ioprio,
            nice,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for BackgroundIo {
    fn drop(&mut self) {
        // SAFETY: same thread (the type is !Send); restoring what was read.
        unsafe {
            if let Some(old) = self.ioprio {
                if libc::syscall(
                    libc::SYS_ioprio_set,
                    IOPRIO_WHO_PROCESS,
                    self.tid as libc::c_long,
                    old,
                ) != 0
                {
                    warn!(error = %std::io::Error::last_os_error(),
                        "could not restore the I/O priority after the copy");
                }
            }
            if let Some(old) = self.nice {
                if libc::setpriority(libc::PRIO_PROCESS, self.tid as libc::id_t, old) != 0 {
                    warn!(error = %std::io::Error::last_os_error(),
                        "could not restore the CPU priority after the copy");
                }
            }
        }
    }
}

/// Put the machine to sleep now.
pub fn sleep_now() -> std::result::Result<(), String> {
    info!("after backup: putting the machine to sleep");
    system_manager()
        .and_then(|m| sleep_with(&m))
        .map_err(|e| tr!("power_linux.logind_failed", error = e).into_owned())
}

/// Not interactive: the engine is root, and nobody is there to answer polkit.
fn sleep_with(manager: &Manager) -> zbus::Result<()> {
    zbus::block_on(manager.suspend(false))
}

/// Turn the machine off after `grace_secs`. logind tells everyone signed in,
/// with `message`, and `shutdown -c` cancels it — as Windows' own notice does.
pub fn shutdown_in(grace_secs: u32, message: &str) -> std::result::Result<(), String> {
    info!(grace_secs, "after backup: shutting the machine down");
    let manager =
        system_manager().map_err(|e| tr!("power_linux.logind_failed", error = e).into_owned())?;
    shutdown_with(&manager, std::time::SystemTime::now(), grace_secs, message)
}

fn shutdown_with(
    manager: &Manager,
    now: std::time::SystemTime,
    grace_secs: u32,
    message: &str,
) -> std::result::Result<(), String> {
    let at = shutdown_time_usec(now, grace_secs)
        .ok_or_else(|| tr!("power_linux.clock_before_epoch").into_owned())?;
    // Our words in the announcement; without them it is logind's generic one.
    if let Err(e) = zbus::block_on(manager.set_wall_message(message, true)) {
        warn!(error = %e, "the shutdown notice keeps logind's own text");
    }
    zbus::block_on(manager.schedule_shutdown("poweroff", at))
        .map_err(|e| tr!("power_linux.logind_failed", error = e).into_owned())
}

/// CLOCK_REALTIME, in microseconds, `grace_secs` after `now`.
fn shutdown_time_usec(now: std::time::SystemTime, grace_secs: u32) -> Option<u64> {
    let since_epoch = now.duration_since(std::time::UNIX_EPOCH).ok()?;
    let at = since_epoch + Duration::from_secs(u64::from(grace_secs));
    u64::try_from(at.as_micros()).ok()
}

/// A `delay` lock on shutdown: logind waits (up to its `InhibitDelayMaxSec`,
/// five seconds by default) for it to be released after announcing a
/// shutdown, so the engine hears `PrepareForShutdown` before systemd starts
/// stopping it.
pub struct ShutdownDelay(#[allow(dead_code)] OwnedFd);

impl ShutdownDelay {
    pub fn take() -> Option<Self> {
        match inhibit(
            "shutdown",
            "finish or cancel the running backup cleanly",
            "delay",
        ) {
            Ok(fd) => Some(Self(fd)),
            Err(e) => {
                warn!(error = %e, "no shutdown delay lock (logind) — a shutdown is seen only as SIGTERM");
                None
            }
        }
    }
}

/// What logind says about the machine and its sessions, for the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogindEvent {
    /// A shutdown or reboot has begun (`PrepareForShutdown(true)`).
    ShuttingDown,
    /// It was called off (`PrepareForShutdown(false)`).
    ShutdownCancelled,
    SessionNew(String),
    SessionRemoved(String),
}

/// Follow logind's signals until `on_event` says to stop or the bus goes
/// away. Blocking: run it on a thread of its own.
pub fn follow_events(on_event: impl FnMut(LogindEvent) -> bool) -> std::result::Result<(), String> {
    let manager = system_manager().map_err(|e| e.to_string())?;
    follow_events_with(&manager, on_event)
}

fn follow_events_with(
    manager: &Manager,
    mut on_event: impl FnMut(LogindEvent) -> bool,
) -> std::result::Result<(), String> {
    // The signals are heard on a thread of their own, inside one `block_on`
    // that polls all three streams; `on_event` runs here, outside it. It
    // takes logind locks itself (`ShutdownDelay::take`), and a `block_on`
    // inside another one panics.
    let (tx, rx) = std::sync::mpsc::channel::<LogindEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let manager = manager.clone();
    std::thread::Builder::new()
        .name("logind-signals".into())
        .spawn(move || zbus::block_on(hear_signals(manager, tx, ready_tx)))
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv()
        .map_err(|_| "the logind signal thread ended".to_string())??;
    for event in rx {
        if !on_event(event) {
            return Ok(());
        }
    }
    Err("the system bus connection to logind was lost".into())
}

/// Subscribe to the three signals, say whether that worked, then forward
/// each one as it arrives until the bus or the receiver goes away. Within one
/// signal the order is the bus's; across them, a shutdown is looked at first.
async fn hear_signals(
    manager: Manager,
    tx: std::sync::mpsc::Sender<LogindEvent>,
    ready: std::sync::mpsc::Sender<std::result::Result<(), String>>,
) {
    use std::task::Poll;
    use zbus::export::futures_core::Stream;

    let subscribed = async {
        Ok::<_, zbus::Error>((
            manager.receive_prepare_for_shutdown().await?,
            manager.receive_session_new().await?,
            manager.receive_session_removed().await?,
        ))
    }
    .await;
    let (shutdown, new, removed) = match subscribed {
        Ok(streams) => {
            let _ = ready.send(Ok(()));
            streams
        }
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let mut shutdown = std::pin::pin!(shutdown);
    let mut new = std::pin::pin!(new);
    let mut removed = std::pin::pin!(removed);
    loop {
        // `None`: a stream ended — the connection is gone.
        let next = std::future::poll_fn(|cx| loop {
            let mut again = false;
            match shutdown.as_mut().poll_next(cx) {
                Poll::Ready(Some(signal)) => match signal.args() {
                    Ok(args) => {
                        return Poll::Ready(Some(if args.start {
                            LogindEvent::ShuttingDown
                        } else {
                            LogindEvent::ShutdownCancelled
                        }))
                    }
                    Err(_) => again = true,
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }
            match new.as_mut().poll_next(cx) {
                Poll::Ready(Some(signal)) => match signal.args() {
                    Ok(args) => return Poll::Ready(Some(LogindEvent::SessionNew(args.session_id))),
                    Err(_) => again = true,
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }
            match removed.as_mut().poll_next(cx) {
                Poll::Ready(Some(signal)) => match signal.args() {
                    Ok(args) => {
                        return Poll::Ready(Some(LogindEvent::SessionRemoved(args.session_id)))
                    }
                    Err(_) => again = true,
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }
            if !again {
                return Poll::Pending;
            }
        })
        .await;
        let Some(event) = next else { return };
        if tx.send(event).is_err() {
            return;
        }
    }
}

/// A timer that wakes the machine from sleep at the time it is armed for,
/// then holds it awake for `hold` so a scheduler that ticks once a minute has
/// time to start the copy (which then holds it awake by itself).
///
/// `CLOCK_REALTIME_ALARM`: wall-clock time, like the schedule itself, and the
/// kernel sets the RTC alarm from it when the machine suspends. Waking from
/// hibernation or power-off is not covered.
pub struct WakeTimer {
    fd: OwnedFd,
}

impl WakeTimer {
    /// Create the timer and the thread that answers it.
    pub fn start(hold: Duration) -> Option<Arc<Self>> {
        // SAFETY: plain syscall; the result is checked before it is used.
        let raw = unsafe { libc::timerfd_create(libc::CLOCK_REALTIME_ALARM, libc::TFD_CLOEXEC) };
        if raw < 0 {
            let e = std::io::Error::last_os_error();
            warn!(error = %e, "could not create the wake timer — scheduled copies will not wake the machine");
            return None;
        }
        // SAFETY: a fresh descriptor that nothing else owns.
        let timer = Arc::new(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        });
        let waiter = timer.clone();
        let _ = std::thread::Builder::new()
            .name("wake-timer".into())
            .spawn(move || loop {
                let mut expirations = 0u64;
                // SAFETY: an 8-byte read into a u64 from a timerfd we own.
                let n = unsafe {
                    libc::read(
                        waiter.fd.as_raw_fd(),
                        (&mut expirations as *mut u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                if n != std::mem::size_of::<u64>() as isize {
                    let e = std::io::Error::last_os_error();
                    // A clock change cancels nothing here (no
                    // TFD_TIMER_CANCEL_ON_SET); EINTR is just a retry.
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    warn!(error = %e, "wake timer wait failed; wake-ups stop");
                    return;
                }
                info!(
                    hold_s = hold.as_secs(),
                    "woken for a scheduled copy — holding the machine awake"
                );
                let _awake = KeepAwake::new(&tr!("power_linux.wake_reason"));
                std::thread::sleep(hold);
            });
        Some(timer)
    }

    /// Wake at `at`. Re-arming replaces the previous time.
    pub fn arm(&self, at: chrono::DateTime<chrono::Utc>) {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: at.timestamp(),
                tv_nsec: i64::from(at.timestamp_subsec_nanos()),
            },
        };
        self.set(&spec, libc::TFD_TIMER_ABSTIME);
    }

    /// Nothing to wake for: take the time back.
    pub fn disarm(&self) {
        // SAFETY: all-zero is a valid itimerspec and means "disarmed".
        let spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        self.set(&spec, 0);
    }

    fn set(&self, spec: &libc::itimerspec, flags: libc::c_int) {
        // SAFETY: a timerfd we own and a valid itimerspec; no old value asked.
        let rc = unsafe {
            libc::timerfd_settime(self.fd.as_raw_fd(), flags, spec, std::ptr::null_mut())
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            warn!(error = %e, "could not set the wake timer");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_ioprio() -> libc::c_long {
        // SAFETY: plain syscalls on this thread's own id.
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid);
            libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, tid)
        }
    }

    /// The copy thread is at the lowest best-effort I/O priority while the
    /// guard lives and back where it was after — the blocking pool reuses it.
    #[test]
    fn background_io_lowers_and_restores_the_thread() {
        std::thread::spawn(|| {
            let before = thread_ioprio();
            assert!(before >= 0);
            let guard = BackgroundIo::enter().expect("ioprio_set needs no privilege");
            assert_eq!(thread_ioprio(), (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7);
            drop(guard);
            assert_eq!(thread_ioprio(), before);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_shutdown_time_is_grace_seconds_from_now_in_microseconds() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(shutdown_time_usec(now, 90), Some(1_700_000_090 * 1_000_000));
        let before = std::time::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(shutdown_time_usec(before, 90), None);
    }

    /// A stand-in for logind on a private connection: it records what it is
    /// asked, in D-Bus terms, so the method names and signatures the engine
    /// sends are checked against what logind declares
    /// (org.freedesktop.login1(5)), not only against our own proxy.
    struct FakeLogind {
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl FakeLogind {
        fn inhibit(
            &self,
            what: &str,
            who: &str,
            why: &str,
            mode: &str,
        ) -> zbus::fdo::Result<zbus::OwnedFd> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("Inhibit({what}, {who}, {why}, {mode})"));
            let (read, _write) =
                std::io::pipe().map_err(|e| zbus::fdo::Error::IOError(e.to_string()))?;
            Ok(OwnedFd::from(read).into())
        }

        fn suspend(&self, interactive: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("Suspend({interactive})"));
        }

        fn schedule_shutdown(&self, kind: &str, usec: u64) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("ScheduleShutdown({kind}, {usec})"));
        }

        fn set_wall_message(&self, wall_message: &str, enable: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("SetWallMessage({wall_message}, {enable})"));
        }
    }

    /// A client connection to a [`FakeLogind`], peer to peer.
    fn fake_logind() -> (
        Manager,
        zbus::Connection,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fake = FakeLogind {
            calls: calls.clone(),
        };
        // Each side is built in a `block_on` of its own thread (the two
        // handshakes wait for each other); zbus's helper thread runs both
        // connections afterwards.
        let server = std::thread::spawn(move || {
            zbus::block_on(
                zbus::connection::Builder::unix_stream(theirs)
                    .server(zbus::Guid::generate())
                    .p2p()
                    .serve_at("/org/freedesktop/login1", fake)
                    .build(),
            )
            .unwrap()
        });
        let manager = zbus::block_on(async {
            let client = zbus::connection::Builder::unix_stream(ours)
                .p2p()
                .build()
                .await?;
            Login1ManagerProxy::new(&client).await
        })
        .unwrap();
        let server = server.join().unwrap();
        (manager, server, calls)
    }

    #[test]
    fn staying_awake_is_a_block_lock_on_sleep_named_for_us() {
        let (manager, _server, calls) = fake_logind();
        let fd = inhibit_with(&manager, "sleep", "copying D:", "block").unwrap();
        assert!(fd.as_raw_fd() >= 0);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["Inhibit(sleep, Own Backup, copying D:, block)"]
        );
    }

    #[test]
    fn sleep_after_a_copy_is_a_non_interactive_suspend() {
        let (manager, _server, calls) = fake_logind();
        sleep_with(&manager).unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["Suspend(false)"]);
    }

    #[test]
    fn poweroff_after_a_copy_is_scheduled_with_our_notice() {
        let (manager, _server, calls) = fake_logind();
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        shutdown_with(&manager, now, 90, "after the copy").unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "SetWallMessage(after the copy, true)".to_string(),
                format!(
                    "ScheduleShutdown(poweroff, {})",
                    1_700_000_090u64 * 1_000_000
                ),
            ]
        );
    }

    #[test]
    fn logind_signals_become_events_in_order() {
        let (manager, server, _calls) = fake_logind();
        let emitter = std::thread::spawn(move || {
            // Give the follower time to subscribe before anything is sent.
            std::thread::sleep(Duration::from_millis(300));
            let path = zbus::ObjectPath::try_from("/org/freedesktop/login1/session/_33").unwrap();
            let emit = |member: &str, body: &dyn Fn(&zbus::Connection) -> zbus::Result<()>| {
                body(&server).unwrap_or_else(|e| panic!("{member}: {e}"));
                std::thread::sleep(Duration::from_millis(50));
            };
            let iface = "org.freedesktop.login1.Manager";
            let at = "/org/freedesktop/login1";
            emit("SessionNew", &|c| {
                zbus::block_on(c.emit_signal(None::<()>, at, iface, "SessionNew", &("3", &path)))
            });
            emit("SessionRemoved", &|c| {
                zbus::block_on(c.emit_signal(
                    None::<()>,
                    at,
                    iface,
                    "SessionRemoved",
                    &("3", &path),
                ))
            });
            emit("PrepareForShutdown", &|c| {
                zbus::block_on(c.emit_signal(None::<()>, at, iface, "PrepareForShutdown", &(true,)))
            });
            emit("PrepareForShutdown", &|c| {
                zbus::block_on(c.emit_signal(
                    None::<()>,
                    at,
                    iface,
                    "PrepareForShutdown",
                    &(false,),
                ))
            });
            server
        });
        let mut seen = Vec::new();
        follow_events_with(&manager, |event| {
            seen.push(event);
            seen.len() < 4
        })
        .unwrap();
        let _server = emitter.join().unwrap();
        // Only the order within one signal is promised; the shutdown pair is
        // one signal.
        assert!(
            seen.contains(&LogindEvent::SessionNew("3".into())),
            "{seen:?}"
        );
        assert!(
            seen.contains(&LogindEvent::SessionRemoved("3".into())),
            "{seen:?}"
        );
        let shutdown: Vec<_> = seen
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    LogindEvent::ShuttingDown | LogindEvent::ShutdownCancelled
                )
            })
            .collect();
        assert_eq!(
            shutdown,
            [&LogindEvent::ShuttingDown, &LogindEvent::ShutdownCancelled]
        );
    }

    /// Without logind (a container, CI) nothing panics and nothing is held:
    /// the copy runs unprotected and the log says so.
    #[test]
    fn without_a_reachable_logind_the_locks_are_simply_absent() {
        let logind_here =
            zbus::block_on(async { manager().await?.inner().introspect().await }).is_ok();
        if logind_here {
            return; // a real logind is here; this test is about its absence
        }
        assert!(KeepAwake::new("test").is_none());
        assert!(ShutdownDelay::take().is_none());
        assert!(sleep_now().is_err());
    }
}
