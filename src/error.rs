use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoolmasterError {
    // #[error("Connection error")]
    // ConnectionError(#[from] std::io::Error),

    #[error("Invalid coolmaster address")]
    InvalidCoolmasterAddress(String),

    #[error("Invalid coolmaster port")]
    InvalidCoolmasterPort(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Coolmaster error: {0}")]
    CoolmasterCommandError(String),

    #[error("Invalid unit state")]
    InvalidUnitState(&'static str, String),

    #[error("Invalid temperature")]
    InvalidTemperature(String),

    #[error("Send to mqtt publisher channel failed")]
    SendToMqttPublisherChannelFailed,

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("In context of '{0}'")]
    Context(String),
}

#[derive(Debug, Error)]
pub enum MqttError {
    #[error("Error: {0} In context of '{1}'")]
    ApiError(String, String),

    #[error("In context of '{0}'")]
    Context(String),
}