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
/// Windows service dispatcher.
#[allow(dead_code)] // some are per-platform; the others' cfg peers still read them
pub const SERVICE_NAME: &str = "MyOwnBackup";
#[allow(dead_code)]
pub const SERVICE_DISPLAY_NAME: &str = "My Own Backup";
#[allow(dead_code)]
pub const SERVICE_DESCRIPTION: &str =
    "Block-level backup engine for My Own Backup: runs scheduled backups and serves the GUI/CLI.";
/// systemd unit base name on Linux (`own-backup.service`).
#[allow(dead_code)]
pub const LINUX_UNIT_NAME: &str = "own-backup";

/// How the binary was invoked.
enum Mode {
    Install,
    Uninstall,
    /// Console run for development, or systemd `ExecStart … --run`.
    Foreground,
    /// Launched by the SCM (Windows) with no args, or as a plain daemon.
    Service,
}

/// Accept both Windows-style (`/install`) and Unix-style (`--install`) flags on
/// every platform, so muscle memory works either way; the actual install path
/// is platform-specific.
fn parse_mode(args: &[String]) -> Mode {
    for a in args {
        match a.as_str() {
            "/install" | "--install" | "install" => return Mode::Install,
            "/uninstall" | "--uninstall" | "uninstall" | "/remove" | "--remove" => {
                return Mode::Uninstall
            }
            "-f" | "--foreground" | "--run" | "run" => return Mode::Foreground,
            _ => {}
        }
    }
    Mode::Service
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_mode(&args) {
        Mode::Install => install::install(),
        Mode::Uninstall => install::uninstall(),
        // Foreground/dev uses TCP on localhost for easy debugging.
        Mode::Foreground => run_loop(true),
        Mode::Service => run_service_mode(),
    }
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

/// Default entry when no mode flag is given: the SCM launches us this way on
/// Windows; on Unix a systemd unit runs us in the foreground with `--run`, so
/// this bare form (a plain daemon over UDS) is mostly a fallback.
fn run_service_mode() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        info!("Starting as Windows Service...");
        windows_service::run_as_service()?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        run_loop(false)
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

    // TCP fallback (dev mode / Windows placeholder)
    let addr: SocketAddr = config.grpc_address.parse()?;
    info!("gRPC server (TCP/localhost) listening on {}", addr);
    let service = BackupServiceImpl::new(state, String::new()).into_service();
    Server::builder()
        .add_service(service)
        .serve_with_shutdown(addr, async move {
            shutdown.wait_for_shutdown().await;
        })
        .await?;

    Ok(())
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
