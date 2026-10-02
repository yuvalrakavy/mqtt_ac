use async_channel::{Receiver, Sender};
use rumqttc::v5::{AsyncClient, EventLoop, MqttOptions, mqttbytes::QoS, mqttbytes::v5::LastWill};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::{Id, JoinSet};
use tokio::time::Duration;
use tracing::{debug, info};

use crate::{
    coolmaster::{Coolmaster, DeviceTiming},
    mailbox::Mailbox,
    messages::ToMqttPublisherMessage,
    mqtt_publisher::MqttPublisher,
    mqtt_pump::BrokerOutage,
    mqtt_subscriber, polling,
    reports::Reports,
};

pub struct Started {}
pub struct Stopped {}

pub struct ServiceConfig {
    pub mqtt_broker_address: String,
    pub controller_name: String,
    pub coolmaster_address: String,
    pub polling_period: Duration,
    pub timing: Timing,
}

/// How the bridge paces its reconnects, and how long an outage lasts before it is a WARN.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Between MQTT sessions: after one gives up (five failed polls in a row), a fresh client.
    pub mqtt_retry: Duration,
    /// How long the broker may be out of reach before the outage is a WARN.
    pub broker_warn_after: Duration,
    /// Between attempts to reach the Coolmaster.
    pub device_retry: Duration,
    /// How long the Coolmaster may be out of reach before the outage is a WARN.
    pub device_warn_after: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        let device = DeviceTiming::default();
        Timing {
            mqtt_retry: Duration::from_secs(10),
            broker_warn_after: Duration::from_secs(30),
            device_retry: device.retry,
            device_warn_after: device.warn_after,
        }
    }
}

pub struct Service<Status = Stopped> {
    config: ServiceConfig,

    workers: JoinSet<()>,
    /// Each worker's name, by its task id: a worker that ends is named in the log.
    names: Vec<(Id, &'static str)>,
    _status: PhantomData<Status>,
}

impl<Status> Service<Status> {
    fn spawn(&mut self, name: &'static str, worker: impl Future<Output = ()> + Send + 'static) {
        let handle = self.workers.spawn(worker);
        self.names.push((handle.id(), name));
    }

    fn name(&self, id: Id) -> &'static str {
        self.names
            .iter()
            .find(|(worker, _)| *worker == id)
            .map_or("unknown", |(_, name)| name)
    }
}

impl Service {
    pub fn new(config: ServiceConfig) -> Service<Stopped> {
        Service {
            config,
            workers: JoinSet::new(),
            names: Vec::new(),
            _status: PhantomData,
        }
    }

    fn connect_to_mqtt_broker(
        mqtt_broker: &str,
        controller_name: &str,
    ) -> (AsyncClient, EventLoop) {
        let client_id = format!("Aircondition-{controller_name}");
        let (host, port) = broker_host_port(mqtt_broker);
        let mut mqtt_options = MqttOptions::new(client_id, host, port);
        let last_will_topic = format!("Aircondition/Active/{controller_name}");
        let last_will = LastWill::new(&last_will_topic, "false".as_bytes(), QoS::AtLeastOnce, true, None);
        mqtt_options
            .set_keep_alive(Duration::from_secs(5))
            .set_last_will(last_will);

        AsyncClient::new(mqtt_options, 100)
    }

    /// One MQTT session: a fresh client, its publisher and its subscriber session, until either
    /// ends. Neither waits on this function, and the first to end ends the other (dropping it,
    /// which drops the pump and its event loop).
    async fn mqtt_session(
        mqtt_broker: &str,
        controller_name: &str,
        publisher: &mut MqttPublisher,
        mailbox: &Mailbox,
        to_mqtt_publisher_tx: Sender<ToMqttPublisherMessage>,
        outage: &Arc<BrokerOutage>,
    ) {
        let (mqtt_client, event_loop) =
            Service::connect_to_mqtt_broker(mqtt_broker, controller_name);
        let reconnected = Notify::new();

        // WAIT: session-join
        tokio::select! {
            result = publisher.session(&mqtt_client, &reconnected) => {
                debug!(?result, "MQTT publisher session ended");
            }
            result = mqtt_subscriber::session(
                event_loop,
                mqtt_client.clone(),
                controller_name.to_owned(),
                mailbox,
                to_mqtt_publisher_tx,
                &reconnected,
                outage.clone(),
            ) => {
                debug!(?result, "MQTT subscriber session ended");
            }
        }
    }

    /// The MQTT side, for the life of the service: sessions one after another. What must outlast a
    /// session lives here — the publisher, with the retained state it knows, and the broker's
    /// outage state, so an outage is one episode however many sessions it spans.
    async fn mqtt_worker(
        mqtt_broker: &str,
        controller_name: &str,
        timing: Timing,
        mailbox: Arc<Mailbox>,
        reports: Arc<Reports>,
        to_mqtt_publisher_rx: Receiver<ToMqttPublisherMessage>,
        to_mqtt_publisher_tx: Sender<ToMqttPublisherMessage>,
    ) {
        let outage = Arc::new(BrokerOutage::new(timing.broker_warn_after));
        let mut publisher = MqttPublisher::new(controller_name.to_owned(), reports, to_mqtt_publisher_rx);
        loop {
            debug!("Starting MQTT session");
            Service::mqtt_session(
                mqtt_broker,
                controller_name,
                &mut publisher,
                &mailbox,
                to_mqtt_publisher_tx.clone(),
                &outage,
            )
            .await;

            // The outage, if that is why, is logged as one episode by `outage`.
            debug!(retry_ms = timing.mqtt_retry.as_millis() as u64, "MQTT session ended; starting a new one");
            tokio::time::sleep(timing.mqtt_retry).await;
        }
    }
}

impl Service<Stopped> {
    /// Spawns the workers on the current runtime; it waits on nothing.
    pub fn start(mut self) -> Service<Started> {
        // The Coolmaster's commands, posted without waiting; its reports, handed over without
        // waiting; the session's errors, queued for the publisher.
        let mailbox = Mailbox::new();
        let reports = Reports::new();
        let (to_mqtt_publisher_tx, to_mqtt_publisher_rx) = async_channel::bounded(10);
        let timing = self.config.timing;

        let coolmaster_address = self.config.coolmaster_address.clone();
        let device_timing = DeviceTiming {
            retry: timing.device_retry,
            warn_after: timing.device_warn_after,
        };
        let (worker_mailbox, worker_reports) = (mailbox.clone(), reports.clone());
        self.spawn("coolmaster", async move {
            Coolmaster::coolmaster_worker(&coolmaster_address, worker_mailbox, worker_reports, device_timing).await;
        });

        let controller_name = self.config.controller_name.clone();
        let mqtt_broker = self.config.mqtt_broker_address.clone();
        let mqtt_mailbox = mailbox.clone();
        self.spawn("mqtt", async move {
            Self::mqtt_worker(
                &mqtt_broker,
                &controller_name,
                timing,
                mqtt_mailbox,
                reports,
                to_mqtt_publisher_rx,
                to_mqtt_publisher_tx,
            )
            .await
        });

        let polling_period = self.config.polling_period;
        self.spawn("polling", async move {
            polling::polling_worker(polling_period, mailbox).await;
        });

        info!("Service started");
        Service {
            config: self.config,
            workers: self.workers,
            names: self.names,
            _status: PhantomData,
        }
    }
}

impl Service<Started> {
    /// The first worker to end, by name, and how: none of them ends on its own, so a panic or an
    /// early return is the end of the bridge (re-review C5). Cancellation-safe.
    pub async fn worker_ended(&mut self) -> (&'static str, String) {
        // WAIT: worker-join
        match self.workers.join_next_with_id().await {
            Some(Ok((id, ()))) => (self.name(id), "returned".to_owned()),
            Some(Err(error)) => (self.name(error.id()), error.to_string()),
            None => {
                // No worker at all: nothing can end.
                // WAIT: worker-join
                std::future::pending::<(&'static str, String)>().await
            }
        }
    }

    /// A worker of the test's own, among the service's.
    #[cfg(test)]
    pub fn spawn_worker(&mut self, name: &'static str, task: impl Future<Output = ()> + Send + 'static) {
        self.spawn(name, task);
    }

    pub async fn stop(mut self) -> Service<Stopped> {
        // WAIT: task-shutdown
        self.workers.shutdown().await;
        info!("Service stopped");

        Service {
            config: self.config,
            workers: self.workers,
            names: self.names,
            _status: PhantomData,
        }
    }
}

/// `host` or `host:port` (default 1883).
fn broker_host_port(broker: &str) -> (&str, u16) {
    match broker.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse() {
            Ok(port) => (host, port),
            Err(_) => (broker, 1883),
        },
        _ => (broker, 1883),
    }
}

#[cfg(test)]
mod tests;
