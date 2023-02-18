use thiserror::Error;
use tokio::net::TcpStream;
use tokio::io::{BufReader, AsyncWriteExt, AsyncBufReadExt};

#[derive(Debug, Error)]
pub enum CoolmasterError {
    #[error("Connection error")]
    ConnectionError(#[from] std::io::Error),

    #[error("Not connected")]
    NotConnected,

    #[error("Invalid reply")]
    InvalidReply(#[from] std::string::FromUtf8Error),

    #[error("Empty coolmaster reply")]
    EmptyReply,

    #[error("Coolmaster error: {0}")]
    CoolmasterCommandError(String),

    #[error("Invalid unit state")]
    InvalidUnitState(&'static str, String),

    #[error("Invalid temperature")]
    InvalidTemperature(String),
}

pub struct Coolmaster {
    stream: Option<TcpStream>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FanSpeed {
    VLow,
    Low,
    Medium,
    High,
    Top,
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OperationMode {
    Cool,
    Heat,
    Dry,
    Fan,
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnitState {
    pub unit: String,
    pub power: bool,
    pub target_temperature: f32,
    pub temperature: f32,
    pub fan_speed: FanSpeed,
    pub operation_mode: OperationMode,
    pub failure_code: Option<u16>,
    pub filter_change: bool,
    pub demand: bool,
}

#[allow(dead_code)]
impl Coolmaster {
    pub fn new() -> Self {
        Coolmaster {
            stream: None,
        }
    }

    pub async fn connect(&mut self, host: &str, port: Option<u16>) -> Result<(), CoolmasterError> {
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

    pub async fn command(&mut self, command: &str) -> Result<String, CoolmasterError> {
        self.send_to_coolmaster(command).await?;
        self.get_reply_from_coolmaster().await
    }

    pub async fn get_unit_state(&mut self, unit: &str) -> Result<UnitState, CoolmasterError> {
        let reply = self.command(&format!("ls2 {}", unit)).await?;
        Coolmaster::parse_unit_state(&reply)
    }

    pub async fn get_unit_states(&mut self) -> Result<Vec<UnitState>, CoolmasterError> {
        let reply = self.command("ls2").await?;
        let states = reply.lines().map(|line| Coolmaster::parse_unit_state(line)).collect::<Result<Vec<UnitState>, CoolmasterError>>()?;

        Ok(states)
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

    fn parse_unit_state(state_line: &str) -> Result<UnitState, CoolmasterError> {
        let parts = state_line.split_whitespace().collect::<Vec<&str>>();

        if parts.len() != 9 {
            return Err(CoolmasterError::InvalidUnitState("Wrong number of fields", String::from(state_line)));
        }

        let unit = parts[0].to_string();
        let power = match parts[1] {
            "ON" => true,
            "OFF" => false,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid power state", String::from(state_line))),
        };

        let target_temperature = Coolmaster::parse_temperature(parts[2])?;
        let temperature = Coolmaster::parse_temperature(parts[3])?;
        let fan_speed = match parts[4] {
            "VLow" => FanSpeed::VLow,
            "Low" => FanSpeed::Low,
            "Med" => FanSpeed::Medium,
            "High" => FanSpeed::High,
            "Top" => FanSpeed::Top,
            "Auto" => FanSpeed::Auto,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid fan speed", String::from(state_line))),
        };
        let operation_mode = match parts[5] {
            "Cool" => OperationMode::Cool,
            "Heat" => OperationMode::Heat,
            "Dry" => OperationMode::Dry,
            "Fan" => OperationMode::Fan,
            "Auto" => OperationMode::Auto,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid operation mode", String::from(state_line))),
        };
        let failure_code = match parts[6] {
            "OK" => None,
            _ => Some(parts[6].parse::<u16>().map_err(|_| CoolmasterError::InvalidUnitState("Invalid failure code", String::from(state_line)))?),
        };
        let filter_change = match parts[7] {
            "-" => false,
            "#" => true,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid filter change state", String::from(state_line))),
        };
        let demand = match parts[8] {
            "0" => false,
            "1" => true,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid demand state", String::from(state_line))),
        };

        Ok(UnitState {
            unit,
            power,
            target_temperature,
            temperature,
            fan_speed,
            operation_mode,
            failure_code,
            filter_change,
            demand,
        })
    }

    fn parse_temperature(temperature: &str) -> Result<f32, CoolmasterError> {
        let unit = temperature.chars().last().ok_or(CoolmasterError::InvalidTemperature(String::from(temperature)))?;
        let value = temperature[0..temperature.len() - 1].parse::<f32>().map_err(|_| CoolmasterError::InvalidTemperature(String::from(temperature)))?;

        let temperature = match unit {
            'C' => value,
            'F' => (value - 32.0) * 5.0 / 9.0,
            _ => return Err(CoolmasterError::InvalidTemperature(String::from(temperature))),
        };
        Ok(temperature)
    }
}


#[cfg(test)]
mod tests {
    // #[tokio::test]
    // async fn test_connect() {
    //     let mut coolmaster = super::Coolmaster::new();
    //     coolmaster.connect("10.0.1.70", None).await.unwrap();
    // }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect("10.0.1.70", None).await.unwrap();
        let reply = coolmaster.command("ls").await.unwrap();

        println!("Reply: {}", reply);
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_bad_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect("10.0.1.70", None).await.unwrap();
        let reply = coolmaster.command("abracadabra").await;

        println!("Reply: {:?}", reply);
        assert!(reply.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_empty_reply_body() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect("10.0.1.70", None).await.unwrap();

        let reply = coolmaster.command("on L7.400").await.unwrap();
        assert!(reply.is_empty());

        let reply = coolmaster.command("off L7.400").await.unwrap();
        assert!(reply.is_empty());
    }

    #[tokio::test]
    async fn test_get_unit_state() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect("10.0.1.70", None).await.unwrap();
        let state = coolmaster.get_unit_state("L7.400").await.unwrap();

        println!("State: {:?}", state);
        let state = coolmaster.get_unit_state("Invalid").await;
        assert!(state.is_err());
        print!("State: {:?}", state);
    }

    #[tokio::test]
    async fn test_get_unit_states() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect("10.0.1.70", None).await.unwrap();
        let states = coolmaster.get_unit_states().await.unwrap();

        println!("States: {:?}", states);
    }

    #[test]
    #[ignore]
    fn test_parse_temperature() {
        let temperature = super::Coolmaster::parse_temperature("25C").unwrap();
        assert_eq!(temperature, 25.0);

        let temperature = super::Coolmaster::parse_temperature("77F").unwrap();
        assert_eq!(temperature, 25.0);

        let temperature = super::Coolmaster::parse_temperature("25X");
        assert!(temperature.is_err());
    }

    #[test]
    #[ignore]
    fn test_parse_unit_state() {
        let state = super::Coolmaster::parse_unit_state("L7.400 ON 25C 25C Med Cool OK - 0").unwrap();
        assert_eq!(state.unit, "L7.400");
        assert_eq!(state.power, true);
        assert_eq!(state.target_temperature, 25.0);
        assert_eq!(state.temperature, 25.0);
        assert_eq!(state.fan_speed, super::FanSpeed::Medium);
        assert_eq!(state.operation_mode, super::OperationMode::Cool);
        assert_eq!(state.failure_code, None);
        assert_eq!(state.filter_change, false);
        assert_eq!(state.demand, false);
    }

}
