//! The publisher on its own, on clients whose event loops are never polled: what it sends is read
//! back out of each client's request channel.

use std::time::Duration;

use rumqttc::v5::{AsyncClient, EventLoop, MqttOptions, Request};
use tokio::sync::Notify;

use super::MqttPublisher;
use crate::ac_unit::{FanSpeed, OperationMode, UnitState};
use crate::messages::ToMqttPublisherMessage;
use crate::reports::Reports;

const NAME: &str = "Publisher";

/// A client that takes `cap` requests and then waits for good: nothing polls its event loop.
fn client(cap: usize) -> (AsyncClient, EventLoop) {
    AsyncClient::new(MqttOptions::new("publisher-test", "127.0.0.1", 1), cap)
}

fn state(unit: &str, temperature: f32) -> UnitState {
    UnitState {
        unit: unit.to_owned(),
        power: true,
        target_temperature: 22.0,
        temperature,
        fan_speed: FanSpeed::High,
        operation_mode: OperationMode::Cool,
        failure_code: None,
        filter_change: false,
        demand: false,
    }
}

/// The unit states a client's request channel holds, in order: (unit, room temperature).
fn unit_states(events: &mut EventLoop) -> Vec<(String, f32)> {
    events.clean();
    let prefix = format!("Aircondition/State/{NAME}/");
    events
        .pending
        .drain(..)
        .filter_map(|request| match request {
            Request::Publish(publish) => {
                let topic = String::from_utf8_lossy(&publish.topic).into_owned();
                let unit = topic.strip_prefix(&prefix)?.to_owned();
                let value: serde_json::Value = serde_json::from_slice(&publish.payload).ok()?;
                Some((unit, value["temperature"].as_f64()? as f32))
            }
            _ => None,
        })
        .collect()
}

/// A report whose publishing is cut short — its MQTT session ended while a publish waited on the
/// request channel — is still what the publisher knows (re-review C2): the next session, after its
/// CONNACK, publishes it again. Folded into the publisher's state only as each publish is taken,
/// the units not yet taken keep their older values, and the reconnect republishes those: the
/// retained state is stale until the unit changes again.
#[tokio::test]
async fn a_report_cut_short_by_the_end_of_its_session_is_published_again_after_the_reconnect() {
    let (_session_errors, rx) = async_channel::bounded::<ToMqttPublisherMessage>(10);
    let reports = Reports::new();
    let mut publisher = MqttPublisher::new(NAME.to_owned(), reports.clone(), rx);
    let reconnected = Notify::new();

    // The first report is published; the second is taken, but its client takes only the first
    // unit's publish: the second unit's waits for good, and the session ends there.
    let (first, _events) = client(2);
    reports.observed([state("L1.001", 20.0), state("L1.002", 20.0)]);
    let ended = tokio::time::timeout(
        Duration::from_millis(300),
        publisher.session(&first, &reconnected),
    )
    .await;
    assert!(
        ended.is_err(),
        "the first session ended by itself, so this test proves nothing"
    );
    let (cut_short, _events) = client(1);
    reports.observed([state("L1.001", 21.0), state("L1.002", 21.0)]);
    let ended = tokio::time::timeout(
        Duration::from_millis(300),
        publisher.session(&cut_short, &reconnected),
    )
    .await;
    assert!(
        ended.is_err(),
        "the second session ended by itself, so this test proves nothing"
    );

    // The next session: told of its CONNACK, it publishes everything it knows again.
    let (next, mut events) = client(100);
    reconnected.notify_one();
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        publisher.session(&next, &reconnected),
    )
    .await;
    let republished = unit_states(&mut events);
    assert!(
        republished.contains(&("L1.002".to_owned(), 21.0)),
        "after the reconnect the publisher published an older state than the last it was given (L1.002 at 21): \
         {republished:?}"
    );
}
