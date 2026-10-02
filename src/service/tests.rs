//! The bridge under saturation (Store no-hang §14.3, mqtt_ac): the task that polls rumqttc's event
//! loop must never wait — itself, or through a bounded queue — on rumqttc's request channel, which
//! only polling drains. These drive the whole service: the real workers, a fake broker that can
//! withhold its acknowledgements, and a stand-in Coolmaster on a local socket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::FakeBroker;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::{broker_host_port, Service, ServiceConfig, Started};
use crate::test_support::Relay;

const NAME: &str = "Saturation";
const UNIT: &str = "L1.001";
const COMMANDS: usize = 300;

fn command_topic() -> String {
    format!("Aircondition/Command/{NAME}")
}

/// A Coolmaster stand-in on `127.0.0.1`: the `>` prompt, then one reply per `\r`-terminated
/// command. `ls2` (with or without a unit) lists `UNIT`; every other command answers `OK`. The
/// room temperature moves on every listing, so each one is a state change, which the publisher
/// publishes (it publishes only what changed).
struct FakeCoolmaster {
    address: String,
    task: JoinHandle<()>,
}

impl Drop for FakeCoolmaster {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCoolmaster {
    async fn start() -> FakeCoolmaster {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake Coolmaster");
        let address = listener.local_addr().expect("the fake Coolmaster's address").to_string();
        let task = tokio::spawn(async move {
            let listings = Arc::new(AtomicUsize::new(0));
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_coolmaster(stream, listings.clone()));
            }
        });
        FakeCoolmaster { address, task }
    }
}

async fn serve_coolmaster(stream: TcpStream, listings: Arc<AtomicUsize>) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    if wr.write_all(b">").await.is_err() {
        return;
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        match rd.read_until(b'\r', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = String::from_utf8_lossy(&line).trim().to_owned();
        let reply = if command.starts_with("ls2") {
            let n = listings.fetch_add(1, Ordering::SeqCst);
            format!("{UNIT} ON 22.0C {}.0C High Cool OK - 0\r\nOK\r\n>", 10 + n % 50)
        } else {
            "OK\r\n>".to_owned()
        };
        if wr.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn start_service(broker: &str, coolmaster: &str) -> Service<Started> {
    Service::new(ServiceConfig {
        controller_name: NAME.to_owned(),
        mqtt_broker_address: broker.to_owned(),
        coolmaster_address: coolmaster.to_owned(),
        // One listing at start; the tests make the rest.
        polling_period: Duration::from_secs(3600),
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

/// Commands that each switch a unit and ask for its state, while the broker withholds its
/// acknowledgements. Each state is a QoS 1 publish: the request channel fills, then the publisher's
/// queue, then the Coolmaster worker's. The acks are then released, and every state must arrive. A
/// poller that hands commands to the Coolmaster worker through its bounded queue waits on that
/// worker, which waits on the publisher, which waits on the request channel only the poller drains.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_unit_commands_against_a_stalled_broker_completes_once_it_recovers() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    let coolmaster = FakeCoolmaster::start().await;
    let service = start_service(&broker.address(), &coolmaster.address);
    let state_topic = format!("Aircondition/State/{NAME}/{UNIT}");
    let command = format!(r#"{{"unit":"{UNIT}","operation":{{"command":"SetPower","power":true}}}}"#);

    assert!(broker.wait_for_subscription(&command_topic(), Duration::from_secs(10)).await, "the session never subscribed");
    // The start-up listing, so it is not counted below.
    assert!(
        broker.wait_until(Duration::from_secs(10), |b| !b.received_on(&state_topic).is_empty()).await,
        "the start-up listing was never published"
    );
    let initial = broker.received_on(&state_topic).len();
    broker.hold_acks();
    for _ in 0..COMMANDS {
        assert!(broker.send(&command_topic(), command.clone()));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    broker.release_acks();
    let want = initial + COMMANDS;
    let done = broker.wait_until(Duration::from_secs(20), |b| b.received_on(&state_topic).len() >= want).await;
    let arrived = broker.received_on(&state_topic).len() - initial;
    stop_service(service).await;
    assert!(
        done,
        "{arrived} of {COMMANDS} unit states arrived after the broker recovered — the poller waited on the Coolmaster \
         worker's queue, behind a publish that only the poller could complete"
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
