use tokio::time::Duration;
use async_channel::Sender;
use crate::messages::ToCoolmasterMessage;

use log::{debug, error};

pub async fn polling_worker(
    poll_period: Duration,
    to_coolmaster_channel: Sender<ToCoolmasterMessage>
) {
    loop {
        debug!("Polling coolmaster");
        let message = ToCoolmasterMessage::PublishUnitsState;
        if to_coolmaster_channel.send(message).await.is_err() {
            error!("Coolmaster channel closed, exiting polling worker");
            return;
        }
        tokio::time::sleep(poll_period).await;
    }
}
