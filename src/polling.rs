use tokio::time::Duration;
use std::sync::Arc;
use crate::{mailbox::Mailbox, messages::ToCoolmasterMessage};

use tracing::debug;

/// Asks for every unit's state each period. A post never waits; while the Coolmaster is down the
/// read is dropped, and its return publishes every unit's state anyway.
pub async fn polling_worker(poll_period: Duration, mailbox: Arc<Mailbox>) {
    loop {
        debug!("Polling coolmaster");
        mailbox.post(ToCoolmasterMessage::PublishUnitsState);
        tokio::time::sleep(poll_period).await;
    }
}
