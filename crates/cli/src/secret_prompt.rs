//! Reading a secret from the console without echoing it.
//!
//! Two commands need this and a third is coming, so it lives in one place: a
//! passphrase that appears on screen ends up in a screenshot, a shoulder, or a
//! terminal's scrollback, and the difference between the platforms is three
//! lines of console mode.
//!
//! On Linux the echo is NOT suppressed — there is no product on Linux yet
//! (`CLAUDE.md`: Windows first), and a prompt that silently pretends to hide a
//! password would be worse than one that says it does not.

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

#[cfg(not(target_os = "windows"))]
pub fn ask_secret(prompt: &str) -> std::io::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
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
