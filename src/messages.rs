use crate::ac_unit::{UnitState, FanSpeed, OperationMode};

#[derive(Debug)]
pub enum ToCoolmasterMessage {
    PublishUnitState(String),
    PublishUnitsState,
    SetUnitPower(String, bool),
    SetUnitMode(String, OperationMode),
    SetFanSpeed(String, FanSpeed),
    SetTargetTemperature(String, f32),
    ResetFilter(String),
}

#[derive(Debug)]
pub enum ToMqttPublisherMessage {
    Error(String),
    UnitState(UnitState),
    UnitsState(Vec<UnitState>),
}

