//! Laboratory only: makes the boot medium's WebView2 DevTools port reachable
//! from the host, so an acceptance script can drive the real interface.
//!
//! WebView2 listens on 127.0.0.1:9222 inside the VM (`--remote-debugging-port`
//! from the medium's own `ownbackup-webview2-args.txt` override); QEMU's
//! user-mode network forwards to the guest's address, not to its loopback.
//! This copies 0.0.0.0:9223 <-> 127.0.0.1:9222, byte for byte. It is never
//! part of a product medium.
use std::io::copy;
use std::net::{TcpListener, TcpStream};
use std::thread;

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let _ = copy(&mut from, &mut to);
    let _ = to.shutdown(std::net::Shutdown::Write);
}

fn main() -> std::io::Result<()> {
    let listen = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:9223".into());
    let target = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:9222".into());
    let listener = TcpListener::bind(&listen)?;
    for client in listener.incoming().flatten() {
        let target = target.clone();
        thread::spawn(move || {
            let Ok(server) = TcpStream::connect(&target) else {
                return;
            };
            let (Ok(c2), Ok(s2)) = (client.try_clone(), server.try_clone()) else {
                return;
            };
            thread::spawn(move || pipe(c2, s2));
            pipe(server, client);
        });
    }
    Ok(())
}
