//! rumqttc's event loop, polled by a task that waits on nothing else (Store no-hang §14.3).
//!
//! rumqttc's request channel drains only while the event loop is polled. The subscriber session
//! waits on the publisher's queue and the request channel, and the publisher waits on the request
//! channel: a poller that did any of that work could wait on itself. So the pump polls, and
//! forwards what arrives on an unbounded queue; the session does the rest.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Packet, Publish};
use rumqttc::v5::{ConnectionError, Event, EventLoop, Request};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

/// Poll errors in a row before the pump gives up and the worker starts a new session; below it,
/// rumqttc reconnects on the next poll.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// What the pump hands the session.
pub enum PumpEvent {
    /// The broker accepted the connection: the first, or rumqttc's reconnect inside the same event
    /// loop. The broker keeps no session, so the session subscribes and announces itself again.
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

/// The forward queue's depth, and whether its high-water WARN is standing. Atomics only, so
/// neither the pump nor the session ever waits on it, and nothing is logged under a lock.
#[derive(Default)]
struct Backlog {
    depth: AtomicUsize,
    /// When the standing episode began, in milliseconds since `epoch` plus one; 0 when none
    /// stands.
    high_since: AtomicU64,
    epoch: Epoch,
}

/// The instant a `Backlog`'s stamps count from.
struct Epoch(Instant);

impl Default for Epoch {
    fn default() -> Epoch {
        Epoch(Instant::now())
    }
}

impl Backlog {
    fn stamp(&self) -> u64 {
        self.epoch.0.elapsed().as_millis() as u64 + 1
    }

    /// The pump is about to forward a publish.
    fn pushed(&self) {
        let depth = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
        if depth >= HIGH_WATER
            && self
                .high_since
                .compare_exchange(0, self.stamp(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            warn!(
                kind = "mqtt_backlog_high",
                depth,
                high_water = HIGH_WATER,
                "MQTT commands are arriving faster than the bridge handles them"
            );
        }
    }

    /// The session took a publish. Saturating: a count that went wrong must not panic the session.
    fn popped(&self) {
        let before = self
            .depth
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| {
                Some(d.saturating_sub(1))
            })
            .unwrap_or_default();
        let depth = before.saturating_sub(1);
        if depth <= LOW_WATER {
            self.close(depth, false);
        }
    }

    /// Whether a high-water episode is standing.
    #[cfg(test)]
    fn high(&self) -> bool {
        self.high_since.load(Ordering::SeqCst) != 0
    }

    /// End a standing high-water episode with its INFO: the queue drained, or its session ended.
    fn close(&self, depth: usize, session_ended: bool) {
        let since = self.high_since.swap(0, Ordering::SeqCst);
        if since != 0 {
            let lasted_ms = self.stamp().saturating_sub(since);
            info!(
                kind = "mqtt_backlog_drained",
                depth,
                lasted_ms,
                session_ended,
                "MQTT command backlog drained"
            );
        }
    }
}

/// How long a connection made during an outage must last before the outage is over (the fleet's
/// F2: recovery on proof of life, not on a bare connect). A broker that takes each connection and
/// loses it — a second instance with the same client id, a broker that drops what it accepts — is
/// then one outage with its WARN, not an INFO pair per connection. The pump sees an event at least
/// every keep-alive (5 s), so the end is said within one keep-alive of the window.
pub const STABLE_AFTER: Duration = Duration::from_secs(5);

/// The broker's reachability as the pumps see it. It outlives every session — the pump gives up
/// after `MAX_CONSECUTIVE_ERRORS` and the worker starts a new session — so an outage is one
/// episode however many attempts and sessions it takes (logging policy): an INFO when it starts,
/// one WARN once it has lasted `warn_after`, the attempts in between at DEBUG, and an INFO with its
/// length once a connection has lasted `STABLE_AFTER` — a connection lost sooner is one more failed
/// attempt of the same outage. Atomics only: one pump at a time uses it, and nothing is logged under
/// a lock.
pub struct BrokerOutage {
    warn_after: Duration,
    epoch: Instant,
    /// When the outage began, in milliseconds since `epoch` plus one; 0 while the broker is
    /// reachable.
    since: AtomicU64,
    /// When the broker accepted the connection that may end the outage, the same way; 0 when none
    /// is on trial.
    connected_at: AtomicU64,
    attempts: AtomicU32,
    warned: AtomicBool,
}

impl BrokerOutage {
    pub fn new(warn_after: Duration) -> BrokerOutage {
        BrokerOutage {
            warn_after,
            epoch: Instant::now(),
            since: AtomicU64::new(0),
            connected_at: AtomicU64::new(0),
            attempts: AtomicU32::new(0),
            warned: AtomicBool::new(false),
        }
    }

    fn stamp(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// A poll failed: the connection was lost, or a reconnect failed. A connection on trial lost
    /// is one more attempt of its outage.
    fn failed(&self, error: &ConnectionError) {
        let now = self.stamp();
        self.connected_at.store(0, Ordering::SeqCst);
        if self
            .since
            .compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.attempts.store(1, Ordering::SeqCst);
            self.warned.store(false, Ordering::SeqCst);
            info!(kind = "connection_lost", error = %error, "MQTT broker connection lost; reconnecting");
            return;
        }
        let attempts = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let down_for_ms = now.saturating_sub(self.since.load(Ordering::SeqCst));
        let past_threshold = down_for_ms >= self.warn_after.as_millis() as u64;
        if past_threshold && !self.warned.swap(true, Ordering::SeqCst) {
            warn!(
                kind = "external_failure",
                error = %error,
                attempts,
                down_for_ms,
                "MQTT broker unreachable; still reconnecting"
            );
        } else {
            debug!(error = %error, attempts, down_for_ms, "MQTT broker still unreachable");
        }
    }

    /// The broker accepted a connection. Outside an outage that is all; during one, the
    /// connection is on trial until it has lasted `STABLE_AFTER` (`alive`).
    fn connected(&self) {
        if self.since.load(Ordering::SeqCst) == 0 {
            info!("Connected to MQTT broker");
            return;
        }
        self.connected_at.store(self.stamp(), Ordering::SeqCst);
        debug!(
            attempts = self.attempts.load(Ordering::SeqCst),
            "MQTT broker accepted a connection; the outage ends once it lasts"
        );
    }

    /// The broker sent something on the current connection. Once a connection on trial has lasted
    /// `STABLE_AFTER`, the outage is over: an INFO with its length, up to that connection.
    fn alive(&self) {
        let connected_at = self.connected_at.load(Ordering::SeqCst);
        if connected_at == 0 || self.stamp().saturating_sub(connected_at) < STABLE_AFTER.as_millis() as u64 {
            return;
        }
        self.connected_at.store(0, Ordering::SeqCst);
        let since = self.since.swap(0, Ordering::SeqCst);
        if since == 0 {
            return;
        }
        info!(
            kind = "external_recovered",
            attempts = self.attempts.load(Ordering::SeqCst),
            down_for_ms = connected_at.saturating_sub(since),
            "MQTT broker reachable again"
        );
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
        // WAIT: mqtt-pump-queue
        let event = self.rx.recv().await;
        if matches!(event, Some(PumpEvent::Publish(_))) {
            self.backlog.popped();
        }
        event
    }
}

/// The session is over, and the next one gets a queue of its own: the commands still unread here
/// are dropped. Said once, with how many, and a standing high-water WARN is closed with its INFO
/// rather than left standing for good.
impl Drop for Incoming {
    fn drop(&mut self) {
        let depth = self.backlog.depth.load(Ordering::SeqCst);
        if depth > 0 {
            warn!(
                kind = "mqtt_commands_discarded",
                depth, "MQTT commands left unread when the session ended were discarded"
            );
        }
        self.backlog.close(depth, true);
    }
}

impl Pump {
    pub fn start(events: EventLoop, outage: Arc<BrokerOutage>) -> (Pump, Incoming) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::default());
        let task = tokio::spawn(Pump::run(events, tx, backlog.clone(), outage));
        (Pump { task }, Incoming { rx, backlog })
    }

    async fn run(
        mut events: EventLoop,
        tx: UnboundedSender<PumpEvent>,
        backlog: Arc<Backlog>,
        outage: Arc<BrokerOutage>,
    ) {
        let mut consecutive_errors: u32 = 0;
        loop {
            // WAIT: mqtt-poll
            let event = match events.poll().await {
                Ok(event) => event,
                Err(e) => {
                    // rumqttc 0.25's clean-up after a failed poll moves the in-flight publishes to
                    // `pending` but leaves a packet-id collision standing, and while one stands the
                    // event loop takes no request — waiting for a PUBACK that cannot come once the
                    // connection is gone. Move it with the rest (`MqttState::clean`).
                    if let Some(publish) = events.state.collision.take() {
                        events.pending.push_back(Request::Publish(publish));
                    }
                    outage.failed(&e);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        // Part of the outage's one episode: the worker starts a new session.
                        debug!(
                            error = %e,
                            consecutive_errors,
                            "MQTT poll failed repeatedly; ending the session"
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
                outage.alive();
            }

            let forward = match event {
                Event::Incoming(Packet::ConnAck(_)) => {
                    outage.connected();
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
