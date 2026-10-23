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

use own_backup_common::tr;
use std::path::{Path, PathBuf};

// Where the medium keeps its own files, and how it is recognised. Defined in
// `common` because the desktop has to find the stick WITHOUT this crate: it is
// started by `winpeshl` and inherits nothing from the launcher script, so it
// asks the same question from its own side.
pub use own_backup_common::media_paths::{
    MEDIA_DIR, MEDIA_LOGS_DIR, MEDIA_MARKER, MEDIA_MARKER_TEXT,
};

/// Program files that go inside the image, so they are present even if the
/// medium's own partition somehow does not come up.
///
/// `own-backup-shell.exe` is the medium's desktop: the strip along the bottom
/// with the clock and — the reason it exists at all — which keyboard layout the
/// window in front is using. It is listed here with the rest so that a build
/// missing it fails at the check, rather than producing a medium that starts it
/// and finds nothing.
pub const PROGRAM_FILES: [&str; 4] = [
    "own-backup-gui.exe",
    "own-backup-service.exe",
    "own-backup-cli.exe",
    "own-backup-shell.exe",
];

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
    "virtdisk.dll",
    "newdev.dll",
    "dwmapi.dll",
    "comctl32.dll",
    "shlwapi.dll",
    "cfgmgr32.dll",
    "setupapi.dll",
    "crypt32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "mf.dll",
    "mfplat.dll",
    "mfreadwrite.dll",
    "mfcore.dll",
    "mfsrcsnk.dll",
    "mfperfhelper.dll",
    "evr.dll",
    "d3d11.dll",
    "d3d10_1.dll",
    "d3d10.dll",
    "d3d9.dll",
    "dxgi.dll",
    "d2d1.dll",
    "dwrite.dll",
    "d3dcompiler_47.dll",
    "windowscodecs.dll",
    "propsys.dll",
    "userenv.dll",
    "winhttp.dll",
    "usp10.dll",
];

/// The user-mode half of Windows' own iSCSI initiator — how a backup image
/// becomes a disk on the medium ([`crate::iscsi_win`]). The kernel driver
/// (`msiscsi.sys`) is already in WinRE; these are not, and without them the
/// initiator's API is not there to call. Copied from the building machine,
/// which is safe for the same reason [`SYSTEM_DLLS`] is: same Windows.
pub const ISCSI_FILES: [&str; 6] = [
    "iscsidsc.dll",
    "iscsiexe.dll",
    "iscsium.dll",
    "iscsiwmi.dll",
    "iscsilog.dll",
    "iscsicli.exe",
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
///
/// `EBWebView\x86` is the 32-bit client; the medium is x64 throughout. `BHO`
/// and `PdfPreview` are the Internet Explorer shim and the PDF viewer.
pub const RUNTIME_DROP_DIRS: [&str; 8] = [
    "ResiliencyLinks",
    "WidevineCdm",
    "Installer",
    "edge_game_assist",
    "BHO",
    "PdfPreview",
    "identity_proxy",
    "EBWebView\\x86",
];

/// Files in the runtime's root that belong to features the medium has no way to
/// reach: the full browser and its updaters, Copilot, the PDF viewer, the
/// hardware shader compilers, telemetry
/// and sign-in, and the Internet Explorer engine adapter.
///
/// Checked before choosing, not guessed: none of these appear in the import
/// tables of `msedge.dll`, `msedgewebview2.exe` or `EmbeddedBrowserWebView.dll`,
/// statically or delay-loaded — they are opened by name when a feature asks for
/// them. `msedge_elf.dll` IS a static import and must stay; so does the ANGLE
/// set (`libEGL`, `libGLESv2`, `d3dcompiler_47`) and SwiftShader/Vulkan,
/// which Chromium uses for software rendering even with `--disable-gpu`.
/// Removing the latter leaves WinPE with EGL_NOT_INITIALIZED and an empty GUI.
/// `ffmpeg.dll` is delay-loaded and must also stay.
pub const RUNTIME_DROP_FILES: [&str; 20] = [
    // the full browser and everything that installs or updates it
    "msedge.exe",
    "setup.exe",
    "elevation_service.exe",
    "elevated_tracing_service.exe",
    "notification_helper.exe",
    "cookie_exporter.exe",
    "identity_helper.exe",
    "platform_experiences_helper.exe",
    "msedge_proxy.exe",
    "msedge_pwa_launcher.exe",
    "pwahelper.exe",
    // Copilot and the on-device model runtime
    "copilotapp.exe",
    "mscopilot.exe",
    "onnxruntime.dll",
    "learning_tools.dll",
    "onramp.dll",
    // PDF viewer
    "mspdf.dll",
    // hardware shader compilers; software Vulkan/SwiftShader must stay
    "dxcompiler.dll",
    "dxil.dll",
    // the Internet Explorer engine adapter
    "dual_engine_adapter_x64.dll",
];

/// Locales worth keeping: the interface ships these two, and Chromium falls back
/// to en-US for any language whose `.pak` is missing. The other 166 are 120 MB.
pub const KEEP_LOCALES: [&str; 2] = ["ru.pak", "en-US.pak"];

/// Which device classes belong on a rescue medium, and which buses each is
/// allowed to sit on.
///
/// GUIDs rather than class names: the name comes back localised on some systems
/// and has changed spelling between releases, while these values have been fixed
/// since Windows NT. A filter built on names silently matches nothing on a
/// machine that answers in Russian.
///
/// The class alone is not enough either, and it fails in two opposite ways.
/// `Net` covers the Ethernet card and the VPN adapter installed beside it —
/// same class, both present — but a card is enumerated on `PCI` or `USB` while
/// anything invented in software hangs off `ROOT`. And `USB` covers both the
/// host controller and everything plugged into it: the controller is a PCI
/// device, the printer is a USB one. Requiring PCI for controllers, and allowing
/// USB only where a real device attaches that way (a network dongle, an external
/// disk), separates them.
///
/// `System` is deliberately absent however tempting it looks: it is where
/// chipset drivers live, and also Logitech's receiver, ACPI oddities and half
/// the vendor utilities on a laptop.
///
/// Checked against a real machine (2026-08-01): of the 23 packages the old
/// class-name filter accepted, this leaves the NVMe controller, the AHCI
/// controller and the Ethernet card — and drops the Logitech receiver, the Canon
/// and Epson printers, five Intel chipset packages, WireGuard, OpenVPN and the
/// Avast network filter.
pub const RESCUE_CLASSES: [(&str, &[&str], RescueRole); 5] = [
    // storage controllers — RAID, NVMe, SAS
    (
        "{4d36e97b-e325-11ce-bfc1-08002be10318}",
        &["PCI"],
        RescueRole::DiskController,
    ),
    // IDE / AHCI controllers
    (
        "{4d36e96a-e325-11ce-bfc1-08002be10318}",
        &["PCI"],
        RescueRole::DiskController,
    ),
    // the disks themselves, which can hang off either
    (
        "{4d36e967-e325-11ce-bfc1-08002be10318}",
        &["SCSI", "USB"],
        RescueRole::Disk,
    ),
    // network adapters: cards on PCI, dongles on USB
    (
        "{4d36e972-e325-11ce-bfc1-08002be10318}",
        &["PCI", "USB"],
        RescueRole::NetworkAdapter,
    ),
    // USB host controllers, which are PCI devices — not what is plugged in
    (
        "{36fc9e60-c465-11cf-8056-444553540000}",
        &["PCI"],
        RescueRole::UsbController,
    ),
];

/// What a device of a rescue class is, for the interface to say why its driver
/// is on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescueRole {
    DiskController,
    Disk,
    NetworkAdapter,
    UsbController,
}

impl RescueRole {
    /// In words, in the window's language.
    pub fn text(self) -> &'static str {
        crate::esp_scan::static_text(match self {
            RescueRole::DiskController => tr!("media_layout.role_disk_controller"),
            RescueRole::Disk => tr!("media_layout.role_disk"),
            RescueRole::NetworkAdapter => tr!("media_layout.role_network_adapter"),
            RescueRole::UsbController => tr!("media_layout.role_usb_controller"),
        })
    }
}

/// Just the GUIDs, for handing to a device query that should not walk classes we
/// are going to throw away anyway.
pub const RESCUE_CLASS_GUIDS: [&str; 5] = [
    "{4d36e97b-e325-11ce-bfc1-08002be10318}",
    "{4d36e96a-e325-11ce-bfc1-08002be10318}",
    "{4d36e967-e325-11ce-bfc1-08002be10318}",
    "{4d36e972-e325-11ce-bfc1-08002be10318}",
    "{36fc9e60-c465-11cf-8056-444553540000}",
];

/// Is this the published name of a third-party package (`oem12.inf`)?
///
/// That is the only kind `pnputil` can export. Everything else a device can be
/// bound to — `stornvme.inf`, `net1ic64.inf` — ships with Windows, and whether
/// the medium has it depends on the base image, not on this machine's store.
pub fn is_published_inf(inf: &str) -> bool {
    let lower = inf.trim().to_ascii_lowercase();
    lower
        .strip_prefix("oem")
        .and_then(|rest| rest.strip_suffix(".inf"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Does this class/bus pair belong on a rescue medium? Returns what it is, in
/// words and in the window's language ([`RescueRole::text`]), so the interface
/// can say why a driver is on the list.
pub fn rescue_role(class_guid: &str, bus: &str) -> Option<&'static str> {
    let guid = class_guid.trim().to_ascii_lowercase();
    let bus = bus.trim().to_ascii_uppercase();
    RESCUE_CLASSES.iter().find_map(|(g, buses, role)| {
        (g.eq_ignore_ascii_case(&guid) && buses.iter().any(|b| *b == bus)).then(|| role.text())
    })
}

/// WinPE's default 32 MB of writable space is not enough for a browser engine.
pub const SCRATCH_SPACE_MB: u32 = 512;

/// Text a console tool printed, decoded.
///
/// DISM and its kind write in the console's OEM code page, not UTF-8: on a
/// Russian Windows its errors arrived as `\u{fffd}訡��: 2 Cannot find…` and
/// went into the report exactly like that. UTF-8 is tried first (everything
/// ASCII, and our own tools, come out right), and the fallback is CP866 —
/// the OEM page of the Windows this program is built for. A machine on CP437
/// prints ASCII for the same messages, so it takes the first path anyway.
pub fn console_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp866_char(b)).collect(),
    }
}

/// One CP866 byte as a character. 0x00–0x7F is ASCII; above that the page is
/// three contiguous runs of Cyrillic with the box-drawing block between them.
fn cp866_char(b: u8) -> char {
    match b {
        0x00..=0x7F => b as char,
        // А…п — 0x80..0xAF is А..я without the last sixteen letters
        0x80..=0xAF => char::from_u32(0x0410 + (b - 0x80) as u32).unwrap_or('?'),
        // 0xB0..0xDF is the box-drawing block; it carries no meaning in a
        // message and a middle dot is less noisy than a wrong letter.
        0xB0..=0xDF => '·',
        // р…я
        0xE0..=0xEF => char::from_u32(0x0440 + (b - 0xE0) as u32).unwrap_or('?'),
        0xF0 => 'Ё',
        0xF1 => 'ё',
        _ => ' ',
    }
}

/// The last percentage in a chunk of DISM's output, as a whole percent.
///
/// DISM draws its progress as `[==========           ]  50.0%` and redraws that
/// same line with a carriage return, so a chunk read from its pipe holds
/// several of those; the last one is where it is now. Here rather than beside
/// the code that reads the pipe, because that code is Windows-only and this is
/// a parser — the thing most worth having a test for.
pub fn dism_percent(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let at = text.rfind('%')?;
    // Walk back over the digits (and the one decimal point) before the sign.
    let mut start = at;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_digit() || c == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == at {
        return None;
    }
    let value: f64 = text[start..at].parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value as u64)
}

/// Keyboard layouts the medium boots with: US English, then Russian.
///
/// WinPE starts with the single layout of the image it was built from, and the
/// medium is exactly where a BitLocker or LUKS passphrase has to be typed — a
/// Russian passphrase cannot be entered at all without a Russian layout, and
/// there is no taskbar to add one from. `0409:00000409` is en-US and
/// `0419:00000419` is ru-RU; the form is the one both DISM (`/Set-InputLocale`)
/// and `wpeutil SetKeyboardLayout` take, and Alt+Shift switches between them.
///
/// English is first on purpose: it is the layout every password was typed in
/// when it was set, and it is what the machine falls back to if the second
/// layout fails to load.
pub const INPUT_LOCALES: &str = "0409:00000409;0419:00000419";

/// Layout DLLs the above needs inside the image. WinRE normally carries them
/// (its own BitLocker recovery prompt has the same problem), but an image
/// trimmed of them would offer a layout that cannot load, so the builder checks
/// and fills them in from the machine it runs on.
pub const KEYBOARD_DLLS: [&str; 2] = ["kbdus.dll", "kbdru.dll"];

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
pub fn browser_args() -> String {
    "--allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu \
     --disable-gpu-compositing --disable-direct-composition \
     --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired \
     --no-first-run"
        .into()
}

/// The same, with Chromium's own log — the diagnostic line, not the default.
pub fn browser_args_logging(log_path: &str) -> String {
    format!(
        "{} --enable-logging=file --v=1 --log-file={log_path}",
        browser_args()
    )
}

/// The whole `webview2-args.txt`: the live line, then the alternatives, because
/// the medium is a RAM disk and swapping the top line with notepad is faster
/// than any rebuild.
///
/// **No Chromium logging by default.** Asking for it at all makes this Edge
/// build allocate a console for a process that has none — a window that lands
/// over the interface and fills with errors about services WinPE does not have
/// (the network-change watcher retries `WSALookupServiceBegin` once a second
/// forever, because there is no Network Location Awareness here; the Edge LLM
/// and geolocation lines are the same story). Directing the log to a file with
/// `--enable-logging=file` cut the flood but not the window: the switch's
/// presence is what opens it. Our own `own-backup-gui.log` is always written
/// and says whether the webview was created, which is the question that
/// actually gets asked; Chromium's log is a line away when it is needed.
pub fn webview2_args_file(log_path: &str) -> String {
    let base = "--allow-run-as-system --no-sandbox";
    let feats = "msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity";
    lines(&[
        &browser_args(),
        "",
        "Only the line above is used. To try another set, move its line to the top",
        "with notepad and run start-media.cmd again.",
        "",
        "A - with Chromium's own log (opens a console window of its own):",
        &browser_args_logging(log_path),
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
/// The interface starts by itself, which is what a rescue medium should do: the
/// person booting it wants the program, not a prompt.
///
/// The rule that survives from the medium that trapped its user (2026-07-31) is
/// not "start nothing" but "hold nothing". That first medium ran the interface
/// in the FOREGROUND of the only console; when the window failed to open, the
/// process stayed resident and the console was gone — nothing could be typed,
/// nothing inspected, nothing killed. Autostart returns now that the window
/// works, and everything is launched detached (see [`start_media_cmd`]) so this
/// console comes straight back to a prompt whether the interface appears or
/// not. That property is what the tests guard; the autostart itself is a
/// preference.
///
/// `start_desktop` says whether OUR desktop is launched from here, and exactly
/// one of the two boot files may do it. With a [`winpeshl_ini`] on the medium,
/// winpeshl starts the desktop itself and this script must not: a second copy
/// would find the first one's mutex, exit at once, and winpeshl treats its last
/// application exiting as the end of the session — a machine that reboots
/// seconds after it booted.
pub fn startnet_cmd(app_dir: &str, start_desktop: bool) -> String {
    let mut rows: Vec<String> = [
        "@echo off".to_string(),
        "wpeinit".to_string(),
        // Drivers added to the image sit in its driver store, and wpeinit's PnP
        // pass does not reliably START them — the display driver in particular
        // stayed installed-but-idle while Microsoft Basic Display kept the
        // screen at 1152x864. drvload is WinPE's own tool for exactly this:
        // load a driver from its INF into the running system, now. Loading one
        // that is already running is a cheap no-op, so every added package is
        // offered rather than guessing which ones PnP missed. Before
        // start-media.cmd, so the mode picker there sees the driver's modes.
        "for %%i in (%SystemRoot%\\INF\\oem*.inf) do drvload \"%%i\" >nul 2>&1".to_string(),
        // Set at build time in the image registry as well; this line is what
        // makes it true on a medium whose image had to be unpacked instead of
        // mounted, where DISM cannot write that registry.
        format!("wpeutil SetKeyboardLayout {INPUT_LOCALES} >nul 2>&1"),
        "echo.".to_string(),
        "echo === Own Backup ===".to_string(),
        "echo.".to_string(),
        format!("call \"{app_dir}\\start-media.cmd\""),
        "echo.".to_string(),
        "echo This console stays usable. To start the interface again:".to_string(),
        format!("echo   \"{app_dir}\\start-media.cmd\""),
    ]
    .to_vec();

    if start_desktop {
        // The classic medium: `cmd` is still the shell and the desktop is one
        // more program started detached at the end, so a failure here costs the
        // strip and nothing else.
        rows.extend([
            String::new(),
            "rem The strip along the bottom: clock, and which keyboard layout the".to_string(),
            "rem window in front is using - the thing a BitLocker passphrase is".to_string(),
            "rem typed into with nothing on screen to say which alphabet it is in.".to_string(),
            format!("start \"\" \"{app_dir}\\own-backup-shell.exe\""),
        ]);
    }
    lines(&rows.iter().map(String::as_str).collect::<Vec<_>>())
}

/// `winpeshl.ini` — what WinPE runs INSTEAD of its default shell.
///
/// Without this file the default shell is `cmd /k startnet.cmd`, and that
/// console is the whole desktop. With it, the desktop is the ONLY entry, and it
/// is the first thing on the screen.
///
/// **One entry, not two, and that is the whole design.** The obvious spelling
/// puts the boot script first and the desktop second, and it is what the first
/// medium did: a console window opened, the interface came up inside it, and
/// the desktop appeared last, under everything. Entries here run in order and
/// each is waited for, so the desktop cannot possibly come first that way.
/// Instead the desktop runs the boot script itself, with no window
/// ([`crate::media_layout::BOOT_SCRIPT`]) — the screen is ours from the first
/// second, and `wpeinit`, the drivers and the screen mode go on behind it.
///
/// **When this entry exits, the machine reboots.** That is winpeshl's rule and
/// not something added here — it is also exactly what closing the console has
/// always done on this medium. It is why the desktop offers no way out of
/// itself in WinPE, and why a desktop that cannot start must end in a console
/// rather than in a return.
pub fn winpeshl_ini(app_dir: &str) -> String {
    lines(&[
        "[LaunchApps]",
        &format!("\"{app_dir}\\own-backup-shell.exe\""),
    ])
}

/// `no-interface.cmd` — то, что человек ВИДИТ, когда интерфейса не будет.
///
/// Печатать диагноз в консоль запускателя бессмысленно: рабочий стол носителя
/// запускает `start-media.cmd` через `cmd.exe` с `CREATE_NO_WINDOW`, и этой
/// консоли на экране нет. Проверено 2026-09-20: с переименованной папкой
/// WebView2 человек получал пустой стол — ни окна, ни текста, ни намёка, что
/// делать. Поэтому неудача открывает СВОЁ окно и оставляет его открытым
/// (`cmd /k`): в нём повторяется разбор окружения и сказано, что командная
/// строка делает всю работу целиком.
pub fn no_interface_cmd(app_dir: &str) -> String {
    let cli = format!("{app_dir}\\own-backup-cli.exe");
    lines(&[
        "@echo off",
        "rem UTF-8 first: everything this window is here to show - the checks and",
        "rem their advice - comes out of our own program in Russian, and the",
        "rem console's default codepage would turn it into rubbish. The script's",
        "rem own lines stay ASCII, so they read the same either way.",
        "chcp 65001 >nul 2>&1",
        "title Own Backup - interface not started",
        "echo.",
        "echo ==================================================================",
        // Два разных исхода, и говорить о них одним текстом нельзя. Когда не
        // пустила проверка — причина ниже, в её строках. Когда проверки прошли,
        // а окна так и не появилось, ниже будут три плюса, и обещание «причина
        // напечатана ниже» станет неправдой (видно 2026-09-20 на пустом файле
        // движка: проверка его пропускает, потому что файл на месте).
        "if /i \"%~1\"==\"nowindow\" goto :nowindow",
        "echo  The interface was not started. The reason is printed below.",
        "goto :reason",
        ":nowindow",
        "echo  The interface was started but its window never appeared.",
        "echo  The checks below all passed - so the trouble is past them:",
        "echo  a broken engine file, or the engine dying on start. The logs say.",
        ":reason",
        "echo ==================================================================",
        "echo.",
        &format!("\"{cli}\" media-preflight --report-only"),
        "echo.",
        "echo The command line does the whole job - backup, verify, restore:",
        &format!("echo   \"{cli}\" --help"),
        "echo.",
        "echo Logs:",
        &format!("echo   \"{app_dir}\\Logs\\own-backup-gui-*.log\""),
        &format!("echo   \"{app_dir}\\Logs\\own-backup-service-*.log\""),
        "echo.",
    ])
}

// The script the desktop runs for itself, with no window. `CreateProcess`
// cannot start a `.cmd` at all — a medium that named the script directly booted
// into a bare console (2026-07-31) — so the desktop puts `cmd /c` in front of
// it. Defined in `common`, which the desktop can see.
pub use own_backup_common::media_paths::BOOT_SCRIPT;

/// `start-media.cmd` — everything the medium needs set before the programs run.
///
/// The WebView2 runtime lives on the medium's partition, not in the image, so
/// the drive it booted from is found by its marker file: WinPE assigns that
/// volume whatever letter it likes, and it is not `X:`.
pub fn start_media_cmd(app_dir: &str, temp_dir: &str) -> String {
    let args_file = format!("{app_dir}\\webview2-args.txt");
    let marker = format!("{MEDIA_DIR}\\{MEDIA_MARKER}");
    let logs = format!("{MEDIA_DIR}\\{MEDIA_LOGS_DIR}");
    lines(&[
        "@echo off",
        "title Own Backup",
        "rem Media mode: state goes to the RAM disk instead of the rescued machine's",
        "rem C:, the engine runs as a process, and no VSS is attempted (the system",
        "rem here is offline, so a direct read is already consistent). Set first:",
        "rem every program below, the screen-mode picker included, must know it.",
        "set \"OWN_BACKUP_MEDIA=1\"",
        "",
        "rem The drive the medium booted from, found by its marker - WinPE gives it",
        "rem whatever letter it likes. X: is the RAM disk and is never it.",
        "set \"MEDIA=\"",
        &format!(
            "for %%D in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do \
             if exist \"%%D:\\{marker}\" if not defined MEDIA set \"MEDIA=%%D:\""
        ),
        "",
        "rem A letterless volume answers no letter. The medium laid out on a FAT32",
        "rem EFI partition is exactly that - EFI partitions are not given one - and",
        "rem then the loop above finds nothing, the log mirror never starts, and the",
        "rem logs plus the safety copy of the boot entries stay on a RAM disk that",
        "rem does not survive the restart (seen 2026-09-20). For a rescue program",
        "rem that reads: fix the disk, then tell us what went wrong - with nothing",
        "rem to tell it from. So: ask the volumes themselves, and lend one a letter.",
        "rem Runs only when the letters gave nothing, so the worst case is today's.",
        &format!(
            "if not defined MEDIA for /f \"delims=\" %%V in \
             ('mountvol ^| findstr /i /c:\"\\\\?\\Volume\"') do if not defined MEDIA \
             if not exist Z:\\ (mountvol Z: \"%%V\" >nul 2>&1 & \
             if exist \"Z:\\{marker}\" (set \"MEDIA=Z:\") else (mountvol Z: /D >nul 2>&1))"
        ),
        "if defined MEDIA echo Boot medium: %MEDIA%",
        "",
        "rem Every log is copied there, line by line, if the medium is writable: a",
        "rem session that ends in a dark screen or a reboot leaves its logs on a RAM",
        "rem disk that is gone. A DVD or a mounted ISO is read-only and simply skipped.",
        "set \"OWN_BACKUP_LOG_MIRROR=\"",
        &format!("if defined MEDIA md \"%MEDIA%\\{logs}\" 2>nul"),
        &format!("if defined MEDIA copy /y nul \"%MEDIA%\\{logs}\\write.test\" >nul 2>&1"),
        &format!(
            "if defined MEDIA if exist \"%MEDIA%\\{logs}\\write.test\" \
             set \"OWN_BACKUP_LOG_MIRROR=%MEDIA%\\{logs}\""
        ),
        "if defined OWN_BACKUP_LOG_MIRROR del \"%OWN_BACKUP_LOG_MIRROR%\\write.test\" >nul 2>&1",
        "if defined OWN_BACKUP_LOG_MIRROR echo Logs are copied to: %OWN_BACKUP_LOG_MIRROR%",
        "if not defined OWN_BACKUP_LOG_MIRROR echo Logs stay on the RAM disk only - no writable boot medium found.",
        "rem The monitor's own mode. The picker waits for a display driver PnP may",
        "rem still be installing - on 2026-09-17 it decided one second in, while the",
        "rem screen was still the basic driver's.",
        "rem",
        "rem --no-confirm because this script has no console any more: the desktop",
        "rem runs it without a window, so nobody can press the key that keeps the",
        "rem new mode, and a question nobody can answer means the mode ALWAYS",
        "rem reverts. That is worse than the case the question guards against - a",
        "rem mode the driver listed AND accepted, that then fails to display.",
        "rem --max-scale: switching to the monitor's recommended resolution, the",
        "rem driver scales everything to 200% to go with it. On a desktop that is",
        "rem turned down in the settings; WinPE has no settings to turn it down in.",
        &format!(
            "\"{app_dir}\\own-backup-cli.exe\" display --min 1280x720 --no-confirm --max-scale 150"
        ),
        "rem Which driver ended up drawing, and what PnP said about it. AFTER the",
        "rem pick on purpose: by then the stack has settled, and that is the state",
        "rem the question is about - setupapi.dev.log keeps the earlier lines anyway.",
        &format!(
            "if defined OWN_BACKUP_LOG_MIRROR \"{app_dir}\\own-backup-cli.exe\" display --diag \
             >\"%OWN_BACKUP_LOG_MIRROR%\\display-diag.txt\" 2>&1"
        ),
        "rem What the firmware says about booting. Read-only, and written to the",
        "rem medium rather than the RAM disk: this is the machine someone is here",
        "rem to repair, and the list of its boot entries is the first thing anyone",
        "rem will ask for. It also answers whether reading them works in WinPE at",
        "rem all, which is otherwise only knowable by booting the medium.",
        &format!(
            "if defined OWN_BACKUP_LOG_MIRROR \"{app_dir}\\own-backup-cli.exe\" boot list \
             >\"%OWN_BACKUP_LOG_MIRROR%\\boot-entries.txt\" 2>&1"
        ),
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
        "rem The runtime is either in the image or on the medium's own partition (it",
        "rem is 250 MB that WinPE would otherwise hold in RAM). The marker is on",
        "rem every medium now, so the runtime itself is what is looked for.",
        &format!("set \"WV={app_dir}\\WebView2\""),
        &format!(
            "if defined MEDIA if exist \"%MEDIA%\\{MEDIA_DIR}\\WebView2\\msedgewebview2.exe\" \
             set \"WV=%MEDIA%\\{MEDIA_DIR}\\WebView2\""
        ),
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
        "rem WinPE's firewall resets loopback connections BETWEEN processes. A",
        "rem process reaching its own listener is fine, which is why the engine and",
        "rem the interface talk happily - but the kernel half of a mount (qcow2",
        "rem mounting, and exclusions inside the image) connects from outside our",
        "rem process, and that one got CONNECTION_REFUSED (2026-08-07). The iSCSI",
        "rem initiator that replaced it is in the kernel too, so it is the same case.",
        "rem The service turns the firewall off itself too: this line only helps",
        "rem the case where it never starts. Nothing listens on external",
        "rem interfaces here - it is a rescue environment on a RAM disk.",
        "wpeutil disablefirewall >nul 2>&1",
        "",
        "set \"RUST_BACKTRACE=1\"",
        "set \"RUST_LOG=info,own_backup_gui=debug\"",
        "rem The engine, unless the caller asked for the interface alone.",
        "rem",
        "rem A SECOND engine is not a harmless extra process: every one of them",
        "rem mints a new authentication token and writes it over the old file,",
        "rem and the interface that is already running holds the previous one.",
        "rem From then on every screen it opens answers \"Invalid or missing",
        "rem token\" while the engine sits there, running and reachable. That is",
        "rem what the Start menu's own entry did, by calling this script whole.",
        "if /i \"%~1\"==\"gui\" goto :interface",
        "echo Starting the engine...",
        &format!(
            "start \"own-backup engine\" /min \"{app_dir}\\own-backup-service.exe\" --run --media"
        ),
        ":interface",
        "rem The environment check runs BEFORE the interface, and its verdict is",
        "rem obeyed. Not decoration: without this line a medium whose WebView2 is",
        "rem missing shows Tauri's own dialog - in English, offering to DOWNLOAD",
        "rem the runtime from the internet, on a machine booted into WinPE with no",
        "rem browser and usually no network - behind an empty window of ours (seen",
        "rem 2026-09-20). The interface itself deliberately does not refuse on a",
        "rem failed check, because on an ordinary Windows the loader can still find",
        "rem a runtime through the registry that this check does not read; it",
        "rem refuses HERE instead, where the layout is known. That was the design",
        "rem all along - the call was simply never written.",
        &format!("\"{app_dir}\\own-backup-cli.exe\" media-preflight"),
        "if errorlevel 1 goto :nointerface",
        "echo Starting the interface...",
        &format!("start \"\" \"{app_dir}\\own-backup-gui.exe\""),
        "rem A passed check is not a window. The engine file can be there and",
        "rem broken, the loader can die on its own - and the old failure mode was",
        "rem a live process with no window holding the only console. This waits for",
        "rem the window to say it exists, and says so plainly if it never does.",
        &format!("\"{app_dir}\\own-backup-cli.exe\" media-wait --timeout 40"),
        "if errorlevel 1 goto :nowindow",
        "goto :aftergui",
        ":nowindow",
        "rem The checks passed and no window came: the failure window must say",
        "rem that, not point at checks that are all green.",
        &format!("start \"Own Backup\" cmd /k \"\"{app_dir}\\no-interface.cmd\" nowindow\""),
        "goto :aftergui",
        ":nointerface",
        "rem In a window of its OWN, not in this console: the desktop starts this",
        "rem script through cmd with CREATE_NO_WINDOW, so everything printed here",
        "rem goes nowhere. Without this line the user saw an empty desktop and no",
        "rem explanation at all (2026-09-20).",
        &format!("start \"Own Backup\" cmd /k \"\"{app_dir}\\no-interface.cmd\"\""),
        ":aftergui",
        "echo.",
        "echo The engine runs without a window of its own - its log is the place to look.",
        "echo If no window appears, look at:",
        &format!("echo   \"{app_dir}\\Logs\\own-backup-gui-*.log\""),
        &format!("echo   \"{app_dir}\\Logs\\own-backup-service-*.log\""),
        &format!("echo   {temp_dir}\\webview2.log"),
        "",
        "rem Everything below is boot-time setup and is done once. Asked for the",
        "rem interface alone, there is nothing left to do - and taking the MiniNT",
        "rem key away again under a running system is not a no-op.",
        "if /i \"%~1\"==\"gui\" goto :eof",
        "",
        "rem The iSCSI initiator, LAST on purpose. It is how a backup copy becomes",
        "rem a real disk here without a driver of ours, and it is needed only when",
        "rem someone asks to mount one - minutes away at the earliest. Ahead of the",
        "rem engine it could stop the product from starting at all.",
        "rem",
        "rem The service will not start in PE as it stands: it wants the Event Log,",
        "rem which PE keeps off through the MiniNT key. The key is taken away for",
        "rem the few seconds the service needs and put straight back - MiniNT is",
        "rem what tells Windows it is running from a RAM disk, and leaving it off",
        "rem changes how the environment treats disks, letters and profiles.",
        "rem `sc` does not exist in WinPE; `net` does.",
        "net stop eventlog >nul 2>&1",
        "reg delete \"HKLM\\SYSTEM\\CurrentControlSet\\Control\\MiniNT\" /f >nul 2>&1",
        "net start eventlog >nul 2>&1",
        "net start msiscsi",
        "reg add \"HKLM\\SYSTEM\\CurrentControlSet\\Control\\MiniNT\" /f >nul 2>&1",
        "echo If the line above says the service started or was already running,",
        "echo qcow2 copies can be mounted here; the engine starts it on demand too.",
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
    KEEP_LOCALES
        .iter()
        .any(|k| k.eq_ignore_ascii_case(file_name))
}

/// Files and folders to remove from a copied WebView2 runtime, given its root.
/// Returned rather than deleted here so the caller can log and count.
pub fn runtime_trim_list(runtime_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = RUNTIME_DROP_DIRS
        .iter()
        .map(|d| runtime_root.join(d))
        .collect();
    out.extend(RUNTIME_DROP_FILES.iter().map(|f| runtime_root.join(f)));
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

    /// What made the first medium unusable was not that it started something —
    /// it was that it started something in the foreground and never came back.
    /// The interface may be launched at boot; the console must survive it.
    #[test]
    fn boot_hands_the_console_back() {
        let app = r"X:\Program Files\OwnBackup";
        let s = startnet_cmd(app, true);
        assert!(s.contains("wpeinit"));
        assert!(
            s.contains(&format!("call \"{app}\\start-media.cmd\"")),
            "the interface should start at boot"
        );
        for line in s.lines() {
            let l = line.trim_start();
            if l.starts_with("echo") || l.starts_with("call") || l.starts_with("wpeinit") {
                continue;
            }
            // `rem` runs nothing, and `start "..."` is the detached form — the
            // console comes straight back from it, which is the property this
            // test is about.
            if l.starts_with("rem") || l.starts_with("start \"") {
                continue;
            }
            assert!(
                !l.contains(".exe"),
                "nothing may run in the foreground of the boot console: {line:?}"
            );
        }
    }

    /// DISM's errors are the text that ends up in the report, and they arrive
    /// in the console's code page, not UTF-8.
    #[test]
    fn console_output_is_decoded_out_of_its_code_page() {
        assert_eq!(
            console_text(b"Error: 2 Cannot find the file"),
            "Error: 2 Cannot find the file"
        );
        // CP866 "Ошибка: 2" — invalid UTF-8, so the fallback decodes it.
        let oem = [0x8E, 0xE8, 0xA8, 0xA1, 0xAA, 0xA0, 0x3A, 0x20, 0x32];
        assert_eq!(console_text(&oem), "Ошибка: 2");
        // UTF-8 wins when the bytes are valid UTF-8 — our own messages are.
        assert_eq!(console_text("не найден".as_bytes()), "не найден");
    }

    /// The two long steps report through this and nothing else — a parser that
    /// quietly returns None turns the bar back into the frozen one it replaced.
    #[test]
    fn dism_progress_is_read_out_of_its_redrawn_line() {
        // A chunk off the pipe: several redraws, the last one is the answer.
        let chunk = "[=====                      ]  20.0%\r[==========                 ]  40.0%\r";
        assert_eq!(dism_percent(chunk), Some(40));
        assert_eq!(
            dism_percent("[==========================] 100.0%"),
            Some(100)
        );
        assert_eq!(
            dism_percent("\r[=                          ]   1.5%"),
            Some(1)
        );
        // Nothing to read: the bar keeps whatever it had.
        assert_eq!(
            dism_percent("Deployment Image Servicing and Management tool"),
            None
        );
        assert_eq!(dism_percent("Version: 10.0.19041.1"), None);
        assert_eq!(dism_percent("100%"), Some(100));
        // A stray sign with no number in front of it is not a percentage.
        assert_eq!(dism_percent("% done"), None);
    }

    /// A driver in the store that nobody starts is a screen stuck at 1152x864:
    /// the boot script must drvload what the build put in.
    #[test]
    fn boot_starts_the_added_drivers() {
        let s = startnet_cmd(r"X:\Program Files\OwnBackup", true);
        assert!(
            s.contains("drvload"),
            "added drivers must be loaded, not just stored"
        );
        assert!(
            s.contains(r"%SystemRoot%\INF\oem*.inf"),
            "every added package is offered"
        );
        // Order matters: the driver first, the mode picker (start-media) after.
        let drv = s.find("drvload").unwrap();
        let start = s.find("start-media.cmd").unwrap();
        assert!(
            drv < start,
            "the display driver must be up before the mode is picked"
        );
    }

    /// A passphrase that cannot be typed is a medium that cannot restore
    /// anything: the boot script must offer more than the image's own layout.
    #[test]
    fn boot_sets_up_the_keyboard_layouts() {
        let s = startnet_cmd(r"X:\Program Files\OwnBackup", true);
        assert!(s.contains("SetKeyboardLayout"));
        assert!(
            s.contains(INPUT_LOCALES),
            "both layouts must be set at boot"
        );
        assert!(
            INPUT_LOCALES.contains("0419"),
            "Russian has to be one of them"
        );
    }

    /// `cmd` reads a batch file in the console's OEM code page, and that page
    /// comes from the base image's language: an en-US WinRE printed a UTF-8
    /// dash as `ΓÇö`. Only ASCII reads the same on every base.
    #[test]
    fn the_boot_scripts_are_plain_ascii() {
        let app = r"X:\Program Files\OwnBackup";
        for (name, text) in [
            ("startnet.cmd", startnet_cmd(app, true)),
            (
                "start-media.cmd",
                start_media_cmd(app, r"X:\Windows\Temp\OwnBackup"),
            ),
            (
                "webview2-args.txt",
                webview2_args_file(r"X:\Windows\Temp\OwnBackup\webview2.log"),
            ),
            ("no-interface.cmd", no_interface_cmd(app)),
        ] {
            if let Some(line) = text.lines().find(|l| !l.is_ascii()) {
                panic!("{name} has a non-ASCII line: {line:?}");
            }
        }
    }

    /// The console has to survive a window that never appears — that is how the
    /// first medium trapped its user.
    #[test]
    fn the_interface_is_launched_detached() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        assert!(s.contains(r#"start "" "X:\Program Files\OwnBackup\own-backup-gui.exe""#));
        assert!(
            s.contains("--run --media"),
            "the engine must know it is on the medium"
        );
        assert!(s.contains("OWN_BACKUP_MEDIA=1"));
    }

    /// The strip is cosmetic while `cmd` is still the shell, so it goes last:
    /// by then everything that matters has already run, and a desktop that
    /// fails to appear costs nothing but the desktop.
    #[test]
    fn the_desktop_starts_detached_and_after_the_work() {
        let app = r"X:\Program Files\OwnBackup";
        let s = startnet_cmd(app, true);
        let work = s
            .find("start-media.cmd")
            .expect("the interface still starts");
        let desktop = s
            .find("own-backup-shell.exe")
            .expect("the desktop starts too");
        assert!(
            desktop > work,
            "the desktop comes after the work, not before it"
        );
        assert!(
            s.contains(&format!("start \"\" \"{app}\\own-backup-shell.exe\"")),
            "detached, like everything else this medium launches"
        );
    }

    /// The boot script runs with no console now, so it must not ask anything.
    ///
    /// The screen-mode picker keeps a new mode only if a key is pressed and
    /// reverts otherwise. With no window to press a key in, that is not a
    /// safety net but a guarantee the mode goes back — the medium would boot
    /// at the firmware's resolution every time, which is the whole problem the
    /// picker exists to solve.
    /// Носитель, разложенный на разделе EFI, буквы не получает — и тогда
    /// журнал остаётся на ОЗУ-диске, который не переживёт перезагрузку.
    ///
    /// Запасной путь спрашивает сами тома и одалживает найденному букву. Он
    /// выполняется ТОЛЬКО когда перебор букв ничего не дал, поэтому худший
    /// случай равен сегодняшнему поведению — это и проверяется здесь: порядок,
    /// охрана `if not defined MEDIA` и возврат буквы при промахе.
    #[test]
    fn a_letterless_medium_is_still_found_by_asking_the_volumes() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Temp");
        let by_letter = s
            .lines()
            .position(|l| l.contains("for %%D in (C D E"))
            .expect("перебор букв");
        let idx = s
            .lines()
            .position(|l| l.contains("mountvol"))
            .expect("запасной путь");
        assert!(idx > by_letter, "сначала буквы, и только потом тома");

        let line = s.lines().nth(idx).unwrap();
        assert!(
            line.starts_with("if not defined MEDIA"),
            "только когда буквы не дали: {line}"
        );
        assert!(
            line.contains(r"findstr /i /c:"),
            "поиск по строке, а не по образцу: {line}"
        );
        assert!(line.contains(r"\\?\Volume"), "ищем пути томов: {line}");
        assert!(
            line.contains("if not exist Z:\\"),
            "занятую букву не трогаем: {line}"
        );
        assert!(
            line.contains("mountvol Z: /D"),
            "промах — букву вернуть: {line}"
        );
        assert!(
            line.contains(&format!("Z:\\{MEDIA_DIR}\\{MEDIA_MARKER}")),
            "узнаём носитель по его метке: {line}"
        );
    }

    #[test]
    fn nothing_in_the_boot_script_waits_for_an_answer() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Temp");
        let pick = s
            .lines()
            .find(|l| l.contains("display --min"))
            .expect("the mode picker");
        assert!(
            pick.contains("--no-confirm"),
            "nobody can answer it: {pick}"
        );
    }

    /// The screen's own driver scales to 200 % on a large monitor, which is
    /// right for a desktop read from a sofa and wrong for a rescue environment
    /// — and WinPE has no settings page to turn it down in.
    #[test]
    fn the_boot_script_caps_how_large_everything_is_drawn() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Temp");
        let pick = s
            .lines()
            .find(|l| l.contains("display --min"))
            .expect("the mode picker");
        assert!(pick.contains("--max-scale 150"), "{pick}");
    }

    /// A second engine is not a spare engine.
    ///
    /// Every one of them mints a new authentication token and writes it over
    /// the old file; the interface already running holds the previous one, and
    /// from then on every screen it opens answers "Invalid or missing token"
    /// while the engine sits there, running and reachable. Seen on the medium
    /// (2026-09-19) after the Start menu's own entry called this script whole.
    #[test]
    fn asking_for_the_interface_alone_starts_no_second_engine() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Temp");
        let guard = s.find("goto :interface").expect("the gui-only path");
        let engine = s
            .find("own-backup-service.exe")
            .expect("the engine still starts");
        let label = s.find("\r\n:interface").expect("and is jumped over");
        assert!(
            guard < engine && engine < label,
            "the jump has to clear the engine"
        );

        // And the boot-time setup below it is not repeated either: taking the
        // MiniNT key away again under a running system is not a no-op.
        let iscsi = s.find("net start msiscsi").expect("the initiator");
        let stop = s.rfind("goto :eof").expect("the second guard");
        assert!(stop < iscsi, "the gui-only path must stop before it");
    }

    /// The rule that keeps a medium from rebooting seconds after it booted.
    ///
    /// With `winpeshl.ini` present, winpeshl starts the desktop and waits for
    /// it; the session ends when it exits. If `startnet.cmd` had ALSO started
    /// one, the second copy would find the first one's mutex, exit immediately,
    /// and winpeshl would take that for the end of the session.
    #[test]
    fn exactly_one_of_the_two_boot_files_starts_the_desktop() {
        let app = r"X:\Program Files\OwnBackup";
        let desktop = "own-backup-shell.exe";

        // The medium where our desktop IS the shell.
        let shell_medium = format!("{}{}", startnet_cmd(app, false), winpeshl_ini(app));
        assert_eq!(
            shell_medium.matches(desktop).count(),
            1,
            "once, from winpeshl"
        );
        assert!(
            !startnet_cmd(app, false).contains(desktop),
            "and not from the script"
        );

        // The medium where `cmd` is still the shell: no winpeshl.ini is written
        // at all, and the script is the only one starting it.
        assert_eq!(startnet_cmd(app, true).matches(desktop).count(), 1);
    }

    /// The desktop has to be the FIRST thing on the screen, and the only way
    /// to get that from winpeshl is to be the only entry it has.
    ///
    /// The obvious spelling — boot script first, desktop second — is what the
    /// first medium did, and it showed a console window, then the interface
    /// inside it, and the desktop last of all. Entries are run in order and
    /// each is waited for, so there is no arrangement of two entries that puts
    /// the desktop first.
    #[test]
    fn winpeshl_starts_the_desktop_and_nothing_else() {
        let ini = winpeshl_ini(r"X:\Program Files\OwnBackup");
        assert!(
            ini.starts_with("[LaunchApps]"),
            "winpeshl reads nothing else"
        );
        let apps: Vec<&str> = ini
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('['))
            .collect();
        assert_eq!(
            apps.len(),
            1,
            "one entry, or the desktop cannot be first: {apps:?}"
        );
        assert!(
            !ini.contains("startnet.cmd"),
            "the desktop runs the script itself"
        );
        assert_eq!(
            apps[0], "\"X:\\Program Files\\OwnBackup\\own-backup-shell.exe\"",
            "a path with a space in it has to be quoted"
        );
    }

    /// The script the desktop runs is named the way `CreateProcess` can be
    /// given it — a `.cmd` cannot be started directly, which is how a medium
    /// once booted into a bare console.
    #[test]
    fn the_boot_script_is_where_winpe_keeps_it() {
        assert!(BOOT_SCRIPT.ends_with("startnet.cmd"));
        assert!(
            BOOT_SCRIPT.starts_with(r"X:\"),
            "the RAM disk, always X: in WinPE"
        );
    }

    /// A program the boot scripts launch but the build never copies in gives a
    /// medium that boots and then cannot find its own parts — and the only way
    /// to discover it is to boot one.
    #[test]
    fn every_program_the_boot_scripts_run_is_one_that_is_copied_in() {
        let app = r"X:\Program Files\OwnBackup";
        let scripts = format!(
            "{}{}",
            startnet_cmd(app, true),
            start_media_cmd(app, r"X:\Temp")
        );
        let mut rest = scripts.as_str();
        while let Some(at) = rest.find("own-backup-") {
            let tail = &rest[at..];
            let end = tail
                .find(|c: char| c == '"' || c.is_whitespace())
                .unwrap_or(tail.len());
            let (name, after) = tail.split_at(end);
            // Log files and window titles share the prefix; only programs count.
            if name.ends_with(".exe") {
                assert!(
                    PROGRAM_FILES.contains(&name),
                    "{name} is launched by the boot scripts but never copied into the image"
                );
            }
            rest = after;
        }
    }

    /// WinPE's firewall resets loopback connections between processes, which is
    /// exactly how the iSCSI road works: the initiator lives in the KERNEL, so
    /// its connection to our target is cross-process by definition.
    /// Without this the qcow2 mount and the in-image exclusion pass die with
    /// CONNECTION_REFUSED — and they die AFTER the copy, so the run looks
    /// successful and quietly keeps every excluded file.
    #[test]
    fn the_medium_turns_the_firewall_off_before_starting_anything() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        let firewall = s
            .find("wpeutil disablefirewall")
            .expect("firewall must be disabled");
        let engine = s
            .find("own-backup-service.exe")
            .expect("the engine must be started");
        assert!(
            firewall < engine,
            "the firewall must be down before the engine comes up"
        );
    }

    /// Проверка окружения обязана стоять ПЕРЕД запуском интерфейса, и её отказ
    /// обязан что-то значить.
    ///
    /// Обе половины были написаны и покрыты тестами, а вызов не написан —
    /// и носитель без WebView2 показывал англоязычное окно Tauri с
    /// предложением скачать среду выполнения из интернета (2026-09-20, WinPE,
    /// где нет ни браузера, ни сети), за которым стояло пустое окно нашего
    /// интерфейса. Сам интерфейс при проваленной проверке не отказывается
    /// стартовать намеренно: на обычной Windows загрузчик находит среду через
    /// реестр, куда эта проверка не смотрит. Отказывать должен запускатель.
    #[test]
    fn the_environment_is_checked_before_the_interface_and_the_verdict_is_obeyed() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        let check = s
            .find("media-preflight")
            .expect("проверка окружения должна вызываться");
        let gui = s
            .find("own-backup-gui.exe")
            .expect("интерфейс должен запускаться");
        assert!(
            check < gui,
            "проверка идёт до запуска, иначе она ничего не решает"
        );
        assert!(
            s.contains("if errorlevel 1 goto :nointerface"),
            "отказ проверки должен уводить в обход запуска"
        );
        assert!(
            s.contains(":nointerface"),
            "и эта метка должна существовать"
        );
        // Пройденная проверка — ещё не окно: файл движка бывает на месте и
        // битый. Тогда носитель ждёт окна и говорит, если его нет.
        let wait = s
            .find("media-wait")
            .expect("после запуска надо дождаться окна");
        assert!(gui < wait, "ждать окна имеет смысл только после запуска");
        // И тот и другой исход должны быть ВИДНЫ. Консоль запускателя не
        // видна: рабочий стол зовёт его через cmd с CREATE_NO_WINDOW.
        assert_eq!(
            s.matches("start \"Own Backup\" cmd /k").count(),
            2,
            "своё окно нужно обоим исходам: и отказу проверки, и «окна нет»"
        );
        // И говорить они должны разное: при «окна нет» проверки зелёные, и
        // обещание «причина напечатана ниже» было бы неправдой.
        assert!(
            s.contains("no-interface.cmd\" nowindow"),
            "второй исход зовётся по имени"
        );
    }

    /// Окно неудачи повторяет разбор и оставляет человеку рабочий путь.
    #[test]
    fn the_failure_window_says_what_happened_and_what_still_works() {
        let s = no_interface_cmd(r"X:\Program Files\OwnBackup");
        assert!(
            s.contains("media-preflight --report-only"),
            "разбор повторяется в этом окне"
        );
        assert!(
            s.contains("if /i \"%~1\"==\"nowindow\" goto :nowindow"),
            "окно должно различать «не пустила проверка» и «окна так и не появилось»"
        );
        assert!(
            s.contains("own-backup-cli.exe\" --help"),
            "и сказано, чем работать дальше"
        );
        assert!(
            s.contains("Logs\\own-backup-gui-*.log"),
            "и где искать подробности"
        );
        // Русский приходит из самой программы, и без UTF-8 консоль покажет
        // кашу. Строки скрипта остаются ASCII — их проверяет соседний тест.
        assert!(s.contains("chcp 65001"), "иначе разбор будет нечитаемым");
    }

    /// The runtime is off-image, so the launcher must locate the boot volume by
    /// its marker instead of assuming a letter.
    #[test]
    fn the_runtime_is_found_by_marker_with_a_fallback() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        assert!(s.contains(&format!(r"{MEDIA_DIR}\{MEDIA_MARKER}")));
        assert!(
            s.contains(r#"set "WV=X:\Program Files\OwnBackup\WebView2""#),
            "the in-image runtime is the fallback"
        );
        // The marker is on every medium now, so it alone must not redirect the
        // runtime to a folder that is not there.
        assert!(s.contains(r"WebView2\msedgewebview2.exe"));
        assert!(s.contains("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=%WV%"));
    }

    /// The initiator has to come up AFTER the product, and the MiniNT key has
    /// to go back: it is what tells Windows it is running from a RAM disk.
    #[test]
    fn the_initiator_starts_last_and_gives_the_key_back() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        let engine = s.find("own-backup-service.exe").unwrap();
        let del = s.find("reg delete").expect("MiniNT taken away");
        let start = s
            .find("net start msiscsi")
            .expect("the initiator is started");
        let add = s.find("reg add").expect("MiniNT put back");
        assert!(
            engine < del,
            "the product must start before the initiator is touched"
        );
        assert!(del < start && start < add, "key off -> start -> key back");
        // `sc` is not in WinPE: reaching for it once took the whole script down.
        assert!(!s.contains("sc config") && !s.contains("sc start"));
    }

    /// A dark screen ended the session of 2026-09-17 with every log on the RAM
    /// disk. The copy on the stick has to be switched on before the first
    /// program runs — the screen-mode picker is the one that did it — and media
    /// mode has to be known before any of them reads its settings.
    #[test]
    fn logs_go_to_the_boot_medium_before_anything_runs() {
        let s = start_media_cmd(r"X:\Program Files\OwnBackup", r"X:\Windows\Temp\OwnBackup");
        let media = s.find("OWN_BACKUP_MEDIA=1").unwrap();
        let mirror = s
            .find(r#"set "OWN_BACKUP_LOG_MIRROR=%MEDIA%\OwnBackup\logs""#)
            .expect("mirror set");
        let display = s.find("display --min").unwrap();
        let engine = s.find("own-backup-service.exe").unwrap();
        assert!(media < display && mirror < display && display < engine);
        // Only on a medium that takes a write: a DVD must not end the script.
        assert!(s.contains("write.test"));
    }

    #[test]
    fn every_generated_file_uses_windows_line_endings() {
        for s in [
            startnet_cmd("X:\\app", true),
            startnet_cmd("X:\\app", false),
            winpeshl_ini("X:\\app"),
            start_media_cmd("X:\\app", "X:\\tmp"),
            webview2_args_file("X:\\tmp\\webview2.log"),
        ] {
            assert!(s.contains("\r\n"));
            assert!(
                !s.replace("\r\n", "").contains('\n'),
                "a bare LF slipped in"
            );
        }
    }

    /// Asking for a Chromium log at all opens a console window over the
    /// interface in this Edge build — even directed at a file.
    #[test]
    fn the_default_arguments_ask_for_no_chromium_log() {
        let a = browser_args();
        assert!(
            !a.contains("--enable-logging"),
            "this opens a console window"
        );
        assert!(!a.contains("--log-file"));
        // Still available one line away, for when something needs diagnosing.
        assert!(browser_args_logging("X:\\log").contains("--enable-logging=file"));
    }

    /// Both of these were found the hard way; a regression would show up as a
    /// black window or a refusal to start, neither of which points at the cause.
    #[test]
    fn the_arguments_carry_what_winpe_needs() {
        let a = browser_args();
        assert!(
            a.contains("--allow-run-as-system"),
            "WinPE runs everything as SYSTEM"
        );
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
        assert!(
            f.lines().count() > 5,
            "the alternatives should be there too"
        );
    }

    /// The DLL that made the window black must never be shipped, by any route.
    #[test]
    fn the_composition_dll_is_not_in_the_copy_list() {
        assert!(
            !SYSTEM_DLLS
                .iter()
                .any(|d| d.eq_ignore_ascii_case(FORBIDDEN_DLL)),
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

    /// Everything the runtime cannot start without must survive the trim. The
    /// list was chosen by reading the import tables; a regression here shows up
    /// as a medium that boots to no window at all.
    #[test]
    fn the_trim_keeps_what_the_runtime_is_linked_against() {
        for keep in [
            "msedge.dll",         // the engine
            "msedge_elf.dll",     // static import of the engine
            "msedgewebview2.exe", // the host we actually launch
            "ffmpeg.dll",         // delay-loaded, but loaded
            "libEGL.dll",         // ANGLE, for software rendering
            "libGLESv2.dll",
            "d3dcompiler_47.dll",
            "vk_swiftshader.dll", // Chromium's renderer with --disable-gpu
            "vulkan-1.dll",
            "icudtl.dat",
            "resources.pak",
        ] {
            assert!(
                !RUNTIME_DROP_FILES
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(keep)),
                "{keep} must not be trimmed"
            );
        }
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
        let names: Vec<String> = list
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"ResiliencyLinks".to_string()), "{names:?}");
        assert!(names.contains(&"af.pak".to_string()));
        assert!(names.contains(&"de.pak".to_string()));
        assert!(!names.contains(&"ru.pak".to_string()));
        assert!(!names.contains(&"en-US.pak".to_string()));
    }

    /// Both halves of the rule matter, and the cases below are the ones a real
    /// machine actually produced (2026-08-01).
    #[test]
    fn storage_and_network_on_a_real_bus_are_taken() {
        // Samsung NVMe, Intel AHCI — the two that make disks visible at all.
        assert_eq!(
            rescue_role("{4d36e97b-e325-11ce-bfc1-08002be10318}", "PCI"),
            Some("контроллер дисков")
        );
        assert_eq!(
            rescue_role("{4d36e96a-e325-11ce-bfc1-08002be10318}", "PCI"),
            Some("контроллер дисков")
        );
        // Intel Ethernet on PCI, and a USB network dongle.
        assert_eq!(
            rescue_role("{4d36e972-e325-11ce-bfc1-08002be10318}", "PCI"),
            Some("сетевой адаптер")
        );
        assert_eq!(
            rescue_role("{4d36e972-e325-11ce-bfc1-08002be10318}", "USB"),
            Some("сетевой адаптер")
        );
    }

    /// A VPN adapter is class Net and present, exactly like the card beside it.
    /// The bus is the only thing that tells them apart.
    #[test]
    fn software_adapters_are_left_out() {
        for bus in ["ROOT", "SWD"] {
            assert_eq!(
                rescue_role("{4d36e972-e325-11ce-bfc1-08002be10318}", bus),
                None,
                "a {bus} adapter has no cable — WireGuard, OpenVPN, the Avast filter"
            );
        }
        // Software storage controllers register on ROOT — a rescue medium has
        // no use for them, and our own iSCSI disks arrive that way too.
        assert_eq!(
            rescue_role("{4d36e97b-e325-11ce-bfc1-08002be10318}", "ROOT"),
            None
        );
    }

    /// Class USB holds the host controller AND everything plugged into it. The
    /// controller is a PCI device; a printer is not.
    #[test]
    fn usb_takes_the_controller_and_not_the_printer() {
        assert_eq!(
            rescue_role("{36fc9e60-c465-11cf-8056-444553540000}", "PCI"),
            Some("контроллер USB")
        );
        assert_eq!(
            rescue_role("{36fc9e60-c465-11cf-8056-444553540000}", "USB"),
            None,
            "the Canon and Epson packages arrive here"
        );
    }

    /// System is where the chipset lives — and the Logitech receiver, which is
    /// how a mouse driver reached a rescue medium.
    #[test]
    fn the_system_class_is_not_a_rescue_class() {
        for bus in ["PCI", "ROOT", "ACPI"] {
            assert_eq!(
                rescue_role("{4d36e97d-e325-11ce-bfc1-08002be10318}", bus),
                None
            );
        }
    }

    /// The class name is localised on some systems; the GUID never is. Case
    /// varies between the registry and PowerShell, so matching ignores it.
    /// The short list handed to the device query must stay in step with the
    /// table the decision is made from, or the query filters out a class the
    /// rule would have accepted and the driver silently never appears.
    #[test]
    fn the_query_guids_are_the_table_guids() {
        for (g, _, _) in RESCUE_CLASSES {
            assert!(
                RESCUE_CLASS_GUIDS.contains(&g),
                "{g} missing from the query list"
            );
        }
        assert_eq!(RESCUE_CLASS_GUIDS.len(), RESCUE_CLASSES.len());
    }

    /// A video adapter is never a rescue device. Its driver is the largest
    /// package in the store, and on the medium it never gave a picture — the
    /// resolution comes from the firmware, with CSM disabled (2026-09-18).
    #[test]
    fn a_video_adapter_is_not_a_rescue_device() {
        let display = "{4d36e968-e325-11ce-bfc1-08002be10318}";
        for bus in ["PCI", "ROOT", "SWD", "USB"] {
            assert_eq!(rescue_role(display, bus), None, "{bus}");
        }
        assert!(!RESCUE_CLASS_GUIDS.contains(&display));
    }

    /// The names from a real Windows 11 machine (2026-09-17): every disk and
    /// network device there ran on an inbox driver, and the wizard said
    /// "nothing found" about hardware that plainly had drivers.
    #[test]
    fn published_names_are_told_from_inbox_ones() {
        for inf in ["oem3.inf", "OEM12.INF", " oem105.inf "] {
            assert!(is_published_inf(inf), "{inf}");
        }
        for inf in [
            "stornvme.inf",
            "net1ic64.inf",
            "netrtwlane.inf",
            "mshdc.inf",
            "oem.inf",
            "oemsetup.inf",
            "oem3.sys",
        ] {
            assert!(!is_published_inf(inf), "{inf}");
        }
    }

    #[test]
    fn class_guids_match_whatever_the_case() {
        assert!(rescue_role("{4D36E97B-E325-11CE-BFC1-08002BE10318}", "pci").is_some());
        assert!(rescue_role("  {4d36e972-e325-11ce-bfc1-08002be10318}  ", " PCI ").is_some());
    }
}
