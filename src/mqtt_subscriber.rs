use async_channel::Sender;
use error_stack::{Report, ResultExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Notify;

use tracing::{debug, info, Instrument};

use crate::{
    ac_unit::{FanSpeed, OperationMode},
    error::MqttError,
    mailbox::{refusal_text, Mailbox, Posted},
    messages::{ToCoolmasterMessage, ToMqttPublisherMessage},
    mqtt_pump::{BrokerOutage, Pump, PumpEvent},
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
/// what arrives. It never waits on the Coolmaster either: commands go to the Coolmaster's mailbox,
/// which never waits (the owner's device-down policy), so every CONNACK is read and answered —
/// resubscribed and announced — whatever the Coolmaster is doing. It may wait on the publisher's
/// queue and on rumqttc's request channel: back-pressure, which the pump's polling keeps moving
/// (Store no-hang §14.3). It ends when the pump does, and the worker starts a new session.
pub async fn session(
    mqtt_event_loop: v5::EventLoop,
    mqtt_client: v5::AsyncClient,
    controller_name: String,
    mailbox: &Mailbox,
    to_mqtt_publish_channel: Sender<ToMqttPublisherMessage>,
    reconnected: &Notify,
    outage: Arc<BrokerOutage>,
) -> Result<(), Report<MqttError>> {
    let into_context = || MqttError::Context("MQTT subscriber session".to_string());
    let command_topic = format!("Aircondition/Command/{controller_name}");
    let active_topic = format!("Aircondition/Active/{controller_name}");
    let version_topic = format!("Aircondition/Version/{controller_name}");
    let version = crate::get_version();
    // Polling first, so every publish below has an event loop draining it.
    let (_pump, mut incoming) = Pump::start(mqtt_event_loop, outage);

    loop {
        debug!("Waiting for MQTT message");

        // WAIT: mqtt-pump-queue
        let event = incoming.recv().await;
        let publish_packet = match event {
            Some(PumpEvent::Publish(publish_packet)) => publish_packet,
            Some(PumpEvent::Connected) => {
                let topics = [&command_topic, &active_topic, &version_topic];
                announce(&mqtt_client, topics, &version)
                    .await
                    .change_context_lazy(into_context)?;
                // A reconnect without a session dropped whatever rumqttc still held: the
                // publisher sends again the retained state it knows.
                reconnected.notify_one();
                continue;
            }
            Some(PumpEvent::Ended(e)) => {
                let context = "Too many consecutive MQTT poll errors".to_string();
                return Err(MqttError::ApiError(e, context).into());
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
                Ok(Command { unit, operation }) => {
                    let actions = match operation {
                        Operation::Action(action) => vec![action],
                        Operation::ActionList(actions) => actions,
                    };
                    for action in &actions {
                        let command = get_coolmaster_message_from_action(&unit, action);
                        submit(command, mailbox, &to_mqtt_publish_channel).await?;
                    }
                    // Then the unit's state, so the change shows. A read is never refused.
                    let read = ToCoolmasterMessage::PublishUnitState(unit);
                    submit(read, mailbox, &to_mqtt_publish_channel).await?;
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
/// the command subscription, then the active flag and the version (`topics` is the command, the
/// active and the version topic). Made here, never by the pump, so a full request channel holds
/// this session until the pump drains it, instead of stopping the polling that drains it.
async fn announce(
    mqtt_client: &v5::AsyncClient,
    topics: [&String; 3],
    version: &str,
) -> Result<(), Report<MqttError>> {
    let [command_topic, active_topic, version_topic] = topics;
    let failed = |what: &str| {
        let what = what.to_string();
        move |e: rumqttc::v5::ClientError| MqttError::ApiError(e.to_string(), what)
    };
    // Stamp outbound publishes with traceparent if a trace is active.
    let mut props = PublishProperties::default();
    if let Some(tp) = tracing_init::traceparent::current() {
        props.user_properties.push(("traceparent".into(), tp));
    }

    // WAIT: mqtt-request
    mqtt_client
        .subscribe(command_topic, QoS::AtLeastOnce)
        .await
        .map_err(failed("Subscribe to commands"))?;

    let active = "true".as_bytes();
    // WAIT: mqtt-request
    mqtt_client
        .publish_with_properties(active_topic, QoS::AtLeastOnce, true, active, props.clone())
        .await
        .map_err(failed("Publish Active"))?;

    let version = version.as_bytes().to_vec();
    // WAIT: mqtt-request
    mqtt_client
        .publish_with_properties(version_topic, QoS::AtLeastOnce, true, version, props)
        .await
        .map_err(failed("Publish Version"))?;

    Ok(())
}

/// Post a command to the Coolmaster's mailbox, which never waits. A refused one is reported on the
/// error topic, through the publisher's queue: back-pressure toward the broker, never the
/// Coolmaster.
async fn submit(
    command: ToCoolmasterMessage,
    mailbox: &Mailbox,
    publisher: &Sender<ToMqttPublisherMessage>,
) -> Result<(), Report<MqttError>> {
    if let Posted::Refused(why) = mailbox.post(command.clone()) {
        let text = refusal_text(&command, why);
        // WAIT: publisher-queue
        publisher
            .send(ToMqttPublisherMessage::Error(text))
            .await
            .change_context_lazy(|| MqttError::Context("Reporting a refused command".to_string()))?;
    }
    Ok(())
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
