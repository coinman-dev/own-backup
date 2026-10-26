//! Waiting for a call that can block in the kernel for good, with a deadline.
//!
//! Attaching an image to Windows (an iSCSI login, `AttachVirtualDisk`) and
//! taking it down again wait on Plug and Play, and a device PnP is stuck on
//! holds every such call with it — for hours, until a reboot (2026-10-24, a
//! failing USB stick). Such a call cannot be interrupted, only left behind:
//! it runs in a thread of its own, the caller stops waiting at the deadline
//! and goes on, and whatever the call produces after that is handed to
//! `late`, in that thread, to be undone there.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The call did not finish within its limit; it goes on in its thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

struct Handover<T> {
    result: Option<T>,
    /// The caller stopped waiting: what arrives now goes to `late`.
    abandoned: bool,
}

/// Run `call` in a thread named `name` and wait at most `limit` for it.
///
/// `Ok` is the call's own result, whenever it came in time. `Err(TimedOut)`:
/// the caller is on its own; the call's result, if it ever comes, is given to
/// `late` in the call's thread (and nowhere else — the handover is decided
/// under one lock, so a result is used by exactly one side). A thread that
/// cannot be started runs nothing and is reported as `Err(TimedOut)` too:
/// either way the caller did not get what it waited for.
pub fn run_within<T, F, L>(name: &str, limit: Duration, call: F, late: L) -> Result<T, TimedOut>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    L: FnOnce(T) + Send + 'static,
{
    let shared = Arc::new((
        Mutex::new(Handover {
            result: None,
            abandoned: false,
        }),
        Condvar::new(),
    ));
    let theirs = shared.clone();
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let value = call();
            let (lock, ready) = &*theirs;
            let mut h = lock.lock().unwrap_or_else(|p| p.into_inner());
            if h.abandoned {
                drop(h);
                late(value);
                return;
            }
            h.result = Some(value);
            ready.notify_all();
        });
    if spawned.is_err() {
        return Err(TimedOut);
    }

    let (lock, ready) = &*shared;
    let h = lock.lock().unwrap_or_else(|p| p.into_inner());
    let (mut h, _) = ready
        .wait_timeout_while(h, limit, |h| h.result.is_none())
        .unwrap_or_else(|p| p.into_inner());
    match h.result.take() {
        Some(value) => Ok(value),
        None => {
            h.abandoned = true;
            Err(TimedOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn a_call_in_time_is_its_own_result() {
        let got = run_within(
            "t-fast",
            Duration::from_secs(5),
            || 7,
            |_| panic!("not late"),
        );
        assert_eq!(got, Ok(7));
    }

    #[test]
    fn a_late_result_goes_to_late_and_only_there() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (late_tx, late_rx) = mpsc::channel::<u32>();
        let started = Instant::now();
        let got = run_within(
            "t-slow",
            Duration::from_millis(100),
            move || {
                // Held until the caller has given up.
                gate_rx.recv().unwrap();
                42u32
            },
            move |v| late_tx.send(v).unwrap(),
        );
        assert_eq!(got, Err(TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
        gate_tx.send(()).unwrap();
        assert_eq!(
            late_rx.recv_timeout(Duration::from_secs(5)),
            Ok(42),
            "the result that came after the deadline is undone in the call's thread"
        );
    }
}
