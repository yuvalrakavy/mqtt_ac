//! The driver tests' harness: the real bridge (`Bridge::start`, in-process) driving the stand-in
//! CoolMaster ([`coolmaster`]), against `FakeBroker` on 127.0.0.1 — never the house broker, the
//! LAN or a real CoolMaster. The tests play the Store's side over MQTT.
//!
//! These tests cover what the driver adds; the runtime's own behaviour (the mailbox, outages,
//! the conflict rule, the process) is its conformance suite's.

#![allow(dead_code, unused_imports)] // each test binary uses its own part of the harness

pub mod coolmaster;
pub mod log;

use std::time::{Duration, Instant};

use mqtt_ac::{Address, Coolmaster, VERSION};
use mqtt_bridge_kit::topics::Topics;
use mqtt_bridge_kit::{Bridge, MqttTiming, Running, TimingOverrides};
pub use mqtt_test_broker::{BrokerEvent, FakeBroker, SendOptions};
use serde_json::Value;

pub use coolmaster::{Answer, FakeCoolmaster, Unit};

/// The bound on any one wait: a regression fails the test, never hangs the suite.
pub const BOUND: Duration = Duration::from_secs(20);

/// Long enough for a request that should not reach the CoolMaster to have reached it.
pub const SETTLE: Duration = Duration::from_millis(600);

/// The root every test bridge uses.
pub const ROOT: &str = "AcTest";

/// A request as the Store sends it: an hour's expiry, the change at the device winning an
/// outage conflict.
pub const REQUEST: Request = Request { expiry: 3600, conflict: "DeviceWins" };

#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub expiry: u32,
    pub conflict: &'static str,
}

impl Request {
    fn options(&self) -> SendOptions {
        SendOptions::default().expiry(self.expiry).user_property("conflict", self.conflict)
    }
}

/// The driver's timing, short, so an outage plays out in a second or two; no polling — the
/// bridge reads the CoolMaster when it connects and after each command — unless a test asks.
pub fn quick(poll: Option<Duration>) -> TimingOverrides {
    TimingOverrides {
        connect: Some(Duration::from_millis(1000)),
        operation: Some(Duration::from_millis(1000)),
        retry: Some(Duration::from_millis(300)),
        outage_warn: Some(Duration::from_secs(30)),
        poll: Some(poll),
        probe_interval: None,
        probe_deadline: None,
    }
}

fn quick_mqtt() -> MqttTiming {
    MqttTiming {
        keep_alive: Duration::from_secs(5),
        retry: Duration::from_millis(500),
        poll_retry: Duration::from_millis(100),
        warn_after: Duration::from_secs(30),
        stable_after: Duration::from_secs(5),
    }
}

/// One bridge in-process, its broker and its CoolMaster.
pub struct Harness {
    pub broker: FakeBroker,
    pub coolmaster: FakeCoolmaster,
    pub topics: Topics,
    running: Option<Running>,
}

impl Harness {
    /// A bridge without polling, once it has subscribed.
    pub async fn start(instance: &str) -> Harness {
        Harness::start_with(instance, quick(None)).await
    }

    pub async fn start_with(instance: &str, timing: TimingOverrides) -> Harness {
        Harness::start_prepared(instance, timing, |_| {}).await
    }

    /// The same, after `prepare` has set the CoolMaster up for the bridge's first connection.
    pub async fn start_prepared(instance: &str, timing: TimingOverrides, prepare: impl FnOnce(&FakeCoolmaster)) -> Harness {
        log::install();
        let broker = FakeBroker::start().await;
        broker.cap_connections(50);
        let coolmaster = FakeCoolmaster::start().await;
        prepare(&coolmaster);
        let topics = Topics::new(ROOT, instance).expect("the test's topics");
        let address = Address::parse(&coolmaster.address).expect("the stand-in's address");
        let operation = timing.apply(mqtt_ac::info().timing).operation;
        let running = Bridge::new(ROOT)
            .instance(instance)
            .broker(&broker.address())
            .timing(timing)
            .mqtt_timing(quick_mqtt())
            .version(VERSION)
            .start(Coolmaster::new(address).operation_bound(operation))
            .expect("the bridge starts");
        let harness = Harness { broker, coolmaster, topics, running: Some(running) };
        let command = harness.topics.command();
        assert!(harness.broker.wait_for_subscription(&command, BOUND).await, "the bridge never subscribed to {command}");
        harness
    }

    /// Wait until `ready` holds, polling, up to `within`. Returns whether it held.
    pub async fn eventually(&self, within: Duration, mut ready: impl FnMut(&Harness) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if ready(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn settle(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }

    /// Connected, with both units' State published.
    pub async fn connected(&self) -> bool {
        self.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("L1.001").is_some() && h.state("L1.002").is_some())
            .await
    }

    /// The Store's write of one property: retained at the broker, and delivered live.
    pub fn desire(&self, unit: &str, property: &str, value: Value, request: Request) -> bool {
        let topic = self.topics.desired(unit, property);
        self.broker.retain(&topic, value.to_string(), &request.options().retained());
        self.broker.send_with(&topic, value.to_string(), &request.options())
    }

    /// The Store's `desire_many`: the target message, then each property topic, retained.
    pub fn desire_many(&self, unit: &str, values: Value, request: Request) -> bool {
        let sent = self.broker.send_with(&self.topics.desired_target(unit), values.to_string(), &request.options());
        if let Value::Object(map) = &values {
            for (property, value) in map {
                self.desire(unit, property, value.clone(), request);
            }
        }
        sent
    }

    /// A command, as the Store sends it (never retained, 30 s expiry).
    pub fn command(&self, payload: Value) -> bool {
        self.broker.send_with(&self.topics.command(), payload.to_string(), &SendOptions::default().expiry(30))
    }

    /// A unit's latest published `State`.
    pub fn state(&self, unit: &str) -> Option<Value> {
        self.broker.received_on(&self.topics.state(unit)).last().and_then(|r| serde_json::from_slice(&r.payload).ok())
    }

    /// The latest published `Status`.
    pub fn status(&self) -> Option<String> {
        self.broker.received_on(&self.topics.status()).last().map(|r| String::from_utf8_lossy(&r.payload).into_owned())
    }

    /// Every `Status` published, in order.
    pub fn statuses(&self) -> Vec<String> {
        self.broker.received_on(&self.topics.status()).iter().map(|r| String::from_utf8_lossy(&r.payload).into_owned()).collect()
    }

    /// Every `Error` the bridge published, as JSON.
    pub fn errors(&self) -> Vec<Value> {
        self.broker.received_on(&self.topics.error()).iter().filter_map(|r| serde_json::from_slice(&r.payload).ok()).collect()
    }

    /// The `Error`s naming `property` of `unit`.
    pub fn errors_for(&self, unit: &str, property: &str) -> Vec<Value> {
        self.errors().into_iter().filter(|e| e["target"] == unit && e["property"] == property).collect()
    }

    /// Stop the bridge (bounded). Returns whether it stopped in time.
    pub async fn stop(mut self) -> bool {
        let running = self.running.take().expect("the bridge is running");
        tokio::time::timeout(BOUND, running.stop()).await.unwrap_or(false)
    }
}
