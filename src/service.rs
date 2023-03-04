use std::marker::PhantomData;
use tokio::{task::JoinSet, time::Duration};
use rumqttc::{AsyncClient, EventLoop, MqttOptions, LastWill, QoS};
use log::info;

use crate::{coolmaster::Coolmaster, mqtt_publisher::MqttPublisher, messages::{ToCoolmasterMessage, ToMqttPublisherMessage}, mqtt_subscriber, polling};

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

    async fn connect_to_mqtt_broker(mqtt_broker: &str, controller_name: &str) -> (AsyncClient, EventLoop) {
        let mut mqtt_options = MqttOptions::new(controller_name, mqtt_broker, 1883);
        let last_will_topic = format!("Aircondition/Active/{controller_name}");
        let last_will = LastWill::new(&last_will_topic, "false".as_bytes(), QoS::AtLeastOnce, true);
        mqtt_options.set_keep_alive(Duration::from_secs(5)).set_last_will(last_will);
    
        let (mqtt_client, event_loop) = AsyncClient::new(mqtt_options, 10);

        // Publish active state
        mqtt_client.publish(&last_will_topic, QoS::AtLeastOnce, true, "true".as_bytes()).await.unwrap();

        // Subscribe to commands
        mqtt_client.subscribe(format!("Aircondition/{controller_name}/Command"), QoS::AtLeastOnce).await.unwrap();
        (mqtt_client, event_loop)
    }
}

impl Service<Stopped> {
    pub async fn start(mut self) -> Service<Started> {
        let (mqtt_client, mqtt_event_loop) = 
            Service::connect_to_mqtt_broker(&self.config.mqtt_broker_address, &self.config.controller_name).await;

        // Create the channels for the workers
        let (to_coolmaster_tx, to_coolmaster_rx) = tokio::sync::mpsc::channel::<ToCoolmasterMessage>(10);
        let (to_mqtt_publisher_tx, to_mqtt_publisher_rx) = tokio::sync::mpsc::channel::<ToMqttPublisherMessage>(10);

        // Create coolmaster worker

        let coolmaster_address = self.config.coolmaster_address.clone();
        let to_mqtt_publisher_tx_instance = to_mqtt_publisher_tx.clone();
        self.workers.spawn(async move {
            Coolmaster::coolmaster_worker(&coolmaster_address,  to_coolmaster_rx, to_mqtt_publisher_tx_instance).await;
        });

        // Create mqtt publisher worker
        let controller_name = self.config.controller_name.clone();
        self.workers.spawn(async move {
            MqttPublisher::mqtt_publisher_worker(controller_name, mqtt_client, to_mqtt_publisher_rx).await;
        });

        // Create mqtt subscriber worker
        let to_coolmaster_tx_instance = to_coolmaster_tx.clone();
        let to_mqtt_publisher_tx_instance = to_mqtt_publisher_tx;

        self.workers.spawn(async move {
            mqtt_subscriber::mqtt_subscriber_worker(mqtt_event_loop, to_coolmaster_tx_instance, to_mqtt_publisher_tx_instance).await;
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

 
 