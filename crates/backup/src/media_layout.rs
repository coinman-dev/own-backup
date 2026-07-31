//! What goes onto the boot medium, and what the medium does when it boots.
//!
//! Kept apart from the building itself (DISM, driver export, image capture) so
//! the decisions can be read — and tested — without a Windows host. Everything
//! here was learned by booting the thing; the comments say which failure each
//! rule came from, because none of them are obvious and all of them cost a
//! rebuild cycle to rediscover.
//!
//! ## Where the payload lives
//!
//! WinPE loads the whole of `boot.wim` into a RAM disk before anything runs, so
//! every byte inside it is a byte the machine never gets back. Our programs are
//! small and must be there. The WebView2 runtime is 849 MB unpacked (~250 MB of
//! the image) and is only touched when the interface starts, so it sits on the
//! medium's own partition instead, next to `\sources`. The launcher finds that
//! partition by a marker file, because WinPE assigns it whatever letter it likes.

use std::path::{Path, PathBuf};

/// Folder on the medium's partition holding everything that does not need to be
/// in RAM. The marker file inside it is how the running medium recognises which
/// drive it booted from.
pub const MEDIA_DIR: &str = "OwnBackup";
/// Marker file, content is its own explanation for anyone who finds it.
pub const MEDIA_MARKER: &str = "media.id";
pub const MEDIA_MARKER_TEXT: &str =
    "My Own Backup boot medium. This file marks the volume so the medium can \
     find its own WebView2 runtime after boot.\r\n";

/// Program files that go inside the image, so they are present even if the
/// medium's own partition somehow does not come up.
pub const PROGRAM_FILES: [&str; 3] =
    ["own-backup-gui.exe", "own-backup-service.exe", "own-backup-cli.exe"];

/// System DLLs WinPE does not carry but the browser process links against.
///
/// Copied from the building machine's System32, which is safe because the base
/// image comes from the same installation. Never overwrite what the image
/// already has.
///
/// **`dcomp.dll` is deliberately absent and must stay absent.** Edge decides how
/// to present frames by trying to create a DirectComposition device, not by
/// reading a switch; with that DLL present the probe succeeds, a composition
/// swap chain is chosen, and with no DWM in WinPE every Present fails —
/// a black window over a perfectly live page (found 2026-07-31, and it was our
/// own DLL list that put it there).
pub const SYSTEM_DLLS: [&str; 30] = [
    "virtdisk.dll", "newdev.dll", "dwmapi.dll", "comctl32.dll", "shlwapi.dll",
    "cfgmgr32.dll", "setupapi.dll", "crypt32.dll", "bcrypt.dll", "bcryptprimitives.dll",
    "mf.dll", "mfplat.dll", "mfreadwrite.dll", "mfcore.dll", "mfsrcsnk.dll",
    "mfperfhelper.dll", "evr.dll", "d3d11.dll", "d3d10_1.dll", "d3d10.dll",
    "d3d9.dll", "dxgi.dll", "d2d1.dll", "dwrite.dll", "d3dcompiler_47.dll",
    "windowscodecs.dll", "propsys.dll", "userenv.dll", "winhttp.dll", "usp10.dll",
];

/// The DLL whose presence breaks rendering. Deleted from the image if the base
/// already had it, and never copied in.
pub const FORBIDDEN_DLL: &str = "dcomp.dll";

/// Visual C++ runtime: WebView2 is not ours and is not statically linked. It
/// wants these beside the browser executable and in its `x64` subfolder.
pub const VC_RUNTIME: [&str; 3] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];

/// Parts of the WebView2 runtime nothing in our interface can reach.
///
/// `ResiliencyLinks` is a byte-identical second copy of `Locales` and the `.pak`
/// files, kept for Edge's self-repair. Both copies have to go for the space to
/// be freed — a WIM stores identical data once, so dropping one alone frees
/// nothing.
pub const RUNTIME_DROP_DIRS: [&str; 4] =
    ["ResiliencyLinks", "WidevineCdm", "Installer", "edge_game_assist"];

/// Locales worth keeping: the interface ships these two, and Chromium falls back
/// to en-US for any language whose `.pak` is missing. The other 166 are 120 MB.
pub const KEEP_LOCALES: [&str; 2] = ["ru.pak", "en-US.pak"];

/// WinPE's default 32 MB of writable space is not enough for a browser engine.
pub const SCRATCH_SPACE_MB: u32 = 512;

/// Browser arguments, first line of `webview2-args.txt`.
///
/// They are in a file rather than baked into the launcher so a flag can be tried
/// without rebuilding the image, and the GUI passes them through the WebView2
/// API — wry always sets `additionalBrowserArguments` itself, and an API value
/// makes the loader ignore the environment variable.
///
/// The `--disable-features` list REPLACES wry's own defaults, so those are
/// re-listed here. `--allow-run-as-system` is required because WinPE runs
/// everything as SYSTEM and WebView2 refuses SYSTEM without it.
pub fn browser_args(log_path: &str) -> String {
    format!(
        "--allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu \
         --disable-gpu-compositing --disable-direct-composition \
         --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired \
         --no-first-run --enable-logging --v=1 --log-file={log_path}"
    )
}

/// The whole `webview2-args.txt`: the live line, then the alternatives, because
/// the medium is a RAM disk and swapping the top line with notepad is faster
/// than any rebuild.
pub fn webview2_args_file(log_path: &str) -> String {
    let base = "--allow-run-as-system --no-sandbox";
    let feats = "msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity";
    lines(&[
        &browser_args(log_path),
        "",
        "Only the line above is used. To try another set, move its line to the top",
        "with notepad and run start-media.cmd again.",
        "",
        "B - the same, plus forcing software GL:",
        &format!(
            "{base} --in-process-gpu --disable-gpu --disable-gpu-compositing \
             --disable-direct-composition --override-use-software-gl-for-tests \
             --disable-features={feats},RemoveRedirectionBitmap --no-first-run \
             --enable-logging --v=1 --log-file={log_path}"
        ),
        "",
        "C - the other direction: give D3D a working device (WARP) instead of banning it:",
        &format!(
            "{base} --in-process-gpu --use-gl=angle --use-angle=d3d11-warp \
             --disable-features={feats} --no-first-run --enable-logging --v=1 \
             --log-file={log_path}"
        ),
    ])
}

/// `startnet.cmd` — what WinPE's default shell runs at boot.
///
/// It starts NOTHING of ours. The first medium auto-ran the interface in the
/// foreground of the only console; when the window failed to open, the process
/// stayed resident and the console was dead — nothing could be typed, nothing
/// inspected. (User decision 2026-07-31: do not bring autostart back.)
///
/// And it is `startnet.cmd` rather than `winpeshl.ini` because entries in that
/// file go to `CreateProcess`, which cannot run a `.cmd` — the medium booted
/// into a bare console. With no `winpeshl.ini` the default shell runs
/// `cmd /k startnet.cmd`, which is a batch file by definition.
pub fn startnet_cmd(app_dir: &str) -> String {
    lines(&[
        "@echo off",
        "wpeinit",
        "echo.",
        "echo === My Own Backup ===",
        "echo Nothing starts automatically. When ready, run:",
        &format!("echo   \"{app_dir}\\start-media.cmd\""),
        "echo It fixes the screen mode, starts the engine and opens the interface",
        "echo in its own window; this console stays usable.",
        "echo.",
    ])
}

/// `start-media.cmd` — everything the medium needs set before the programs run.
///
/// The WebView2 runtime lives on the medium's partition, not in the image, so
/// the drive it booted from is found by its marker file: WinPE assigns that
/// volume whatever letter it likes, and it is not `X:`.
pub fn start_media_cmd(app_dir: &str, temp_dir: &str) -> String {
    let args_file = format!("{app_dir}\\webview2-args.txt");
    let marker = format!("{MEDIA_DIR}\\{MEDIA_MARKER}");
    lines(&[
        "@echo off",
        "title My Own Backup",
        &format!("\"{app_dir}\\own-backup-cli.exe\" display --min 1280x720"),
        &format!("md \"{temp_dir}\" 2>nul"),
        &format!("md \"{temp_dir}\\Profile\\AppData\\Local\" 2>nul"),
        &format!("md \"{temp_dir}\\Profile\\AppData\\Roaming\" 2>nul"),
        &format!("md \"{temp_dir}\\WebView2Data\" 2>nul"),
        &format!("set \"TEMP={temp_dir}\""),
        &format!("set \"TMP={temp_dir}\""),
        &format!("set \"USERPROFILE={temp_dir}\\Profile\""),
        &format!("set \"LOCALAPPDATA={temp_dir}\\Profile\\AppData\\Local\""),
        &format!("set \"APPDATA={temp_dir}\\Profile\\AppData\\Roaming\""),
        "",
        "rem The runtime is on the medium's own partition, not in the image: it is",
        "rem 250 MB that WinPE would otherwise hold in RAM from boot to shutdown.",
        "rem The letter is not known in advance, so the volume is found by its marker.",
        &format!("set \"WV=\""),
        &format!(
            "for %%D in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do \
             if exist \"%%D:\\{marker}\" if not defined WV set \"WV=%%D:\\{MEDIA_DIR}\\WebView2\""
        ),
        &format!("if not defined WV set \"WV={app_dir}\\WebView2\""),
        "set \"WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=%WV%\"",
        "echo WebView2: %WV%",
        &format!("set \"WEBVIEW2_USER_DATA_FOLDER={temp_dir}\\WebView2Data\""),
        &format!("set \"ARGS_FILE={args_file}\""),
        &format!(
            "for %%D in (C D E F G H) do if exist \"%%D:\\ownbackup-webview2-args.txt\" \
             set \"ARGS_FILE=%%D:\\ownbackup-webview2-args.txt\""
        ),
        "set /p \"WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=\" <\"%ARGS_FILE%\"",
        "echo Browser arguments read from: %ARGS_FILE%",
        "if not defined WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS echo [!] no arguments came out of that file",
        "",
        "rem Media mode: state goes to the RAM disk instead of the rescued machine's",
        "rem C:, the engine runs as a process, and no VSS is attempted (the system",
        "rem here is offline, so a direct read is already consistent).",
        "set \"OWN_BACKUP_MEDIA=1\"",
        "set \"RUST_BACKTRACE=1\"",
        "set \"RUST_LOG=info,own_backup_gui=debug\"",
        "echo Starting the engine...",
        &format!(
            "start \"own-backup engine\" /min \"{app_dir}\\own-backup-service.exe\" --run --media"
        ),
        "echo Starting the interface...",
        &format!("start \"\" \"{app_dir}\\own-backup-gui.exe\""),
        "echo.",
        "echo The interface starts in its own window; this console stays usable.",
        "echo If no window appears, look at:",
        &format!("echo   \"{app_dir}\\own-backup-gui.log\""),
        &format!("echo   {temp_dir}\\webview2.log"),
    ])
}

/// CRLF, because these files are read by `cmd` on a machine that has never seen
/// a Unix line ending.
fn lines(rows: &[&str]) -> String {
    let mut s = String::new();
    for r in rows {
        s.push_str(r);
        s.push_str("\r\n");
    }
    s
}

/// Should this locale file be kept?
pub fn keep_locale(file_name: &str) -> bool {
    KEEP_LOCALES.iter().any(|k| k.eq_ignore_ascii_case(file_name))
}

/// Files and folders to remove from a copied WebView2 runtime, given its root.
/// Returned rather than deleted here so the caller can log and count.
pub fn runtime_trim_list(runtime_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = RUNTIME_DROP_DIRS.iter().map(|d| runtime_root.join(d)).collect();
    let locales = runtime_root.join("Locales");
    if let Ok(rd) = std::fs::read_dir(&locales) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.to_ascii_lowercase().ends_with(".pak") && !keep_locale(&name) {
                out.push(e.path());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Autostart is the one thing that made the medium unusable rather than
    /// merely broken: a dead console with no way in.
    #[test]
    fn boot_starts_nothing_by_itself() {
        let s = startnet_cmd(r"X:\Program Files\OwnBackup");
        assert!(s.contains("wpeinit"));
        for line in s.lines() {
            let l = line.trim_start();
            assert!(
                !(l.starts_with("start ") || l.contains(".exe\"") && !l.starts_with("echo")),
                "startnet must not launch anything: {line:?}"
            );
        }
        assert!(s.contains("Nothing starts automatically"));
    }

    /// The console has to survive a window that never appears — that is how the
    /// first medium trapped its user.
    #[test]
    fn the_interface_is_launched_detached() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        assert!(s.contains(r#"start "" "X:\Program Files\OwnBackup\own-backup-gui.exe""#));
        assert!(s.contains("--run --media"), "the engine must know it is on the medium");
        assert!(s.contains("OWN_BACKUP_MEDIA=1"));
    }

    /// The runtime is off-image, so the launcher must locate the boot volume by
    /// its marker instead of assuming a letter.
    #[test]
    fn the_runtime_is_found_by_marker_with_a_fallback() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        assert!(s.contains(&format!(r"{MEDIA_DIR}\{MEDIA_MARKER}")));
        assert!(s.contains(r"if not defined WV set "), "a fallback path must exist");
        assert!(s.contains("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=%WV%"));
    }

    #[test]
    fn every_generated_file_uses_windows_line_endings() {
        for s in [
            startnet_cmd("X:\\app"),
            start_media_cmd("X:\\app", "X:\\tmp"),
            webview2_args_file("X:\\tmp\\webview2.log"),
        ] {
            assert!(s.contains("\r\n"));
            assert!(!s.replace("\r\n", "").contains('\n'), "a bare LF slipped in");
        }
    }

    /// Both of these were found the hard way; a regression would show up as a
    /// black window or a refusal to start, neither of which points at the cause.
    #[test]
    fn the_arguments_carry_what_winpe_needs() {
        let a = browser_args(r"X:\Windows\Temp\OwnBackup\webview2.log");
        assert!(a.contains("--allow-run-as-system"), "WinPE runs everything as SYSTEM");
        assert!(a.contains("--no-sandbox"), "no sandbox in WinPE");
        // wry's own defaults are replaced by whatever we pass, so they must be here.
        for f in ["msWebOOUI", "msPdfOOUI", "msSmartScreenProtection"] {
            assert!(a.contains(f), "wry's default feature {f} was dropped");
        }
    }

    /// Only the first line is read (`set /p`), so the alternatives below it are
    /// documentation, not configuration.
    #[test]
    fn the_arguments_file_is_usable_line_one_alone() {
        let f = webview2_args_file("X:\\log");
        let first = f.lines().next().unwrap();
        assert!(first.starts_with("--allow-run-as-system"));
        assert!(f.lines().count() > 5, "the alternatives should be there too");
    }

    /// The DLL that made the window black must never be shipped, by any route.
    #[test]
    fn the_composition_dll_is_not_in_the_copy_list() {
        assert!(
            !SYSTEM_DLLS.iter().any(|d| d.eq_ignore_ascii_case(FORBIDDEN_DLL)),
            "dcomp.dll in the copy list means a black window on the medium"
        );
    }

    #[test]
    fn trimming_keeps_the_two_locales_we_ship() {
        assert!(keep_locale("ru.pak"));
        assert!(keep_locale("en-US.pak"));
        assert!(!keep_locale("af.pak"));
        assert!(!keep_locale("zh-CN.pak"));
    }

    #[test]
    fn trim_list_names_the_duplicate_copy_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Locales")).unwrap();
        for f in ["ru.pak", "en-US.pak", "af.pak", "de.pak"] {
            std::fs::write(root.join("Locales").join(f), b"x").unwrap();
        }
        let list = runtime_trim_list(root);
        let names: Vec<String> =
            list.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert!(names.contains(&"ResiliencyLinks".to_string()), "{names:?}");
        assert!(names.contains(&"af.pak".to_string()));
        assert!(names.contains(&"de.pak".to_string()));
        assert!(!names.contains(&"ru.pak".to_string()));
        assert!(!names.contains(&"en-US.pak".to_string()));
    }
}
