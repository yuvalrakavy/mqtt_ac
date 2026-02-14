use async_channel::Sender;
use error_stack::{Report, ResultExt};
use serde::Deserialize;
use std::time::Duration;

use log::{debug, error, info};

use crate::{
    ac_unit::{FanSpeed, OperationMode},
    error::MqttError,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
};
use rumqttc::{self, Packet, QoS};

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
    mut mqtt_event_loop: rumqttc::EventLoop,
    mqtt_client: rumqttc::AsyncClient,
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
            Ok(event) => {
                consecutive_errors = 0;
                event
            }
            Err(e) => {
                consecutive_errors += 1;
                error!(
                    "MQTT poll error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
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

        match event {
            rumqttc::Event::Incoming(Packet::ConnAck(_)) => {
                info!("Connected to MQTT broker, setting up session");

                mqtt_client
                    .publish(&active_topic, QoS::AtLeastOnce, true, "true".as_bytes())
                    .await
                    .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Active".to_string()))
                    .change_context_lazy(into_context)?;

                mqtt_client
                    .publish(&version_topic, QoS::AtLeastOnce, true, version.as_bytes())
                    .await
                    .map_err(|e| MqttError::ApiError(e.to_string(), "Publish Version".to_string()))
                    .change_context_lazy(into_context)?;

                mqtt_client
                    .subscribe(&command_topic, QoS::AtLeastOnce)
                    .await
                    .map_err(|e| {
                        MqttError::ApiError(e.to_string(), "Subscribe to commands".to_string())
                    })
                    .change_context_lazy(into_context)?;
            }

            rumqttc::Event::Incoming(Packet::Publish(publish_packet)) => {
                debug!("Received MQTT message: {publish_packet:?}");

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
                        error!("Error parsing MQTT command message: {e:?}");
                        to_mqtt_publish_channel
                            .send(ToMqttPublisherMessage::Error(format!(
                                "Error parsing MQTT command message: {e:?}"
                            )))
                            .await
                            .change_context_lazy(into_context)?;
                    }
                }
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
