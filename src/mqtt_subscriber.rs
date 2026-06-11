use async_channel::Sender;
use error_stack::{Report, ResultExt};
use serde::Deserialize;
use std::time::Duration;

use tracing::{debug, info, warn, Instrument};

use crate::{
    ac_unit::{FanSpeed, OperationMode},
    error::MqttError,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
};
use rumqttc::v5::{self, mqttbytes::QoS, mqttbytes::v5::Packet};

const MAX_CONSECUTIVE_ERRORS: u32 = 5;

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

pub async fn session(
    mut mqtt_event_loop: v5::EventLoop,
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
    let mut consecutive_errors: u32 = 0;

    loop {
        debug!("Waiting for MQTT message");

        let event = match mqtt_event_loop.poll().await {
            Ok(event) => event,
            Err(e) => {
                consecutive_errors += 1;
                // Per-iteration poll error is INFO (transient, recovers); the threshold
                // cross is the WARN that signals a persistent outage.
                if consecutive_errors < MAX_CONSECUTIVE_ERRORS {
                    info!(
                        kind = "external_failure",
                        error = %e,
                        consecutive_errors,
                        max = MAX_CONSECUTIVE_ERRORS,
                        "MQTT poll error"
                    );
                } else {
                    warn!(
                        kind = "external_failure",
                        error = %e,
                        consecutive_errors,
                        max = MAX_CONSECUTIVE_ERRORS,
                        "MQTT poll error threshold reached, giving up"
                    );
                    return Err(MqttError::ApiError(
                        e.to_string(),
                        "Too many consecutive MQTT poll errors".to_string(),
                    )
                    .into());
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        // Only reset consecutive_errors on Incoming events (proof of broker
        // connectivity). Outgoing events (e.g. ConnectRequest) are emitted by
        // rumqttc during reconnection attempts and must NOT reset the counter.
        if matches!(event, v5::Event::Incoming(_)) {
            consecutive_errors = 0;
        }

        match event {
            v5::Event::Incoming(Packet::ConnAck(_)) => {
                info!(kind = "external_recovered", "Connected to MQTT broker, setting up session");

                // Use non-blocking try_publish/try_subscribe to avoid deadlock:
                // the subscriber is the only task calling poll() which drains the
                // internal rumqttc channel. If we block here on a full channel,
                // poll() never runs and the channel never drains.

                // Stamp outbound publishes with traceparent if a trace is active.
                let mut props = rumqttc::v5::mqttbytes::v5::PublishProperties::default();
                if let Some(tp) = tracing_init::traceparent::current() {
                    props.user_properties.push(("traceparent".into(), tp));
                }

                mqtt_client
                    .try_publish_with_properties(
                        &active_topic,
                        QoS::AtLeastOnce,
                        true,
                        "true".as_bytes(),
                        props.clone(),
                    )
                    .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Active".to_string()))
                    .change_context_lazy(into_context)?;

                mqtt_client
                    .try_publish_with_properties(
                        &version_topic,
                        QoS::AtLeastOnce,
                        true,
                        version.as_bytes().to_vec(),
                        props,
                    )
                    .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Version".to_string()))
                    .change_context_lazy(into_context)?;

                mqtt_client
                    .try_subscribe(&command_topic, QoS::AtLeastOnce)
                    .map_err(|e| {
                        MqttError::ApiError(e.to_string(), "Subscribe to commands".to_string())
                    })
                    .change_context_lazy(into_context)?;
            }

            v5::Event::Incoming(Packet::Publish(publish_packet)) => {
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

            _ => {}
        }
    }
}

async fn perform_action(
    unit: &str,
    action: &Action,
    to_coolmaster_channel: &Sender<ToCoolmasterMessage>,
) -> Result<(), Report<MqttError>> {
    let to_coolmaster_message = get_coolmaster_message_from_action(unit, action);

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
