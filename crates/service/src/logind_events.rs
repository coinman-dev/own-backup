//! What systemd-logind tells the engine (Linux): the counterpart of the SCM's
//! shutdown and session-change controls in `windows_service.rs`.
//!
//! * A shutdown (`PrepareForShutdown`) is forwarded to the scheduler as
//!   [`OsEvent::Shutdown`] and starts the engine's graceful stop at once, as
//!   the Windows service does on `Preshutdown`. A `delay` inhibitor lock
//!   holds logind for the moment this takes; systemd's own SIGTERM follows
//!   and waits for the running operation (TimeoutStopSec in the unit).
//! * A user session that starts or ends (`SessionNew`/`SessionRemoved`) is
//!   [`OsEvent::Logon`]/[`OsEvent::Logoff`]. Only a person's sessions count —
//!   class `user*`, as in `peer_access` — not the greeter, the per-user
//!   manager or a background session.
//!
//! All of it is a thread of its own around blocking D-Bus calls; without
//! logind it logs once and the engine runs as before, stopped by SIGTERM.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use own_backup_backup::power_linux::{follow_events, LogindEvent, ShutdownDelay};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::scheduler::OsEvent;

/// Where logind keeps one `KEY=value` file per session.
const SESSIONS_DIR: &str = "/run/systemd/sessions";

/// Set when a shutdown the engine had already begun stopping for was called
/// off: the engine then exits with an error so that systemd starts it again
/// (`Restart=on-failure`) instead of leaving the machine without one.
#[derive(Clone, Default)]
pub struct CancelledShutdown(Arc<AtomicBool>);

impl CancelledShutdown {
    pub fn happened(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Start following logind. `stop` begins the engine's graceful stop.
pub fn spawn(os_tx: UnboundedSender<OsEvent>, stop: UnboundedSender<()>) -> CancelledShutdown {
    let cancelled = CancelledShutdown::default();
    let flag = cancelled.clone();
    let spawned = std::thread::Builder::new()
        .name("logind-events".into())
        .spawn(move || {
            let mut delay = ShutdownDelay::take();
            let mut stopping = false;
            let mut sessions = Sessions::default();
            let followed = follow_events(|event| {
                match event {
                    LogindEvent::ShuttingDown => {
                        info!("System shutdown (logind)");
                        let _ = os_tx.send(OsEvent::Shutdown);
                        if !stopping {
                            stopping = true;
                            let _ = stop.send(());
                        }
                        // Let logind go on: the stop has begun, and systemd
                        // waits for it when it stops the unit.
                        delay = None;
                    }
                    LogindEvent::ShutdownCancelled => {
                        if stopping {
                            warn!("the shutdown was called off after the engine began stopping — it will be restarted");
                            flag.0.store(true, Ordering::SeqCst);
                        } else if delay.is_none() {
                            delay = ShutdownDelay::take();
                        }
                    }
                    LogindEvent::SessionNew(id) => {
                        if sessions.started(&id, read_session) {
                            let _ = os_tx.send(OsEvent::Logon);
                        }
                    }
                    LogindEvent::SessionRemoved(id) => {
                        if sessions.ended(&id) {
                            let _ = os_tx.send(OsEvent::Logoff);
                        }
                    }
                }
                // Nobody is listening any more once the engine has stopped.
                !os_tx.is_closed()
            });
            if let Err(e) = followed {
                warn!(error = %e, "logind events are not followed — logon/logoff jobs will not fire and a shutdown is seen only as SIGTERM");
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "could not start the logind event thread");
    }
    cancelled
}

/// The session file of `id`, if logind has written it.
fn read_session(id: &str) -> Option<crate::peer_access::Session> {
    // SessionNew can arrive a moment before the file is complete.
    for _ in 0..10 {
        if let Ok(text) = std::fs::read_to_string(format!("{SESSIONS_DIR}/{id}")) {
            let session = crate::peer_access::parse_session(id, &text);
            if session.class.is_some() {
                return Some(session);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

/// The sessions that counted as a person's logon, so that only their end
/// counts as a logoff.
#[derive(Default)]
struct Sessions(HashSet<String>);

impl Sessions {
    /// A session began; `true` when it is a person's.
    fn started(
        &mut self,
        id: &str,
        read: impl Fn(&str) -> Option<crate::peer_access::Session>,
    ) -> bool {
        let Some(session) = read(id) else {
            return false;
        };
        if !session
            .class
            .as_deref()
            .is_some_and(|c| c.starts_with("user"))
        {
            return false;
        }
        self.0.insert(id.to_string())
    }

    /// A session ended; `true` when its start was counted.
    fn ended(&mut self, id: &str) -> bool {
        self.0.remove(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(class: &str) -> Option<crate::peer_access::Session> {
        Some(crate::peer_access::parse_session(
            "1",
            &format!("UID=1000\nCLASS={class}\nACTIVE=1\nREMOTE=0\n"),
        ))
    }

    #[test]
    fn a_persons_session_is_a_logon_and_its_end_a_logoff() {
        let mut sessions = Sessions::default();
        assert!(sessions.started("3", |_| session("user")));
        assert!(sessions.ended("3"));
        assert!(!sessions.ended("3"), "one logoff per logon");
    }

    #[test]
    fn greeters_managers_and_unreadable_sessions_are_not_logons() {
        let mut sessions = Sessions::default();
        assert!(!sessions.started("c1", |_| session("greeter")));
        assert!(!sessions.started("c2", |_| session("manager")));
        assert!(!sessions.started("c3", |_| session("background")));
        assert!(!sessions.started("c4", |_| None));
        for id in ["c1", "c2", "c3", "c4"] {
            assert!(!sessions.ended(id), "{id}: no logon, so no logoff");
        }
    }

    #[test]
    fn systemd_256_user_classes_count() {
        let mut sessions = Sessions::default();
        assert!(sessions.started("5", |_| session("user-early")));
        assert!(sessions.started("6", |_| session("user-light")));
    }
}
