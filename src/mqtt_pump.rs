//! rumqttc's event loop, polled by a task that waits on nothing else (Store no-hang §14.3).
//!
//! rumqttc's request channel drains only while the event loop is polled. The subscriber session
//! waits on the Coolmaster worker's queue, the publisher's queue and the request channel, and the
//! first two wait on the publisher, which waits on the request channel: a poller that did any of
//! that work could wait on itself. So the pump polls, and forwards what arrives on an unbounded
//! queue; the session does the rest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Packet, Publish};
use rumqttc::v5::{Event, EventLoop, Request};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

/// Poll errors in a row before the pump gives up and the worker starts a new session; below it,
/// rumqttc reconnects on the next poll.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// What the pump hands the session.
pub enum PumpEvent {
    /// The broker accepted the connection: the first, or rumqttc's reconnect inside the same event
    /// loop. The broker keeps no session, so the session announces itself and subscribes again.
    Connected,
    Publish(Publish),
    /// The connection failed `MAX_CONSECUTIVE_ERRORS` times in a row; the session ends and the
    /// worker reconnects.
    Ended(String),
}

/// A forward queue past this many unread publishes is a WARN, once; back under `LOW_WATER`, an
/// INFO with how long it lasted. Nothing is dropped (Store no-hang §14.6, ruling 1).
const HIGH_WATER: usize = 1000;
const LOW_WATER: usize = 100;

/// The forward queue's depth, and whether its high-water WARN is standing.
#[derive(Default)]
struct Backlog {
    depth: AtomicUsize,
    high_since: Mutex<Option<Instant>>,
}

impl Backlog {
    fn pushed(&self) {
        let depth = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
        if depth >= HIGH_WATER {
            let mut high = self.high_since.lock().unwrap_or_else(|p| p.into_inner()); // WAIT: mqtt-backlog-lock
            if high.is_none() {
                *high = Some(Instant::now());
                warn!(kind = "mqtt_backlog_high", depth, "MQTT commands are arriving faster than the bridge handles them");
            }
        }
    }

    fn popped(&self) {
        let depth = self.depth.fetch_sub(1, Ordering::SeqCst) - 1;
        if depth <= LOW_WATER {
            let mut high = self.high_since.lock().unwrap_or_else(|p| p.into_inner()); // WAIT: mqtt-backlog-lock
            if let Some(since) = high.take() {
                info!(kind = "mqtt_backlog_drained", depth, lasted_ms = since.elapsed().as_millis() as u64, "MQTT command backlog drained");
            }
        }
    }
}

/// Polls rumqttc's event loop in a task of its own. It is given no client, so it cannot publish
/// or subscribe, and its only send is to an unbounded queue: it waits on nothing in this process.
/// Dropping it stops the task, and with it the event loop, so a publish still waiting on the
/// request channel fails instead of waiting for good.
pub struct Pump {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The session's end of the pump's queue.
pub struct Incoming {
    rx: UnboundedReceiver<PumpEvent>,
    backlog: Arc<Backlog>,
}

impl Incoming {
    pub async fn recv(&mut self) -> Option<PumpEvent> {
        let event = self.rx.recv().await; // WAIT: mqtt-pump-queue
        if matches!(event, Some(PumpEvent::Publish(_))) {
            self.backlog.popped();
        }
        event
    }
}

impl Pump {
    pub fn start(events: EventLoop) -> (Pump, Incoming) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::default());
        let task = tokio::spawn(Pump::run(events, tx, backlog.clone()));
        (Pump { task }, Incoming { rx, backlog })
    }

    async fn run(mut events: EventLoop, tx: UnboundedSender<PumpEvent>, backlog: Arc<Backlog>) {
        let mut consecutive_errors: u32 = 0;
        loop {
            let event = match events.poll().await { // WAIT: mqtt-poll
                Ok(event) => event,
                Err(e) => {
                    // rumqttc 0.25's clean-up after a failed poll moves the in-flight publishes to
                    // `pending` but leaves a packet-id collision standing, and while one stands the
                    // event loop takes no request — waiting for a PUBACK that cannot come once the
                    // connection is gone. Move it with the rest (`MqttState::clean`).
                    if let Some(publish) = events.state.collision.take() {
                        events.pending.push_back(Request::Publish(publish));
                    }
                    consecutive_errors += 1;
                    // Per-iteration poll error is INFO (transient, recovers); the threshold
                    // cross is the WARN that signals a persistent outage.
                    if consecutive_errors < MAX_CONSECUTIVE_ERRORS {
                        info!(
                            kind = "external_failure",
                            error = %e,
                            consecutive_errors,
                            max = MAX_CONSECUTIVE_ERRORS,
                            "MQTT poll error"
                        );
                    } else {
                        warn!(
                            kind = "external_failure",
                            error = %e,
                            consecutive_errors,
                            max = MAX_CONSECUTIVE_ERRORS,
                            "MQTT poll error threshold reached, giving up"
                        );
                        let _ = tx.send(PumpEvent::Ended(e.to_string()));
                        return;
                    }
                    // rumqttc reconnects on the next poll.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            // Only reset consecutive_errors on Incoming events (proof of broker
            // connectivity). Outgoing events (e.g. ConnectRequest) are emitted by
            // rumqttc during reconnection attempts and must NOT reset the counter.
            if matches!(event, Event::Incoming(_)) {
                consecutive_errors = 0;
            }

            let forward = match event {
                Event::Incoming(Packet::ConnAck(_)) => {
                    info!(kind = "external_recovered", "Connected to MQTT broker, setting up session");
                    PumpEvent::Connected
                }
                Event::Incoming(Packet::Publish(publish)) => {
                    backlog.pushed();
                    PumpEvent::Publish(publish)
                }
                _ => continue,
            };
            if tx.send(forward).is_err() {
                return; // the session is gone
            }
        }
    }
}

#[cfg(test)]
mod tests;
