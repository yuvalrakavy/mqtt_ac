use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoolmasterError {
    #[error("Connection error")]
    ConnectionError(#[from] std::io::Error),

    #[error("Invalid coolmaster address")]
    InvalidCoolmasterAddress(String),

    #[error("Invalid coolmaster port")]
    InvalidCoolmasterPort(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Invalid reply")]
    InvalidReply(#[from] std::string::FromUtf8Error),

    #[error("Coolmaster error: {0}")]
    CoolmasterCommandError(String),

    #[error("Invalid unit state")]
    InvalidUnitState(&'static str, String),

    #[error("Invalid temperature")]
    InvalidTemperature(String),

    #[error("Send to mqtt publisher channel failed")]
    SendToMqttPublisherChannelFailed,
}
