use async_channel::Receiver;
use error_stack::{Report, ResultExt};
use std::collections::HashMap;

use tracing::debug;

use crate::ac_unit::UnitState;
use crate::error::MqttError;
use crate::messages::ToMqttPublisherMessage;
use rumqttc::v5::{self, mqttbytes::QoS, mqttbytes::v5::PublishProperties};

pub struct MqttPublisher {
    controller_name: String,
    unit_states: HashMap<String, UnitState>,
    mqtt_client: v5::AsyncClient,
    to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
}

impl MqttPublisher {
    pub async fn session(
        controller_name: String,
        mqtt_client: v5::AsyncClient,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
    ) -> Result<(), Report<MqttError>> {
        let mut mqtt_publisher =
            MqttPublisher::new(controller_name, mqtt_client, to_mqtt_publisher_channel);

        mqtt_publisher.run_session().await
    }

    fn new(
        controller_name: String,
        mqtt_client: v5::AsyncClient,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
    ) -> Self {
        MqttPublisher {
            controller_name,
            unit_states: HashMap::new(),
            mqtt_client,
            to_mqtt_publisher_channel,
        }
    }

    async fn run_session(&mut self) -> Result<(), Report<MqttError>> {
        let into_context = || MqttError::Context("MQTT Publisher session".to_string());

        loop {
            let message = self
                .to_mqtt_publisher_channel
                .recv()
                .await
                .change_context_lazy(into_context)?;

            match message {
                ToMqttPublisherMessage::UnitState(unit_state) => {
                    self.publish_if_modified(&unit_state).await.change_context_lazy(into_context)?
                }

                ToMqttPublisherMessage::UnitsState(unit_states) => {
                    for unit_state in unit_states {
                        self.publish_if_modified(&unit_state).await.change_context_lazy(into_context)?;
                    }
                }

                ToMqttPublisherMessage::Error(error_message) => {
                    let topic = format!("Aircondition/Error/{}", self.controller_name);
                    debug!(
                        topic = %topic,
                        "Publishing error message"
                    );
                    let mut props = PublishProperties::default();
                    if let Some(tp) = tracing_init::traceparent::current() {
                        props.user_properties.push(("traceparent".into(), tp));
                    }
                    self.mqtt_client
                        .publish_with_properties(
                            topic,
                            QoS::AtLeastOnce,
                            true,
                            serde_json::to_vec(&error_message).unwrap(),
                            props,
                        )
                        .await
                        .map_err(|e| MqttError::ApiError(e.to_string(), "Publish error".to_owned()))?;
                }

                ToMqttPublisherMessage::CoolmasterConnected(connected) => {
                    let topic = format!("Aircondition/Coolmaster/{}", self.controller_name);

                    debug!(
                        topic = %topic,
                        connected,
                        "Publishing coolmaster connection status"
                    );

                    let mut props = PublishProperties::default();
                    if let Some(tp) = tracing_init::traceparent::current() {
                        props.user_properties.push(("traceparent".into(), tp));
                    }
                    self.mqtt_client
                        .publish_with_properties(
                            topic,
                            QoS::AtLeastOnce,
                            true,
                            serde_json::to_vec(&connected).unwrap(),
                            props,
                        )
                        .await
                        .map_err(|e| MqttError::ApiError(e.to_string(), "Publish coolmaster connected".to_owned()))?;
                }
            }
        }
    }

    async fn publish_if_modified(&mut self, unit_state: &UnitState) -> Result<(), Report<MqttError>> {
        let unit = &unit_state.unit;
        let old_unit_state = self.unit_states.get(unit);
        if old_unit_state.is_none() || old_unit_state.unwrap() != unit_state {
            self.unit_states.insert(unit.clone(), unit_state.clone());
            self.publish_unit_state(unit_state).await?;
        }

        Ok(())
    }

    async fn publish_unit_state(&mut self, unit_state: &UnitState) -> Result<(), Report<MqttError>> {
        let topic = format!(
            "Aircondition/State/{}/{}",
            self.controller_name, unit_state.unit
        );

        debug!(
            topic = %topic,
            unit = %unit_state.unit,
            "Publishing unit state"
        );

        let mut props = PublishProperties::default();
        if let Some(tp) = tracing_init::traceparent::current() {
            props.user_properties.push(("traceparent".into(), tp));
        }

        self.mqtt_client
            .publish_with_properties(
                &topic,
                QoS::AtLeastOnce,
                true,
                serde_json::to_vec(unit_state).unwrap(),
                props,
            )
            .await
            .map_err(|e| MqttError::ApiError(e.to_string(), format!("Publish unit {} state", unit_state.unit)).into())
    }
}
