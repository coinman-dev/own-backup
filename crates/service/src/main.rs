mod app_state;
mod grpc_server;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let foreground = args.iter().any(|a| a == "--foreground" || a == "-f");

    #[cfg(target_os = "windows")]
    {
        if !foreground {
            info!("Starting as Windows Service...");
            windows_service::run_as_service()?;
            return Ok(());
        }
    }

    info!("Starting My Own Backup Service in foreground mode");
    let (shutdown_controller, shutdown_signal) = create_shutdown_channel();

    // Handle Ctrl+C to trigger graceful shutdown
    let ctrl_c_controller = shutdown_controller;
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        info!("Ctrl+C received, shutting down...");
        ctrl_c_controller.trigger_shutdown();
    });

    run_main_loop(shutdown_signal).await?;
    Ok(())
}

pub async fn run_main_loop(mut shutdown: ShutdownSignal) -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::default();

    std::fs::create_dir_all(&config.data_dir)?;
    let _conn = own_backup_metadata::open_database(&config.db_path())?;
    info!("Metadata database initialized");

    let state = AppState::new(config.clone());

    // On Unix: use Unix Domain Socket (no TCP, prevents local privilege escalation).
    // On Windows: Named Pipe (TODO), falls back to TCP localhost for now.
    // Dev/foreground mode: always uses TCP for easy debugging.
    let args: Vec<String> = std::env::args().collect();
    let use_tcp = args.iter().any(|a| a == "--foreground" || a == "-f" || a == "--tcp");

    #[cfg(unix)]
    if !use_tcp {
        return serve_uds(config, state, shutdown).await;
    }

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
