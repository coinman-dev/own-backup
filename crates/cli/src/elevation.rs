//! Self-elevation for disk-touching commands (Windows only).
//!
//! Raw volume I/O, VSS and virtual-disk attach all fail with
//! ERROR_ACCESS_DENIED (0x80070005) in a non-elevated console. Instead of
//! surfacing that cryptic error, the CLI relaunches itself through the UAC
//! prompt (`runas` verb), waits for the elevated child and exits with its
//! code. The child runs in a NEW console window (that is how `runas` works
//! for console apps), so it is started with the hidden `--elevated` flag —
//! `main` then holds the window open on exit for the output to be readable.

#![cfg(target_os = "windows")]

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

/// True when the current process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut info = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut info as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && info.TokenIsElevated != 0
    }
}

/// Relaunch the current command elevated (UAC prompt) and wait for it.
/// Returns the child's exit code; an error means the prompt was declined or
/// the launch failed.
pub fn relaunch_elevated() -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    // `--elevated` first, then the original arguments verbatim: the child
    // knows it owns a fresh console and must hold it open before exiting.
    let mut params = String::from("--elevated");
    for arg in std::env::args().skip(1) {
        params.push(' ');
        params.push_str(&quote_arg(&arg));
    }

    let exe_w = to_wide(&exe.to_string_lossy());
    let verb_w = to_wide("runas");
    let params_w = to_wide(&params);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };

    // SAFETY: all pointers stay alive for the duration of the call.
    unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            // ERROR_CANCELLED (1223) = the user declined the UAC prompt.
            let err = GetLastError().0;
            return Err(if err == 1223 {
                "запрос повышения прав отклонён (UAC)".into()
            } else {
                format!(
                    "ShellExecuteExW failed: {}",
                    std::io::Error::from_raw_os_error(err as i32)
                )
            });
        }
        if info.hProcess.is_invalid() {
            return Err("elevated process handle not returned".into());
        }
        let wait = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let got_code = GetExitCodeProcess(info.hProcess, &mut code).is_ok();
        let _ = CloseHandle(info.hProcess);
        if wait != WAIT_OBJECT_0 || !got_code {
            return Err("could not wait for the elevated process".into());
        }
        Ok(code as i32)
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Quote one argument per the MSVC `CommandLineToArgvW` rules, so the elevated
/// child parses exactly the argv the user typed (paths with spaces included).
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            // Backslashes before a quote must be doubled, plus one to escape it.
            out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat('\\').take(backslashes));
        }
        out.push(c);
        backslashes = 0;
    }
    // Trailing backslashes are doubled so the closing quote stays a quote.
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting_matches_argv_rules() {
        assert_eq!(quote_arg("C:"), "C:");
        assert_eq!(
            quote_arg("e:\\backup-test\\c.qcow2"),
            "e:\\backup-test\\c.qcow2"
        );
        assert_eq!(quote_arg("path with spaces"), "\"path with spaces\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("dir\\"), "dir\\");
        assert_eq!(quote_arg("dir with spaces\\"), "\"dir with spaces\\\\\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(
            quote_arg("back\\\\slash end\\"),
            "\"back\\\\slash end\\\\\""
        );
    }
}
