use async_channel::Receiver;
use error_stack::{Report, ResultExt};
use std::sync::Arc;
use tokio::sync::Notify;

use tracing::debug;

use crate::ac_unit::UnitState;
use crate::error::MqttError;
use crate::messages::ToMqttPublisherMessage;
use crate::reports::{Changes, Model, Reports};
use rumqttc::v5::{self, mqttbytes::QoS, mqttbytes::v5::PublishProperties};

/// Publishes what the Coolmaster worker reports and what the session hands it. It outlives the MQTT
/// sessions — the MQTT worker keeps it and lends it to each. The retained state it publishes is the
/// worker's model (`Reports`), which the worker updates without waiting: a unit's state is
/// published when it changed, and the whole model again after every CONNACK, since a reconnect
/// without a session drops whatever rumqttc still held (rumqttc 0.25, `EventLoop::clean`, then
/// `pending.clear()`) and a broker that restarted may have lost its retained messages. A publish
/// cut short by the end of its session loses nothing: the model already holds the report, and the
/// next session publishes it after its CONNACK (re-review C2).
pub struct MqttPublisher {
    controller_name: String,
    reports: Arc<Reports>,
    session_errors: Receiver<ToMqttPublisherMessage>,
}

impl MqttPublisher {
    pub fn new(
        controller_name: String,
        reports: Arc<Reports>,
        session_errors: Receiver<ToMqttPublisherMessage>,
    ) -> Self {
        MqttPublisher {
            controller_name,
            reports,
            session_errors,
        }
    }

    /// One MQTT session's publishing, on its client. Ends with an error when a publish fails (the
    /// session's event loop is gone) or the session's queue closes. `reconnected` is the session's
    /// word that the broker accepted a connection.
    pub async fn session(
        &mut self,
        mqtt_client: &v5::AsyncClient,
        reconnected: &Notify,
    ) -> Result<(), Report<MqttError>> {
        let into_context = || MqttError::Context("MQTT Publisher session".to_string());

        loop {
            // WAIT: publisher-messages
            tokio::select! {
                _ = reconnected.notified() => {
                    let model = self.reports.model();
                    self.republish(mqtt_client, model).await?;
                }
                _ = self.reports.reported() => {
                    let changes = self.reports.take();
                    self.publish_changes(mqtt_client, changes).await?;
                }
                message = self.session_errors.recv() => {
                    let ToMqttPublisherMessage::Error(error) = message.change_context_lazy(into_context)?;
                    self.publish_error(mqtt_client, error).await?;
                }
            }
        }
    }

    /// After a CONNACK: everything known, again.
    async fn republish(&self, mqtt_client: &v5::AsyncClient, model: Model) -> Result<(), Report<MqttError>> {
        debug!(
            units = model.units.len(),
            "Publishing the known state again after a reconnect"
        );
        if let Some(connected) = model.connected {
            self.publish_coolmaster_connected(mqtt_client, connected)
                .await?;
        }
        for unit_state in &model.units {
            self.publish_unit_state(mqtt_client, unit_state).await?;
        }
        Ok(())
    }

    /// What the Coolmaster worker reported since the last take.
    async fn publish_changes(&self, mqtt_client: &v5::AsyncClient, changes: Changes) -> Result<(), Report<MqttError>> {
        if let Some(connected) = changes.connected {
            self.publish_coolmaster_connected(mqtt_client, connected)
                .await?;
        }
        for unit_state in &changes.units {
            self.publish_unit_state(mqtt_client, unit_state).await?;
        }
        for error in changes.errors {
            self.publish_error(mqtt_client, error).await?;
        }
        Ok(())
    }

    async fn publish_error(&self, mqtt_client: &v5::AsyncClient, error: String) -> Result<(), Report<MqttError>> {
        let topic = format!("Aircondition/Error/{}", self.controller_name);
        debug!(topic = %topic, "Publishing error message");
        let payload = serde_json::to_vec(&error).unwrap();
        // WAIT: mqtt-request
        mqtt_client
            .publish_with_properties(topic, QoS::AtLeastOnce, true, payload, props())
            .await
            .map_err(|e| MqttError::ApiError(e.to_string(), "Publish error".to_owned()).into())
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

#[cfg(test)]
mod tests;
