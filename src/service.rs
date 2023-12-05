use async_channel::{Receiver, Sender};
use error_stack::{Result, ResultExt};
use log::info;
use rumqttc::{AsyncClient, EventLoop, LastWill, MqttOptions, QoS};
use std::marker::PhantomData;
use tokio::{task::JoinSet, time::Duration};
use tokio_util::sync::CancellationToken;

use crate::{
    coolmaster::Coolmaster,
    error::MqttError,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
    mqtt_publisher::MqttPublisher,
    mqtt_subscriber, polling, get_version,
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

    async fn connect_to_mqtt_broker(
        mqtt_broker: &str,
        controller_name: &str,
    ) -> Result<(AsyncClient, EventLoop), MqttError> {
        let into_context =
            || MqttError::Context(format!("Connecting to MQTT broker '{}'", mqtt_broker));
        let client_id = format!("Aircondition-{}", controller_name);
        let mut mqtt_options = MqttOptions::new(client_id, mqtt_broker, 1883);
        let last_will_topic = format!("Aircondition/Active/{controller_name}");
        let last_will = LastWill::new(&last_will_topic, "false".as_bytes(), QoS::AtLeastOnce, true);
        mqtt_options
            .set_keep_alive(Duration::from_secs(5))
            .set_last_will(last_will);

        let (mqtt_client, event_loop) = AsyncClient::new(mqtt_options, 10);

        // Publish active state
        mqtt_client
            .publish(&last_will_topic, QoS::AtLeastOnce, true, "true".as_bytes())
            .await
            .change_context_lazy(into_context)?;

            let version = get_version();
            mqtt_client
                .publish(
                    &format!("Aircondition/Version/{controller_name}"),
                    QoS::AtLeastOnce,
                    true,
                    version.as_bytes(),
                )
                .await
                .change_context_lazy(into_context)?;
    
        // Subscribe to commands
        mqtt_client
            .subscribe(
                format!("Aircondition/Command/{controller_name}"),
                QoS::AtLeastOnce,
            )
            .await
            .change_context_lazy(into_context)?;
        Ok((mqtt_client, event_loop))
    }

    async fn mqtt_session(
        mqtt_broker: &str,
        controller_name: impl Into<String>,
        to_mqtt_publisher_rx: Receiver<ToMqttPublisherMessage>,
        to_mqtt_publisher_tx: Sender<ToMqttPublisherMessage>,
        to_coolmaster_tx: Sender<ToCoolmasterMessage>,
    ) {
        let controller_name = controller_name.into();
        let publisher_cancel_token = CancellationToken::new();
        let subscriber_cancel_token = publisher_cancel_token.clone();

        let (mqtt_client, event_loop) =
            match Service::connect_to_mqtt_broker(mqtt_broker, &controller_name).await {
                Ok((mqtt_client, event_loop)) => (mqtt_client, event_loop),
                Err(e) => {
                    info!("Error connecting to MQTT broker: {:?}", e);
                    return;
                }
            };

        let publish_handle = tokio::spawn(async move {
            match MqttPublisher::mqtt_publisher_session(
                controller_name,
                mqtt_client,
                to_mqtt_publisher_rx,
                publisher_cancel_token.clone(),
            )
            .await
            {
                Ok(_) => info!("MQTT publisher session finished"),
                Err(e) => info!("MQTT publisher session finished with error: {:?}", e),
            }

            // This should ensure that the subscriber session is also cancelled
            publisher_cancel_token.cancel();
        });

        let subscriber_handle = tokio::spawn(async move {
            match mqtt_subscriber::mqtt_subscriber_session(
                event_loop,
                to_coolmaster_tx,
                to_mqtt_publisher_tx,
                subscriber_cancel_token.clone(),
            )
            .await
            {
                Ok(_) => info!("MQTT subscriber session finished"),
                Err(e) => info!("MQTT subscriber session finished with error: {:?}", e),
            }

            // This should ensure that the publisher session is cancelled
            subscriber_cancel_token.cancel();
        });

        // At this stage, both the publisher and subscriber sessions should have been aborted
        publish_handle.await.unwrap();
        subscriber_handle.await.unwrap();
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
