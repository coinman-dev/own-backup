mod app_state;
mod grpc_server;
mod install;
mod scheduler;
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
    args.iter().any(|a| matches!(a.as_str(), "--media" | "/media"))
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
            "-f" | "--foreground" => return Mode::Foreground,
            "--run" | "run" | "--daemon" | "daemon" => return Mode::Daemon,
            _ => {}
        }
    }
    Mode::Service
}

/// Today's service log, in the shared `Logs` folder beside the exe (what the
/// user looks for), else the data dir (LocalSystem can always write there),
/// else temp.
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
            let removed = own_backup_common::log_files::sweep(
                &dir,
                own_backup_common::log_files::KEEP_DAYS,
            );
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
    let fallback = if own_backup_common::media_mode::is_windows_pe() { "debug" } else { "info" };
    let level = log_level::configured_level(&config_path, fallback);
    let from_env = tracing_subscriber::EnvFilter::try_from_default_env().ok();
    let pinned = from_env.is_some();
    let filter = from_env
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(log_level::filter_directives(&level)));
    let (log_path, mirror_path, reload) = match open_service_log() {
        Some((f, path)) => {
            // The second copy, on whatever drive the user named — the only one
            // that survives a restore that takes the machine down with it.
            let tee = TeeLog::new(
                f,
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
            tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter).init();
            (None, None, None)
        }
    };
    if let (Some(handle), false) = (reload, pinned) {
        log_level::follow_config(config_path, fallback, level.clone(), move |directives| {
            if handle.reload(tracing_subscriber::EnvFilter::new(directives)).is_ok() {
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse_mode(&args);

    // Media mode only: a dev `--foreground` run wants its console, and under
    // the SCM there is none to hide.
    #[cfg(target_os = "windows")]
    if own_backup_common::media_mode::is_media_mode() && matches!(mode, Mode::Daemon | Mode::Service)
    {
        hide_own_console();
        disable_winpe_firewall();
    }

    // The SCM control operations (register/remove/start/stop) require admin on
    // Windows. Self-elevate via UAC rather than failing with access-denied.
    #[cfg(target_os = "windows")]
    if matches!(
        mode,
        Mode::Install | Mode::Uninstall | Mode::Start | Mode::Stop | Mode::Restart | Mode::Update
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
        // Foreground/dev uses TCP on localhost for easy debugging.
        Mode::Foreground => run_loop(true),
        // Daemon (systemd) uses the OS production transport (UDS/named pipe).
        Mode::Daemon => run_loop(false),
        Mode::Service => run_service_mode(),
    }
}

/// Human-facing description printed when the binary is launched directly in a
/// console (rather than by the SCM). It is a background service, so a bare run
/// should explain itself and list its commands instead of trying to serve.
fn print_info() {
    println!("{} v{}", SERVICE_DISPLAY_NAME, env!("CARGO_PKG_VERSION"));
    println!("{SERVICE_DESCRIPTION}");
    println!();
    println!(
        "Это фоновая служба «{SERVICE_DISPLAY_NAME}». Обычно ею управляет система \
         (автозапуск при загрузке)."
    );
    println!("Запускать вручную из консоли не нужно.");
    println!();
    println!("Команды (принимаются и /flag, и --flag):");
    println!("  /install      установить и запустить службу");
    println!("  /uninstall    остановить и удалить службу");
    println!("  /start        запустить службу");
    println!("  /stop         остановить службу");
    println!("  /restart      перезапустить службу");
    println!("  /status       показать состояние службы");
    println!(
        "  /update       поставить ЭТОТ файл на место установленного: \
остановить, заменить, запустить"
    );
    println!("  --foreground  запустить в консоли для отладки (TCP localhost:50051)");
    println!("  --run         режим демона (UDS/named pipe) — так её запускает systemd");
    println!(
        "  --media       режим загрузочного носителя: состояние на диске носителя, \
         службу не устанавливать (в WinPE включается сам)"
    );
}

/// Build a tokio runtime and run the gRPC loop until shutdown. `use_tcp`
/// selects the transport: loopback TCP for dev/foreground, else the OS
/// production transport (UDS on Unix, a DACL-restricted named pipe on Windows).
fn run_loop(use_tcp: bool) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting {SERVICE_DISPLAY_NAME} ({})", if use_tcp { "foreground/TCP" } else { "daemon/UDS" });
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (controller, signal) = create_shutdown_channel();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("Ctrl+C received, shutting down...");
            controller.trigger_shutdown();
        });
        // No SCM control handler here (foreground/daemon), so no OS events — keep
        // the sender alive so the scheduler's receiver simply never yields.
        let (_os_tx, os_rx) = tokio::sync::mpsc::unbounded_channel::<scheduler::OsEvent>();
        run_main_loop(signal, use_tcp, os_rx).await
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
        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
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

    std::fs::create_dir_all(&config.data_dir)?;

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

    let _conn = own_backup_metadata::open_database(&config.db_path())?;
    info!("Metadata database initialized");

    // Build the orchestrator core: job store, serial run queue, cancel flag,
    // an event fan-out for progress streams, and the background worker that
    // actually drives backups.
    let store = own_backup_core::JobStore::load(&config.data_dir);
    // A local authorization whose job is gone opens an archive on this machine
    // with no password and is accounted for by nothing. Deleting a job retires
    // its own; this catches what older builds and interrupted transactions
    // left. Read as diagnostics, not as a failure to start: on a machine with
    // no encrypted job the store does not even exist.
    match store.sweep_local_archive_keys() {
        Ok(0) => {}
        Ok(n) => info!(retired = n, "unreferenced local archive authorizations removed"),
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
            info!(cleared, "orphaned iSCSI sessions of an earlier run logged out");
        }
    });
    let queue = own_backup_core::RunQueue::new();
    // Everything destructive — a copy, a restore, a verify, a read-write
    // mount, deleting a job's images — goes through this one slot (AUD-02).
    let busy = own_backup_core::busy::Busy::new();
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (events, _bootstrap_rx) =
        tokio::sync::broadcast::channel::<grpc_server::proto::Event>(256);

    let sink: std::sync::Arc<dyn own_backup_core::EventSink> =
        std::sync::Arc::new(grpc_server::GrpcEventSink { tx: events.clone() });
    // Passwords for encrypted volumes: cached for this session, plus the
    // OS-protected store (metadata DB) for remembered ones.
    let passwords = std::sync::Arc::new(own_backup_core::PasswordStore::new(config.db_path()));
    tokio::spawn(own_backup_core::run_queue_worker(
        sink,
        store.clone(),
        queue.clone(),
        busy.clone(),
        cancel.clone(),
        passwords.clone(),
        job_rx,
    ));
    info!("Backup orchestrator ready ({} job(s) loaded)", store.list().len());

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

    let state =
        AppState::new(config.clone(), store, queue, busy, job_tx, cancel, events, passwords);

    // Production transport (no listening TCP port, so no local process can reach
    // the privileged service by opening a socket): Unix Domain Socket on Linux,
    // a DACL-restricted named pipe on Windows. Dev/foreground (`use_tcp`) uses
    // loopback TCP on both.
    #[cfg(unix)]
    if !use_tcp {
        return serve_uds(config, state, shutdown).await;
    }
    #[cfg(windows)]
    if !use_tcp {
        return serve_named_pipe(config, state, shutdown).await;
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
    let service = BackupServiceImpl::new(state, token).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_shutdown(addr, async move {
            shutdown.wait_for_shutdown().await;
        })
        .await?;

    #[cfg(windows)]
    drop_our_token(&config, &kept_token);
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
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    let fresh = path.with_extension("token.new");
    std::fs::write(&fresh, token.as_bytes())?;
    std::fs::rename(&fresh, &path)?;

    // Lock down the DACL: strip inheritance (ProgramData grants Users:read),
    // then allow only SYSTEM + Administrators (full) and INTERACTIVE (read) — so
    // the unelevated GUI/CLI in the desktop session can read the token, but
    // services, network logons and other sessions cannot. Best-effort.
    let ok = std::process::Command::new("icacls")
        .arg(&path)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:F",     // NT AUTHORITY\SYSTEM
            "*S-1-5-32-544:F", // BUILTIN\Administrators
            "*S-1-5-4:R",      // NT AUTHORITY\INTERACTIVE
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        tracing::warn!(?path, "icacls failed on token file — it may be world-readable");
    }

    info!("Service token written to {:?}", path);
    Ok(token)
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

/// Serve gRPC over Unix Domain Socket.
///
/// Socket: /run/own-backup/service.sock, 0o660, chowned to the `own-backup`
/// group — group members (added by `--install`) reach the service without
/// root; everyone else is refused by filesystem permissions. There is no
/// listening TCP port in this mode.
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
        std::fs::create_dir_all(parent)?;
        // 0o750 + group own-backup so members can traverse to the socket/token.
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750))?;
        if let Some(gid) = group_gid {
            chown_to_group(parent, gid);
        }
    }

    // Remove stale socket from previous run
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
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

    let service = BackupServiceImpl::new(state, token).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(
            UnixListenerStream::new(uds),
            async move { shutdown.wait_for_shutdown().await },
        )
        .await?;

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&config.token_path());
    Ok(())
}

/// A connected named-pipe instance, adapted to tonic's connection trait. The
/// pipe is a byte stream just like a socket, so read/write delegate straight to
/// the inner [`NamedPipeServer`].
#[cfg(windows)]
struct NamedPipeConn(tokio::net::windows::named_pipe::NamedPipeServer);

#[cfg(windows)]
impl tonic::transport::server::Connected for NamedPipeConn {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
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

/// A named-pipe security descriptor granting SYSTEM + Administrators full
/// control and the interactive desktop user connect/read/write (but NOT the
/// right to create pipe instances). This mirrors the token-file DACL exactly:
/// the unelevated GUI/CLI in the console session can reach the service, while
/// the network, other logon sessions and non-interactive local services cannot.
/// Freed via `LocalFree` on drop.
#[cfg(windows)]
struct PipeSecurity(windows::Win32::Security::PSECURITY_DESCRIPTOR);

// The descriptor is immutable after construction; only a raw pointer prevents
// the auto-derive, so it is safe to move between threads.
#[cfg(windows)]
unsafe impl Send for PipeSecurity {}

#[cfg(windows)]
impl PipeSecurity {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;

        // D:P = protected DACL (no inheritance from ProgramData etc.);
        // GA = GENERIC_ALL, GR|GW = GENERIC_READ|WRITE. SY=SYSTEM, BA=Builtin
        // Administrators, IU=Interactive Users.
        let sddl: Vec<u16> = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)\0"
            .encode_utf16()
            .collect();
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
    let security = PipeSecurity::new()?;

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
                            yield Ok(NamedPipeConn(connected));
                            yield Err(e);
                            return;
                        }
                    };
                    yield Ok(NamedPipeConn(connected));
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
    let service = BackupServiceImpl::new(state, token).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(incoming, async move {
            shutdown.wait_for_shutdown().await
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
        for line in ["--run --media", "--media --run", "/media --run", "--run /media"] {
            assert!(wants_media(&args(line)), "{line}");
            assert_eq!(mode_of(&args(line)), Mode::Daemon, "{line}");
        }
    }

    /// And the other way round: ordinary Windows must not fall into media mode
    /// because of a word that merely looks like the flag.
    #[test]
    fn nothing_else_turns_media_mode_on() {
        for line in ["", "--run", "--foreground", "/install", "--run media", "--mediaX"] {
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
    fn the_first_mode_word_wins() {
        assert_eq!(mode_of(&args("/install --run")), Mode::Install);
        assert_eq!(mode_of(&args("--run /install")), Mode::Daemon);
        assert_eq!(mode_of(&args("")), Mode::Service);
    }
}
