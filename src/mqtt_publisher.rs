use error_stack::{Result, ResultExt};
use std::collections::HashMap;
use tokio::select;
use async_channel::Receiver;
use tokio_util::sync::CancellationToken;

use log::{debug, info};

use crate::ac_unit::UnitState;
use crate::error::MqttError;
use crate::messages::ToMqttPublisherMessage;

pub struct MqttPublisher {
    controller_name: String,
    unit_states: HashMap<String, UnitState>,
    mqtt_client: rumqttc::AsyncClient,
    cancellation_token: CancellationToken,
    to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
}

impl MqttPublisher {
    pub async fn mqtt_publisher_session(
        controller_name: String,
        mqtt_client: rumqttc::AsyncClient,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
        cancellation_token: CancellationToken,
    ) -> Result<(), MqttError> {
        let mut coolmaster = MqttPublisher::new(
            controller_name,
            mqtt_client,
            to_mqtt_publisher_channel,
            cancellation_token,
        );

        coolmaster.do_work().await
    }

    fn new(
        controller_name: String,
        mqtt_client: rumqttc::AsyncClient,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
        cancellation_token: CancellationToken,
    ) -> Self {
        MqttPublisher {
            controller_name,
            unit_states: HashMap::new(),
            mqtt_client,
            to_mqtt_publisher_channel,
            cancellation_token,
        }
    }

    async fn do_work(&mut self) -> Result<(), MqttError> {
        loop {
            select! {
                _ = self.cancellation_token.cancelled() => {
                    info!("MQTT publisher session cancelled");
                    break Ok(());
                }

                message = self.to_mqtt_publisher_channel.recv() => {
                    match message {
                        Ok(message) => match message {
                            ToMqttPublisherMessage::UnitState(unit_state) => {
                                self.publish_if_modified(&unit_state).await?
                            }

                            ToMqttPublisherMessage::UnitsState(unit_states) => {
                                for unit_state in unit_states {
                                    self.publish_if_modified(&unit_state).await?;
                                }
                            }

                            ToMqttPublisherMessage::Error(error_message) => {
                                let topic = format!("Aircondition/Error/{}", self.controller_name);
                                debug!(
                                    "Publishing to topic {} error_message: {}",
                                    topic, error_message
                                );
                                self.mqtt_client
                                    .publish(
                                        topic,
                                        rumqttc::QoS::AtLeastOnce,
                                        true,
                                        serde_json::to_vec(&error_message).unwrap(),
                                    )
                                    .await
                                    .unwrap();
                            }
                        },

                        Err(_) => {
                            info!("MQTT publisher channel closed");
                            break Ok(());
                        }
                    }

                }
            }
        }
    }

    async fn publish_if_modified(&mut self, unit_state: &UnitState) -> Result<(), MqttError> {
        let unit = &unit_state.unit;
        let old_unit_state = self.unit_states.get(unit);
        if old_unit_state.is_none() || old_unit_state.unwrap() != unit_state {
            self.unit_states.insert(unit.clone(), unit_state.clone());
            self.publish_unit_state(unit_state).await?;
        }

        Ok(())
    }

    async fn publish_unit_state(&mut self, unit_state: &UnitState) -> Result<(), MqttError> {
        let topic = format!(
            "Aircondition/State/{}/{}",
            self.controller_name, unit_state.unit
        );
        let into_context = || MqttError::Context(format!("publish_unit_state: topic: {}", topic));

        debug!(
            "Publishing to topic {} unit_state: {:#?}",
            topic, unit_state
        );
        self.mqtt_client
            .publish(
                &topic,
                rumqttc::QoS::AtLeastOnce,
                true,
                serde_json::to_vec(unit_state).unwrap(),
            )
            .await
            .change_context_lazy(into_context)
    }
}
