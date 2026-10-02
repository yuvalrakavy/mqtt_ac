//! The bridge under saturation (Store no-hang §14.3, mqtt_ac): the task that polls rumqttc's event
//! loop must never wait — itself, or through a bounded queue — on rumqttc's request channel, which
//! only polling drains. These drive the whole service: the real workers, a fake broker that can
//! withhold its acknowledgements, and a stand-in Coolmaster on a local socket that can be taken
//! down. With the Coolmaster down, the MQTT session must never wait on it either (the owner's
//! device-down policy, 2026-10-02): commands go to a mailbox that never waits, the latest value per
//! unit and property waits for the Coolmaster's return, and a momentary command is refused.

use std::time::Duration;

use mqtt_test_broker::FakeBroker;

use super::{broker_host_port, Service, ServiceConfig, Started, Timing};
use crate::test_support::{Capture, FakeCoolmaster, Relay, UNITS};

const NAME: &str = "Saturation";
const UNIT: &str = UNITS[0];
const COMMANDS: usize = 300;

fn command_topic() -> String {
    format!("Aircondition/Command/{NAME}")
}

fn start_service(broker: &str, coolmaster: &str) -> Service<Started> {
    start_service_with(broker, coolmaster, Timing::default())
}

fn start_service_with(broker: &str, coolmaster: &str, timing: Timing) -> Service<Started> {
    Service::new(ServiceConfig {
        controller_name: NAME.to_owned(),
        mqtt_broker_address: broker.to_owned(),
        coolmaster_address: coolmaster.to_owned(),
        // One listing at start; the tests make the rest.
        polling_period: Duration::from_secs(3600),
        timing,
    })
    .start()
}

/// Stopping aborts the workers, so it ends even when they are deadlocked — bounded anyway, so a
/// regression here cannot hold the test suite.
async fn stop_service(service: Service<Started>) {
    assert!(
        tokio::time::timeout(Duration::from_secs(10), service.stop()).await.is_ok(),
        "the service did not stop within 10 s"
    );
}

#[test]
fn a_broker_address_may_carry_its_port() {
    assert_eq!(broker_host_port("10.0.0.5"), ("10.0.0.5", 1883));
    assert_eq!(broker_host_port("127.0.0.1:41883"), ("127.0.0.1", 41883));
    assert_eq!(broker_host_port("broker.local:x"), ("broker.local:x", 1883));
}

/// Malformed commands, each answered with an error publish at QoS 1, while the broker withholds
/// its acknowledgements: rumqttc's request channel fills, then the publisher's queue. The acks are
/// then released, and every error publish must arrive. A poller that hands its errors to the
/// publisher through that bounded queue waits on the publisher, which waits on the request
/// channel, which only the poller drains — so nothing more ever arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_bad_commands_against_a_stalled_broker_completes_once_it_recovers() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    let coolmaster = FakeCoolmaster::start().await;
    let service = start_service(&broker.address(), &coolmaster.address);
    let error_topic = format!("Aircondition/Error/{NAME}");

    assert!(broker.wait_for_subscription(&command_topic(), Duration::from_secs(10)).await, "the session never subscribed");
    let initial = broker.received_on(&error_topic).len();
    broker.hold_acks();
    for _ in 0..COMMANDS {
        assert!(broker.send(&command_topic(), "not a command"));
    }
    // Let the burst saturate: the bridge takes commands until its queues are full.
    tokio::time::sleep(Duration::from_secs(2)).await;
    broker.release_acks();
    let want = initial + COMMANDS;
    let done = broker.wait_until(Duration::from_secs(20), |b| b.received_on(&error_topic).len() >= want).await;
    let arrived = broker.received_on(&error_topic).len() - initial;
    stop_service(service).await;
    assert!(
        done,
        "{arrived} of {COMMANDS} error publishes arrived after the broker recovered — the poller waited on the publisher's \
         queue, behind a publish that only the poller could complete"
    );
}

/// Commands that each set a unit's target temperature and ask for its state, while the broker
/// withholds its acknowledgements. Each state is a QoS 1 publish: the request channel fills, then
/// the publisher's queue, and the Coolmaster worker waits on it. The acks are then released, and
/// the bridge must catch up: the last temperature reaches the Coolmaster, the unit is listed after
/// it, and its state is published. While they wait, values and reads for one unit are coalesced
/// (the device-down policy's mailbox), so the Coolmaster sees fewer writes than commands — but
/// always the last.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_unit_commands_against_a_stalled_broker_completes_once_it_recovers() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    let coolmaster = FakeCoolmaster::start().await;
    let service = start_service(&broker.address(), &coolmaster.address);
    let state_topic = format!("Aircondition/State/{NAME}/{UNIT}");
    let command = |temperature: usize| {
        format!(r#"{{"unit":"{UNIT}","operation":{{"command":"TargetTemperature","temperature":{temperature}}}}}"#)
    };

    assert!(broker.wait_for_subscription(&command_topic(), Duration::from_secs(10)).await, "the session never subscribed");
    // The start-up listing, so it is not counted below.
    assert!(
        broker.wait_until(Duration::from_secs(10), |b| !b.received_on(&state_topic).is_empty()).await,
        "the start-up listing was never published"
    );
    broker.hold_acks();
    for i in 0..COMMANDS - 1 {
        assert!(broker.send(&command_topic(), command(16 + i % 8)));
    }
    assert!(broker.send(&command_topic(), command(30)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let held = broker.received_on(&state_topic).len();
    broker.release_acks();
    let caught_up = wait_for(Duration::from_secs(20), || {
        let commands = coolmaster.commands();
        let last_write = commands.iter().rev().find(|c| !c.starts_with("ls2"));
        last_write.is_some_and(|c| c == "temp L1.001 30") && commands.last().is_some_and(|c| c.starts_with("ls2"))
    })
    .await;
    let published = broker.wait_until(Duration::from_secs(10), |b| b.received_on(&state_topic).len() > held).await;
    stop_service(service).await;
    assert!(
        caught_up && published,
        "the bridge did not catch up once the broker recovered (the last command {} the Coolmaster; unit states {} \
         published after the broker recovered) — something on the Coolmaster's path waited on a publish that only the \
         poller could complete",
        if caught_up { "reached" } else { "never reached" },
        if published { "were" } else { "were not" },
    );
}

/// The connection drops while the bridge is saturated. rumqttc reconnects inside the same event
/// loop, and the broker keeps no session, so the bridge must notice the drop while it is
/// back-pressured, reconnect, and — once the broker recovers — announce itself and resubscribe to
/// its commands. A poller stuck in a send never sees the drop; one that publishes on CONNACK would
/// wait on the full request channel it alone drains.
///
/// Only what arrives after the drop is counted: on a reconnect without a session rumqttc discards
/// the requests still queued in its channel (`EventLoop::clean`, then `poll`), and the first
/// subscription may be one of them, queued behind the held publishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_lost_while_saturated_is_restored_and_resubscribed() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    let relay = Relay::start(broker.address()).await;
    let coolmaster = FakeCoolmaster::start().await;
    let service = start_service(&relay.address, &coolmaster.address);
    let state_topic = format!("Aircondition/State/{NAME}/{UNIT}");
    let active_topic = format!("Aircondition/Active/{NAME}");
    let subscriptions = |b: &FakeBroker| b.subscriptions().iter().filter(|f| **f == command_topic()).count();
    let announcements = |b: &FakeBroker| b.received_on(&active_topic).len();

    // Ready once the start-up listing is published. The broker delivers commands whether or not
    // the bridge has subscribed, so the subscriptions are counted only around the drop.
    assert!(
        broker.wait_until(Duration::from_secs(10), |b| !b.received_on(&state_topic).is_empty()).await,
        "the start-up listing was never published"
    );
    broker.hold_acks();
    for _ in 0..COMMANDS {
        assert!(broker.send(&command_topic(), "not a command"));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (subscribed, announced) = (subscriptions(&broker), announcements(&broker));
    relay.cut();
    // The poller retries a second after the drop.
    let reconnected = broker.wait_until(Duration::from_secs(5), |b| b.connections() >= 2).await;
    broker.release_acks();
    let resubscribed = broker
        .wait_until(Duration::from_secs(20), |b| subscriptions(b) > subscribed && announcements(b) > announced)
        .await;
    let (subscribed, announced) = (subscriptions(&broker) - subscribed, announcements(&broker) - announced);
    stop_service(service).await;
    assert!(reconnected, "the bridge never reconnected after its connection dropped — nothing was polling its event loop");
    assert!(
        resubscribed,
        "the bridge reconnected but did not announce itself and resubscribe ({subscribed} subscriptions and {announced} \
         active flags since the drop; 1 each expected)"
    );
}

fn error_topic() -> String {
    format!("Aircondition/Error/{NAME}")
}

fn state_topic(unit: &str) -> String {
    format!("Aircondition/State/{NAME}/{unit}")
}

fn active_topic() -> String {
    format!("Aircondition/Active/{NAME}")
}

fn set_power(unit: &str, power: bool) -> String {
    format!(r#"{{"unit":"{unit}","operation":{{"command":"SetPower","power":{power}}}}}"#)
}

fn set_temperature(unit: &str, temperature: f32) -> String {
    format!(
        r#"{{"unit":"{unit}","operation":{{"command":"TargetTemperature","temperature":{temperature}}}}}"#
    )
}

fn reset_filter(unit: &str) -> String {
    format!(r#"{{"unit":"{unit}","operation":{{"command":"ResetFilter"}}}}"#)
}

fn command_subscriptions(broker: &FakeBroker) -> usize {
    broker
        .subscriptions()
        .iter()
        .filter(|f| **f == command_topic())
        .count()
}

fn announcements(broker: &FakeBroker) -> usize {
    broker.received_on(&active_topic()).len()
}

/// Wait until `ready` holds, or `within` passes. Returns whether it held.
async fn wait_for(within: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while !ready() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The broker has seen the bridge report the Coolmaster down: the error publish of its first
/// failed connect, made once the worker has found it down.
async fn coolmaster_reported_down(broker: &FakeBroker) -> bool {
    broker
        .wait_until(Duration::from_secs(10), |b| {
            !b.received_on(&error_topic()).is_empty()
        })
        .await
}

/// The device-down policy's reason (review B3): the MQTT session never waits on the Coolmaster.
/// With the Coolmaster unreachable, commands keep coming — more than its old queue held — and then
/// the broker connection drops. The bridge must reconnect, announce itself and resubscribe. A
/// session that hands commands to the Coolmaster worker through a bounded queue parks on it once
/// it is full (nothing drains it while the Coolmaster is down) and never reads the CONNACK the
/// pump forwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_blip_while_the_coolmaster_is_down_is_announced_and_resubscribed() {
    let broker = FakeBroker::start().await;
    let relay = Relay::start(broker.address()).await;
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.set_up(false);
    let service = start_service(&relay.address, &coolmaster.address);

    assert!(
        broker
            .wait_for_subscription(&command_topic(), Duration::from_secs(10))
            .await,
        "the session never subscribed"
    );
    // 60 commands for the Coolmaster (a write and a read each); its old queue held 10.
    for i in 0..30 {
        assert!(broker.send(&command_topic(), set_power(UNIT, i % 2 == 0)));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (subscribed, announced) = (command_subscriptions(&broker), announcements(&broker));
    relay.cut();
    let reconnected = broker
        .wait_until(Duration::from_secs(5), |b| b.connections() >= 2)
        .await;
    let resubscribed = broker
        .wait_until(Duration::from_secs(10), |b| {
            command_subscriptions(b) > subscribed && announcements(b) > announced
        })
        .await;
    stop_service(service).await;
    assert!(
        reconnected,
        "the bridge never reconnected after its connection dropped"
    );
    assert!(
        resubscribed,
        "the bridge reconnected but did not announce itself and resubscribe: its MQTT session was parked behind the \
         Coolmaster, which was down"
    );
}

/// The device-down policy, through the whole bridge: while the Coolmaster is down the latest value
/// per unit and property waits for it, and nothing else does; when it is back those values are
/// applied in the order they were last set, and then the units' observed state is read and
/// published, so the Store shows what is true. Replaying every command instead applies values
/// that were already superseded, as late as the Coolmaster's return.
#[tokio::test(flavor = "multi_thread")]
async fn state_set_while_the_coolmaster_is_down_is_applied_latest_only_on_reconnect() {
    let broker = FakeBroker::start().await;
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.set_up(false);
    let service = start_service(&broker.address(), &coolmaster.address);
    let (u1, u2) = (UNITS[0], UNITS[1]);
    let writes = |c: &FakeCoolmaster| -> Vec<String> {
        c.commands()
            .into_iter()
            .filter(|c| !c.starts_with("ls2"))
            .collect()
    };

    assert!(
        broker
            .wait_for_subscription(&command_topic(), Duration::from_secs(10))
            .await,
        "the session never subscribed"
    );
    assert!(
        coolmaster_reported_down(&broker).await,
        "the bridge never reported the Coolmaster down"
    );
    for command in [
        set_power(u1, true),
        set_temperature(u1, 20.0),
        set_power(u2, true),
        set_power(u1, false),
        set_temperature(u1, 22.0),
    ] {
        assert!(broker.send(&command_topic(), command));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let states_before = broker.received_on(&state_topic(u1)).len();
    coolmaster.set_up(true);

    // The worker retries every 5 s; once back it writes, then lists.
    let applied = wait_for(Duration::from_secs(12), || writes(&coolmaster).len() >= 3).await;
    let listed_after = |commands: &[String]| {
        let last_write = commands.iter().rposition(|c| !c.starts_with("ls2"));
        last_write.is_some_and(|i| commands[i + 1..].iter().any(|c| c == "ls2"))
    };
    let listed = wait_for(Duration::from_secs(5), || listed_after(&coolmaster.commands())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let observed = broker
        .wait_until(Duration::from_secs(5), |b| {
            b.received_on(&state_topic(u1)).len() > states_before
        })
        .await;
    let commands = coolmaster.commands();
    stop_service(service).await;

    assert!(
        applied,
        "the Coolmaster came back but the bridge did not apply what was set meanwhile: {commands:?}"
    );
    assert_eq!(
        writes(&coolmaster),
        ["on L1.002", "off L1.001", "temp L1.001 22"],
        "while the Coolmaster was down, only the latest value per unit and property should have waited for it, applied \
         in the order last set (all it received: {commands:?})"
    );
    assert!(
        listed,
        "the units' observed state was not read after the pending state was applied: {commands:?}"
    );
    assert!(
        observed,
        "the units' observed state was not published once the Coolmaster was back"
    );
}

/// A momentary command means nothing later, so while the Coolmaster is down it is refused at
/// once, with an error publish, and never replayed when the Coolmaster comes back.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_filter_while_the_coolmaster_is_down_is_refused_with_an_error() {
    let broker = FakeBroker::start().await;
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.set_up(false);
    let service = start_service(&broker.address(), &coolmaster.address);
    let refusal = |b: &FakeBroker| {
        b.received_on(&error_topic())
            .iter()
            .any(|r| String::from_utf8_lossy(&r.payload).contains("ResetFilter"))
    };

    assert!(
        broker
            .wait_for_subscription(&command_topic(), Duration::from_secs(10))
            .await,
        "the session never subscribed"
    );
    assert!(
        coolmaster_reported_down(&broker).await,
        "the bridge never reported the Coolmaster down"
    );
    assert!(broker.send(&command_topic(), reset_filter(UNIT)));
    let refused = broker.wait_until(Duration::from_secs(5), refusal).await;
    coolmaster.set_up(true);
    let back = wait_for(Duration::from_secs(12), || {
        coolmaster.commands().iter().any(|c| c.starts_with("ls2"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let commands = coolmaster.commands();
    stop_service(service).await;

    assert!(
        refused,
        "no error was published for the ResetFilter sent while the Coolmaster was down"
    );
    assert!(
        back,
        "the Coolmaster came back but the bridge never listed its units"
    );
    assert!(
        !commands.iter().any(|c| c.starts_with("filt")),
        "the ResetFilter sent while the Coolmaster was down was replayed when it came back: {commands:?}"
    );
}

/// rumqttc drops the publishes still queued when a connection is lost and the broker keeps no
/// session (`EventLoop::clean`, then `pending.clear()` on a CONNACK without one). The publisher
/// publishes a unit's state only when it changed, so a dropped state is never sent again while the
/// unit stays as it is, and the Store keeps showing the old one. After every CONNACK the publisher
/// sends again the retained state it knows.
///
/// The broker withholds its acks and takes two QoS 1 publishes in flight, so the unit's state from
/// the start-up listing is still waiting in rumqttc when the connection drops. (Sometimes only one
/// is taken: a subscription takes a packet id too, and a publish whose id collides with the held
/// one stops rumqttc taking requests. Either way the state is still waiting — which is checked.)
#[tokio::test(flavor = "multi_thread")]
async fn a_state_a_reconnect_dropped_is_published_again() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    broker.hold_acks();
    let relay = Relay::start(broker.address()).await;
    let coolmaster = FakeCoolmaster::start_steady().await;
    let service = start_service(&relay.address, &coolmaster.address);

    assert!(
        broker
            .wait_until(Duration::from_secs(10), |b| b.held_acks() >= 1)
            .await,
        "the broker never held an ack"
    );
    assert!(
        wait_for(Duration::from_secs(10), || coolmaster
            .commands()
            .iter()
            .any(|c| c.starts_with("ls2")))
        .await,
        "the start-up listing never reached the Coolmaster"
    );
    // Long enough for the listing's state to reach rumqttc's queue.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        broker.received_on(&state_topic(UNIT)).is_empty(),
        "the unit's state got through before the drop, so this test proves nothing"
    );
    relay.cut();
    assert!(
        broker
            .wait_until(Duration::from_secs(5), |b| b.connections() >= 2)
            .await,
        "the bridge never reconnected"
    );
    broker.release_acks();
    let published = broker
        .wait_until(Duration::from_secs(10), |b| {
            !b.received_on(&state_topic(UNIT)).is_empty()
        })
        .await;
    stop_service(service).await;
    assert!(
        published,
        "the unit's state never reached the broker: the reconnect dropped its publish, and the publisher, which \
         publishes only what changed, never sent it again"
    );
}

/// A broker outage is one episode however many MQTT sessions it outlasts (logging policy): an
/// INFO when it starts, one WARN once it has lasted past its threshold, and an INFO with its
/// length when the broker is back. The pump gives up after five failed polls a second apart and
/// the worker starts a new session, so an outage spans several; outage state kept per session
/// logs each one as a new outage, with a WARN of its own.
#[tokio::test]
async fn a_broker_outage_across_sessions_is_one_episode_in_the_log() {
    let broker = FakeBroker::start().await;
    let relay = Relay::start(broker.address()).await;
    let coolmaster = FakeCoolmaster::start().await;
    relay.refuse(true);
    let log = Capture::start();
    let timing = Timing {
        mqtt_retry: Duration::from_secs(1),
        broker_warn_after: Duration::from_secs(2),
        ..Timing::default()
    };
    let service = start_service_with(&relay.address, &coolmaster.address, timing);
    // Two sessions: five failed polls, a second's pause, and failed polls again.
    tokio::time::sleep(Duration::from_secs(8)).await;
    relay.refuse(false);
    let back = broker
        .wait_for_subscription(&command_topic(), Duration::from_secs(20))
        .await;
    stop_service(service).await;

    assert!(back, "the bridge never reconnected once the broker was back");
    let lost = log.of_kind("connection_lost");
    assert!(
        lost.len() == 1 && lost[0].level == tracing::Level::INFO,
        "the outage's start was not one INFO `connection_lost`; the log: {:#?}",
        log.records()
    );
    let warnings = log.at_least(tracing::Level::WARN);
    assert!(
        warnings.len() == 1
            && warnings[0].kind == "external_failure"
            && warnings[0].field("down_for_ms").is_some(),
        "the outage was not one WARN past its threshold, with its length so far; the log: {:#?}",
        log.records()
    );
    let recovered = log.of_kind("external_recovered");
    let down_for = recovered
        .first()
        .and_then(|r| r.field("down_for_ms"))
        .and_then(|ms| ms.parse::<u64>().ok());
    assert!(
        recovered.len() == 1
            && recovered[0].level == tracing::Level::INFO
            && down_for.is_some_and(|ms| ms >= 7000),
        "the outage's end was not one INFO `external_recovered` with its whole length (`down_for_ms`); the log: {:#?}",
        log.records()
    );
}
