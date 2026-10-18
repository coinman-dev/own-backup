//! Reading a secret from the console without echoing it.
//!
//! Two commands need this and a third is coming, so it lives in one place: a
//! passphrase that appears on screen ends up in a screenshot, a shoulder, or a
//! terminal's scrollback, and the difference between the platforms is three
//! lines of console mode — or, on Unix, of termios.

use std::io::{BufRead, Write};

/// Ask for a secret and read one line of it.
#[cfg(target_os = "windows")]
pub fn ask_secret(prompt: &str) -> std::io::Result<String> {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
        STD_INPUT_HANDLE,
    };
    print!("{prompt}");
    std::io::stdout().flush()?;

    // SAFETY: console-mode query and set on our own standard handle; the mode
    // is put back below whatever the read does.
    let restore = unsafe {
        GetStdHandle(STD_INPUT_HANDLE).ok().and_then(|h| {
            let mut mode = CONSOLE_MODE(0);
            GetConsoleMode(h, &mut mode).ok()?;
            SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT).ok()?;
            Some((h, mode))
        })
    };
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if let Some((h, mode)) = restore {
        // SAFETY: same handle, the mode it had before.
        let _ = unsafe { SetConsoleMode(h, mode) };
    }
    println!();
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Ask for a secret and read one line of it, with the terminal's echo off.
///
/// When stdin is not a terminal (a pipe, a file) there is no echo to turn off
/// and the line is simply read. The terminal's settings are put back on every
/// way out of the read, an error included. Ctrl+C at the prompt ends the
/// process before that can run; interactive shells restore the terminal of a
/// job a signal killed.
#[cfg(unix)]
pub fn ask_secret(prompt: &str) -> std::io::Result<String> {
    use std::os::unix::io::AsRawFd;

    /// The terminal's settings before the prompt, put back on drop.
    struct EchoOff {
        fd: i32,
        saved: libc::termios,
    }
    impl Drop for EchoOff {
        fn drop(&mut self) {
            // SAFETY: the same descriptor and the settings read from it.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
        }
    }

    print!("{prompt}");
    std::io::stdout().flush()?;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    // SAFETY: isatty/tcgetattr/tcsetattr on our own standard input; the
    // termios is fully written by tcgetattr before it is read.
    let guard = unsafe {
        if libc::isatty(fd) == 1 {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut quiet = saved;
            // ECHONL keeps the newline the user types visible, so the next
            // output starts on its own line.
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Some(EchoOff { fd, saved })
        } else {
            None
        }
    };
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    drop(guard);
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Ask twice and insist the two agree. For a secret that is being **set**: a
/// typo in a password nobody can read back is a locked archive.
pub fn ask_secret_twice(prompt: &str, again: &str) -> std::io::Result<String> {
    let first = ask_secret(prompt)?;
    let second = ask_secret(again)?;
    if first != second {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "введённые значения не совпадают",
        ));
    }
    Ok(first)
}
