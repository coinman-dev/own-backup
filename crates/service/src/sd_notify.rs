//! systemd's readiness protocol (`Type=notify` in the packaged unit).
//!
//! With `Type=simple` systemd called the engine started the moment it was
//! forked, so `systemctl start` — and the package's postinst — returned before
//! the socket existed, and a window opened right after an install found
//! nothing to talk to. Now the unit is "active" only once the engine has said
//! `READY=1`, after the socket and the token are in place.
//!
//! The protocol is one datagram per message to the socket named in
//! `$NOTIFY_SOCKET` (a leading `@` means the abstract namespace). Without the
//! variable — a run by hand, a test, the boot medium — there is nobody to tell
//! and nothing is sent. Failure to send is not fatal: systemd then times the
//! start out and says so in its own status, which is where an administrator
//! looks.

/// Tell systemd that the engine is serving.
pub fn ready() {
    send("READY=1\nSTATUS=serving");
}

/// Tell systemd that a stop has begun; the unit shows "deactivating" while the
/// running operation winds down instead of looking hung.
pub fn stopping() {
    send("STOPPING=1\nSTATUS=stopping: waiting for the running operation");
}

fn send(message: &str) {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send_to(&socket, message) {
        tracing::warn!(error = %e, "could not notify systemd");
    }
}

#[cfg(target_os = "linux")]
fn send_to(socket: &std::ffi::OsStr, message: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let bytes = socket.as_bytes();
    let addr = match bytes.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(std::path::Path::new(socket))?,
    };
    UnixDatagram::unbound()?.send_to_addr(message.as_bytes(), &addr)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send_to(_socket: &std::ffi::OsStr, _message: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::send_to;

    #[test]
    fn a_message_reaches_a_path_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        send_to(path.as_os_str(), "READY=1").unwrap();
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn a_leading_at_means_the_abstract_namespace() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("own-backup-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = std::os::unix::net::UnixDatagram::bind_addr(&addr).unwrap();
        send_to(std::ffi::OsStr::new(&format!("@{name}")), "STOPPING=1").unwrap();
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"STOPPING=1");
    }
}
