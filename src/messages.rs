use crate::ac_unit::{FanSpeed, OperationMode};

/// A command for the Coolmaster worker, posted to its mailbox (`mailbox.rs` classifies them).
#[derive(Debug, Clone, PartialEq)]
pub enum ToCoolmasterMessage {
    PublishUnitState(String),
    PublishUnitsState,
    SetUnitPower(String, bool),
    SetUnitMode(String, OperationMode),
    SetFanSpeed(String, FanSpeed),
    SetTargetTemperature(String, f32),
    ResetFilter(String),
}

/// What the MQTT session hands the publisher, through its bounded queue: back-pressure toward the
/// broker, which the pump keeps moving. The Coolmaster worker reports through `Reports` instead,
/// which never waits.
#[derive(Debug)]
pub enum ToMqttPublisherMessage {
    /// A malformed or refused command's error, for the error topic.
    Error(String),
}
