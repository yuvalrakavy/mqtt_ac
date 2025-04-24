use async_channel::{Receiver, Sender};
use error_stack::{Result, ResultExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use log::{error, info};

use crate::ac_unit::{self, UnitState};
use crate::error::CoolmasterError;
use crate::messages::{ToCoolmasterMessage, ToMqttPublisherMessage};

pub struct Coolmaster {
    stream: Option<TcpStream>,
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

            to_mqtt_publisher_channel
            .send(ToMqttPublisherMessage::CoolmasterConnected(false))
            .await
            .unwrap();

            loop {

                // Reconnect loop
                let connect_result = coolmaster.connect(coolmaster_address).await;
                
                match connect_result {
                    Ok(_) => {
                        info!("Coolmaster worker connected to coolmaster controller");
                        to_mqtt_publisher_channel
                            .send(ToMqttPublisherMessage::CoolmasterConnected(true))
                            .await
                            .unwrap();
                        break;
                    }

                    Err(e) => {
                        info!(
                            "Coolmaster worker failed to connect to coolmaster controller: {}",
                            e
                        );

                        to_mqtt_publisher_channel
                            .send(ToMqttPublisherMessage::Error(format!("{:#?}", e)))
                            .await
                            .map_err(|_| CoolmasterError::SendToMqttPublisherChannelFailed)
                            .unwrap();

                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        info!("Coolmaster worker retrying to connect to coolmaster controller");
                    }
                }
            }

            loop {
                let message = to_coolmaster_channel.recv().await;

                match message {
                    Err(_) => {
                        info!("Coolmaster worker received None message");
                    }

                    Ok(message) => {
                        if let Err(e) = coolmaster
                            .handle_message(&message, &to_mqtt_publisher_channel)
                            .await
                        {
                            to_mqtt_publisher_channel
                                .send(ToMqttPublisherMessage::Error(format!(
                                    "Failed to handle {:#?} - {}",
                                    message, e
                                )))
                                .await
                                .map_err(|_| CoolmasterError::SendToMqttPublisherChannelFailed)
                                .unwrap();

                            if let Some(coolmaster_error) = e.downcast_ref::<CoolmasterError>() {
                                if let CoolmasterError::CoolmasterCommandError(e)= coolmaster_error {
                                    error!("Coolmaster replied with an error while handling message: {:#?} - error {:#?}", message, e);
                                }
                                else {
                                    error!("Coolmaster worker failed to handle message: {:#?} - error {:#?} - disconnecting from coolmaster", message, e);
                                    coolmaster.stream = None; // Drop the connection, and reconnect
                                    break;
                                }
                            }
                            else {
                                error!("Coolmaster worker failed handling message {:#?} with unexpected error: {:#?} - disconnecting from coolmaster", message, e);
                                coolmaster.stream = None; // Drop the connection, and reconnect
                                break;

                            }
                        }
                    }
                }
            }
        }
    }

    fn new() -> Self {
        Coolmaster { stream: None }
    }

    fn split_host_port(host_port: &str) -> Result<(String, u16), CoolmasterError> {
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

    async fn connect(&mut self, host: &str) -> Result<(), CoolmasterError> {
        let into_context =
            || CoolmasterError::Context(format!("Connecting to coolmaster controller at {}", host));
        let (host, port) = Coolmaster::split_host_port(host).change_context_lazy(into_context)?;
        let stream = TcpStream::connect(format!("{}:{}", host, port))
            .await
            .change_context_lazy(into_context)?;
        self.stream = Some(stream);

        // Get the initial '>' prompt
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();

        reader
            .read_until(b'>', &mut bytes)
            .await
            .map_err(CoolmasterError::IoError)
            .change_context_lazy(into_context)?;

        Ok(())
    }

    async fn handle_message(
        &mut self,
        message: &ToCoolmasterMessage,
        to_mqtt_publisher_channel: &Sender<ToMqttPublisherMessage>,
    ) -> Result<(), CoolmasterError> {
        match message {
            ToCoolmasterMessage::SetUnitPower(unit, power) => {
                self.set_unit_power(unit, *power).await?
            }

            ToCoolmasterMessage::PublishUnitState(unit) => {
                let unit_state = self.get_unit_state(unit).await?;
                let _ = to_mqtt_publisher_channel
                    .send(ToMqttPublisherMessage::UnitState(unit_state))
                    .await;
            }

            ToCoolmasterMessage::PublishUnitsState => {
                let units_state = self.get_units_state().await?;
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

    async fn set_unit_power(&mut self, unit: &str, power: bool) -> Result<(), CoolmasterError> {
        let _ = match power {
            true => self.command(&format!("on {}", unit)).await?,
            false => self.command(&format!("off {}", unit)).await?,
        };
        Ok(())
    }

    async fn set_unit_mode(
        &mut self,
        unit: &str,
        mode: ac_unit::OperationMode,
    ) -> Result<(), CoolmasterError> {
        let _ = match mode {
            ac_unit::OperationMode::Cool => self.command(&format!("cool {}", unit)).await?,
            ac_unit::OperationMode::Heat => self.command(&format!("heat {}", unit)).await?,
            ac_unit::OperationMode::Dry => self.command(&format!("dry {}", unit)).await?,
            ac_unit::OperationMode::Fan => self.command(&format!("fan {}", unit)).await?,
            ac_unit::OperationMode::Auto => self.command(&format!("auto {}", unit)).await?,
        };
        Ok(())
    }

    async fn set_unit_target_temperature(
        &mut self,
        unit: &str,
        temperature: f32,
    ) -> Result<(), CoolmasterError> {
        let _ = self
            .command(&format!("temp {} {}", unit, temperature))
            .await?;
        Ok(())
    }

    async fn set_unit_fan_speed(
        &mut self,
        unit: &str,
        speed: &ac_unit::FanSpeed,
    ) -> Result<(), CoolmasterError> {
        let speed = match speed {
            ac_unit::FanSpeed::VLow => "v",
            ac_unit::FanSpeed::Low => "l",
            ac_unit::FanSpeed::Medium => "m",
            ac_unit::FanSpeed::High => "h",
            ac_unit::FanSpeed::Top => "t",
            ac_unit::FanSpeed::Auto => "a",
        };

        self.command(&format!("fspeed {} {}", unit, speed)).await?;
        Ok(())
    }

    async fn reset_filter(&mut self, unit: &str) -> Result<(), CoolmasterError> {
        let _ = self.command(&format!("filt {}", unit)).await?;
        Ok(())
    }

    async fn get_unit_state(&mut self, unit: &str) -> Result<UnitState, CoolmasterError> {
        let into_context = || CoolmasterError::Context(format!("Getting state for unit {}", unit));
        let reply = self.command(&format!("ls2 {}", unit)).await?;

        UnitState::from_str(&reply).change_context_lazy(into_context)
    }

    async fn get_units_state(&mut self) -> Result<Vec<UnitState>, CoolmasterError> {
        let into_context = || CoolmasterError::Context("Getting states for all units".to_string());
        let reply = self.command("ls2").await?;
        let states = reply
            .lines()
            .map(UnitState::from_str)
            .collect::<Result<Vec<UnitState>, CoolmasterError>>()
            .change_context_lazy(into_context)?;

        Ok(states)
    }

    // Lower level functions to communicate with coolmaster controller

    async fn send_to_coolmaster(&mut self, command: &str) -> Result<(), CoolmasterError> {
        let into_context =
            || CoolmasterError::Context(format!("Sending command to coolmaster: '{}'", command));
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;

        TcpStream::write_all(stream, command.as_bytes())
            .await
            .map_err(CoolmasterError::IoError)
            .change_context_lazy(into_context)?;

        if !command.ends_with('\r') && !command.ends_with('\n') {
            TcpStream::write_all(stream, "\r".as_bytes())
                .await
                .map_err(CoolmasterError::IoError)
                .change_context_lazy(into_context)?;
        }

        Ok(())
    }

    async fn get_reply_from_coolmaster(&mut self) -> Result<String, CoolmasterError> {
        let into_context = || CoolmasterError::Context("Getting reply from coolmaster".to_string());
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();

        reader
            .read_until(b'>', &mut bytes)
            .await
            .map_err(CoolmasterError::IoError)
            .change_context_lazy(into_context)?;

        if let Some(last_byte) = bytes.last() {
            if *last_byte == b'>' {
                bytes.pop();
            }
        }

        let reply = String::from_utf8(bytes).change_context_lazy(into_context)?;
        Coolmaster::parse_reply(&reply)
    }

    async fn command(&mut self, command: &str) -> Result<String, CoolmasterError> {
        self.send_to_coolmaster(command).await?;
        self.get_reply_from_coolmaster().await
    }

    fn parse_reply(reply: &str) -> Result<String, CoolmasterError> {
        let reply = reply.trim();
        let body_status_split = reply.rsplit_once("\r\n");

        match body_status_split {
            Some((body, status)) => Coolmaster::parse_status(body, status),
            None => Coolmaster::parse_status("", reply), // Reply body is empty, so the whole reply is the status
        }
    }

    fn parse_status(body: &str, status: &str) -> Result<String, CoolmasterError> {
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

    #[tokio::test]
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
                println!("MQTT message: {:?}", mqtt_message);
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
        coolmaster.connect(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("ls").await.unwrap();

        println!("Reply: {}", reply);
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_bad_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("abracadabra").await;

        println!("Reply: {:?}", reply);
        assert!(reply.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_empty_reply_body() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS).await.unwrap();

        let reply = coolmaster.command("on L7.400").await.unwrap();
        assert!(reply.is_empty());

        let reply = coolmaster.command("off L7.400").await.unwrap();
        assert!(reply.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_state() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS).await.unwrap();
        let state = coolmaster.get_unit_state("L7.400").await.unwrap();

        println!("State: {:?}", state);
        let state = coolmaster.get_unit_state("Invalid").await;
        assert!(state.is_err());
        print!("State: {:?}", state);
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_states() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS).await.unwrap();
        let states = coolmaster.get_units_state().await.unwrap();

        println!("States: {:?}", states);
    }
}
