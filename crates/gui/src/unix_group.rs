//! Is this window kept from the service by the access group?
//!
//! On Linux the service's socket is `0660` and its token `0640`, both owned by
//! the group `own-backup`; a user outside it cannot open either. That looked
//! like "the service is not running" — the token read as missing, the socket
//! did not answer — when the service was running fine and the user simply was
//! not in the group, or was added to it after logging in: a process keeps the
//! groups its session started with, so membership needs a new login.
//!
//! Told apart from the files alone, no child process: the token file says
//! whether the group is in the way at all (`EACCES`), `/proc/self/status`
//! which groups this process has, `/etc/group` and `/etc/passwd` whether the
//! user has been added since.

use std::path::Path;

/// Why the access group keeps this window out, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAccess {
    /// Nothing in the way, or something other than the group.
    Ok,
    /// The user is not a member of the group.
    NotMember,
    /// A member now, but this session started before that: log in again.
    Relogin,
}

/// The answer for this process and the service's token at `token_path`.
pub fn group_access(token_path: &Path) -> GroupAccess {
    match std::fs::File::open(token_path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        // Readable, or absent (the service is not there): not the group.
        _ => return GroupAccess::Ok,
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    decide(
        own_backup_common::UNIX_ACCESS_GROUP,
        &status,
        &group,
        &passwd,
    )
}

/// [`group_access`] once the token has turned this process away, from the
/// three files' contents.
fn decide(name: &str, status: &str, group: &str, passwd: &str) -> GroupAccess {
    // A group with no entry: nobody can be in it, and logging in again
    // changes nothing.
    let Some((gid, members)) = group_entry(group, name) else {
        return GroupAccess::NotMember;
    };
    if process_groups(status).contains(&gid) {
        // This process has the group and is still refused: something else
        // (the file's own mode) — not for this message.
        return GroupAccess::Ok;
    }
    let uid = status_field(status, "Uid:").and_then(|v| v.first().copied());
    let user = uid.and_then(|uid| user_entry(passwd, uid));
    let listed = user
        .as_ref()
        .is_some_and(|(login, primary)| members.iter().any(|m| m == login) || *primary == gid);
    if listed {
        GroupAccess::Relogin
    } else {
        GroupAccess::NotMember
    }
}

/// The numbers of a `/proc/<pid>/status` line such as `Groups: 4 24 1000`.
fn status_field(status: &str, key: &str) -> Option<Vec<u32>> {
    let line = status.lines().find(|l| l.starts_with(key))?;
    Some(
        line[key.len()..]
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect(),
    )
}

/// The groups this process has: supplementary and its own.
fn process_groups(status: &str) -> Vec<u32> {
    let mut all = status_field(status, "Groups:").unwrap_or_default();
    if let Some(gid) = status_field(status, "Gid:").and_then(|v| v.first().copied()) {
        all.push(gid);
    }
    all
}

/// `name:x:gid:a,b` → (gid, members).
fn group_entry(group: &str, name: &str) -> Option<(u32, Vec<String>)> {
    group.lines().find_map(|line| {
        let mut f = line.split(':');
        if f.next()? != name {
            return None;
        }
        let gid = f.nth(1)?.trim().parse().ok()?;
        let members = f
            .next()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect();
        Some((gid, members))
    })
}

/// The login and primary group of `uid` in `/etc/passwd`.
fn user_entry(passwd: &str, uid: u32) -> Option<(String, u32)> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 || f[2].parse::<u32>().ok()? != uid {
            return None;
        }
        Some((f[0].to_string(), f[3].parse().ok()?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          anna:x:1000:1000:Anna:/home/anna:/bin/bash\n";

    fn status(gids: &str) -> String {
        format!("Name:\tgui\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nGroups:\t{gids}\n")
    }

    #[test]
    fn a_user_outside_the_group_is_told_to_join_it() {
        let group = "sudo:x:27:anna\nown-backup:x:990:\n";
        assert_eq!(
            decide("own-backup", &status("27 1000"), group, PASSWD),
            GroupAccess::NotMember
        );
    }

    #[test]
    fn a_user_added_after_login_is_told_to_log_in_again() {
        let group = "sudo:x:27:anna\nown-backup:x:990:bob,anna\n";
        assert_eq!(
            decide("own-backup", &status("27 1000"), group, PASSWD),
            GroupAccess::Relogin
        );
    }

    #[test]
    fn a_session_that_has_the_group_is_not_this_problem() {
        let group = "own-backup:x:990:anna\n";
        assert_eq!(
            decide("own-backup", &status("27 990 1000"), group, PASSWD),
            GroupAccess::Ok
        );
    }

    #[test]
    fn no_group_at_all_means_not_a_member() {
        assert_eq!(
            decide("own-backup", &status("27"), "sudo:x:27:anna\n", PASSWD),
            GroupAccess::NotMember
        );
    }

    #[test]
    fn a_readable_or_missing_token_is_not_the_group() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(group_access(&dir.path().join("absent")), GroupAccess::Ok);
        let token = dir.path().join("token");
        std::fs::write(&token, "t").unwrap();
        assert_eq!(group_access(&token), GroupAccess::Ok);
    }
}
