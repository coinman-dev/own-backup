use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use own_backup_core::busy::Busy;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ShutdownSignal {
    receiver: watch::Receiver<bool>,
}

pub struct ShutdownController {
    sender: watch::Sender<bool>,
}

pub fn create_shutdown_channel() -> (ShutdownController, ShutdownSignal) {
    let (sender, receiver) = watch::channel(false);
    (ShutdownController { sender }, ShutdownSignal { receiver })
}

impl ShutdownController {
    pub fn trigger_shutdown(&self) {
        info!("Shutdown signal sent");
        let _ = self.sender.send(true);
    }
}

impl ShutdownSignal {
    pub async fn wait_for_shutdown(&mut self) {
        while !*self.receiver.borrow_and_update() {
            if self.receiver.changed().await.is_err() {
                break;
            }
        }
    }

    pub fn is_shutdown_requested(&self) -> bool {
        *self.receiver.borrow()
    }
}

/// How long open connections get to close once the work has stopped.
///
/// An HTTP/2 connection closes gracefully only after the client answers a
/// ping. The Total Commander plugin drives its runtime during a call only, so
/// between calls it answers nothing, and a server waiting for it never stopped:
/// the installer hung on «Остановка службы» while TC was open (2026-10-28).
pub const CONNECTION_DRAIN: Duration = Duration::from_secs(3);

/// `served` — the gRPC server with its own graceful shutdown — until it ends,
/// or, once a stop is asked for and the work has finished, for
/// [`CONNECTION_DRAIN`] more; then the connections still open are dropped.
pub async fn until_stopped<E>(
    served: impl Future<Output = Result<(), E>>,
    mut shutdown: ShutdownSignal,
    busy: Arc<Busy>,
) -> Result<(), E> {
    tokio::pin!(served);
    tokio::select! {
        r = &mut served => return r,
        _ = async {
            shutdown.wait_for_shutdown().await;
            busy.wait_until_idle().await;
        } => {}
    }
    match tokio::time::timeout(CONNECTION_DRAIN, served).await {
        Ok(r) => r,
        Err(_) => {
            warn!("a client kept its connection open after the stop; closing it");
            Ok(())
        }
    }
}
