mod app_state;
mod grpc_server;
mod install;
mod scheduler;
mod shutdown;

#[cfg(target_os = "windows")]
mod windows_service;

use std::net::SocketAddr;

use tonic::transport::Server;
use tracing::info;

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
enum Mode {
    Install,
    Uninstall,
    /// Control the already-installed service (SCM on Windows / systemd on Linux).
    Start,
    Stop,
    Restart,
    Status,
    /// Console run for development, or systemd `ExecStart … --run`.
    Foreground,
    /// Launched by the SCM (Windows) with no args, or as a plain daemon.
    Service,
}

/// Accept both Windows-style (`/install`) and Unix-style (`--install`) flags on
/// every platform, so muscle memory works either way; the actual work is
/// platform-specific.
fn parse_mode(args: &[String]) -> Mode {
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
            "-f" | "--foreground" | "--run" | "run" => return Mode::Foreground,
            _ => {}
        }
    }
    Mode::Service
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Logs go to stderr so `own-backup-service.exe` (info to stdout) stays clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse_mode(&args);

    // The SCM control operations (register/remove/start/stop) require admin on
    // Windows. Self-elevate via UAC rather than failing with access-denied.
    #[cfg(target_os = "windows")]
    if matches!(
        mode,
        Mode::Install | Mode::Uninstall | Mode::Start | Mode::Stop | Mode::Restart
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
        // Foreground/dev uses TCP on localhost for easy debugging.
        Mode::Foreground => run_loop(true),
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
    println!("  --foreground  запустить в консоли для отладки (TCP localhost:50051)");
}

/// Build a tokio runtime and run the gRPC loop until shutdown. `use_tcp`
/// selects the transport (TCP for dev; UDS for a real Unix daemon).
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
        run_main_loop(signal, use_tcp).await
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
) -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::default();

    std::fs::create_dir_all(&config.data_dir)?;
    let _conn = own_backup_metadata::open_database(&config.db_path())?;
    info!("Metadata database initialized");

    // Build the orchestrator core: job store, serial run queue, cancel flag,
    // an event fan-out for progress streams, and the background worker that
    // actually drives backups.
    let store = own_backup_core::JobStore::load(&config.data_dir);
    let queue = own_backup_core::RunQueue::new();
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
        cancel.clone(),
        passwords.clone(),
        job_rx,
    ));
    info!("Backup orchestrator ready ({} job(s) loaded)", store.list().len());

    // The scheduler enqueues due jobs on a timer (stops on shutdown).
    scheduler::spawn(
        store.clone(),
        queue.clone(),
        job_tx.clone(),
        passwords.clone(),
        shutdown.clone(),
    );

    let state = AppState::new(config.clone(), store, queue, job_tx, cancel, events, passwords);

    // Production transport: Unix Domain Socket on Linux (no TCP, prevents local
    // privilege escalation). Dev/foreground uses TCP on localhost. Windows
    // named-pipe transport is a TODO; it falls back to TCP for now.
    #[cfg(unix)]
    if !use_tcp {
        return serve_uds(config, state, shutdown).await;
    }
    #[cfg(not(unix))]
    let _ = use_tcp;

    // TCP on loopback. On Windows this is the production transport (until a
    // named pipe lands): require a per-boot bearer token stored in an ACL'd file
    // that only the service, admins and interactive users can read. On other
    // platforms the TCP path is dev/foreground only and stays token-less (the
    // real Unix transport is the UDS above).
    let addr: SocketAddr = config.grpc_address.parse()?;
    #[cfg(windows)]
    let token = write_service_token(&config).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not write service token — running WITHOUT auth");
        String::new()
    });
    #[cfg(not(windows))]
    let token = String::new();
    info!(
        "gRPC server (TCP/localhost) listening on {} (auth: {})",
        addr,
        if token.is_empty() { "off" } else { "token" }
    );
    let service = BackupServiceImpl::new(state, token).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_shutdown(addr, async move {
            shutdown.wait_for_shutdown().await;
        })
        .await?;

    #[cfg(windows)]
    let _ = std::fs::remove_file(config.token_path());
    Ok(())
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
    std::fs::write(&path, token.as_bytes())?;

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

/// Serve gRPC over Unix Domain Socket.
///
/// Socket: /run/own-backup/service.sock, permissions 0o660.
/// This prevents other local users (non-root, not in "own-backup" group)
/// from connecting to the service and issuing privileged operations.
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

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove stale socket from previous run
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let uds = UnixListener::bind(&socket_path)?;

    // 0o660: owner (root) and group (own-backup) can read/write; others cannot.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;

    // Generate per-boot service authentication token
    let token = generate_and_write_token(&config)?;

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
