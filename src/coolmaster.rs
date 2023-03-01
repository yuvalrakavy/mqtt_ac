use tokio::net::TcpStream;
use tokio::io::{BufReader, AsyncWriteExt, AsyncBufReadExt};
use tokio::sync::mpsc::{Sender, Receiver};
use tokio::sync::oneshot::Receiver as OneshotReceiver;

use log::{info, debug, error};

use crate::error::CoolmasterError;
use crate::messages::{ToCoolmasterMessage, ToMqttPublisherMessage};
use crate::ac_unit::{UnitState, self, FanSpeed};


pub struct Coolmaster {
    stream: Option<TcpStream>,
}

#[allow(dead_code)]
impl Coolmaster {
    pub async fn coolmaster_worker(
        host: &str, port: Option<u16>,
        to_coolmaster_channel: Receiver<ToCoolmasterMessage>,
        to_mqtt_publisher_channel: Sender<ToMqttPublisherMessage>,
        terminate_request: OneshotReceiver<()>
    )  -> Result<(), CoolmasterError>{
        let mut coolmaster = Coolmaster::new();

        tokio::select! {
            _ = terminate_request => {
                info!("Coolmaster worker received terminate request");
            }

            _ = coolmaster.do_work(host, port, to_coolmaster_channel, to_mqtt_publisher_channel) => {

            }
        };

        Ok(())
    }

    async fn do_work(&mut self,
        host: &str, port: Option<u16>,
        mut to_coolmaster_channel: Receiver<ToCoolmasterMessage>,
        to_mqtt_publisher_channel: Sender<ToMqttPublisherMessage>,
    ) -> Result<(), CoolmasterError> {
        loop {      // Work loop

            loop {  // Reconnect loop
                let connect_result = self.connect(host, port).await;

                match connect_result {
                    Ok(_) => {
                        info!("Coolmaster worker connected to coolmaster controller");
                        break;
                    }

                    Err(e) => {
                        info!("Coolmaster worker failed to connect to coolmaster controller: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        info!("Coolmaster worker retrying to connect to coolmaster controller");
                    }
                }
            }

            loop {
                let message = to_coolmaster_channel.recv().await;

                match message {
                    None => {
                        info!("Coolmaster worker received None message");
                        return Ok(());
                    }

                    Some(message) => {
                        if let Err(e) = self.handle_message(&message, &to_mqtt_publisher_channel).await {
                            error!("Coolmaster worker failed to handle message: {:#?} - error {:#?}", message, e);
                            self.stream = None;     // Drop the connection, and reconnect
                            break;
                        }
                    }
                }
            }
        }
    }

    fn new() -> Self {
        Coolmaster {
            stream: None,
        }
    }

    async fn connect(&mut self, host: &str, port: Option<u16>) -> Result<(), CoolmasterError> {
        let port = port.unwrap_or(10102);
        let stream = TcpStream::connect(format!("{}:{}", host, port)).await?;
        self.stream = Some(stream);

        // Get the initial '>' prompt
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();

        reader.read_until(b'>', &mut bytes).await?;

        Ok(())
    }

    async fn handle_message(&mut self, message: &ToCoolmasterMessage, to_mqtt_publisher_channel: &Sender<ToMqttPublisherMessage>) -> Result<(), CoolmasterError> {
        match message {
            ToCoolmasterMessage::SetUnitPower(unit, power) => self.set_unit_power(unit, *power).await?,

            ToCoolmasterMessage::PublishUnitState(unit) => {
                let unit_state = self.get_unit_state(unit).await?;
                let _ = to_mqtt_publisher_channel.send(ToMqttPublisherMessage::PublishUnitState(unit_state)).await;
            },

            ToCoolmasterMessage::PublishUnitsState => {
                let units_state = self.get_units_state().await?;
                let _ = to_mqtt_publisher_channel.send(ToMqttPublisherMessage::PublishUnitsState(units_state)).await;
            },
            ToCoolmasterMessage::SetUnitMode(unit, mode) => self.set_unit_mode(unit, mode.clone()).await?,
            ToCoolmasterMessage::SetFanSpeed(unit, fan_speed) => self.set_unit_fan_speed(unit, fan_speed).await?,
            ToCoolmasterMessage::SetTargetTemperature(unit, temperature) => self.set_unit_target_temperature(unit, *temperature).await?,
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

    async fn set_unit_mode(&mut self, unit: &str, mode: ac_unit::OperationMode) -> Result<(), CoolmasterError> {
        let _ = match mode {
            ac_unit::OperationMode::Cool => self.command(&format!("cool {}", unit)).await?,
            ac_unit::OperationMode::Heat => self.command(&format!("heat {}", unit)).await?,
            ac_unit::OperationMode::Dry => self.command(&format!("dry {}", unit)).await?,
            ac_unit::OperationMode::Fan => self.command(&format!("fan {}", unit)).await?,
            ac_unit::OperationMode::Auto => self.command(&format!("auto {}", unit)).await?,
        };
        Ok(())
    }

    async fn set_unit_target_temperature(&mut self, unit: &str, temperature: f32) -> Result<(), CoolmasterError> {
        let _ = self.command(&format!("temp {} {}", unit, temperature)).await?;
        Ok(())
    }

    async fn set_unit_fan_speed(&mut self, unit: &str, speed: &ac_unit::FanSpeed) -> Result<(), CoolmasterError> {
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
        let reply = self.command(&format!("ls2 {}", unit)).await?;
        UnitState::from_str(&reply)
    }

    async fn get_units_state(&mut self) -> Result<Vec<UnitState>, CoolmasterError> {
        let reply = self.command("ls2").await?;
        let states = reply.lines().map(UnitState::from_str).collect::<Result<Vec<UnitState>, CoolmasterError>>()?;

        Ok(states)
    }

    // Lower level functions to communicate with coolmaster controller

    async fn send_to_coolmaster(&mut self, command: &str) -> Result<(), CoolmasterError> {
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;

        TcpStream::write_all(stream, command.as_bytes()).await?;

        if !command.ends_with('\r') && !command.ends_with('\n') {
            TcpStream::write_all(stream, "\r".as_bytes()).await?;
        }

        Ok(())
    }

    async fn get_reply_from_coolmaster(&mut self) -> Result<String, CoolmasterError> {
        let stream = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();

        reader.read_until(b'>', &mut bytes).await?;

        if let Some(last_byte) = bytes.last() {
            if *last_byte == b'>' {
                bytes.pop();
            }
        }

        let reply = String::from_utf8(bytes)?;
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
            None => Coolmaster::parse_status("", reply),            // Reply body is empty, so the whole reply is the status
        }
    }

    fn parse_status(body: &str, status: &str) -> Result<String, CoolmasterError> {
        match status {
            "OK" | "ERROR: 0" => Ok(body.to_string()),
            _ => Err(CoolmasterError::CoolmasterCommandError(status.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    const COOLMASTER_ADDRESS: &str = "10.0.1.70";

    fn set_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_coolmaster_worker() {
        set_logger();

        let (to_coolmaster_tx, to_coolmaster_rx) = tokio::sync::mpsc::channel(10);
        let (to_mqtt_tx, mut to_mqtt_rx) = tokio::sync::mpsc::channel(10);
        let (terminate_tx, terminate_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            _ = super::Coolmaster::coolmaster_worker(COOLMASTER_ADDRESS, None, to_coolmaster_rx, to_mqtt_tx, terminate_rx).await;
        });

        tokio::spawn(async move {
            loop {
                let mqtt_message = to_mqtt_rx.recv().await;

                match mqtt_message {
                    None => break,
                    Some(mqtt_message) => println!("MQTT message: {:?}", mqtt_message),
                }
            }
        });

        to_coolmaster_tx.send(super::ToCoolmasterMessage::PublishUnitsState).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        terminate_tx.send(()).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS, None).await.unwrap();
        let reply = coolmaster.command("ls").await.unwrap();

        println!("Reply: {}", reply);
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_bad_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS, None).await.unwrap();
        let reply = coolmaster.command("abracadabra").await;

        println!("Reply: {:?}", reply);
        assert!(reply.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_empty_reply_body() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS, None).await.unwrap();

        let reply = coolmaster.command("on L7.400").await.unwrap();
        assert!(reply.is_empty());

        let reply = coolmaster.command("off L7.400").await.unwrap();
        assert!(reply.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_state() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect(COOLMASTER_ADDRESS, None).await.unwrap();
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
        coolmaster.connect(COOLMASTER_ADDRESS, None).await.unwrap();
        let states = coolmaster.get_units_state().await.unwrap();

        println!("States: {:?}", states);
    }

}
