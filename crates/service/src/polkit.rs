//! Destructive operations ask polkit first (Linux, decision 4 of plan §6).
//!
//! Membership in the access group and an active local session
//! (`peer_access`) let a person drive the engine: see disks, make and run
//! jobs. Overwriting a disk, deleting backups, replacing an archive's keys
//! and changing how the machine boots additionally need an administrator,
//! the way `systemctl` or a software centre asks: the engine — root — asks
//! polkit about the CALLER, polkit's agent in the caller's session shows the
//! password dialog, and the operation goes ahead only on its "yes". The
//! actions and their defaults (`auth_admin_keep` for an active local session,
//! no elsewhere) are in `packaging/linux/com.ownbackup.policy`; an
//! administrator relaxes or tightens them with a polkit rule.
//!
//! Root needs no question; a call that did not arrive over the Unix socket
//! (the loopback TCP of a `--foreground` development run) is not covered, as
//! in `peer_access`. Without a reachable polkit the answer is no — the
//! operation can still be done as root (the CLI under sudo).

use std::collections::HashMap;

use own_backup_common::tr;
use tonic::transport::server::UdsConnectInfo;
use tonic::{Request, Status};
use zbus::zvariant::Value;

/// The polkit actions of `com.ownbackup.policy` that the engine checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Overwriting a volume or a disk (restore RPCs, plan §6 stage 3).
    Restore,
    DeleteBackups,
    ManageKeys,
    /// The boot configuration has no Linux RPC yet; a plan's boot steps are
    /// covered by `Restore`.
    #[allow(dead_code)]
    BootConfig,
}

impl Action {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Action::Restore => "com.ownbackup.restore",
            Action::DeleteBackups => "com.ownbackup.delete-backups",
            Action::ManageKeys => "com.ownbackup.manage-keys",
            Action::BootConfig => "com.ownbackup.boot-config",
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    /// `(sa{sv}) s a{ss} u s` → `(bba{ss})`: `subject` is `(kind, details)`,
    /// the reply `(is_authorized, is_challenge, details)`.
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// `AllowUserInteraction`: polkit may ask the caller for a password.
const ALLOW_USER_INTERACTION: u32 = 1;

/// May the caller of `req` do `action`? Asks polkit, which may show the
/// caller a password dialog first; the call waits for the answer.
#[allow(clippy::result_large_err)] // tonic::Status, as every RPC handler returns
pub(crate) async fn authorize<T>(req: &Request<T>, action: Action) -> Result<(), Status> {
    let Some(info) = req.extensions().get::<UdsConnectInfo>() else {
        return Ok(()); // development TCP, as in peer_access
    };
    let Some(cred) = info.peer_cred else {
        return Err(Status::permission_denied(
            tr!("peer_access.unknown_peer").into_owned(),
        ));
    };
    if cred.uid() == 0 {
        return Ok(());
    }
    // polkit judges a process, and a PID alone may have been reused by the
    // time it looks: the start time pins it to the one that called.
    let Some(pid) = cred.pid().and_then(|p| u32::try_from(p).ok()) else {
        return Err(Status::permission_denied(
            tr!("peer_access.unknown_peer").into_owned(),
        ));
    };
    let start_time = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| process_start_time(&stat));
    let Some(start_time) = start_time else {
        return Err(Status::permission_denied(
            tr!("peer_access.unknown_peer").into_owned(),
        ));
    };
    match check(pid, start_time, cred.uid(), action).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            tracing::warn!(target: "security", uid = cred.uid(), pid, action = action.id(), "polkit said no");
            Err(Status::permission_denied(
                tr!("polkit.denied", action = action.id()).into_owned(),
            ))
        }
        Err(e) => {
            tracing::warn!(target: "security", error = %e, action = action.id(), "polkit could not be asked");
            Err(Status::permission_denied(
                tr!("polkit.unavailable", action = action.id(), error = e).into_owned(),
            ))
        }
    }
}

async fn check(pid: u32, start_time: u64, uid: u32, action: Action) -> zbus::Result<bool> {
    let connection = zbus::Connection::system().await?;
    check_with(
        &AuthorityProxy::new(&connection).await?,
        pid,
        start_time,
        uid,
        action,
    )
    .await
}

async fn check_with(
    authority: &AuthorityProxy<'_>,
    pid: u32,
    start_time: u64,
    uid: u32,
    action: Action,
) -> zbus::Result<bool> {
    let uid = i32::try_from(uid).map_err(|_| zbus::Error::Failure("uid out of range".into()))?;
    let subject_details = HashMap::from([
        ("pid", Value::from(pid)),
        ("start-time", Value::from(start_time)),
        ("uid", Value::from(uid)),
    ]);
    let (authorized, _challenge, _details) = authority
        .check_authorization(
            &("unix-process", subject_details),
            action.id(),
            HashMap::new(),
            ALLOW_USER_INTERACTION,
            "",
        )
        .await?;
    Ok(authorized)
}

/// Field 22 of `/proc/<pid>/stat` (`starttime`, clock ticks since boot) —
/// what polkit compares a `unix-process` subject against. The command name
/// in field 2 may hold spaces and parentheses, so fields are counted after
/// its LAST closing parenthesis.
fn process_start_time(stat: &str) -> Option<u64> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    // after_name starts at field 3 (state); starttime is field 22.
    after_name.split_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;

    #[test]
    fn the_start_time_is_field_22_even_with_an_odd_command_name() {
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 19 20";
        assert_eq!(
            process_start_time(&format!("1234 (own-backup-gui) {tail}")),
            Some(987654)
        );
        assert_eq!(
            process_start_time(&format!("1234 (a) b (c)) {tail}")),
            Some(987654)
        );
        assert_eq!(process_start_time("1234 (cut"), None);
        assert_eq!(process_start_time("1234 (short) S 1 2"), None);
    }

    #[test]
    fn our_own_start_time_reads_from_proc() {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        assert!(process_start_time(&stat).is_some(), "{stat}");
    }

    #[test]
    fn every_checked_action_is_declared_in_the_packaged_policy() {
        let policy = include_str!("../../../packaging/linux/com.ownbackup.policy");
        for action in [
            Action::Restore,
            Action::DeleteBackups,
            Action::ManageKeys,
            Action::BootConfig,
        ] {
            assert!(
                policy.contains(&format!("<action id=\"{}\">", action.id())),
                "{} is not in com.ownbackup.policy",
                action.id()
            );
        }
    }

    /// A stand-in for polkit that answers from a table and records what it
    /// was asked, in D-Bus terms.
    struct FakeAuthority {
        allow: bool,
        asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
    impl FakeAuthority {
        fn check_authorization(
            &self,
            subject: (String, HashMap<String, OwnedValue>),
            action_id: String,
            _details: HashMap<String, String>,
            flags: u32,
            _cancellation_id: String,
        ) -> (bool, bool, HashMap<String, String>) {
            let (kind, details) = subject;
            let pid = u32::try_from(&details["pid"]).unwrap();
            let start = u64::try_from(&details["start-time"]).unwrap();
            let uid = i32::try_from(&details["uid"]).unwrap();
            self.asked.lock().unwrap().push(format!(
                "{kind} pid={pid} start={start} uid={uid} {action_id} flags={flags}"
            ));
            (self.allow, false, HashMap::new())
        }
    }

    async fn fake_authority(
        allow: bool,
    ) -> (
        zbus::Connection,
        zbus::Connection,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fake = FakeAuthority {
            allow,
            asked: asked.clone(),
        };
        let server = async {
            zbus::connection::Builder::async_io_unix_stream(theirs)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .serve_at("/org/freedesktop/PolicyKit1/Authority", fake)
                .unwrap()
                .build()
                .await
                .unwrap()
        };
        let client = async {
            zbus::connection::Builder::async_io_unix_stream(ours)
                .p2p()
                .build()
                .await
                .unwrap()
        };
        let (server, client) = tokio::join!(server, client);
        (client, server, asked)
    }

    #[tokio::test]
    async fn the_caller_process_is_asked_about_with_interaction_allowed() {
        let (client, _server, asked) = fake_authority(true).await;
        let authority = AuthorityProxy::new(&client).await.unwrap();
        assert!(check_with(&authority, 4321, 987654, 1000, Action::Restore)
            .await
            .unwrap());
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            ["unix-process pid=4321 start=987654 uid=1000 com.ownbackup.restore flags=1"]
        );
    }

    #[tokio::test]
    async fn a_no_from_polkit_is_a_no() {
        let (client, _server, _asked) = fake_authority(false).await;
        let authority = AuthorityProxy::new(&client).await.unwrap();
        assert!(
            !check_with(&authority, 4321, 987654, 1000, Action::DeleteBackups)
                .await
                .unwrap()
        );
    }
}
