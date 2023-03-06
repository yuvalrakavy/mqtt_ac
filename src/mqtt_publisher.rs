use std::collections::HashMap;
use tokio::sync::mpsc::Receiver;

use log::{info, debug};

use crate::messages::ToMqttPublisherMessage;
use crate::ac_unit::UnitState;

pub struct MqttPublisher {
    controller_name: String,
    unit_states: HashMap<String, UnitState>,
    mqtt_client: rumqttc::AsyncClient,
    to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
}

impl MqttPublisher {
    pub async fn mqtt_publisher_worker(
        controller_name: String,
        mqtt_client: rumqttc::AsyncClient,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
    ) {
        let mut coolmaster = MqttPublisher::new(controller_name, mqtt_client, to_mqtt_publisher_channel);

        coolmaster.do_work().await;
    }

    fn new(controller_name: String, mqtt_client: rumqttc::AsyncClient, to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>) -> Self {
        MqttPublisher {
            controller_name,
            unit_states: HashMap::new(),
            mqtt_client,
            to_mqtt_publisher_channel,
        }
    }

    async fn do_work(&mut self) {
        loop {
            match self.to_mqtt_publisher_channel.recv().await {
                Some(message) => {
                    match message {
                        ToMqttPublisherMessage::UnitState(unit_state) => self.publish_if_modified(&unit_state).await,
                        
                        ToMqttPublisherMessage::UnitsState(unit_states) => {
                            for unit_state in unit_states {
                                self.publish_if_modified(&unit_state).await;
                            }
                        },

                        ToMqttPublisherMessage::Error(error_message) => {
                            let topic = format!("Aircondition/Error/{}", self.controller_name);
                            debug!("Publishing to topic {} error_message: {}", topic, error_message);
                            self.mqtt_client.publish(topic, rumqttc::QoS::AtLeastOnce, true, serde_json::to_vec(&error_message).unwrap()).await.unwrap();
                        }
                    }
                }

                None => {
                    info!("MQTT publisher channel closed");
                    break;
                }
            }
        }
    }

    async fn publish_if_modified(&mut self, unit_state: &UnitState) {
        let unit = &unit_state.unit;
        let old_unit_state = self.unit_states.get(unit);
        if old_unit_state.is_none() || old_unit_state.unwrap() != unit_state {
            self.unit_states.insert(unit.clone(), unit_state.clone());
            self.publish_unit_state(unit_state).await;
        }
    }

    async fn publish_unit_state(&mut self, unit_state: &UnitState) {
        let topic = format!("Aircondition/State/{}/{}", self.controller_name, unit_state.unit);
        debug!("Publishing to topic {} unit_state: {:#?}", topic, unit_state);
        self.mqtt_client.publish(topic, rumqttc::QoS::AtLeastOnce, true, serde_json::to_vec(unit_state).unwrap()).await.unwrap();
    }

}