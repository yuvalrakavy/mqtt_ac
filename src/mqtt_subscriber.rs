use async_channel::Sender;
use error_stack::{Report, ResultExt};
use serde::Deserialize;

use tracing::{debug, info, Instrument};

use crate::{
    ac_unit::{FanSpeed, OperationMode},
    error::MqttError,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
    mqtt_pump::{Pump, PumpEvent},
};
use rumqttc::v5::{self, mqttbytes::QoS, mqttbytes::v5::PublishProperties};

#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
enum Action {
    SetPower { power: bool },
    TargetTemperature { temperature: f32 },
    SetMode { mode: OperationMode },
    SetFanSpeed { fan_speed: FanSpeed },
    ResetFilter,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Operation {
    Action(Action),
    ActionList(Vec<Action>),
}

#[derive(Debug, Deserialize)]
struct Command {
    unit: String,
    operation: Operation,
}

/// The bridge's MQTT session. It never polls: the pump polls in a task of its own and forwards
/// what arrives, so this session may wait on the Coolmaster worker's queue, the publisher's queue
/// and rumqttc's request channel — back-pressure, which the pump's polling keeps moving (Store
/// no-hang §14.3). It ends when the pump does, and the worker starts a new one.
pub async fn session(
    mqtt_event_loop: v5::EventLoop,
    mqtt_client: v5::AsyncClient,
    controller_name: String,
    to_coolmaster_channel: Sender<ToCoolmasterMessage>,
    to_mqtt_publish_channel: Sender<ToMqttPublisherMessage>,
) -> Result<(), Report<MqttError>> {
    let into_context = || MqttError::Context("MQTT subscriber session".to_string());
    let command_topic = format!("Aircondition/Command/{controller_name}");
    let active_topic = format!("Aircondition/Active/{controller_name}");
    let version_topic = format!("Aircondition/Version/{controller_name}");
    let version = crate::get_version();
    // Polling first, so every publish below has an event loop draining it.
    let (_pump, mut incoming) = Pump::start(mqtt_event_loop);

    loop {
        debug!("Waiting for MQTT message");

        let publish_packet = match incoming.recv().await { // WAIT: mqtt-pump-queue
            Some(PumpEvent::Publish(publish_packet)) => publish_packet,
            Some(PumpEvent::Connected) => {
                announce(&mqtt_client, &active_topic, &version_topic, &version, &command_topic)
                    .await
                    .change_context_lazy(into_context)?;
                continue;
            }
            Some(PumpEvent::Ended(e)) => {
                return Err(MqttError::ApiError(e, "Too many consecutive MQTT poll errors".to_string()).into())
            }
            None => return Err(MqttError::Context("the MQTT pump stopped".to_string()).into()),
        };

        let topic = String::from_utf8_lossy(&publish_packet.topic).into_owned();
        debug!(topic = %topic, "Received MQTT message");

        // Extract inbound traceparent and attach to the handling span.
        let traceparent = publish_packet
            .properties
            .as_ref()
            .and_then(|p| {
                p.user_properties
                    .iter()
                    .find(|(k, _)| k == "traceparent")
                    .map(|(_, v)| v.clone())
            });

        let span = tracing::info_span!("mqtt_command", topic = %topic);
        if let Some(tp) = &traceparent {
            tracing_init::traceparent::set_remote_parent(&span, tp);
        }

        // Span entry via .instrument(): never hold span.enter()
        // across .await — on the multi-thread runtime it corrupts
        // the current-span thread-local (fleet logging policy).
        let handled: Result<(), Report<MqttError>> = async {
            match serde_json::from_slice::<Command>(&publish_packet.payload) {
                Ok(Command {
                    unit,
                    operation: Operation::Action(action),
                }) => {
                    perform_action(&unit, &action, &to_coolmaster_channel).await?;
                    // WAIT: coolmaster-queue
                    to_coolmaster_channel
                        .send(ToCoolmasterMessage::PublishUnitState(unit))
                        .await
                        .change_context_lazy(into_context)?;
                }
                Ok(Command {
                    unit,
                    operation: Operation::ActionList(actions),
                }) => {
                    for action in actions {
                        perform_action(&unit, &action, &to_coolmaster_channel).await?;
                    }

                    // WAIT: coolmaster-queue
                    to_coolmaster_channel
                        .send(ToCoolmasterMessage::PublishUnitState(unit))
                        .await
                        .change_context_lazy(into_context)?;
                }
                Err(e) => {
                    // Malformed payload is a designed degradation (user error), not a code bug.
                    info!(
                        kind = "validation_rejected",
                        topic = %topic,
                        error = %e,
                        "MQTT command payload rejected: JSON parse failed"
                    );
                    // WAIT: publisher-queue
                    to_mqtt_publish_channel
                        .send(ToMqttPublisherMessage::Error(format!(
                            "Error parsing MQTT command message: {e:?}"
                        )))
                        .await
                        .change_context_lazy(into_context)?;
                }
            }
            Ok(())
        }
        .instrument(span)
        .await;
        handled?;
    }
}

/// On every CONNACK — rumqttc reconnects inside one event loop, and the broker keeps no session:
/// the active flag, the version, and the command subscription. Made here, never by the pump, so a
/// full request channel holds this session until the pump drains it, instead of stopping the
/// polling that drains it.
async fn announce(
    mqtt_client: &v5::AsyncClient,
    active_topic: &str,
    version_topic: &str,
    version: &str,
    command_topic: &str,
) -> Result<(), Report<MqttError>> {
    // Stamp outbound publishes with traceparent if a trace is active.
    let mut props = PublishProperties::default();
    if let Some(tp) = tracing_init::traceparent::current() {
        props.user_properties.push(("traceparent".into(), tp));
    }

    // WAIT: mqtt-request
    mqtt_client
        .publish_with_properties(active_topic, QoS::AtLeastOnce, true, "true".as_bytes(), props.clone())
        .await
        .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Active".to_string()))?;

    // WAIT: mqtt-request
    mqtt_client
        .publish_with_properties(version_topic, QoS::AtLeastOnce, true, version.as_bytes().to_vec(), props)
        .await
        .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Version".to_string()))?;

    // WAIT: mqtt-request
    mqtt_client
        .subscribe(command_topic, QoS::AtLeastOnce)
        .await
        .map_err(|e| MqttError::ApiError(e.to_string(), "Subscribe to commands".to_string()))?;

    Ok(())
}

async fn perform_action(
    unit: &str,
    action: &Action,
    to_coolmaster_channel: &Sender<ToCoolmasterMessage>,
) -> Result<(), Report<MqttError>> {
    let to_coolmaster_message = get_coolmaster_message_from_action(unit, action);

    // WAIT: coolmaster-queue
    to_coolmaster_channel
        .send(to_coolmaster_message)
        .await
        .change_context_lazy(|| {
            MqttError::Context(String::from(
                "Sending action to coolmaster channel",
            ))
        })
}

fn get_coolmaster_message_from_action(unit: &str, action: &Action) -> ToCoolmasterMessage {
    match action {
        Action::SetPower { power } => ToCoolmasterMessage::SetUnitPower(unit.to_string(), *power),
        Action::TargetTemperature { temperature } => {
            ToCoolmasterMessage::SetTargetTemperature(unit.to_string(), *temperature)
        }
        Action::SetMode { mode } => {
            ToCoolmasterMessage::SetUnitMode(unit.to_string(), mode.clone())
        }
        Action::SetFanSpeed { fan_speed } => {
            ToCoolmasterMessage::SetFanSpeed(unit.to_string(), fan_speed.clone())
        }
        Action::ResetFilter => ToCoolmasterMessage::ResetFilter(unit.to_string()),
    }
}
