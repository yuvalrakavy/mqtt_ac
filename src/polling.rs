use tokio::{sync::mpsc::Sender, time::Duration};
use crate::messages::ToCoolmasterMessage;

use log::{debug};

pub async fn polling_worker(
    poll_period: Duration,
    to_coolmaster_channel: Sender<ToCoolmasterMessage>
) {
    loop {
        debug!("Polling coolmaster");
        let message = ToCoolmasterMessage::PublishUnitsState;
        to_coolmaster_channel.send(message).await.unwrap();
        tokio::time::sleep(poll_period).await;
    }
}
