use tokio::sync::watch;
use tracing::info;

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
