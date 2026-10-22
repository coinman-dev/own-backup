// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

mod app_state;
// The decision is plain code tested everywhere; only Windows applies it.
#[cfg_attr(not(windows), allow(dead_code))]
mod data_dir_acl;
mod grpc_server;
mod install;
#[cfg(target_os = "linux")]
mod logind_events;
#[cfg(unix)]
mod peer_access;
#[cfg(target_os = "linux")]
mod polkit;
mod scheduled_verification;
mod scheduler;
#[cfg(unix)]
mod sd_notify;
mod settings;
mod shutdown;
mod single_instance;

#[cfg(target_os = "windows")]
mod windows_service;

use std::net::SocketAddr;

use tonic::transport::Server;
use tracing::{debug, info, warn};

use app_state::AppState;
use grpc_server::BackupServiceImpl;
use own_backup_common::config::AppConfig;
use own_backup_common::tr;
use shutdown::{create_shutdown_channel, ShutdownSignal};

/// Service identity — shared by the SCM registration, the systemd unit and the
/// Windows service dispatcher. Names live in `own-backup-common` so the GUI and
/// CLI address the exact same service.
#[allow(dead_code)] // some are per-platform; the others' cfg peers still read them
pub const SERVICE_NAME: &str = own_backup_common::WINDOWS_SERVICE_NAME;
#[allow(dead_code)]
pub const SERVICE_DISPLAY_NAME: &str = own_backup_common::WINDOWS_SERVICE_DISPLAY_NAME;
#[allow(dead_code)]
pub const SERVICE_DESCRIPTION: &str =
    "Block-level backup engine for Own Backup: runs scheduled backups and serves the GUI/CLI.";
/// systemd unit base name on Linux (`own-backup.service`).
#[allow(dead_code)]
pub const LINUX_UNIT_NAME: &str = own_backup_common::LINUX_UNIT_NAME;

/// How the binary was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Install,
    Uninstall,
    /// Control the already-installed service (SCM on Windows / systemd on Linux).
    Start,
    Stop,
    Restart,
    Status,
    /// Put THIS executable in place of the installed one: stop, replace, start.
    ///
    /// A running service holds its own file open, so a new build cannot simply
    /// be copied over it — and the three-step dance that follows is easy to
    /// forget. Forgetting it once already cost a full test cycle: a fix was
    /// deployed, the service kept running the old binary, and the run that was
    /// supposed to prove the fix reproduced the bug instead.
    Update,
    /// Delete what the service keeps on this machine: settings, jobs, history
    /// and remembered volume keys. The uninstaller's «удалить также настройки».
    PurgeData,
    /// Add a person to the access group (Linux): `--grant-access [USER]`, as
    /// root — by `pkexec` from the window, by `sudo`, or by hand.
    GrantAccess,
    /// Console run for development: loopback TCP, no auth, easy to attach a
    /// debugger/curl. `--foreground` / `-f`.
    Foreground,
    /// Production daemon (systemd `ExecStart … --run`): the OS transport — UDS
    /// on Unix, named pipe on Windows — with a per-boot token, no TCP port.
    Daemon,
    /// Launched by the SCM (Windows) with no args, or a bare console run.
    Service,
}

/// Accept both Windows-style (`/install`) and Unix-style (`--install`) flags on
/// every platform, so muscle memory works either way; the actual work is
/// platform-specific.
///
/// Flags are read in two passes, and the order matters (audit AUD-16). The
/// mode pass RETURNS on the first word it recognises, so anything that merely
/// *colours* a mode has to be looked for before it — `--run --media`, which is
/// exactly how the medium's launcher spells it, used to select the daemon and
/// walk away without ever seeing `--media`. On a real WinPE the MiniNT key
/// usually covered that up; anywhere else the explicit flag was simply ignored.
fn parse_mode(args: &[String]) -> Mode {
    if wants_media(args) {
        // Must happen before anything reads the data directory: the first
        // answer `media_mode` gives out is final.
        own_backup_common::media_mode::set_media_mode(true);
    }
    mode_of(args)
}

/// Does the command line ask for media mode? Not a mode of its own — it
/// colours whichever mode is selected, so it is answered from the whole line
/// regardless of where it appears.
fn wants_media(args: &[String]) -> bool {
    args.iter()
        .any(|a| matches!(a.as_str(), "--media" | "/media"))
}

/// Which mode the command line selects: the first word that names one wins.
fn mode_of(args: &[String]) -> Mode {
    for a in args {
        match a.as_str() {
            "/install" | "--install" | "install" => return Mode::Install,
            "/uninstall" | "--uninstall" | "uninstall" | "/remove" | "--remove" => {
                return Mode::Uninstall
            }
            "/start" | "--start" | "start" => return Mode::Start,
            "/stop" | "--stop" | "stop" => return Mode::Stop,
            "/restart" | "--restart" | "restart" => return Mode::Restart,
            "/status" | "--status" | "status" => return Mode::Status,
            "/update" | "--update" | "update" => return Mode::Update,
            "/purge-data" | "--purge-data" | "purge-data" => return Mode::PurgeData,
            "/grant-access" | "--grant-access" | "grant-access" => return Mode::GrantAccess,
            "-f" | "--foreground" => return Mode::Foreground,
            "--run" | "run" | "--daemon" | "daemon" => return Mode::Daemon,
            _ => {}
        }
    }
    Mode::Service
}

/// Today's service log, in the shared `Logs` folder beside the exe (what the
/// user looks for), else the data dir (LocalSystem can always write there),
/// else temp. On Linux `/var/log/own-backup` (`log_files::log_dir`).
fn open_service_log() -> Option<(own_backup_common::log_files::DailyLog, std::path::PathBuf)> {
    let dir = own_backup_common::log_files::log_dir(&AppConfig::default().data_dir)?;
    let log = own_backup_common::log_files::DailyLog::new(dir, "own-backup-service");
    let path = log.path();
    Some((log, path))
}

/// Delete everyone's logs older than thirty days, and keep doing it.
///
/// The service does this for all three executables because it is the one that
/// is always running: the command line may not be used for a month, and the
/// interface only while somebody is looking at it. Sweeping at startup and
/// once a day covers a machine that is rebooted daily and one that is not.
fn start_log_housekeeping() {
    let Some(dir) = own_backup_common::log_files::log_dir(&AppConfig::default().data_dir) else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let removed =
                own_backup_common::log_files::sweep(&dir, own_backup_common::log_files::KEEP_DAYS);
            if removed > 0 {
                info!(
                    removed,
                    dir = %dir.display(),
                    keep_days = own_backup_common::log_files::KEEP_DAYS,
                    "logs older than the retention window removed"
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        }
    });
}

/// The service log file, and — when systemd started the service with its
/// output going to the journal (`$JOURNAL_STREAM`, Linux) — the same lines on
/// stderr, so `journalctl -u own-backup` shows them too. Each line carries
/// the `<N>` syslog prefix of its level, which journald reads as the priority
/// (`SyslogLevelPrefix=`, on by default). Anywhere else it is the file alone.
struct JournalCopy<W> {
    file: W,
    journal: bool,
}

impl<W> JournalCopy<W> {
    fn new(file: W) -> Self {
        Self {
            file,
            journal: cfg!(target_os = "linux") && std::env::var_os("JOURNAL_STREAM").is_some(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for JournalCopy<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // The file is the log that must exist; the journal copy is best-effort.
        let n = self.file.write(buf)?;
        if self.journal {
            let mut stderr = std::io::stderr().lock();
            for line in buf[..n].split_inclusive(|b| *b == b'\n') {
                let _ = stderr.write_all(syslog_prefix(line).as_bytes());
                let _ = stderr.write_all(line);
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `<N>` for a formatted tracing line — `2026-10-19T…Z  WARN target: …` — by
/// its level word: err 3, warning 4, info 6, debug 7.
fn syslog_prefix(line: &[u8]) -> &'static str {
    let level = std::str::from_utf8(line)
        .ok()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    match level {
        "ERROR" => "<3>",
        "WARN" => "<4>",
        "INFO" => "<6>",
        "DEBUG" | "TRACE" => "<7>",
        _ => "<6>",
    }
}

/// Set up logging to a file next to the exe (so a service running under the SCM,
/// which has no console, still produces a diagnosable log) and record startup
/// facts. The level is the settings' «Уровень журнала», followed while running
/// (`own_backup_common::log_level`); `RUST_LOG` overrides it and then stays put.
fn init_logging() {
    use own_backup_common::log_level;
    use own_backup_common::log_mirror::{MirrorLog, TeeLog};

    let config_path = AppConfig::default_path();
    // The boot medium starts with no config at all, and its log is the one
    // that has to explain a restore afterwards.
    let fallback = if own_backup_common::media_mode::is_windows_pe()
        || own_backup_common::media_mode::is_linux_medium()
    {
        "debug"
    } else {
        "info"
    };
    let level = log_level::configured_level(&config_path, fallback);
    let from_env = tracing_subscriber::EnvFilter::try_from_default_env().ok();
    let pinned = from_env.is_some();
    let filter = from_env.unwrap_or_else(|| {
        tracing_subscriber::EnvFilter::new(log_level::filter_directives(&level))
    });
    let (log_path, mirror_path, reload) = match open_service_log() {
        Some((f, path)) => {
            // The second copy, on whatever drive the user named — the only one
            // that survives a restore that takes the machine down with it.
            let tee = TeeLog::new(
                JournalCopy::new(f),
                MirrorLog::new(AppConfig::default_path(), "own-backup-service.log"),
            );
            let mirror = tee.mirror_path();
            let builder = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(tee))
                .with_filter_reloading();
            let handle = builder.reload_handle();
            builder.init();
            (Some(path), mirror, Some(handle))
        }
        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .init();
            (None, None, None)
        }
    };
    if let (Some(handle), false) = (reload, pinned) {
        log_level::follow_config(config_path, fallback, level.clone(), move |directives| {
            if handle
                .reload(tracing_subscriber::EnvFilter::new(directives))
                .is_ok()
            {
                info!(filter = directives, "log level changed in the settings");
            }
        });
    }
    info!(
        service = SERVICE_DISPLAY_NAME,
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        pid = std::process::id(),
        "=== Own Backup service starting ==="
    );
    if let Ok(exe) = std::env::current_exe() {
        info!(
            exe = %exe.display(),
            built = %own_backup_common::exe_build_stamp(),
            "running executable"
        );
    }
    if let Some(p) = &log_path {
        info!(log = %p.display(), "log file");
    }
    match &mirror_path {
        Some(p) => info!(mirror = %p.display(), "log is duplicated here, flushed line by line"),
        None => info!("log is not duplicated (no directory set in параметры)"),
    }
    if pinned {
        info!("log level: RUST_LOG (the settings' level is not followed)");
    } else {
        info!(level = %level, filter = log_level::filter_directives(&level), "log level");
    }
}

/// Hide the console window this process was given.
///
/// On the boot medium the engine is started by a `.cmd` script, so Windows
/// hands it a console — an empty black window that sits on the desktop next to
/// the interface, looking like something the user should read or close. It is
/// neither: the engine logs to `own-backup-service.log`, and nothing is ever
/// printed there. The console itself stays open (stdout keeps working), it just
/// stops being furniture.
#[cfg(target_os = "windows")]
fn hide_own_console() {
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    // SAFETY: both calls take the console window handle and touch nothing else;
    // a null handle (no console — the SCM case) is simply skipped.
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.0.is_null() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Turn WinPE's firewall off (`wpeutil disablefirewall`).
///
/// It filters cross-process loopback connections: user processes talked over
/// 127.0.0.1 fine, while the connection from the kernel half of the mount —
/// qcow2 mounting and in-image exclusions — came back CONNECTION_REFUSED (seen
/// 2026-08-07; the same listener answered a same-process probe within the same
/// millisecond). It applies just as much to the iSCSI road that replaced it:
/// the initiator lives in the kernel, so its connection to our target is
/// cross-process by definition. Done here rather than only
/// in the medium's start script, because the script is baked into boot.wim at
/// media-build time and the usual test loop swaps just the exes — a fix that
/// lives in the script reaches the machine a full media rebuild later.
///
/// Media mode only, and `wpeutil` exists only in WinPE — a rescue environment
/// with nothing listening on external interfaces.
#[cfg(target_os = "windows")]
fn disable_winpe_firewall() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    match std::process::Command::new("wpeutil")
        .arg("disablefirewall")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => info!("WinPE firewall disabled (wpeutil)"),
        Ok(o) => tracing::warn!(
            code = ?o.status.code(),
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "wpeutil disablefirewall failed"
        ),
        Err(e) => tracing::warn!(error = %e, "wpeutil not found — not a WinPE environment?"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    // Messages in the language the window last chose, followed while running.
    own_backup_common::lang::follow_config(AppConfig::default_path());

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse_mode(&args);

    // Media mode only: a dev `--foreground` run wants its console, and under
    // the SCM there is none to hide.
    #[cfg(target_os = "windows")]
    if own_backup_common::media_mode::is_media_mode()
        && matches!(mode, Mode::Daemon | Mode::Service)
    {
        hide_own_console();
        disable_winpe_firewall();
    }

    // The SCM control operations (register/remove/start/stop) require admin on
    // Windows. Self-elevate via UAC rather than failing with access-denied.
    #[cfg(target_os = "windows")]
    if matches!(
        mode,
        Mode::Install
            | Mode::Uninstall
            | Mode::Start
            | Mode::Stop
            | Mode::Restart
            | Mode::Update
            | Mode::PurgeData
    ) && elevate::relaunch_elevated(&args)
    {
        return Ok(()); // an elevated copy is doing the work; this one is done
    }

    match mode {
        Mode::Install => install::install(),
        Mode::Uninstall => install::uninstall(),
        Mode::Start => install::start(),
        Mode::Stop => install::stop(),
        Mode::Restart => install::restart(),
        Mode::Status => install::status(),
        Mode::Update => install::update_from_here(),
        Mode::PurgeData => install::purge_data(),
        Mode::GrantAccess => grant_access(&args),
        // Foreground/dev uses TCP on localhost for easy debugging.
        Mode::Foreground => run_loop(true),
        // Daemon (systemd) uses the OS production transport (UDS/named pipe).
        Mode::Daemon => run_loop(false),
        Mode::Service => run_service_mode(),
    }
}

/// `--grant-access [USER]`: the word after the flag, unless it is another flag.
fn grant_access(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let user = grant_access_user(args);
    #[cfg(not(target_os = "windows"))]
    {
        install::grant_access(user)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = user;
        Err("--grant-access is for Linux; on Windows access follows the console session".into())
    }
}

fn grant_access_user(args: &[String]) -> Option<&str> {
    let at = args.iter().position(|a| {
        matches!(
            a.as_str(),
            "/grant-access" | "--grant-access" | "grant-access"
        )
    })?;
    args.get(at + 1)
        .map(String::as_str)
        .filter(|a| !a.starts_with('-') && !a.starts_with('/'))
}

/// Human-facing description printed when the binary is launched directly in a
/// console (rather than by the SCM). It is a background service, so a bare run
/// should explain itself and list its commands instead of trying to serve.
fn print_info() {
    println!("{} v{}", SERVICE_DISPLAY_NAME, env!("CARGO_PKG_VERSION"));
    println!("{SERVICE_DESCRIPTION}");
    println!();
    println!(
        "{}",
        tr!("main.info_background", name = SERVICE_DISPLAY_NAME)
    );
    println!("{}", tr!("main.info_no_manual_run"));
    println!();
    println!("{}", tr!("main.info_commands"));
    println!("  /install      {}", tr!("main.cmd_install"));
    println!("  /uninstall    {}", tr!("main.cmd_uninstall"));
    println!("  /start        {}", tr!("main.cmd_start"));
    println!("  /stop         {}", tr!("main.cmd_stop"));
    println!("  /restart      {}", tr!("main.cmd_restart"));
    println!("  /status       {}", tr!("main.cmd_status"));
    println!("  /update       {}", tr!("main.cmd_update"));
    println!("  /purge-data   {}", tr!("main.cmd_purge"));
    #[cfg(not(target_os = "windows"))]
    println!("  --grant-access [USER]  {}", tr!("main.cmd_grant_access"));
    println!("  --foreground  {}", tr!("main.cmd_foreground"));
    println!("  --run         {}", tr!("main.cmd_run"));
    println!("  --media       {}", tr!("main.cmd_media"));
}

/// Build a tokio runtime and run the gRPC loop until shutdown. `use_tcp`
/// selects the transport: loopback TCP for dev/foreground, else the OS
/// production transport (UDS on Unix, a DACL-restricted named pipe on Windows).
fn run_loop(use_tcp: bool) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        "Starting {SERVICE_DISPLAY_NAME} ({})",
        if use_tcp {
            "foreground/TCP"
        } else {
            "daemon/UDS"
        }
    );
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (controller, signal) = create_shutdown_channel();
        // Registered here, before anything is spawned: until a handler exists,
        // SIGTERM's default action kills the process outright.
        let stop_requested = stop_signal()?;
        // A second way to be asked to stop: logind announcing a shutdown.
        let (os_stop_tx, mut os_stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop_requested => {}
                Some(()) = os_stop_rx.recv() => {}
            }
            #[cfg(unix)]
            sd_notify::stopping();
            controller.trigger_shutdown();
        });
        // No SCM control handler here. On Linux the installed engine hears
        // logind instead (shutdown, logon, logoff); elsewhere, and for a
        // foreground or boot-medium run, the sender is only kept alive so the
        // scheduler's receiver simply never yields.
        let (os_tx, os_rx) = tokio::sync::mpsc::unbounded_channel::<scheduler::OsEvent>();
        #[cfg(target_os = "linux")]
        let cancelled_shutdown = (!use_tcp && !own_backup_common::media_mode::is_media_mode())
            .then(|| logind_events::spawn(os_tx.clone(), os_stop_tx.clone()));
        let _keep = (os_tx, os_stop_tx);
        run_main_loop(signal, use_tcp, os_rx).await?;
        #[cfg(target_os = "linux")]
        if cancelled_shutdown.is_some_and(|c| c.happened()) {
            // An error exit, so that systemd starts the engine again.
            return Err(tr!("main.shutdown_cancelled").into_owned().into());
        }
        Ok(())
    })
}

/// Resolves when the process is asked to stop: Ctrl+C everywhere, and on Unix
/// also SIGTERM and SIGHUP.
///
/// SIGTERM is how systemd stops the unit — `systemctl stop`, shutdown, a
/// package upgrade. Left to its default action it killed the engine in the
/// middle of writing an image, skipping the worker's final history and the
/// socket/token cleanup. SIGHUP (the terminal of a `--foreground` run went
/// away) is treated the same: there is no configuration to reload.
#[cfg(unix)]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("Ctrl+C received, shutting down..."),
            _ = term.recv() => info!("SIGTERM received, shutting down..."),
            _ = hup.recv() => info!("SIGHUP received, shutting down..."),
        }
    })
}

#[cfg(not(unix))]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl+C received, shutting down...");
    })
}

/// Default entry when no mode flag is given. On Windows the SCM launches us this
/// way; if instead a human ran the exe in a console, the service-control
/// dispatcher reports "not started by the SCM" and we show the info screen. On
/// Unix the systemd unit uses `--run`, so a bare run here is also a human — show
/// info rather than silently daemonizing.
fn run_service_mode() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        if windows_service::run_as_service()? {
            Ok(()) // ran under the SCM and has now stopped
        } else {
            print_info(); // launched from a console — explain and exit
            Ok(())
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        print_info();
        Ok(())
    }
}

/// Windows UAC self-elevation for the SCM control commands.
#[cfg(target_os = "windows")]
mod elevate {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// Is this process already running elevated?
    fn is_elevated() -> bool {
        // SAFETY: standard token-elevation query; the handle is closed after use.
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return true; // can't tell — assume elevated and let the op surface errors
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut ret_len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
            .is_ok();
            let _ = CloseHandle(token);
            if !ok {
                return true;
            }
            elevation.TokenIsElevated != 0
        }
    }

    /// If not elevated, relaunch this exe with the same args via UAC ("runas")
    /// and return `true` (the caller should exit). Returns `false` when already
    /// elevated so the caller proceeds normally.
    pub fn relaunch_elevated(args: &[String]) -> bool {
        if is_elevated() {
            return false;
        }
        let Ok(exe) = std::env::current_exe() else {
            return false; // can't relaunch — let the op run and report access-denied
        };
        let exe_w: Vec<u16> = exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let params = args.join(" ");
        let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
        let params_ptr = if params.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(params_w.as_ptr())
        };
        // SAFETY: all pointers are valid NUL-terminated wide buffers.
        unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(exe_w.as_ptr()),
                params_ptr,
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
        true
    }
}

pub async fn run_main_loop(
    mut shutdown: ShutdownSignal,
    use_tcp: bool,
    os_rx: tokio::sync::mpsc::UnboundedReceiver<scheduler::OsEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::default();

    prepare_data_dir(&config.data_dir)?;
    // The installed service's data folder is SYSTEM's and the
    // Administrators' (`data_dir_acl`). A link to elsewhere somebody else
    // made stops the start before the service writes anything there.
    #[cfg(windows)]
    let protect_data = !use_tcp
        && !own_backup_common::media_mode::is_media_mode()
        && matches!(
            data_dir_acl::check_root(&config.data_dir)?,
            data_dir_acl::RootKind::Folder
        );
    // A config.toml that a user owns stops steering the service here. Before
    // the folder is protected: which mirror folder it may keep depends on
    // who owned it.
    #[cfg(windows)]
    settings::adopt_config_file(&AppConfig::default_path());
    #[cfg(windows)]
    if protect_data {
        data_dir_acl::protect(&config.data_dir);
    }

    // Everyone's logs, swept by the one process that is always here.
    start_log_housekeeping();

    // BEFORE the database, the job store, the worker, the scheduler and the
    // token file — all of which a second copy used to reach before the named
    // pipe finally turned it away (audit AUD-07). Held for the life of the
    // process; the OS releases it even if we are killed.
    let _ownership = match single_instance::claim(&config.data_dir) {
        Ok(o) => {
            if !o.machine_wide {
                warn!(
                    "single instance is guarded only within this session — the machine-wide \
                     name was refused"
                );
            }
            o
        }
        Err(e) => {
            // Nothing above this line changed anything, and nothing below runs.
            warn!(error = %e, "Refusing to start a second engine");
            eprintln!("{e}");
            return Err(Box::new(e));
        }
    };
    // The lock above is per data directory, but the socket and the token are
    // machine-wide: an engine started with another data directory got all the
    // way to the socket — its scheduler already running — and took it over.
    #[cfg(unix)]
    let _runtime_ownership = if use_tcp {
        None
    } else {
        claim_runtime_dir(&config).map_err(|e| {
            warn!(error = %e, "Refusing to start a second engine");
            eprintln!("{e}");
            e
        })?
    };
    // The root CLI on this machine learns from here where this instance
    // keeps its data — above all its key directory, which a copy the CLI
    // makes must leave out too (decision 6/E4). Not a reason to stop: the
    // configured directory is still known to it.
    #[cfg(target_os = "linux")]
    if !use_tcp {
        if let Some(runtime) = config.socket_path().parent() {
            if let Err(e) = own_backup_common::service_data_dir::publish(runtime, &config.data_dir)
            {
                warn!(error = %e, "the data directory could not be published for the CLI");
            }
        }
    }

    let conn = own_backup_metadata::open_database(&config.db_path())?;
    info!("Metadata database initialized");
    // Migration may VACUUM this database. Do not hold an idle connection open
    // for the rest of the service lifetime.
    drop(conn);

    // Build the orchestrator core: job store, serial run queue, cancel flag,
    // an event fan-out for progress streams, and the background worker that
    // actually drives backups.
    let store = own_backup_core::JobStore::try_load(&config.data_dir)?;
    // A local authorization whose job is gone opens an archive on this machine
    // with no password and is accounted for by nothing. Deleting a job retires
    // its own; this catches what older builds and interrupted transactions
    // left. Read as diagnostics, not as a failure to start: on a machine with
    // no encrypted job the store does not even exist.
    match store.sweep_local_archive_keys() {
        Ok(0) => {}
        Ok(n) => info!(
            retired = n,
            "unreferenced local archive authorizations removed"
        ),
        Err(e) => debug!(error = %e, "local archive key store not swept"),
    }
    // A mount lives in the process serving it, so whatever was mounted when
    // the last instance stopped (an update, a crash) is now a session with
    // nothing behind it: a dead disk in Disk Management, and that image's next
    // mount refused. On its own thread — a logout can wait out a busy device
    // for seconds, and nothing else at startup needs to wait for it.
    #[cfg(target_os = "windows")]
    std::thread::spawn(|| {
        let cleared = own_backup_backup::iscsi_win::clear_orphaned_sessions();
        if cleared > 0 {
            info!(
                cleared,
                "orphaned iSCSI sessions of an earlier run logged out"
            );
        }
    });
    let queue = own_backup_core::RunQueue::new();
    // Everything destructive — a copy, a restore, a verify, a read-write
    // mount, deleting a job's images — goes through this one slot (AUD-02).
    let busy = own_backup_core::busy::Busy::new();
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (events, _bootstrap_rx) = tokio::sync::broadcast::channel::<grpc_server::proto::Event>(256);

    let sink: std::sync::Arc<dyn own_backup_core::EventSink> =
        std::sync::Arc::new(grpc_server::GrpcEventSink { tx: events.clone() });
    // The installed Windows service keeps remembered volume passwords in its
    // SYSTEM-only directory. Validate and pin that directory before SQLite can
    // create the private DB or its WAL/SHM files; complete legacy migration
    // before the worker, scheduler or RPC starts.
    #[cfg(target_os = "windows")]
    let _private_secret_guard = if !use_tcp && !own_backup_common::media_mode::is_media_mode() {
        Some(own_backup_backup::seal::local::PrivateSecretDatabase::new(
            &config.data_dir,
        )?)
    } else {
        None
    };
    #[cfg(target_os = "windows")]
    let passwords = if let Some(guard) = &_private_secret_guard {
        let store = own_backup_core::PasswordStore::new_private(
            config.db_path(),
            guard.path().to_path_buf(),
        );
        let moved = store.migrate_legacy(&config.db_path())?;
        guard.validate_files()?;
        info!(
            moved,
            "legacy volume passwords migrated to private database"
        );
        std::sync::Arc::new(store)
    } else {
        std::sync::Arc::new(own_backup_core::PasswordStore::new(config.db_path()))
    };
    // Unfinished VHDX/qcow2 images are recorded in the same SYSTEM-only
    // directory, so that what a killed run left can be proved ours and
    // deleted (`pending_image`). Destinations connected now are swept at
    // once; the rest at the next run.
    #[cfg(target_os = "windows")]
    if let Some(dir) = _private_secret_guard
        .as_ref()
        .and_then(|guard| guard.path().parent())
    {
        match own_backup_backup::pending_image::use_ledger(dir.join("pending-images")) {
            Ok(()) => {
                std::thread::spawn(own_backup_backup::pending_image::reclaim_abandoned);
            }
            Err(e) => tracing::warn!(error = %e, "unfinished images will not be reclaimed"),
        }
    }
    // The same on Linux, in a root-only directory beside the data: only the
    // installed service, which runs as root, keeps the records.
    #[cfg(unix)]
    let installed_service = !use_tcp
        && !own_backup_common::media_mode::is_media_mode()
        // SAFETY: geteuid has no preconditions.
        && unsafe { libc::geteuid() } == 0;
    #[cfg(unix)]
    if installed_service {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = config.data_dir.join("pending-images");
        let made = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
            .map_err(own_backup_common::OwnBackupError::from)
            .and_then(|()| own_backup_backup::pending_image::use_ledger(dir));
        match made {
            Ok(()) => {
                std::thread::spawn(own_backup_backup::pending_image::reclaim_abandoned);
            }
            Err(e) => tracing::warn!(error = %e, "unfinished images will not be reclaimed"),
        }
    }
    // A snapshot a killed run left behind keeps growing with every write to
    // its origin until it fills the volume group (LVM) or its store in the
    // destination folder (device-mapper); a device-mapper one also keeps the
    // origin running through our layer.
    #[cfg(target_os = "linux")]
    if installed_service {
        tokio::spawn(async {
            let removed = own_backup_snapshot::linux::remove_orphaned_snapshots().await;
            if removed > 0 {
                info!(removed, "snapshots left by interrupted runs removed");
            }
        });
        // A udev hold a killed restore left behind keeps udisks, LVM, md and
        // systemd away from that disk until the next reboot.
        own_backup_backup::udev_hold_linux::sweep_stale_holds(std::path::Path::new(
            own_backup_backup::udev_hold_linux::RULES_DIR,
        ));
        // A medium build runs as a transient unit outside this cgroup, so
        // an engine killed or upgraded mid-build leaves it running, its
        // binds under the cache mounted. Stopped and cleaned before any
        // build may start: the slot is held until then.
        let slot = busy
            .reserve(own_backup_core::busy::Doing::new(
                tr!("main.busy_media_cleanup"),
                own_backup_backup::media_linux_build::CACHE_DIR,
            ))
            .ok();
        std::thread::spawn(move || {
            let _slot = slot;
            if own_backup_backup::media_linux_build::recover_abandoned_build() {
                info!("a medium build left by an earlier run was stopped and cleaned");
            }
        });
    }
    // The installed Linux service keeps remembered volume passwords (an
    // opened LUKS mapping's among them) in its root-only key directory —
    // which the engine leaves out of every copy — sealed by the key store's
    // machine key (`systemd-creds`). Passwords an older build sealed with
    // `secret.key` beside the public data move there first.
    // Wherever this instance keeps its key directory, the engine leaves it
    // out of every copy of the volume it is on (decision 6/E4).
    #[cfg(target_os = "linux")]
    own_backup_backup::seal::local::register_data_dir(&config.data_dir);
    #[cfg(target_os = "linux")]
    let passwords = if installed_service {
        // A key store that cannot be opened (a data directory others may
        // write, no systemd-creds, a sealing failure) costs the remembered
        // passwords, not the service: plain copies keep running, and the
        // window does not offer encryption (`with_key_store`).
        let opened = own_backup_backup::seal::local::PrivateSecretDatabase::new(&config.data_dir)
            .and_then(|guard| {
                let store = own_backup_core::PasswordStore::new_private_sealed(
                    config.db_path(),
                    guard.path().to_path_buf(),
                    guard.secret_store(),
                );
                // Rows that cannot move yet stay where they are and are
                // tried again at the next start.
                match store.migrate_legacy(&config.db_path()) {
                    Ok(0) => {}
                    Ok(moved) => info!(
                        moved,
                        "legacy volume passwords migrated to the private database"
                    ),
                    Err(e) => warn!(error = %e, "legacy volume passwords not migrated yet"),
                }
                guard.validate_files()?;
                Ok(store)
            });
        match opened {
            Ok(store) => std::sync::Arc::new(store),
            Err(e) => {
                warn!(
                    error = %e,
                    "service key store unavailable: volume passwords are kept for this run only"
                );
                std::sync::Arc::new(own_backup_core::PasswordStore::memory_only(
                    config.db_path(),
                ))
            }
        }
    } else {
        std::sync::Arc::new(own_backup_core::PasswordStore::new(config.db_path()))
    };
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let passwords = std::sync::Arc::new(own_backup_core::PasswordStore::new(config.db_path()));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let verification_worker = scheduled_verification::spawn(
        store.clone(),
        busy.clone(),
        queue.clone(),
        sink.clone(),
        stop_rx.clone(),
    );
    let worker = tokio::spawn(own_backup_core::run_queue_worker(
        sink,
        store.clone(),
        queue.clone(),
        busy.clone(),
        cancel.clone(),
        passwords.clone(),
        job_rx,
        job_tx.downgrade(),
        stop_rx,
    ));
    let cancel_on_exit = cancel.clone();
    let busy_on_exit = busy.clone();
    let mut worker_shutdown = shutdown.clone();
    let worker_stop = stop_tx.clone();
    let worker_cancel = cancel.clone();
    let worker_busy = busy.clone();
    let shutdown_watcher = tokio::spawn(async move {
        worker_shutdown.wait_for_shutdown().await;
        worker_stop.send_replace(true);
        // The stop is marked before anything is cancelled: a run that ends
        // cancelled now is recorded as interrupted, not as a person's cancel.
        worker_busy.cancel_for_stop();
        worker_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    info!(
        "Backup orchestrator ready ({} job(s) loaded)",
        store.list().len()
    );

    // The scheduler enqueues due jobs on a timer (stops on shutdown) and reacts
    // to OS events (logon/logoff/shutdown) forwarded from the service handler.
    scheduler::spawn(
        store.clone(),
        queue.clone(),
        job_tx.clone(),
        passwords.clone(),
        shutdown.clone(),
        os_rx,
    );

    let state = AppState::new(
        config.clone(),
        store,
        queue,
        busy,
        job_tx,
        cancel,
        events,
        passwords,
    );

    // Production transport (no listening TCP port, so no local process can reach
    // the privileged service by opening a socket): Unix Domain Socket on Linux,
    // a DACL-restricted named pipe on Windows. Dev/foreground (`use_tcp`) uses
    // loopback TCP on both.
    #[cfg(unix)]
    if !use_tcp {
        let served = serve_uds(config, state, shutdown).await;
        stop_worker(
            stop_tx,
            cancel_on_exit,
            busy_on_exit,
            worker,
            verification_worker,
            shutdown_watcher,
        )
        .await?;
        return served;
    }
    #[cfg(windows)]
    if !use_tcp {
        let served = serve_named_pipe(config, state, shutdown).await;
        stop_worker(
            stop_tx,
            cancel_on_exit,
            busy_on_exit,
            worker,
            verification_worker,
            shutdown_watcher,
        )
        .await?;
        return served;
    }

    // TCP on loopback. On Windows this is the production transport (until a
    // named pipe lands): require a per-boot bearer token stored in an ACL'd file
    // that only the service, admins and interactive users can read. On other
    // platforms the TCP path is dev/foreground only and stays token-less (the
    // real Unix transport is the UDS above).
    let addr: SocketAddr = config.grpc_address.parse()?;
    // A failure here USED to fall back to an empty token, and an empty expected
    // token turns `check_auth` into `Ok(())` for every call — the service went
    // on serving with no authentication at all, saying so in a warning nobody
    // reads. A contract that says "a token is required" must not be switched
    // off by a failed write; refusing to start is the honest outcome.
    #[cfg(windows)]
    let token = write_service_token(&config).map_err(|e| {
        tracing::error!(error = %e, "no service token could be written — refusing to start");
        e
    })?;
    #[cfg(not(windows))]
    let token = String::new();
    info!(
        "gRPC server (TCP/localhost) listening on {} (auth: {})",
        addr,
        if token.is_empty() { "off" } else { "token" }
    );
    #[cfg(windows)]
    let kept_token = token.clone();
    let busy = state.busy.clone();
    let service = BackupServiceImpl::new(state, token, shutdown.clone()).into_service();
    let served = Server::builder()
        .add_service(service)
        .serve_with_shutdown(addr, async move {
            shutdown.wait_for_shutdown().await;
            busy.cancel_for_stop();
            busy.wait_until_idle().await;
        })
        .await;

    stop_worker(
        stop_tx,
        cancel_on_exit,
        busy_on_exit,
        worker,
        verification_worker,
        shutdown_watcher,
    )
    .await?;
    served?;

    #[cfg(windows)]
    drop_our_token(&config, &kept_token);
    Ok(())
}

async fn stop_worker(
    stop: tokio::sync::watch::Sender<bool>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    busy: std::sync::Arc<own_backup_core::busy::Busy>,
    worker: tokio::task::JoinHandle<()>,
    verification_worker: tokio::task::JoinHandle<()>,
    watcher: tokio::task::JoinHandle<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    stop.send_replace(true);
    busy.cancel_for_stop();
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    watcher.abort();
    // The worker owns the backup result and writes its final history before
    // returning. Aborting it here would release its busy guard while the
    // blocking disk operation could still be writing.
    worker.await?;
    verification_worker.await?;
    busy.wait_until_idle().await;
    Ok(())
}

/// Take the token file away on shutdown — but only while it is still ours.
///
/// A second engine that started, minted its own token over ours and then failed
/// to take the pipe would otherwise delete OUR token on its way out, and the
/// interface talking to the running service is refused from that moment on.
/// Seen on the boot medium, where the launcher was run more than once
/// (2026-09-19): five engine starts in one session and `token_len=0` after them.
#[cfg(windows)]
fn drop_our_token(config: &AppConfig, ours: &str) {
    let path = config.token_path();
    match std::fs::read_to_string(&path) {
        Ok(on_disk) if on_disk.trim() == ours => {
            let _ = std::fs::remove_file(&path);
        }
        Ok(_) => tracing::info!("the token file belongs to another engine now - left alone"),
        Err(_) => {}
    }
}

/// Generate a per-boot bearer token, write it to the (ACL-restricted) token
/// file, and return it. Windows loopback-TCP transport only.
#[cfg(windows)]
fn write_service_token(config: &AppConfig) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Write;

    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| format!("OS RNG failed: {e}"))?;
    let token: String = raw.iter().map(|b| format!("{:02x}", b)).collect();

    let path = config.token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Written beside itself and moved into place, never over the top of the
    // old one. `write` truncates first, so a reader arriving in that instant
    // gets an EMPTY token and is refused by this very service — which is what
    // happened on the boot medium when a second engine started while the
    // interface was running: `token_len=0` in its log, and every call after it
    // answered "Invalid or missing token" (2026-09-19). A rename cannot be
    // caught half-done.
    // Create with a protected DACL from the first instant. The old path wrote
    // a world-readable inherited file, renamed it, and only then ran icacls;
    // a failed icacls still let the privileged service start.
    let fresh = path.with_extension(format!("token.new.{}", uuid::Uuid::new_v4()));
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut file = create_service_token_file(&fresh)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&fresh, &path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&fresh);
    }
    written?;

    info!("Service token written to {:?}", path);
    Ok(token)
}

/// Create the empty token file with its final protected DACL. Keeping creation
/// separate lets a native Windows test inspect the handle before the first byte
/// is written, including when its parent grants inherited access to Everyone.
#[cfg(windows)]
fn create_service_token_file(
    path: &std::path::Path,
) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    };

    let security = PipeSecurity::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)")?;
    let sa = security.attributes();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: descriptor and terminated path live through CreateFileW. Windows
    // copies the descriptor into the file object before returning its handle.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&sa),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    // SAFETY: File takes sole ownership and closes the native handle once.
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0) })
}

/// Generate a random 32-byte token, write it to the well-known path with 0o640 permissions.
/// The token is later attached to all gRPC requests for authentication.
#[cfg(unix)]
fn generate_and_write_token(config: &AppConfig) -> Result<String, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| format!("OS RNG failed: {}", e))?;
    // Hex-encode: 64 ASCII chars, no special chars, safe in HTTP header values.
    let token: String = raw.iter().map(|b| format!("{:02x}", b)).collect();

    let token_path = config.token_path();
    std::fs::write(&token_path, token.as_bytes())?;
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o640))?;

    info!("Service token written to {:?}", token_path);
    Ok(token)
}

/// GID of the `own-backup` access group, or `None` if it does not exist.
#[cfg(unix)]
fn own_backup_group_gid() -> Option<u32> {
    let name = std::ffi::CString::new(own_backup_common::UNIX_ACCESS_GROUP).ok()?;
    // SAFETY: `name` is a valid C string; getgrnam returns a pointer into a
    // static buffer (read immediately, not stored) or null when absent.
    let grp = unsafe { libc::getgrnam(name.as_ptr()) };
    if grp.is_null() {
        None
    } else {
        // SAFETY: non-null grp points at a valid `group` for this call.
        Some(unsafe { (*grp).gr_gid })
    }
}

/// Set a path's group owner to `gid`, leaving the user owner (root) unchanged.
/// Best-effort: a failure only means group members cannot reach it (logged).
#[cfg(unix)]
fn chown_to_group(path: &std::path::Path, gid: u32) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: `c_path` is a valid C string; uid -1 leaves the owner unchanged.
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        tracing::warn!(path = %path.display(), "could not chgrp to the access group");
    }
}

/// Create the data directory.
///
/// On Unix it is not left to the umask: it holds the job store, the activity
/// journal (with the Argon2id-sealed archive keys) and the metadata database,
/// and a default 0755 made them readable by every local user. 0750 with the
/// access group — members still read `config.toml` — and 0700 for the boot
/// medium's directory. An existing directory (an older build made it, or the
/// log fallback got there first) is tightened to that, never widened.
#[cfg(unix)]
fn prepare_data_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let media = own_backup_common::media_mode::is_media_mode();
    let allowed = if media { 0o700 } else { 0o750 };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(allowed)
        .create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode & !allowed != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & allowed))?;
    }
    // SAFETY: geteuid has no preconditions. Only root may hand the directory
    // to a group; a development run under OWN_BACKUP_DATA_DIR keeps its own.
    if !media && unsafe { libc::geteuid() } == 0 {
        if let Some(gid) = own_backup_group_gid() {
            chown_to_group(dir, gid);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_data_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The runtime directory holding the socket and the token: 0750 and the
/// access group, so members can traverse to the socket and nobody else can.
#[cfg(unix)]
fn prepare_runtime_dir(dir: &std::path::Path, group_gid: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
    if let Some(gid) = group_gid {
        chown_to_group(dir, gid);
    }
    Ok(())
}

/// Claim the machine-wide runtime directory the way the data directory is
/// claimed (`single_instance`), before anything else starts.
///
/// `None` when there is nothing more to claim: the data directory IS the
/// runtime directory, and its lock — the same file — is already held.
#[cfg(unix)]
fn claim_runtime_dir(
    config: &AppConfig,
) -> Result<Option<single_instance::Ownership>, Box<dyn std::error::Error>> {
    let socket_path = config.socket_path();
    let Some(dir) = socket_path.parent() else {
        return Ok(None);
    };
    prepare_runtime_dir(dir, own_backup_group_gid())?;
    if std::fs::canonicalize(dir).ok() == std::fs::canonicalize(&config.data_dir).ok() {
        return Ok(None);
    }
    Ok(Some(single_instance::claim(dir)?))
}

/// Serve gRPC over Unix Domain Socket.
///
/// Socket: /run/own-backup/service.sock, 0o660, chowned to the `own-backup`
/// group — group members (added by `--install`) reach the service without
/// root; everyone else is refused by filesystem permissions. Each call is
/// further checked against the caller's login session (`peer_access`), the
/// counterpart of the Windows console-session check. There is no listening
/// TCP port in this mode.
#[cfg(unix)]
async fn serve_uds(
    config: AppConfig,
    state: std::sync::Arc<AppState>,
    mut shutdown: ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    use tokio_stream::wrappers::UnixListenerStream;

    let socket_path = config.socket_path();

    // Resolve the access group once; None if it does not exist (install failed
    // or was skipped) — then only root reaches the socket, which we log.
    let group_gid = own_backup_group_gid();

    if let Some(parent) = socket_path.parent() {
        prepare_runtime_dir(parent, group_gid)?;
    }

    // A socket left by a killed engine refuses connections and is removed. One
    // that answers belongs to a live engine — an older build without the
    // runtime lock, say — and deleting it would cut that engine off from every
    // client while it keeps running.
    match std::os::unix::net::UnixStream::connect(&socket_path) {
        Ok(_) => {
            let e = tr!("main.socket_in_use", path = socket_path.display()).into_owned();
            warn!(socket = %socket_path.display(), "Refusing to start a second engine");
            eprintln!("{e}");
            return Err(e.into());
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            match std::fs::remove_file(&socket_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => {
            return Err(tr!(
                "main.socket_unknown",
                path = socket_path.display(),
                error = e
            )
            .into_owned()
            .into())
        }
    }

    let uds = UnixListener::bind(&socket_path)?;

    // 0o660 + group own-backup: root and group members read/write, others cannot.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = group_gid {
        chown_to_group(&socket_path, gid);
    } else {
        tracing::warn!(
            group = own_backup_common::UNIX_ACCESS_GROUP,
            "access group not found — only root can reach the service; run `--install` (as root) to create it"
        );
    }

    // Generate per-boot service authentication token (chowned to the group too).
    let token = generate_and_write_token(&config)?;
    if let Some(gid) = group_gid {
        chown_to_group(&config.token_path(), gid);
    }

    info!("gRPC server (UDS) listening on {:?}", socket_path);
    // The socket answers and the token is readable: only now is the unit
    // "active" (Type=notify), so a client started right after `systemctl
    // start` finds both.
    crate::sd_notify::ready();

    let busy = state.busy.clone();
    let service = BackupServiceImpl::new(state, token, shutdown.clone()).into_service();
    let served = Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(UnixListenerStream::new(uds), async move {
            shutdown.wait_for_shutdown().await;
            busy.cancel_for_stop();
            busy.wait_until_idle().await;
        })
        .await;

    // Ours either way: the runtime lock is held, so no other engine has
    // published a socket or a token since.
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&config.token_path());
    served?;
    Ok(())
}

/// A connected named-pipe instance, adapted to tonic's connection trait. The
/// pipe is a byte stream just like a socket, so read/write delegate straight to
/// the inner [`NamedPipeServer`].
#[cfg(windows)]
struct NamedPipeConn(tokio::net::windows::named_pipe::NamedPipeServer, PipeCaller);

/// Who is at the other end of a named-pipe connection, found when it was
/// accepted (`client_in_active_console`); every request on it carries this in
/// its extensions, which is how `settings::caller_of` knows the user.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub(crate) struct PipeCaller {
    /// The client process's user, `S-1-5-…`.
    pub sid: String,
    /// SYSTEM, or a token elevated as an administrator.
    pub elevated: bool,
}

#[cfg(windows)]
impl tonic::transport::server::Connected for NamedPipeConn {
    type ConnectInfo = PipeCaller;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.1.clone()
    }
}

#[cfg(windows)]
impl tokio::io::AsyncRead for NamedPipeConn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl tokio::io::AsyncWrite for NamedPipeConn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// An owned Windows security descriptor used when creating the named pipe or
/// the token file. Freed via `LocalFree` on drop.
#[cfg(windows)]
struct PipeSecurity(windows::Win32::Security::PSECURITY_DESCRIPTOR);

/// The client, if it runs in the active console session as the user logged on
/// there (`None` refuses the connection).
#[cfg(windows)]
fn client_in_active_console(
    server: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> Option<PipeCaller> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeClientSessionId};
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Owned(HANDLE);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    fn user_sid(token: HANDLE) -> Option<Vec<u8>> {
        let mut needed = 0u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
        if needed < std::mem::size_of::<TOKEN_USER>() as u32 || needed > 64 * 1024 {
            return None;
        }
        let mut bytes = vec![0u8; needed as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(bytes.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
        }
        .ok()?;
        Some(bytes)
    }

    let active = unsafe { WTSGetActiveConsoleSessionId() };
    if active == u32::MAX {
        return None;
    }
    let mut client = u32::MAX;
    let pipe = HANDLE(server.as_raw_handle());
    if unsafe { GetNamedPipeClientSessionId(pipe, &mut client) }.is_err() || client != active {
        return None;
    }
    let mut process_id = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut process_id) }.is_err() {
        return None;
    }
    let process =
        Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?);
    let mut client_token = HANDLE::default();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut client_token) }.is_err() {
        return None;
    }
    let client_token = Owned(client_token);
    let a = user_sid(client_token.0)?;
    // TOKEN_USER is pointer-aligned in the API buffer; read_unaligned avoids
    // assuming Vec<u8>'s alignment. The pointed-to SID lives in `a`.
    let a_user = unsafe { std::ptr::read_unaligned(a.as_ptr().cast::<TOKEN_USER>()) };
    let sid = unsafe { own_backup_common::win_acl::sid_string(a_user.User.Sid) }?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let elevated = sid == own_backup_common::win_acl::SYSTEM_SID
        || (unsafe {
            GetTokenInformation(
                client_token.0,
                TokenElevation,
                Some(std::ptr::addr_of_mut!(elevation).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        }
        .is_ok()
            && elevation.TokenIsElevated != 0);
    let caller = PipeCaller { sid, elevated };
    let mut console_token = HANDLE::default();
    if unsafe { WTSQueryUserToken(active, &mut console_token) }.is_err() {
        // A stripped WinPE may have no WTS token; its single temporary console
        // session is still distinguished by the check above.
        return own_backup_common::media_mode::is_media_mode().then_some(caller);
    }
    let console_token = Owned(console_token);
    let b = user_sid(console_token.0)?;
    let b_user = unsafe { std::ptr::read_unaligned(b.as_ptr().cast::<TOKEN_USER>()) };
    unsafe { EqualSid(a_user.User.Sid, b_user.User.Sid).is_ok() }.then_some(caller)
}

// The descriptor is immutable after construction; only a raw pointer prevents
// the auto-derive, so it is safe to move between threads.
#[cfg(windows)]
unsafe impl Send for PipeSecurity {}

#[cfg(windows)]
impl PipeSecurity {
    fn new(sddl: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;

        let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 string; `psd` receives a
        // freshly LocalAlloc'd descriptor that Drop frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?;
        }
        Ok(Self(psd))
    }

    /// A SECURITY_ATTRIBUTES referencing this descriptor. The returned value
    /// borrows `self`'s pointer, so it must not outlive `self`.
    fn attributes(&self) -> windows::Win32::Security::SECURITY_ATTRIBUTES {
        windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: windows::Win32::Foundation::FALSE,
        }
    }
}

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: the pointer came from ConvertString...SecurityDescriptorW,
        // which documents LocalFree as the matching deallocator.
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
                self.0 .0,
            ));
        }
    }
}

/// Serve gRPC over a Windows named pipe (production transport — no listening TCP
/// port). Access is gated by the pipe's DACL (see [`PipeSecurity`]); the bearer
/// token is still written and required, as defence in depth. One listening pipe
/// instance is always kept open ahead of the connected ones so clients are never
/// refused with ERROR_PIPE_BUSY.
#[cfg(windows)]
async fn serve_named_pipe(
    config: AppConfig,
    state: std::sync::Arc<AppState>,
    mut shutdown: ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::c_void;
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_path = own_backup_common::windows_pipe_path();
    // As on the TCP path: no token means no authentication, so a write that
    // failed ends the start instead of quietly removing the check.
    let token = write_service_token(&config).map_err(|e| {
        tracing::error!(error = %e, "no service token could be written — refusing to start");
        e
    })?;
    // 0x0012019B is duplex client I/O without FILE_CREATE_PIPE_INSTANCE
    // (bit 0x4). GENERIC_WRITE would grant that server-only right too.
    let security = PipeSecurity::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019B;;;IU)")?;

    info!(
        "gRPC server (named pipe) listening on {} (auth: {})",
        pipe_path,
        if token.is_empty() { "off" } else { "token" }
    );

    let incoming = async_stream::stream! {
        // First instance uses FIRST_PIPE_INSTANCE so a pre-existing (squatted)
        // pipe of the same name makes creation fail loudly instead of us
        // adopting a rogue endpoint.
        let sa = security.attributes();
        let first = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(
                    &pipe_path,
                    &sa as *const _ as *mut c_void,
                )
        };
        let mut server = match first {
            Ok(s) => s,
            Err(e) => {
                yield Err(e);
                return;
            }
        };
        loop {
            match server.connect().await {
                Ok(()) => {
                    let connected = server;
                    // Open the next listening instance BEFORE handing off the
                    // connected one, so a client arriving now is not refused.
                    let sa = security.attributes();
                    server = match unsafe {
                        ServerOptions::new().create_with_security_attributes_raw(
                            &pipe_path,
                            &sa as *const _ as *mut c_void,
                        )
                    } {
                        Ok(s) => s,
                        Err(e) => {
                            if let Some(caller) = client_in_active_console(&connected) {
                                yield Ok(NamedPipeConn(connected, caller));
                            }
                            yield Err(e);
                            return;
                        }
                    };
                    let Some(caller) = client_in_active_console(&connected) else {
                        tracing::warn!(target: "security", "named-pipe client outside the active console session rejected");
                        continue;
                    };
                    yield Ok(NamedPipeConn(connected, caller));
                }
                Err(e) => {
                    // Recreate the instance and keep serving.
                    yield Err(e);
                    let sa = security.attributes();
                    server = match unsafe {
                        ServerOptions::new().create_with_security_attributes_raw(
                            &pipe_path,
                            &sa as *const _ as *mut c_void,
                        )
                    } {
                        Ok(s) => s,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                }
            }
        }
    };

    let kept_token = token.clone();
    let busy = state.busy.clone();
    let service = BackupServiceImpl::new(state, token, shutdown.clone()).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(incoming, async move {
            shutdown.wait_for_shutdown().await;
            busy.cancel_for_stop();
            busy.wait_until_idle().await;
        })
        .await?;

    drop_our_token(&config, &kept_token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[cfg(windows)]
    #[test]
    fn service_token_dacl_is_protected_before_first_write() {
        use std::os::windows::io::AsRawHandle;
        use windows::core::PWSTR;
        use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
        use windows::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
        };
        use windows::Win32::Security::{
            GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACL,
            DACL_SECURITY_INFORMATION, INHERITED_ACE, PSECURITY_DESCRIPTOR, PSID,
            SE_DACL_PROTECTED,
        };
        use windows::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_GENERIC_READ};

        let dir = tempfile::tempdir().unwrap();
        let grant = std::process::Command::new("icacls.exe")
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .output()
            .unwrap();
        assert!(
            grant.status.success(),
            "cannot prepare inherited Everyone access"
        );
        let file = create_service_token_file(&dir.path().join("service.token.new")).unwrap();
        assert_eq!(
            file.metadata().unwrap().len(),
            0,
            "test must precede the first write"
        );
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: live token handle grants READ_CONTROL; out-pointers are valid.
        unsafe {
            GetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
            .ok()
            .unwrap();
        }
        struct Owned(PSECURITY_DESCRIPTOR);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
            }
        }
        let descriptor = Owned(descriptor);
        let mut control = 0;
        let mut revision = 0;
        unsafe {
            GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision).unwrap();
        }
        assert_ne!(control & SE_DACL_PROTECTED.0, 0);
        assert!(!dacl.is_null(), "a NULL DACL would grant everyone access");
        let count = unsafe { (*dacl).AceCount };
        assert_eq!(count, 3, "unexpected inherited or extra ACE");
        let mut actual = std::collections::BTreeMap::new();
        for index in 0..u32::from(count) {
            let mut entry = std::ptr::null_mut();
            unsafe {
                GetAce(dacl, index, &mut entry).unwrap();
            }
            let ace = unsafe { &*(entry as *const ACCESS_ALLOWED_ACE) };
            assert_eq!(
                ace.Header.AceType, 0,
                "only explicit access-allowed ACEs expected"
            );
            assert_eq!(u32::from(ace.Header.AceFlags) & INHERITED_ACE.0, 0);
            let sid = PSID((&ace.SidStart as *const u32).cast_mut().cast());
            let mut text = PWSTR::null();
            unsafe {
                ConvertSidToStringSidW(sid, &mut text).unwrap();
            }
            let sid = unsafe { text.to_string().unwrap() };
            let _ = unsafe { LocalFree(HLOCAL(text.0.cast())) };
            assert!(actual.insert(sid, ace.Mask).is_none());
        }
        let expected = std::collections::BTreeMap::from([
            ("S-1-5-18".to_string(), FILE_ALL_ACCESS.0),
            ("S-1-5-32-544".to_string(), FILE_ALL_ACCESS.0),
            ("S-1-5-4".to_string(), FILE_GENERIC_READ.0),
        ]);
        assert_eq!(actual, expected);
        println!("A05-TOKEN-CREATE bytes=0 protected=true inherited=false ACEs={actual:?}");
    }

    /// The case that failed: the medium's launcher spells it `--run --media`,
    /// and the old single pass returned on `--run` without ever reading the
    /// rest of the line (audit AUD-16).
    #[test]
    fn media_is_seen_after_the_mode_word_too() {
        assert!(wants_media(&args("--run --media")));
        assert_eq!(mode_of(&args("--run --media")), Mode::Daemon);
    }

    #[test]
    fn every_order_of_run_and_media_means_the_same_thing() {
        for line in [
            "--run --media",
            "--media --run",
            "/media --run",
            "--run /media",
        ] {
            assert!(wants_media(&args(line)), "{line}");
            assert_eq!(mode_of(&args(line)), Mode::Daemon, "{line}");
        }
    }

    /// And the other way round: ordinary Windows must not fall into media mode
    /// because of a word that merely looks like the flag.
    #[test]
    fn nothing_else_turns_media_mode_on() {
        for line in [
            "",
            "--run",
            "--foreground",
            "/install",
            "--run media",
            "--mediaX",
        ] {
            assert!(!wants_media(&args(line)), "{line}");
        }
    }

    #[test]
    fn media_colours_any_mode_it_is_given_with() {
        assert_eq!(mode_of(&args("--media --foreground")), Mode::Foreground);
        assert_eq!(mode_of(&args("--media /status")), Mode::Status);
        // On its own it selects nothing: the SCM launches us with no words.
        assert_eq!(mode_of(&args("--media")), Mode::Service);
    }

    #[test]
    fn journal_lines_carry_their_level_as_a_syslog_priority() {
        let line = |level: &str| format!("2026-10-19T16:26:28.486128Z {level} security: text\n");
        assert_eq!(syslog_prefix(line("ERROR").as_bytes()), "<3>");
        assert_eq!(syslog_prefix(line(" WARN").as_bytes()), "<4>");
        assert_eq!(syslog_prefix(line(" INFO").as_bytes()), "<6>");
        assert_eq!(syslog_prefix(line("DEBUG").as_bytes()), "<7>");
        assert_eq!(syslog_prefix(b"continuation of a message\n"), "<6>");
    }

    #[test]
    fn without_a_journal_the_copy_is_the_file_alone() {
        use std::io::Write;
        let mut copy = JournalCopy {
            file: Vec::new(),
            journal: false,
        };
        copy.write_all(b"one\ntwo\n").unwrap();
        assert_eq!(copy.file, b"one\ntwo\n");
    }

    #[test]
    fn grant_access_takes_the_user_after_it_and_nothing_else() {
        assert_eq!(mode_of(&args("--grant-access alice")), Mode::GrantAccess);
        assert_eq!(
            grant_access_user(&args("--grant-access alice")),
            Some("alice")
        );
        assert_eq!(grant_access_user(&args("--grant-access")), None);
        assert_eq!(grant_access_user(&args("--grant-access --media")), None);
        assert_eq!(grant_access_user(&args("--run")), None);
    }

    #[test]
    fn the_first_mode_word_wins() {
        assert_eq!(mode_of(&args("/install --run")), Mode::Install);
        assert_eq!(mode_of(&args("/purge-data")), Mode::PurgeData);
        assert_eq!(mode_of(&args("--run /install")), Mode::Daemon);
        assert_eq!(mode_of(&args("")), Mode::Service);
    }
}
