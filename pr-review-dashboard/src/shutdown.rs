// Licensed under the MIT License.

use tokio::sync::watch;

use crate::error::AppError;

#[derive(Debug)]
pub(crate) struct ShutdownTrigger {
    sender: watch::Sender<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct ShutdownListener {
    receiver: watch::Receiver<bool>,
}

pub(crate) fn channel() -> (ShutdownTrigger, ShutdownListener) {
    let (sender, receiver) = watch::channel(false);
    (ShutdownTrigger { sender }, ShutdownListener { receiver })
}

pub(crate) async fn wait_for_signal() -> Result<(), AppError> {
    tokio::signal::ctrl_c().await.map_err(AppError::caused_by)
}

impl ShutdownTrigger {
    pub(crate) fn trigger(self) {
        self.sender.send_replace(true);
    }
}

impl ShutdownListener {
    pub(crate) async fn cancelled(mut self) {
        if *self.receiver.borrow() {
            return;
        }

        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::channel;

    #[tokio::test]
    async fn trigger_releases_listener() {
        let (trigger, listener) = channel();

        trigger.trigger();
        listener.cancelled().await;
    }
}
