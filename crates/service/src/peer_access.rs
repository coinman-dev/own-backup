//! Who may drive the engine over the Unix socket.
//!
//! The socket is 0660 to the `own-backup` group and the token beside it is
//! readable by the same group, so on their own they admit every member from
//! anywhere — an SSH login included — to an engine that runs as root and
//! writes raw disks. Windows refuses pipe clients outside the active console
//! session (`client_in_active_console` in `main.rs`); this is the Linux
//! counterpart, asked of the caller's credentials (`SO_PEERCRED`) on every
//! call:
//!
//! * root is let through: it can do everything the engine does anyway;
//! * anyone else needs an active local session in systemd-logind — on a seat,
//!   the seat's foreground session (`ACTIVE=1`), not remote (`REMOTE=0`), of a
//!   user class. A caller running inside a session scope (a terminal on a VT,
//!   an SSH login) is judged by THAT session; one outside any — desktop
//!   applications under `user@UID.service`, which is where KDE and GNOME start
//!   them — by its user's sessions, so a same-user process started outside a
//!   session scope while that user is at the seat is let through too;
//! * without logind (no `/run/systemd/sessions`) there is nothing to ask, and
//!   membership in the group is the only check left; that is logged once.
//!
//! logind's runtime files are read directly rather than over D-Bus: they are
//! what `sd-login` itself reads, the engine has no D-Bus client, and a
//! directory of a few small files per call costs nothing. The files say "do
//! not parse"; should their fields ever change, no session qualifies and the
//! check fails closed, not open.

use std::path::Path;

use own_backup_common::tr;
use tonic::transport::server::UdsConnectInfo;
use tonic::Status;

/// Where systemd-logind keeps one `KEY=value` file per session.
const SESSIONS_DIR: &str = "/run/systemd/sessions";

/// The fields of a logind session file that the decision uses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub id: String,
    pub uid: Option<u32>,
    pub active: bool,
    /// `None` when the file does not say — which is not the same as local.
    pub remote: Option<bool>,
    pub seat: Option<String>,
    pub class: Option<String>,
    pub state: Option<String>,
}

impl Session {
    /// At the machine and in front: the foreground session of a seat, not a
    /// remote one, and a person's. Seatless sessions are always `ACTIVE=1`
    /// (SSH, the per-user `manager` session of systemd 256+), so the seat is
    /// what makes "active" mean anything. Greeter and lock-screen sessions
    /// are not a user at the machine.
    pub fn is_local_and_active(&self) -> bool {
        self.active
            && self.remote == Some(false)
            && self.seat.as_deref().is_some_and(|s| !s.is_empty())
            // `user`, and systemd 256's `user-early`/`user-incomplete`/`user-light`.
            && self.class.as_deref().map_or(true, |c| c.starts_with("user"))
            && self.state.as_deref() != Some("closing")
    }
}

/// Read one session file. Unknown keys and malformed lines are ignored.
pub(crate) fn parse_session(id: &str, text: &str) -> Session {
    let mut session = Session {
        id: id.to_string(),
        ..Default::default()
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "UID" => session.uid = value.parse().ok(),
            "ACTIVE" => session.active = value == "1",
            "REMOTE" => {
                session.remote = match value {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                }
            }
            "SEAT" => session.seat = Some(value.to_string()),
            "CLASS" => session.class = Some(value.to_string()),
            "STATE" => session.state = Some(value.to_string()),
            _ => {}
        }
    }
    session
}

/// The logind session a process belongs to, from its `/proc/<pid>/cgroup`:
/// the `session-<id>.scope` component of its cgroup path. `None` for a
/// process outside any session scope (system services, `user@UID.service`).
pub(crate) fn session_of_cgroup(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .flat_map(|path| path.split('/'))
        .find_map(|part| {
            part.strip_prefix("session-")?
                .strip_suffix(".scope")
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
}

/// What [`decide`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Root,
    LocalSession,
    /// No logind on this system: group membership is all there is to go on.
    NoLogind,
    Refused,
}

/// The decision itself, apart from where its inputs come from.
///
/// `own_session` is the caller's session when it runs inside one; `sessions`
/// is `None` when the system has no logind at all.
pub(crate) fn decide(uid: u32, own_session: Option<&str>, sessions: Option<&[Session]>) -> Verdict {
    if uid == 0 {
        return Verdict::Root;
    }
    let Some(sessions) = sessions else {
        return Verdict::NoLogind;
    };
    let qualifies = |s: &Session| s.uid == Some(uid) && s.is_local_and_active();
    let allowed = match own_session {
        Some(id) => sessions.iter().any(|s| s.id == id && qualifies(s)),
        None => sessions.iter().any(qualifies),
    };
    if allowed {
        Verdict::LocalSession
    } else {
        Verdict::Refused
    }
}

/// Every session file in `dir`; `None` when the directory does not exist.
fn read_sessions(dir: &Path) -> std::io::Result<Option<Vec<Session>>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        // Beside each session file lies its `<id>.ref` FIFO — opening that for
        // reading would block — and logind writes through `.#` temporaries.
        // Session ids never contain a dot.
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|id| !id.contains('.')) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        match std::fs::read_to_string(entry.path()) {
            Ok(text) => sessions.push(parse_session(id, &text)),
            // Ended between the listing and the read.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(sessions))
}

/// Who made the call: the uid and gid of `SO_PEERCRED`, or this process's
/// own for a call that did not come over the Unix socket (the loopback TCP of
/// a `--foreground` development run, whose client is the developer running
/// it). What the engine writes FOR the caller — the boot medium's ISO into
/// their folder — is theirs by these ids.
#[allow(clippy::result_large_err)] // tonic::Status, as every RPC handler returns
pub(crate) fn caller_ids(info: Option<&UdsConnectInfo>) -> Result<(u32, u32), Status> {
    let Some(info) = info else {
        // SAFETY: plain getters.
        return Ok(unsafe { (libc::geteuid(), libc::getegid()) });
    };
    let cred = info
        .peer_cred
        .ok_or_else(|| Status::permission_denied(tr!("peer_access.unknown_peer").into_owned()))?;
    Ok((cred.uid(), cred.gid()))
}

/// Check one call. `None` means it did not arrive over the Unix socket (the
/// loopback TCP of a `--foreground` development run), which this does not
/// cover.
pub(crate) fn check(info: Option<&UdsConnectInfo>) -> Result<(), Status> {
    let Some(info) = info else {
        return Ok(());
    };
    let Some(cred) = info.peer_cred else {
        tracing::warn!(target: "security", "socket client without peer credentials rejected");
        return Err(Status::permission_denied(
            tr!("peer_access.unknown_peer").into_owned(),
        ));
    };
    let uid = cred.uid();
    if decide(uid, None, None) == Verdict::Root {
        return Ok(()); // nothing to read for it
    }
    let pid = cred.pid();
    let own_session = pid
        .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok())
        .and_then(|text| session_of_cgroup(&text));
    let sessions = read_sessions(Path::new(SESSIONS_DIR)).unwrap_or_else(|e| {
        // Unreadable is not absent: refuse rather than fall back to the group.
        tracing::warn!(target: "security", error = %e, "logind sessions unreadable");
        Some(Vec::new())
    });
    match decide(uid, own_session.as_deref(), sessions.as_deref()) {
        Verdict::Root | Verdict::LocalSession => Ok(()),
        Verdict::NoLogind => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    target: "security",
                    dir = SESSIONS_DIR,
                    "no systemd-logind sessions on this system — socket clients are checked by \
                     group membership only"
                )
            });
            Ok(())
        }
        Verdict::Refused => {
            let session = own_session.as_deref().unwrap_or("-");
            if first_refusal_in_a_while(uid, session, std::time::Instant::now()) {
                tracing::warn!(
                    target: "security",
                    uid,
                    pid,
                    session,
                    "socket client outside an active local session rejected \
                     (repeats from it are logged at debug level for ten minutes)"
                );
            } else {
                tracing::debug!(
                    target: "security",
                    uid,
                    pid,
                    session,
                    "socket client outside an active local session rejected again"
                );
            }
            Err(Status::permission_denied(
                tr!("peer_access.not_local_session").into_owned(),
            ))
        }
    }
}

/// Whether a refusal of this caller is worth a WARN: the first one from a
/// user and session, and again only after ten quiet minutes. A refused window
/// keeps asking — its event stream reconnecting, a page polling — and every
/// call wrote the same line, one every two seconds.
fn first_refusal_in_a_while(uid: u32, session: &str, now: std::time::Instant) -> bool {
    use std::sync::Mutex;
    use std::time::Duration;

    const QUIET: Duration = Duration::from_secs(600);
    static SEEN: Mutex<Vec<(u32, String, std::time::Instant)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|(_, _, at)| now.saturating_duration_since(*at) < QUIET);
    if seen.iter().any(|(u, s, _)| *u == uid && s == session) {
        return false;
    }
    seen.push((uid, session.to_string(), now));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop login on KDE (systemd 259), as logind writes it.
    const SEAT_SESSION: &str = "# This is private data. Do not parse.\n\
        UID=1000\nUSER=alice\nACTIVE=1\nIS_DISPLAY=1\nSTATE=active\nREMOTE=0\n\
        TYPE=wayland\nCLASS=user\nSCOPE=session-3.scope\nSEAT=seat0\nTTY=tty2\n\
        SERVICE=sddm\nDESKTOP=KDE\nVTNR=2\nLEADER=1751\n";
    /// The per-user manager session of systemd 256+: active, local, no seat.
    const MANAGER_SESSION: &str = "UID=1000\nACTIVE=1\nSTATE=active\nREMOTE=0\n\
        TYPE=unspecified\nCLASS=manager\nSERVICE=systemd-user\n";
    /// An SSH login: active (seatless sessions always are), remote.
    const SSH_SESSION: &str = "UID=1000\nACTIVE=1\nSTATE=active\nREMOTE=1\n\
        TYPE=tty\nCLASS=user\nSCOPE=session-7.scope\nREMOTE_HOST=10.0.0.5\nSERVICE=sshd\n";

    fn sessions(list: &[(&str, &str)]) -> Vec<Session> {
        list.iter()
            .map(|(id, text)| parse_session(id, text))
            .collect()
    }

    #[test]
    fn a_session_file_is_read_by_its_keys() {
        let s = parse_session("3", SEAT_SESSION);
        assert_eq!(s.id, "3");
        assert_eq!(s.uid, Some(1000));
        assert!(s.active);
        assert_eq!(s.remote, Some(false));
        assert_eq!(s.seat.as_deref(), Some("seat0"));
        assert_eq!(s.class.as_deref(), Some("user"));
        assert!(s.is_local_and_active());
    }

    #[test]
    fn only_the_foreground_seat_session_of_a_person_qualifies() {
        assert!(!parse_session("4", MANAGER_SESSION).is_local_and_active());
        assert!(!parse_session("7", SSH_SESSION).is_local_and_active());
        let switched_away = SEAT_SESSION.replace("ACTIVE=1", "ACTIVE=0");
        assert!(!parse_session("3", &switched_away).is_local_and_active());
        let greeter = SEAT_SESSION.replace("CLASS=user", "CLASS=greeter");
        assert!(!parse_session("3", &greeter).is_local_and_active());
        let closing = SEAT_SESSION.replace("STATE=active", "STATE=closing");
        assert!(!parse_session("3", &closing).is_local_and_active());
        // A file that does not say it is local is not taken to be.
        let unknown = SEAT_SESSION.replace("REMOTE=0\n", "");
        assert!(!parse_session("3", &unknown).is_local_and_active());
        let early = SEAT_SESSION.replace("CLASS=user", "CLASS=user-early");
        assert!(parse_session("3", &early).is_local_and_active());
    }

    #[test]
    fn root_is_let_through_whatever_logind_says() {
        assert_eq!(decide(0, None, Some(&[])), Verdict::Root);
        assert_eq!(decide(0, Some("7"), None), Verdict::Root);
    }

    #[test]
    fn a_user_at_the_seat_is_let_through() {
        let all = sessions(&[("3", SEAT_SESSION), ("4", MANAGER_SESSION)]);
        // The desktop application, under user@1000.service: no session of its own.
        assert_eq!(decide(1000, None, Some(&all)), Verdict::LocalSession);
        // A terminal inside the desktop session.
        assert_eq!(decide(1000, Some("3"), Some(&all)), Verdict::LocalSession);
    }

    #[test]
    fn a_user_reaching_in_from_elsewhere_is_refused() {
        // Logged in over SSH only (plus the manager session that comes with it).
        let remote_only = sessions(&[("7", SSH_SESSION), ("4", MANAGER_SESSION)]);
        assert_eq!(
            decide(1000, Some("7"), Some(&remote_only)),
            Verdict::Refused
        );
        assert_eq!(decide(1000, None, Some(&remote_only)), Verdict::Refused);

        // At the seat AND over SSH: the SSH session's own processes are judged
        // by the session they run in.
        let both = sessions(&[("3", SEAT_SESSION), ("7", SSH_SESSION)]);
        assert_eq!(decide(1000, Some("7"), Some(&both)), Verdict::Refused);
        // A session id that is no longer listed is not a session.
        assert_eq!(decide(1000, Some("9"), Some(&both)), Verdict::Refused);
    }

    #[test]
    fn another_users_seat_session_does_not_count() {
        let all = sessions(&[("3", SEAT_SESSION)]);
        assert_eq!(decide(1001, None, Some(&all)), Verdict::Refused);
        // `su` inside alice's terminal stays in alice's session.
        assert_eq!(decide(1001, Some("3"), Some(&all)), Verdict::Refused);
        assert_eq!(decide(1000, None, Some(&[])), Verdict::Refused);
    }

    #[test]
    fn without_logind_the_group_is_the_only_check() {
        assert_eq!(decide(1000, None, None), Verdict::NoLogind);
    }

    #[test]
    fn the_session_is_found_in_the_cgroup_path() {
        assert_eq!(
            session_of_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n").as_deref(),
            Some("3")
        );
        // cgroup v1 lists one line per hierarchy.
        assert_eq!(
            session_of_cgroup(
                "12:pids:/user.slice/user-1000.slice/session-c2.scope\n\
                 1:name=systemd:/user.slice/user-1000.slice/session-c2.scope\n"
            )
            .as_deref(),
            Some("c2")
        );
        // Desktop applications live under the user manager, not a session.
        assert_eq!(
            session_of_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-x.service\n"
            ),
            None
        );
        assert_eq!(
            session_of_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/session.slice/plasma.service\n"
            ),
            None
        );
        assert_eq!(
            session_of_cgroup("0::/system.slice/own-backup.service\n"),
            None
        );
    }

    #[test]
    fn the_runtime_directory_is_read_without_touching_fifos() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_sessions(&dir.path().join("absent")).unwrap(), None);

        std::fs::write(dir.path().join("3"), SEAT_SESSION).unwrap();
        std::fs::write(dir.path().join(".#3abcdef"), "UID=0\n").unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = std::ffi::CString::new(dir.path().join("3.ref").as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid C path; a FIFO in a private temporary directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::create_dir(dir.path().join("5")).unwrap();

        let found = read_sessions(dir.path()).unwrap().unwrap();
        assert_eq!(found, vec![parse_session("3", SEAT_SESSION)]);
    }

    #[test]
    fn a_refusal_is_warned_about_once_per_caller_and_quiet_spell() {
        use std::time::{Duration, Instant};
        // Uids no other test uses: the record is process-wide.
        let t = Instant::now();
        assert!(first_refusal_in_a_while(70_001, "s1", t));
        assert!(!first_refusal_in_a_while(
            70_001,
            "s1",
            t + Duration::from_secs(2)
        ));
        assert!(first_refusal_in_a_while(70_001, "s2", t), "another session");
        assert!(first_refusal_in_a_while(70_002, "s1", t), "another user");
        assert!(first_refusal_in_a_while(
            70_001,
            "s1",
            t + Duration::from_secs(601)
        ));
    }
}
