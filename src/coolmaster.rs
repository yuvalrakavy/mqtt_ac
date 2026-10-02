use async_channel::{Receiver, Sender};
use error_stack::{Report, ResultExt};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use tracing::{info, warn};

use crate::ac_unit::{self, UnitState};
use crate::error::CoolmasterError;
use crate::messages::{ToCoolmasterMessage, ToMqttPublisherMessage};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Execute a future with a timeout, mapping errors to CoolmasterError.
async fn timed<T>(
    fut: impl std::future::Future<Output = std::io::Result<T>>,
    description: &str,
) -> Result<T, Report<CoolmasterError>> {
    match timeout(COMMAND_TIMEOUT, fut).await { // WAIT: coolmaster-io
        Ok(Ok(value)) => Ok(value),
        Ok(Err(io_err)) => Err(CoolmasterError::IoError(io_err).into()),
        Err(_) => Err(CoolmasterError::Timeout(description.to_string()).into()),
    }
}

pub struct Coolmaster {
    stream: Option<BufReader<TcpStream>>,
}

#[allow(dead_code)]
impl Coolmaster {
    pub async fn coolmaster_worker(
        coolmaster_address: &str,
        to_coolmaster_channel: Receiver<ToCoolmasterMessage>,
        to_mqtt_publisher_channel: Sender<ToMqttPublisherMessage>,
    ) {
        let mut coolmaster = Coolmaster::new();

        loop {
            // Work loop

            // WAIT: publisher-queue
            if to_mqtt_publisher_channel
                .send(ToMqttPublisherMessage::CoolmasterConnected(false))
                .await
                .is_err()
            {
                info!("MQTT publisher channel closed, exiting coolmaster worker");
                return;
            }

            loop {
                // Reconnect loop
                match coolmaster.connect_to(coolmaster_address).await {
                    Ok(_) => {
                        info!("Connected to coolmaster controller");
                        // WAIT: publisher-queue
                        if to_mqtt_publisher_channel
                            .send(ToMqttPublisherMessage::CoolmasterConnected(true))
                            .await
                            .is_err()
                        {
                            info!("MQTT publisher channel closed, exiting coolmaster worker");
                            return;
                        }
                        break;
                    }

                    Err(e) => {
                        info!("Failed to connect to coolmaster controller: {e}");
                        // WAIT: publisher-queue
                        let _ = to_mqtt_publisher_channel
                            .send(ToMqttPublisherMessage::Error(format!("{e:#?}")))
                            .await;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }

            loop {
                let message = match to_coolmaster_channel.recv().await { // WAIT: coolmaster-commands
                    Ok(msg) => msg,
                    Err(_) => {
                        info!("Coolmaster command channel closed, exiting worker");
                        return;
                    }
                };

                if let Err(e) = coolmaster
                    .handle_message(&message, &to_mqtt_publisher_channel)
                    .await
                {
                    // WAIT: publisher-queue
                    let _ = to_mqtt_publisher_channel
                        .send(ToMqttPublisherMessage::Error(format!(
                            "Failed to handle {message:#?} - {e}"
                        )))
                        .await;

                    if let Some(CoolmasterError::CoolmasterCommandError(cmd_err)) =
                        e.downcast_ref::<CoolmasterError>()
                    {
                        // Command rejected by coolmaster (e.g. invalid unit id): external device error.
                        warn!(
                            kind = "external_failure",
                            error = %cmd_err,
                            "Coolmaster command rejected"
                        );
                    } else {
                        // TCP-level failure: designed degradation, reconnect will fire.
                        info!(
                            kind = "connection_lost",
                            error = %e,
                            "Coolmaster connection lost, reconnecting"
                        );
                        coolmaster.stream = None;
                        break;
                    }
                }
            }
        }
    }

    fn new() -> Self {
        Coolmaster { stream: None }
    }

    fn split_host_port(host_port: &str) -> Result<(String, u16), Report<CoolmasterError>> {
        let mut host_port_parts = host_port.split(':');

        let host = host_port_parts
            .next()
            .ok_or_else(|| CoolmasterError::InvalidCoolmasterAddress(host_port.to_string()))?;
        let port_string = host_port_parts.next().unwrap_or("10102");
        let port = port_string
            .parse::<u16>()
            .map_err(|_| CoolmasterError::InvalidCoolmasterPort(host_port.to_string()))?;

        Ok((host.to_string(), port))
    }

    async fn connect_to(&mut self, host: &str) -> Result<(), Report<CoolmasterError>> {
        let into_context =
            || CoolmasterError::Context(format!("Connecting to coolmaster controller at {host}"));
        let (host, port) = Coolmaster::split_host_port(host).change_context_lazy(into_context)?;

        let stream = timed(
            TcpStream::connect(format!("{host}:{port}")),
            "TCP connect",
        )
        .await
        .change_context_lazy(into_context)?;

        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();

        // Get the initial '>' prompt
        let n = timed(reader.read_until(b'>', &mut bytes), "read initial prompt")
            .await
            .change_context_lazy(into_context)?;

        if n == 0 {
            return Err(CoolmasterError::ConnectionClosed.into());
        }

        self.stream = Some(reader);
        Ok(())
    }

    async fn handle_message(
        &mut self,
        message: &ToCoolmasterMessage,
        to_mqtt_publisher_channel: &Sender<ToMqttPublisherMessage>,
    ) -> Result<(), Report<CoolmasterError>> {
        match message {
            ToCoolmasterMessage::SetUnitPower(unit, power) => {
                self.set_unit_power(unit, *power).await?
            }

            ToCoolmasterMessage::PublishUnitState(unit) => {
                let unit_state = self.get_unit_state(unit).await?;
                // WAIT: publisher-queue
                let _ = to_mqtt_publisher_channel
                    .send(ToMqttPublisherMessage::UnitState(unit_state))
                    .await;
            }

            ToCoolmasterMessage::PublishUnitsState => {
                let units_state = self.get_units_state().await?;
                // WAIT: publisher-queue
                let _ = to_mqtt_publisher_channel
                    .send(ToMqttPublisherMessage::UnitsState(units_state))
                    .await;
            }
            ToCoolmasterMessage::SetUnitMode(unit, mode) => {
                self.set_unit_mode(unit, mode.clone()).await?
            }
            ToCoolmasterMessage::SetFanSpeed(unit, fan_speed) => {
                self.set_unit_fan_speed(unit, fan_speed).await?
            }
            ToCoolmasterMessage::SetTargetTemperature(unit, temperature) => {
                self.set_unit_target_temperature(unit, *temperature).await?
            }
            ToCoolmasterMessage::ResetFilter(unit) => self.reset_filter(unit).await?,
        }

        Ok(())
    }

    // Commands

    async fn set_unit_power(&mut self, unit: &str, power: bool) -> Result<(), Report<CoolmasterError>> {
        let _ = match power {
            true => self.command(&format!("on {unit}")).await?,
            false => self.command(&format!("off {unit}")).await?,
        };
        Ok(())
    }

    async fn set_unit_mode(
        &mut self,
        unit: &str,
        mode: ac_unit::OperationMode,
    ) -> Result<(), Report<CoolmasterError>> {
        let _ = match mode {
            ac_unit::OperationMode::Cool => self.command(&format!("cool {unit}")).await?,
            ac_unit::OperationMode::Heat => self.command(&format!("heat {unit}")).await?,
            ac_unit::OperationMode::Dry => self.command(&format!("dry {unit}")).await?,
            ac_unit::OperationMode::Fan => self.command(&format!("fan {unit}")).await?,
            ac_unit::OperationMode::Auto => self.command(&format!("auto {unit}")).await?,
        };
        Ok(())
    }

    async fn set_unit_target_temperature(
        &mut self,
        unit: &str,
        temperature: f32,
    ) -> Result<(), Report<CoolmasterError>> {
        let _ = self
            .command(&format!("temp {unit} {temperature}"))
            .await?;
        Ok(())
    }

    async fn set_unit_fan_speed(
        &mut self,
        unit: &str,
        speed: &ac_unit::FanSpeed,
    ) -> Result<(), Report<CoolmasterError>> {
        let speed = match speed {
            ac_unit::FanSpeed::VLow => "v",
            ac_unit::FanSpeed::Low => "l",
            ac_unit::FanSpeed::Medium => "m",
            ac_unit::FanSpeed::High => "h",
            ac_unit::FanSpeed::Top => "t",
            ac_unit::FanSpeed::Auto => "a",
        };

        self.command(&format!("fspeed {unit} {speed}")).await?;
        Ok(())
    }

    async fn reset_filter(&mut self, unit: &str) -> Result<(), Report<CoolmasterError>> {
        let _ = self.command(&format!("filt {unit}")).await?;
        Ok(())
    }

    async fn get_unit_state(&mut self, unit: &str) -> Result<UnitState, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context(format!("Getting state for unit {unit}"));
        let reply = self.command(&format!("ls2 {unit}")).await?;

        UnitState::from_str(&reply).change_context_lazy(into_context)
    }

    async fn get_units_state(&mut self) -> Result<Vec<UnitState>, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context("Getting states for all units".to_string());
        let reply = self.command("ls2").await?;
        let states = reply
            .lines()
            .map(UnitState::from_str)
            .collect::<Result<Vec<UnitState>, Report<CoolmasterError>>>()
            .change_context_lazy(into_context)?;

        Ok(states)
    }

    // Lower level functions to communicate with coolmaster controller

    async fn send_to_coolmaster(&mut self, command: &str) -> Result<(), Report<CoolmasterError>> {
        let into_context =
            || CoolmasterError::Context(format!("Sending command to coolmaster: '{command}'"));
        let reader = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let stream = reader.get_mut();

        timed(stream.write_all(command.as_bytes()), "send command")
            .await
            .change_context_lazy(into_context)?;

        if !command.ends_with('\r') && !command.ends_with('\n') {
            timed(stream.write_all(b"\r"), "send CR")
                .await
                .change_context_lazy(into_context)?;
        }

        Ok(())
    }

    async fn get_reply_from_coolmaster(&mut self) -> Result<String, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context("Getting reply from coolmaster".to_string());
        let reader = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut bytes = Vec::new();

        let n = timed(reader.read_until(b'>', &mut bytes), "read reply")
            .await
            .change_context_lazy(into_context)?;

        if n == 0 {
            return Err(CoolmasterError::ConnectionClosed.into());
        }

        if let Some(last_byte) = bytes.last() {
            if *last_byte == b'>' {
                bytes.pop();
            }
        }

        let reply = String::from_utf8(bytes).change_context_lazy(into_context)?;
        Coolmaster::parse_reply(&reply)
    }

    async fn command(&mut self, command: &str) -> Result<String, Report<CoolmasterError>> {
        self.send_to_coolmaster(command).await?;
        self.get_reply_from_coolmaster().await
    }

    fn parse_reply(reply: &str) -> Result<String, Report<CoolmasterError>> {
        let reply = reply.trim();
        let body_status_split = reply.rsplit_once("\r\n");

        match body_status_split {
            Some((body, status)) => Coolmaster::parse_status(body, status),
            None => Coolmaster::parse_status("", reply), // Reply body is empty, so the whole reply is the status
        }
    }

    fn parse_status(body: &str, status: &str) -> Result<String, Report<CoolmasterError>> {
        match status {
            "OK" | "ERROR: 0" => Ok(body.to_string()),
            _ => Err(CoolmasterError::CoolmasterCommandError(status.to_string()).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    const COOLMASTER_ADDRESS: &str = "10.0.1.70";

    fn set_logger() {
        _ = tracing_init::TracingInit::builder("mqtt_ac")
            .log_to_console(true)
            .init();
    }

    // Talks to the real Coolmaster, like the rest of this module's tests: run by hand only.
    #[tokio::test]
    #[ignore]
    async fn test_coolmaster_worker() {
        set_logger();

        let (to_coolmaster_tx, to_coolmaster_rx) = async_channel::bounded(10);
        let (to_mqtt_tx, to_mqtt_rx) = async_channel::bounded(10);

        let handle = tokio::spawn(async move {
            super::Coolmaster::coolmaster_worker(COOLMASTER_ADDRESS, to_coolmaster_rx, to_mqtt_tx)
                .await;
        });

        tokio::spawn(async move {
            loop {
                let mqtt_message = to_mqtt_rx.recv().await.unwrap();
                println!("MQTT message: {mqtt_message:?}");
            }
        });

        to_coolmaster_tx
            .send(super::ToCoolmasterMessage::PublishUnitsState)
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        handle.abort();
        println!("Test done");
    }

    #[tokio::test]
    #[ignore]
    async fn test_split_host_port() {
        let (host, port) = super::Coolmaster::split_host_port("10.0.1.70").unwrap();
        assert_eq!(host, "10.0.1.70");
        assert_eq!(port, 10102);

        let (host, port) = super::Coolmaster::split_host_port("10.0.1.70:7777").unwrap();
        assert_eq!(host, "10.0.1.70");
        assert_eq!(port, 7777);
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("ls").await.unwrap();

        println!("Reply: {reply}");
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_bad_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("abracadabra").await;

        println!("Reply: {reply:?}");
        assert!(reply.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_empty_reply_body() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();

        let reply = coolmaster.command("on L7.400").await.unwrap();
        assert!(reply.is_empty());

        let reply = coolmaster.command("off L7.400").await.unwrap();
        assert!(reply.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_state() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let state = coolmaster.get_unit_state("L7.400").await.unwrap();

        println!("State: {state:?}");
        let state = coolmaster.get_unit_state("Invalid").await;
        assert!(state.is_err());
        print!("State: {state:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_states() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let states = coolmaster.get_units_state().await.unwrap();

        println!("States: {states:?}");
    }
}
