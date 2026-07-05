//! Connection from the GUI to the backup service (the orchestrator core).
//!
//! The GUI is a thin client: jobs, the run queue, activity and the scheduler all
//! live in `own-backup-service`, which is installed as an auto-start Windows
//! service (LocalSystem) / systemd unit (root). On a normal machine it is
//! already running when the GUI opens and we simply attach over gRPC (localhost
//! TCP). If it is installed but stopped we ask the OS to start it (best effort —
//! starting a service needs admin). We never launch the service as a child
//! process: the whole point is a single, system-managed, privileged service. If
//! it is not installed at all, the GUI warns the user to install it.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use own_backup_client::BackupClient;

pub struct ServiceLink {
    endpoint: String,
    /// Per-boot bearer token file written by the service (Windows). Read fresh on
    /// each connect so a token that appears after startup is picked up; empty
    /// when absent (dev/foreground TCP has no token).
    token_path: PathBuf,
    /// Cached client (cheap to clone; shares the HTTP/2 channel, auto-reconnects).
    client: AsyncMutex<Option<BackupClient>>,
}

impl ServiceLink {
    /// `grpc_address` is a bare `host:port` (as configured for the TCP bind); we
    /// address it over http. `token_path` is where the service writes its
    /// per-boot token.
    pub fn new(grpc_address: &str, token_path: PathBuf) -> Self {
        Self {
            endpoint: format!("http://{grpc_address}"),
            token_path,
            client: AsyncMutex::new(None),
        }
    }

    fn read_token(&self) -> String {
        std::fs::read_to_string(&self.token_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    async fn connect(&self) -> Result<BackupClient, String> {
        let token = self.read_token();
        let r = BackupClient::connect(self.endpoint.clone(), token.clone()).await;
        tracing::debug!(
            endpoint = %self.endpoint,
            token_len = token.len(),
            ok = r.is_ok(),
            "service connect attempt"
        );
        r
    }

    /// A connected client. Connects on first use; if the service is unreachable
    /// it nudges the OS to start the installed service and retries. Fails with a
    /// user-facing message (incl. an install hint) when it still can't connect.
    ///
    /// The `client` mutex is only ever held for the brief cache read/write — NEVER
    /// across a connect or sleep — so a concurrent caller (e.g. the startup health
    /// check) is never blocked behind this method's retry loop.
    pub async fn client(&self) -> Result<BackupClient, String> {
        if let Some(c) = self.cached().await {
            return Ok(c);
        }
        if let Ok(c) = self.connect().await {
            self.store(c.clone()).await;
            return Ok(c);
        }
        // Not reachable — try to start the installed service, then wait (~5s).
        // `sc`/`net` block, so run them off the async runtime.
        tracing::info!("service not reachable; attempting to start the installed service");
        let _ = tokio::task::spawn_blocking(start_installed_service).await;
        for _ in 0..25 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(c) = self.connect().await {
                self.store(c.clone()).await;
                return Ok(c);
            }
        }
        Err(tokio::task::spawn_blocking(unreachable_message)
            .await
            .unwrap_or_else(|_| "Служба резервного копирования недоступна.".into()))
    }

    async fn cached(&self) -> Option<BackupClient> {
        self.client.lock().await.as_ref().cloned()
    }

    async fn store(&self, c: BackupClient) {
        *self.client.lock().await = Some(c);
    }

    /// Drop the cached client so the next [`client`](Self::client) reconnects.
    pub async fn invalidate(&self) {
        *self.client.lock().await = None;
    }

    /// Quick probe: can we reach the service right now? Always does a fresh
    /// connect (never trusts a stale cached client) and updates the cache to
    /// match, so the startup health check reflects reality. Does not try to
    /// start the service.
    pub async fn is_reachable(&self) -> bool {
        match self.connect().await {
            Ok(c) => {
                *self.client.lock().await = Some(c);
                true
            }
            Err(_) => {
                *self.client.lock().await = None;
                false
            }
        }
    }
}

/// Message shown when the service can't be reached, tailored to whether it is
/// installed (stopped) or missing entirely.
fn unreachable_message() -> String {
    let name = own_backup_common::WINDOWS_SERVICE_DISPLAY_NAME;
    if service_installed() {
        format!(
            "Служба «{name}» установлена, но не запущена. Запустите её \
             (нужны права администратора) или перезагрузите компьютер."
        )
    } else {
        format!(
            "Служба «{name}» не установлена. Для работы программы установите её: \
             запустите own-backup-service.exe /install (появится запрос прав администратора)."
        )
    }
}

/// Whether the backup service is registered with the OS (regardless of running).
pub fn service_installed() -> bool {
    #[cfg(target_os = "windows")]
    {
        // `sc query <name>` exits 0 if the service exists, 1060 if it does not.
        run_hidden("sc", &["query", own_backup_common::WINDOWS_SERVICE_NAME])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::path::Path::new("/etc/systemd/system/own-backup.service").exists()
    }
}

/// Best-effort nudge to start the installed service. Starting a service needs
/// admin, so this only succeeds if the GUI is elevated or the service DACL
/// allows it; the auto-start service is normally already running anyway.
fn start_installed_service() {
    #[cfg(target_os = "windows")]
    {
        let _ = run_hidden("net", &["start", own_backup_common::WINDOWS_SERVICE_DISPLAY_NAME]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["start", "own-backup"])
            .status();
    }
}

/// Run a console helper (`sc` / `net`) without flashing a console window and
/// capturing its output (so nothing prints into the GUI).
#[cfg(target_os = "windows")]
fn run_hidden(program: &str, args: &[&str]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
}
