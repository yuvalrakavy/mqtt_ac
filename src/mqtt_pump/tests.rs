//! The pump on its own: its overload flag, what it says when a session ends with commands unread,
//! and the event loop it keeps alive across a reconnect.

use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::{FakeBroker, QoS};
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::{AsyncClient, MqttOptions};

use super::{Backlog, BrokerOutage, Incoming, Pump, PumpEvent, HIGH_WATER, LOW_WATER};
use crate::test_support::{Capture, Relay};

/// The owner's overload ruling (no-hang §14.6): the forward queue drops nothing; past HIGH_WATER
/// unread commands it raises its WARN once, and clears it back under LOW_WATER.
#[test]
fn a_backlog_past_high_water_is_flagged_once_and_cleared_when_it_drains() {
    let log = Capture::start();
    let backlog = Backlog::default();
    for _ in 0..HIGH_WATER - 1 {
        backlog.pushed();
    }
    assert!(!backlog.high(), "flagged below the high-water mark");
    backlog.pushed();
    assert!(backlog.high(), "not flagged at the high-water mark");
    backlog.pushed();
    assert_eq!(log.of_kind("mqtt_backlog_high").len(), 1, "flagged again while it stood");
    while backlog.depth.load(std::sync::atomic::Ordering::SeqCst) > LOW_WATER + 1 {
        backlog.popped();
    }
    assert!(backlog.high(), "cleared above the low-water mark");
    backlog.popped();
    assert!(!backlog.high(), "not cleared at the low-water mark");
    assert_eq!(log.of_kind("mqtt_backlog_drained").len(), 1, "the drain was not said once");
}

/// rumqttc 0.25 stops taking requests while a packet-id collision stands (a QoS 1 publish whose
/// id is still in flight), until that id's PUBACK arrives. A connection lost meanwhile is cleaned
/// up without the collision: the in-flight publishes are dropped by the reconnect (the broker
/// keeps no session), so that PUBACK never comes, and the event loop never takes another request —
/// no publish, no subscription, for good, while it goes on reconnecting. The bridge makes the
/// collision whenever a slow broker meets its announcement: the subscription takes a packet id
/// between two publishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_collision_standing_when_the_connection_drops_does_not_stop_the_event_loop_for_good() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    let relay = Relay::start(broker.address()).await;
    let (host, port) = relay.address.rsplit_once(':').expect("the relay's host and port");
    let mut options = MqttOptions::new("collision", host, port.parse::<u16>().expect("the relay's port"));
    options.set_keep_alive(Duration::from_secs(5));
    let (client, events) = AsyncClient::new(options, 10);

    // Queued before the first poll, so the event loop takes them back to back once connected:
    // `a` takes packet id 1 (its ack held), the subscription 2, and `b` wraps to 1 — a collision,
    // standing while `a`'s ack is held.
    broker.hold_acks();
    client.try_publish("t/a", QoS::AtLeastOnce, false, "a").expect("queue a");
    client.try_subscribe("t/s", QoS::AtLeastOnce).expect("queue the subscription");
    client.try_publish("t/b", QoS::AtLeastOnce, false, "b").expect("queue b");
    let outage = Arc::new(BrokerOutage::new(Duration::from_secs(30)));
    let (_pump, _incoming) = Pump::start(events, outage);
    assert!(broker.wait_for_subscription("t/s", Duration::from_secs(10)).await, "the client never subscribed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        broker.received_on("t/b").is_empty(),
        "`b` was sent, so no collision stood and this test proves nothing"
    );

    relay.cut();
    assert!(
        broker.wait_until(Duration::from_secs(5), |b| b.connections() >= 2).await,
        "the pump never reconnected"
    );
    broker.release_acks();
    client.try_publish("t/c", QoS::AtLeastOnce, false, "c").expect("queue c");
    let sent = broker.wait_until(Duration::from_secs(10), |b| !b.received_on("t/c").is_empty()).await;
    assert!(
        sent,
        "nothing was sent after the reconnect: the collision standing at the drop outlived it, and the event loop \
         takes no request while one stands"
    );
}

/// A session that ends with commands still in the pump's queue drops them — the next session
/// has a queue of its own. One WARN says how many (`mqtt_commands_discarded`), and a high-water
/// WARN standing for that queue is closed with its INFO, instead of standing for good.
#[test]
fn commands_left_unread_when_the_session_ends_are_reported() {
    let log = Capture::start();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let backlog = Arc::new(Backlog::default());
    for _ in 0..HIGH_WATER {
        backlog.pushed();
        let publish = Publish::new("t", QoS::AtLeastOnce, "x", None);
        assert!(tx.send(PumpEvent::Publish(publish)).is_ok());
    }
    drop(Incoming { rx, backlog });

    let discarded = log.of_kind("mqtt_commands_discarded");
    assert!(
        discarded.len() == 1
            && discarded[0].level == tracing::Level::WARN
            && discarded[0].field("depth") == Some("1000"),
        "the commands left in the queue when the session ended were dropped silently; the log: {:#?}",
        log.records()
    );
    let drained = log.of_kind("mqtt_backlog_drained");
    assert!(
        drained.len() == 1,
        "the high-water WARN standing for the session's queue was never closed; the log: {:#?}",
        log.records()
    );
}

/// A session that ends with nothing unread says nothing.
#[test]
fn a_session_that_ends_with_nothing_unread_is_silent() {
    let log = Capture::start();
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(Incoming {
        rx,
        backlog: Arc::new(Backlog::default()),
    });
    assert!(
        log.records().is_empty(),
        "a session that ended with its queue empty was logged: {:#?}",
        log.records()
    );
}
