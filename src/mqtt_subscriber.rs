use tokio::sync::mpsc::Sender;
use serde::{Deserialize};

use log::{info, debug, error};

use rumqttc::{self, Packet};
use crate::{messages::{ToCoolmasterMessage, ToMqttPublisherMessage}, ac_unit::{OperationMode,FanSpeed}};

#[derive(Debug, Deserialize)]
#[serde(tag="command")]
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

pub async fn mqtt_subscriber_worker(
    mut mqtt_event_loop: rumqttc::EventLoop,
    to_coolmaster_channel: Sender<ToCoolmasterMessage>,
    to_mqtt_publish_channel: Sender<ToMqttPublisherMessage>,
) {
    loop {
        info!("Waiting for MQTT message");
        match mqtt_event_loop.poll().await {
            Ok(notification) => {
                if let rumqttc::Event::Incoming(Packet::Publish(publish_packet)) = notification {
                    debug!("Received MQTT message: {:?}", publish_packet);
                    match serde_json::from_slice::<Command>(&publish_packet.payload) {
                        Ok(Command { unit, operation: Operation::Action(action) }) => {
                            perform_action(&unit, &action, &to_coolmaster_channel).await;
                            to_coolmaster_channel.send(ToCoolmasterMessage::PublishUnitState(unit)).await.unwrap();
                        },
                        Ok(Command { unit, operation: Operation::ActionList(actions)}) => {
                            for action in actions {
                                perform_action(&unit, &action, &to_coolmaster_channel).await;
                            }

                            to_coolmaster_channel.send(ToCoolmasterMessage::PublishUnitState(unit)).await.unwrap();
                        },
                        Err(e) => {
                            error!("Error parsing MQTT command message: {:?}", e);
                            to_mqtt_publish_channel.send(ToMqttPublisherMessage::Error(format!("Error parsing MQTT command message: {:?}", e))).await.unwrap();
                        }
                    }
                }
            },

            Err(error) => {
                error!("MQTT notification error: {:?}", error);
            }
        }
    }
}

async fn perform_action(unit: &str, action: &Action, to_coolmaster_channel: &Sender<ToCoolmasterMessage>) {
    let to_coolmaster_message =  get_coolmaster_message_from_action(unit, action);
    to_coolmaster_channel.send(to_coolmaster_message).await.unwrap();
}

fn get_coolmaster_message_from_action(unit: &str, action: &Action) -> ToCoolmasterMessage {
    match action {
        Action::SetPower { power } => ToCoolmasterMessage::SetUnitPower(unit.to_string(), *power),
        Action::TargetTemperature { temperature } => ToCoolmasterMessage::SetTargetTemperature(unit.to_string(), *temperature),
        Action::SetMode { mode } => ToCoolmasterMessage::SetUnitMode(unit.to_string(), mode.clone()),
        Action::SetFanSpeed { fan_speed } => ToCoolmasterMessage::SetFanSpeed(unit.to_string(), fan_speed.clone()),
        Action::ResetFilter => ToCoolmasterMessage::ResetFilter(unit.to_string()),
    }
}
