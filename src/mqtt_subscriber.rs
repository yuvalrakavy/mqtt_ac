use async_channel::Sender;
use error_stack::{Result, ResultExt};
use serde::Deserialize;

use log::{debug, error};

use crate::{
    ac_unit::{FanSpeed, OperationMode},
    error::MqttError,
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
};
use rumqttc::{self, Packet};

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
    to_coolmaster_channel: Sender<ToCoolmasterMessage>,
    to_mqtt_publish_channel: Sender<ToMqttPublisherMessage>,
) -> Result<(), MqttError> {
    let into_context = || MqttError::Context("MQTT subscriber session".to_string());

    loop {
        debug!("Waiting for MQTT message");

        let event = mqtt_event_loop
            .poll()
            .await
            .map_err(|e| MqttError::ApiError(e.to_string(), "Polling MQTT event loop".to_string()))?;

        if let rumqttc::Event::Incoming(Packet::Publish(publish_packet)) = event {
            debug!("Received MQTT message: {:?}", publish_packet);

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
                    error!("Error parsing MQTT command message: {:?}", e);
                    to_mqtt_publish_channel
                        .send(ToMqttPublisherMessage::Error(format!(
                            "Error parsing MQTT command message: {:?}",
                            e
                        )))
                        .await
                        .change_context_lazy(into_context)?;
                }
            }
        }
    }
}

async fn perform_action(
    unit: &str,
    action: &Action,
    to_coolmaster_channel: &Sender<ToCoolmasterMessage>,
) -> Result<(), MqttError> {
    let to_coolmaster_message = get_coolmaster_message_from_action(unit, action);

    to_coolmaster_channel
        .send(to_coolmaster_message)
        .await
        .change_context_lazy(|| {
            MqttError::Context(String::from(
                "Sending action to MQTT publisher channel action",
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
