//! Connection from the GUI to the backup service (the orchestrator core).
//!
//! The GUI is a thin client: jobs, the run queue, activity and the scheduler all
//! live in `own-backup-service`. This link connects to that service over gRPC
//! (localhost TCP). When nothing is listening — e.g. the app is run without
//! installing the Windows service / systemd unit — it spawns a private
//! foreground instance so the GUI still works, at the cost of the scheduler only
//! running while the GUI is open. On a normal install the auto-start service is
//! already running as LocalSystem/root and we simply attach to it.

use std::process::Child;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use own_backup_client::BackupClient;

pub struct ServiceLink {
    endpoint: String,
    token: String,
    /// Cached client (cheap to clone; shares the HTTP/2 channel, auto-reconnects).
    client: AsyncMutex<Option<BackupClient>>,
    /// A foreground service we spawned ourselves, kept alive for our lifetime.
    child: StdMutex<Option<Child>>,
}

impl ServiceLink {
    /// `grpc_address` is a bare `host:port` (as configured for the TCP bind); we
    /// address it over http. Token is empty on the TCP path (dev / Windows).
    pub fn new(grpc_address: &str, token: impl Into<String>) -> Self {
        Self {
            endpoint: format!("http://{grpc_address}"),
            token: token.into(),
            client: AsyncMutex::new(None),
            child: StdMutex::new(None),
        }
    }

    /// A connected client. Connects on first use, spawning a foreground service
    /// if the endpoint is unreachable, then polls until it accepts.
    pub async fn client(&self) -> Result<BackupClient, String> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        if let Ok(c) = BackupClient::connect(self.endpoint.clone(), self.token.clone()).await {
            *guard = Some(c.clone());
            return Ok(c);
        }
        // Nothing listening — start our own and wait for it to accept (~6s).
        self.spawn_service()?;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(c) = BackupClient::connect(self.endpoint.clone(), self.token.clone()).await {
                *guard = Some(c.clone());
                return Ok(c);
            }
        }
        Err("Не удалось подключиться к службе резервного копирования (localhost:50051).".into())
    }

    /// Drop the cached client so the next [`client`](Self::client) reconnects
    /// (and re-spawns the service if it has since exited).
    pub async fn invalidate(&self) {
        *self.client.lock().await = None;
    }

    /// Spawn `own-backup-service --foreground` next to our exe, at most once
    /// (re-spawns only if a previously spawned child has exited).
    fn spawn_service(&self) -> Result<(), String> {
        let mut child = self.child.lock().unwrap();
        if let Some(existing) = child.as_mut() {
            if matches!(existing.try_wait(), Ok(None)) {
                return Ok(()); // still running
            }
        }
        let exe = service_exe_path()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--foreground");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let spawned = cmd
            .spawn()
            .map_err(|e| format!("не удалось запустить службу {}: {e}", exe.display()))?;
        tracing::info!(exe = %exe.display(), "spawned foreground backup service");
        *child = Some(spawned);
        Ok(())
    }
}

/// `own-backup-service[.exe]` next to the running GUI executable.
fn service_exe_path() -> Result<std::path::PathBuf, String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("у исполняемого файла нет родительского каталога")?
        .to_path_buf();
    let name = if cfg!(windows) {
        "own-backup-service.exe"
    } else {
        "own-backup-service"
    };
    let path = dir.join(name);
    if !path.exists() {
        return Err(format!(
            "не найден исполняемый файл службы: {}",
            path.display()
        ));
    }
    Ok(path)
}
