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
use own_backup_common::tr;

pub struct ServiceLink {
    endpoint: String,
    /// Per-boot bearer token file written by the service (Windows). Read fresh on
    /// each connect so a token that appears after startup is picked up; empty
    /// when absent (dev/foreground TCP has no token).
    token_path: PathBuf,
    /// Cached client (cheap to clone; shares the HTTP/2 channel, auto-reconnects)
    /// together with the token it was built with.
    ///
    /// The token is kept beside it because the service mints a NEW one every
    /// time it starts, and a client holding the previous one is refused by
    /// every call — "Invalid or missing token", with the service plainly
    /// running and reachable. On the boot medium that is not a rare event: the
    /// Start menu's «Резервное копирование» used to bring up a second engine,
    /// which rewrote the token under the interface that was already open.
    client: AsyncMutex<Option<(String, BackupClient)>>,
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
        if token.is_empty() && !self.endpoint.is_empty() {
            // Reachable and useless. Every call through such a client comes
            // back "Invalid or missing token", and the interface shows that
            // instead of its contents — which is what the medium did when the
            // interface started in the second the service was still writing
            // the file. Counted as "not up yet", so the caller waits for it.
            tracing::debug!(endpoint = %self.endpoint, "no service token yet");
            return Err(tr!("service_link.no_token_yet").into_owned());
        }
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
            .unwrap_or_else(|_| tr!("service_link.unavailable").into_owned()))
    }

    /// A client if the service answers right now — the cached one or one
    /// fresh attempt — without starting it and waiting the way
    /// [`client`](Self::client) does. For reads that have an answer of their
    /// own to fall back on, where five seconds of «Загрузка…» buy nothing.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub async fn connected(&self) -> Option<BackupClient> {
        if let Some(c) = self.cached().await {
            return Some(c);
        }
        let c = self.connect().await.ok()?;
        self.store(c.clone()).await;
        Some(c)
    }

    /// The cached client, if it is still holding the token the service is
    /// currently answering to.
    ///
    /// Checked on every use rather than on failure, because a stale token does
    /// not fail the connection — it fails each CALL, one screen at a time, and
    /// the message it gives blames the token rather than the restart behind it.
    /// The file is a few dozen bytes on a RAM disk; reading it is nothing.
    async fn cached(&self) -> Option<BackupClient> {
        let held = self.client.lock().await;
        let (used, client) = held.as_ref()?;
        let now = self.read_token();
        // Unreadable is not a reason to throw away a working connection — the
        // file could be mid-rename. A DIFFERENT one is: the service restarted.
        if !now.is_empty() && now != *used {
            tracing::info!("the service minted a new token - reconnecting");
            return None;
        }
        Some(client.clone())
    }

    async fn store(&self, c: BackupClient) {
        *self.client.lock().await = Some((self.read_token(), c));
    }

    /// Drop the cached client so the next [`client`](Self::client) reconnects.
    pub async fn invalidate(&self) {
        *self.client.lock().await = None;
    }

    /// Whether the service, reachable, turns this caller away: the Unix
    /// socket's peer check answers `PermissionDenied` to anyone outside an
    /// active local session (an SSH login, a session switched away from).
    /// The connection itself succeeds then — the check is made per call — so
    /// only a call can tell; the queue is the cheapest one to ask.
    #[cfg(unix)]
    pub async fn access_refused(&self) -> bool {
        let Some(mut client) = self.cached().await else {
            return false;
        };
        matches!(
            client.get_queue_with_status().await,
            Err(status) if status.code() == tonic::Code::PermissionDenied
        )
    }

    /// Quick probe: can we reach the service right now? Always does a fresh
    /// connect (never trusts a stale cached client) and updates the cache to
    /// match, so the startup health check reflects reality. Does not try to
    /// start the service.
    pub async fn is_reachable(&self) -> bool {
        match self.connect().await {
            Ok(c) => {
                self.store(c).await;
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
    // On the medium "install it" is not advice anyone can follow.
    if own_backup_common::media_mode::is_media_mode() {
        return tr!("service_link.media_engine_silent", name = name).into_owned();
    }
    #[cfg(target_os = "windows")]
    let (start_hint, install_hint) = (
        tr!("service_link.win_start_hint"),
        tr!("service_link.win_install_hint"),
    );
    #[cfg(not(target_os = "windows"))]
    let (start_hint, install_hint) = (
        tr!("service_link.linux_start_hint"),
        tr!(
            "service_link.linux_install_hint",
            group = own_backup_common::UNIX_ACCESS_GROUP
        ),
    );
    if service_installed() {
        tr!(
            "service_link.installed_not_running",
            name = name,
            hint = start_hint
        )
        .into_owned()
    } else {
        tr!(
            "service_link.not_installed",
            name = name,
            hint = install_hint
        )
        .into_owned()
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
        // The package puts the unit in /usr/lib; an administrator's copy in
        // /etc overrides it.
        let name = own_backup_common::LINUX_UNIT_NAME;
        [
            format!("/etc/systemd/system/{name}.service"),
            format!("/usr/lib/systemd/system/{name}.service"),
            format!("/lib/systemd/system/{name}.service"),
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists())
    }
}

/// Best-effort nudge to start the installed service. Starting a service needs
/// admin, so this only succeeds if the GUI is elevated or the service DACL
/// allows it; the auto-start service is normally already running anyway.
fn start_installed_service() {
    // On the boot medium there is no installed service to start — asking the
    // SCM would just fail. Re-launch the engine as a process instead, the same
    // way the medium's launcher does, so losing it (a crash, a stray Ctrl+C in
    // the console it shares) is recoverable from the interface.
    if own_backup_common::media_mode::is_media_mode() {
        start_media_engine();
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = run_hidden(
            "net",
            &["start", own_backup_common::WINDOWS_SERVICE_DISPLAY_NAME],
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Deliberately NON-interactive: `systemctl start` of a system unit from
        // an unprivileged GUI makes polkit pop a password dialog — exactly what
        // a thin client must never do. `--no-ask-password` suppresses it; if the
        // caller lacks rights the start simply fails and unreachable_message()
        // tells the user to start it. The unit is `enable --now`, so it is
        // normally already running anyway.
        let _ = std::process::Command::new("systemctl")
            .args([
                "--no-ask-password",
                "start",
                own_backup_common::LINUX_UNIT_NAME,
            ])
            .status();
    }
}

/// Start the engine as a plain process from next to this exe — the boot
/// medium's equivalent of "start the service". Nothing is installed and nothing
/// is elevated: on the medium everything already runs as SYSTEM.
fn start_media_engine() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    else {
        return;
    };
    let exe = dir.join(if cfg!(windows) {
        "own-backup-service.exe"
    } else {
        "own-backup-service"
    });
    if !exe.exists() {
        tracing::warn!(path = %exe.display(), "engine binary not found next to the interface");
        return;
    }
    tracing::info!(path = %exe.display(), "boot medium: starting the engine as a process");
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["--run", "--media"]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(child) => tracing::info!(pid = child.id(), "engine started"),
        Err(e) => tracing::warn!(error = %e, "could not start the engine"),
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
