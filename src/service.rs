use async_channel::{Receiver, Sender};
use rumqttc::v5::{AsyncClient, EventLoop, MqttOptions, mqttbytes::QoS, mqttbytes::v5::LastWill};
use std::marker::PhantomData;
use tokio::{task::JoinSet, time::Duration};
use tracing::info;

use crate::{
    coolmaster::Coolmaster,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
    mqtt_publisher::MqttPublisher,
    mqtt_subscriber, polling,
};

pub struct Started {}
pub struct Stopped {}

pub struct ServiceConfig {
    pub mqtt_broker_address: String,
    pub controller_name: String,
    pub coolmaster_address: String,
    pub polling_period: Duration,
}

pub struct Service<Status = Stopped> {
    config: ServiceConfig,

    workers: JoinSet<()>,
    _status: PhantomData<Status>,
}

impl Service {
    pub fn new(config: ServiceConfig) -> Service<Stopped> {
        Service {
            config,
            workers: JoinSet::new(),
            _status: PhantomData,
        }
    }

    fn connect_to_mqtt_broker(
        mqtt_broker: &str,
        controller_name: &str,
    ) -> (AsyncClient, EventLoop) {
        let client_id = format!("Aircondition-{controller_name}");
        let mut mqtt_options = MqttOptions::new(client_id, mqtt_broker, 1883);
        let last_will_topic = format!("Aircondition/Active/{controller_name}");
        let last_will = LastWill::new(&last_will_topic, "false".as_bytes(), QoS::AtLeastOnce, true, None);
        mqtt_options
            .set_keep_alive(Duration::from_secs(5))
            .set_last_will(last_will);

        AsyncClient::new(mqtt_options, 100)
    }

    async fn mqtt_session(
        mqtt_broker: &str,
        controller_name: impl Into<String>,
        to_mqtt_publisher_rx: Receiver<ToMqttPublisherMessage>,
        to_mqtt_publisher_tx: Sender<ToMqttPublisherMessage>,
        to_coolmaster_tx: Sender<ToCoolmasterMessage>,
    ) {
        let controller_name = controller_name.into();
        let mut sessions = JoinSet::new();

        let (mqtt_client, event_loop) =
            Service::connect_to_mqtt_broker(mqtt_broker, &controller_name);

        let subscriber_client = mqtt_client.clone();
        let subscriber_controller_name = controller_name.clone();

        sessions.spawn(async move {
            match MqttPublisher::session(
                controller_name,
                mqtt_client,
                to_mqtt_publisher_rx,
            )
            .await
            {
                Ok(_) => info!("MQTT publisher session finished"),
                Err(e) => info!("MQTT publisher session finished with error: {e:?}"),
            }
        });

        sessions.spawn(async move {
            match mqtt_subscriber::session(
                event_loop,
                subscriber_client,
                subscriber_controller_name,
                to_coolmaster_tx,
                to_mqtt_publisher_tx,
            )
            .await
            {
                Ok(_) => info!("MQTT subscriber session finished"),
                Err(e) => info!("MQTT subscriber session finished with error: {e:?}"),
            }
        });

        _ = sessions.join_next().await;
        _ = sessions.shutdown().await;
    }

    async fn mqtt_worker(
        mqtt_broker: &str,
        controller_name: &str,
        to_coolmaster_tx: Sender<ToCoolmasterMessage>,
        to_mqtt_publisher_rx: Receiver<ToMqttPublisherMessage>,
        to_mqtt_publisher_tx: Sender<ToMqttPublisherMessage>,
    ) {
        loop {
            info!("Starting MQTT session");
            Service::mqtt_session(
                mqtt_broker,
                controller_name,
                to_mqtt_publisher_rx.clone(),
                to_mqtt_publisher_tx.clone(),
                to_coolmaster_tx.clone(),
            )
            .await;

            info!("MQTT terminated, waiting 10 seconds before restarting");
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}

impl Service<Stopped> {
    pub async fn start(mut self) -> Service<Started> {
        // Create the channels for the workers
        let (to_coolmaster_tx, to_coolmaster_rx) = async_channel::bounded(10);
        let (to_mqtt_publisher_tx, to_mqtt_publisher_rx) = async_channel::bounded(10);

        // Create coolmaster worker

        let coolmaster_address = self.config.coolmaster_address.clone();
        let to_mqtt_publisher_tx_instance = to_mqtt_publisher_tx.clone();
        self.workers.spawn(async move {
            Coolmaster::coolmaster_worker(
                &coolmaster_address,
                to_coolmaster_rx,
                to_mqtt_publisher_tx_instance,
            )
            .await;
        });

        // Create mqtt publisher worker
        let controller_name = self.config.controller_name.clone();
        let to_coolmaster_tx_instance = to_coolmaster_tx.clone();
        let mqtt_broker = self.config.mqtt_broker_address.clone();

        self.workers.spawn(async move {
            Self::mqtt_worker(
                &mqtt_broker,
                &controller_name,
                to_coolmaster_tx_instance,
                to_mqtt_publisher_rx,
                to_mqtt_publisher_tx,
            )
            .await
        });

        // Create polling worker
        self.workers.spawn(async move {
            polling::polling_worker(self.config.polling_period, to_coolmaster_tx).await;
        });

        info!("Service started");
        Service {
            config: self.config,
            workers: self.workers,
            _status: PhantomData,
        }
    }
}

impl Service<Started> {
    pub async fn stop(mut self) -> Service<Stopped> {
        self.workers.shutdown().await;
        info!("Service stopped");

        Service {
            config: self.config,
            workers: self.workers,
            _status: PhantomData,
        }
    }
}
