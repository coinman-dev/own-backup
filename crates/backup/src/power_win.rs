//! The machine's power state around a backup: stay awake while copying, wake
//! for a scheduled copy, sleep or turn off after one, and copy quietly.
//!
//! All of it runs in the service, as LocalSystem, in session 0. That is why the
//! wake-up and "stay awake" use power *requests* and a waitable timer, not
//! `SetThreadExecutionState`: a request belongs to a handle, not to whichever
//! thread a tokio runtime happens to run the job on.
//!
//! Before 2026-09-26 the schedule offered "не давать уснуть" and "пробуждать"
//! as switches that were saved and never read (audit AUD-16). A laptop that
//! slept halfway through a nightly copy produced a failed run and nothing else.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::time::Duration;

use own_backup_common::tr;
use tracing::{info, warn};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOLEAN, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
    SetSuspendState,
};
use windows::Win32::System::Shutdown::{
    InitiateShutdownW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER, SHUTDOWN_POWEROFF,
};
use windows::Win32::System::Threading::{
    CancelWaitableTimer, CreateWaitableTimerExW, GetCurrentThread, SetThreadPriority, SetWaitableTimer,
    WaitForSingleObject, POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END, INFINITE,
};

/// Windows does not go to sleep while this is alive. The display may still
/// turn off; only system sleep is held back.
///
/// Best-effort: if the request cannot be made, the copy runs anyway — a copy
/// that might be interrupted is better than no copy.
pub struct KeepAwake(HANDLE);

// The handle is a kernel object; any thread may clear and close it.
unsafe impl Send for KeepAwake {}
unsafe impl Sync for KeepAwake {}

impl KeepAwake {
    /// `reason` is what `powercfg /requests` shows while it is held.
    pub fn new(reason: &str) -> Option<Self> {
        let mut text: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
        let ctx = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 { SimpleReasonString: PWSTR(text.as_mut_ptr()) },
        };
        // SAFETY: `ctx` and the string it points at live across the call; the
        // request copies the string.
        let handle = match unsafe { PowerCreateRequest(&ctx) } {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "could not create a power request — the machine may sleep");
                return None;
            }
        };
        // SAFETY: a valid request handle, closed on the failure path and in Drop.
        if let Err(e) = unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } {
            warn!(error = %e, "could not hold the machine awake — it may sleep");
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: the handle was created and set in `new`.
        unsafe {
            let _ = PowerClearRequest(self.0, PowerRequestSystemRequired);
            let _ = CloseHandle(self.0);
        }
    }
}

/// The calling thread reads and writes at background priority while this is
/// alive: CPU and — what matters for a backup — disk I/O are scheduled behind
/// everything the user is doing. Must be dropped on the thread that made it;
/// the blocking pool reuses threads, and one left in background mode would slow
/// whatever it runs next.
pub struct BackgroundIo {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl BackgroundIo {
    pub fn enter() -> Option<Self> {
        // SAFETY: the pseudo-handle of the current thread.
        match unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } {
            Ok(()) => Some(Self { _not_send: std::marker::PhantomData }),
            Err(e) => {
                warn!(error = %e, "could not switch the copy to background priority");
                None
            }
        }
    }
}

impl Drop for BackgroundIo {
    fn drop(&mut self) {
        // SAFETY: same thread (the type is !Send).
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}

/// Put the machine to sleep now. Hibernation is not asked for; if the machine
/// only hibernates, Windows decides.
pub fn sleep_now() -> std::result::Result<(), String> {
    if !crate::privileges_win::enable_shutdown_privilege() {
        return Err(tr!("power_win.no_shutdown_privilege").into_owned());
    }
    info!("after backup: putting the machine to sleep");
    // SAFETY: plain call; returns FALSE on failure.
    let ok = unsafe { SetSuspendState(BOOLEAN(0), BOOLEAN(0), BOOLEAN(0)) };
    if ok.0 == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// Turn the machine off after `grace_secs`, with Windows' own notice to whoever
/// is signed in — `shutdown /a` cancels it. Programs with unsaved work are not
/// forced closed: on a machine someone is using, their work wins.
pub fn shutdown_in(grace_secs: u32, message: &str) -> std::result::Result<(), String> {
    if !crate::privileges_win::enable_shutdown_privilege() {
        return Err(tr!("power_win.no_shutdown_privilege").into_owned());
    }
    info!(grace_secs, "after backup: shutting the machine down");
    let msg = HSTRING::from(message);
    // SAFETY: null machine name = this machine; the message outlives the call.
    let rc = unsafe {
        InitiateShutdownW(
            PCWSTR::null(),
            &msg,
            grace_secs,
            SHUTDOWN_POWEROFF,
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32).to_string());
    }
    Ok(())
}

/// A timer that wakes the machine from sleep at the time it is armed for, and
/// then holds it awake for `hold` so a scheduler that ticks once a minute has
/// time to start the copy (which then holds it awake by itself).
///
/// Waking needs "Разрешить таймеры пробуждения" in the power plan — on a
/// laptop on battery it is often off, and then nothing here can help.
pub struct WakeTimer {
    handle: HANDLE,
}

unsafe impl Send for WakeTimer {}
unsafe impl Sync for WakeTimer {}

impl WakeTimer {
    /// Create the timer and the thread that answers it.
    pub fn start(hold: Duration) -> Option<Arc<Self>> {
        // SAFETY: an unnamed, auto-reset timer with default security.
        let handle = match unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), 0, 0x1F0003 /* TIMER_ALL_ACCESS */)
        } {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "could not create the wake timer — scheduled copies will not wake the machine");
                return None;
            }
        };
        let timer = Arc::new(Self { handle });
        let waiter = timer.clone();
        let _ = std::thread::Builder::new().name("wake-timer".into()).spawn(move || loop {
            // SAFETY: the handle lives as long as `waiter`, i.e. forever.
            let r = unsafe { WaitForSingleObject(waiter.handle, INFINITE) };
            if r != WAIT_OBJECT_0 {
                warn!("wake timer wait failed; wake-ups stop");
                return;
            }
            info!(hold_s = hold.as_secs(), "woken for a scheduled copy — holding the machine awake");
            let _awake = KeepAwake::new(&tr!("power_win.wake_reason"));
            std::thread::sleep(hold);
        });
        Some(timer)
    }

    /// Wake at `at`. Re-arming replaces the previous time.
    pub fn arm(&self, at: chrono::DateTime<chrono::Utc>) {
        // FILETIME: 100 ns since 1601-01-01 UTC; a positive due time is absolute.
        const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
        let due: i64 = (at.timestamp() + EPOCH_DIFF_SECS) * 10_000_000
            + i64::from(at.timestamp_subsec_nanos() / 100);
        // SAFETY: a valid timer handle; no completion routine; `fResume` = wake.
        if let Err(e) = unsafe { SetWaitableTimer(self.handle, &due, 0, None, None, true) } {
            warn!(error = %e, "could not arm the wake timer");
        }
    }

    /// Nothing to wake for: take the time back.
    pub fn disarm(&self) {
        // SAFETY: a valid timer handle.
        unsafe {
            let _ = CancelWaitableTimer(self.handle);
        }
    }
}
