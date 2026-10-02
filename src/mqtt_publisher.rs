use async_channel::Receiver;
use error_stack::{Report, ResultExt};
use std::collections::HashMap;
use tokio::sync::Notify;

use tracing::debug;

use crate::ac_unit::UnitState;
use crate::error::MqttError;
use crate::messages::ToMqttPublisherMessage;
use rumqttc::v5::{self, mqttbytes::QoS, mqttbytes::v5::PublishProperties};

/// Publishes what the Coolmaster worker and the session report. It outlives the MQTT sessions —
/// the MQTT worker keeps it and lends it to each — so it knows the retained state it published
/// across all of them: a unit's state is published only when it changed, and everything it knows
/// is published again after every CONNACK, since a reconnect without a session drops whatever
/// rumqttc still held (rumqttc 0.25, `EventLoop::clean`, then `pending.clear()`).
pub struct MqttPublisher {
    controller_name: String,
    unit_states: HashMap<String, UnitState>,
    coolmaster_connected: Option<bool>,
    to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
}

impl MqttPublisher {
    pub fn new(
        controller_name: String,
        to_mqtt_publisher_channel: Receiver<ToMqttPublisherMessage>,
    ) -> Self {
        MqttPublisher {
            controller_name,
            unit_states: HashMap::new(),
            coolmaster_connected: None,
            to_mqtt_publisher_channel,
        }
    }

    /// One MQTT session's publishing, on its client. Ends with an error when a publish fails (the
    /// session's event loop is gone) or the queue closes. `reconnected` is the session's word that
    /// the broker accepted a connection.
    pub async fn session(
        &mut self,
        mqtt_client: &v5::AsyncClient,
        reconnected: &Notify,
    ) -> Result<(), Report<MqttError>> {
        let into_context = || MqttError::Context("MQTT Publisher session".to_string());

        loop {
            // WAIT: publisher-messages
            let message = tokio::select! {
                _ = reconnected.notified() => {
                    self.republish(mqtt_client).await?;
                    continue;
                }
                message = self.to_mqtt_publisher_channel.recv() => {
                    message.change_context_lazy(into_context)?
                }
            };

            match message {
                ToMqttPublisherMessage::UnitState(unit_state) => self
                    .publish_if_modified(mqtt_client, unit_state)
                    .await
                    .change_context_lazy(into_context)?,

                ToMqttPublisherMessage::UnitsState(unit_states) => {
                    for unit_state in unit_states {
                        self.publish_if_modified(mqtt_client, unit_state)
                            .await
                            .change_context_lazy(into_context)?;
                    }
                }

                ToMqttPublisherMessage::Error(error_message) => {
                    let topic = format!("Aircondition/Error/{}", self.controller_name);
                    debug!(topic = %topic, "Publishing error message");
                    let payload = serde_json::to_vec(&error_message).unwrap();
                    // WAIT: mqtt-request
                    mqtt_client
                        .publish_with_properties(topic, QoS::AtLeastOnce, true, payload, props())
                        .await
                        .map_err(|e| {
                            MqttError::ApiError(e.to_string(), "Publish error".to_owned())
                        })?;
                }

                ToMqttPublisherMessage::CoolmasterConnected(connected) => {
                    self.coolmaster_connected = Some(connected);
                    self.publish_coolmaster_connected(mqtt_client, connected)
                        .await?;
                }
            }
        }
    }

    /// After a CONNACK: everything known, again.
    async fn republish(&self, mqtt_client: &v5::AsyncClient) -> Result<(), Report<MqttError>> {
        debug!(
            units = self.unit_states.len(),
            "Publishing the known state again after a reconnect"
        );
        if let Some(connected) = self.coolmaster_connected {
            self.publish_coolmaster_connected(mqtt_client, connected)
                .await?;
        }
        for unit_state in self.unit_states.values() {
            self.publish_unit_state(mqtt_client, unit_state).await?;
        }
        Ok(())
    }

    async fn publish_coolmaster_connected(
        &self,
        mqtt_client: &v5::AsyncClient,
        connected: bool,
    ) -> Result<(), Report<MqttError>> {
        let topic = format!("Aircondition/Coolmaster/{}", self.controller_name);
        debug!(topic = %topic, connected, "Publishing coolmaster connection status");
        let payload = serde_json::to_vec(&connected).unwrap();
        // WAIT: mqtt-request
        mqtt_client
            .publish_with_properties(topic, QoS::AtLeastOnce, true, payload, props())
            .await
            .map_err(|e| {
                let context = "Publish coolmaster connected".to_owned();
                MqttError::ApiError(e.to_string(), context).into()
            })
    }

    async fn publish_if_modified(
        &mut self,
        mqtt_client: &v5::AsyncClient,
        unit_state: UnitState,
    ) -> Result<(), Report<MqttError>> {
        if self.unit_states.get(&unit_state.unit) != Some(&unit_state) {
            self.publish_unit_state(mqtt_client, &unit_state).await?;
            self.unit_states.insert(unit_state.unit.clone(), unit_state);
        }
        Ok(())
    }

    async fn publish_unit_state(
        &self,
        mqtt_client: &v5::AsyncClient,
        unit_state: &UnitState,
    ) -> Result<(), Report<MqttError>> {
        let topic = format!(
            "Aircondition/State/{}/{}",
            self.controller_name, unit_state.unit
        );
        debug!(topic = %topic, unit = %unit_state.unit, "Publishing unit state");
        let payload = serde_json::to_vec(unit_state).unwrap();
        // WAIT: mqtt-request
        mqtt_client
            .publish_with_properties(&topic, QoS::AtLeastOnce, true, payload, props())
            .await
            .map_err(|e| {
                let context = format!("Publish unit {} state", unit_state.unit);
                MqttError::ApiError(e.to_string(), context).into()
            })
    }
}

/// Outbound publishes carry the current trace's traceparent, if a trace is active.
fn props() -> PublishProperties {
    let mut props = PublishProperties::default();
    if let Some(tp) = tracing_init::traceparent::current() {
        props.user_properties.push(("traceparent".into(), tp));
    }
    props
}
