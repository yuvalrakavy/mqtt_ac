
use core::fmt::{Display, Formatter};
use crate::error::CoolmasterError;


#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanSpeed {
    VLow,
    Low,
    Medium,
    High,
    Top,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

impl FanSpeed {
    pub fn from_str(s: &str) -> Result<FanSpeed, CoolmasterError> {
        match s {
            "VLow" => Ok(FanSpeed::VLow),
            "Low" => Ok(FanSpeed::Low),
            "Med" => Ok(FanSpeed::Medium),
            "High" => Ok(FanSpeed::High),
            "Top" => Ok(FanSpeed::Top),
            "Auto" => Ok(FanSpeed::Auto),
            _ => Err(CoolmasterError::InvalidUnitState("Invalid fan speed", String::from(s))),
        }
    }
}

impl Display for FanSpeed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FanSpeed::VLow => write!(f, "VLow"),
            FanSpeed::Low => write!(f, "Low"),
            FanSpeed::Medium => write!(f, "Med"),
            FanSpeed::High => write!(f, "High"),
            FanSpeed::Top => write!(f, "Top"),
            FanSpeed::Auto => write!(f, "Auto"),
        }
    }
}

impl OperationMode {
    pub fn from_str(s: &str) -> Result<OperationMode, CoolmasterError> {
        match s {
            "Cool" => Ok(OperationMode::Cool),
            "Heat" => Ok(OperationMode::Heat),
            "Dry" => Ok(OperationMode::Dry),
            "Fan" => Ok(OperationMode::Fan),
            "Auto" => Ok(OperationMode::Auto),
            _ => Err(CoolmasterError::InvalidUnitState("Invalid operation mode", String::from(s))),
        }
    }
}

impl UnitState {
    pub fn from_str(state_line: &str) -> Result<UnitState, CoolmasterError> {
        let fields: Vec<&str> = state_line.split(' ').filter(|s| !s.is_empty()).collect();

        if fields.len() != 9 {
            return Err(CoolmasterError::InvalidUnitState("Wrong number of fields", String::from(state_line)));
        }

        let unit = fields[0].to_string();
        let power = match fields[1] {
            "ON" => true,
            "OFF" => false,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid power state", String::from(state_line))),
        };

        let target_temperature = UnitState::parse_temperature(fields[2])?;
        let temperature = UnitState::parse_temperature(fields[3])?;
        let fan_speed = FanSpeed::from_str(fields[4])?;
        let operation_mode = OperationMode::from_str(fields[5])?;
        let failure_code = match fields[6] {
            "OK" => None,
            _ => Some(fields[6].parse::<u16>().map_err(|_| CoolmasterError::InvalidUnitState("Invalid failure code", String::from(state_line)))?),
        };
        let filter_change = match fields[7] {
            "-" => false,
            "#" => true,
            _ => return Err(CoolmasterError::InvalidUnitState("Invalid filter change state", String::from(state_line))),
        };
        let demand = match fields[8] {
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
        let unit = temperature.chars().last().ok_or_else(|| CoolmasterError::InvalidTemperature(String::from(temperature)))?;
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
    #[test]
    #[ignore]
    fn test_parse_temperature() {
        let temperature = super::UnitState::parse_temperature("25C").unwrap();
        assert_eq!(temperature, 25.0);

        let temperature = super::UnitState::parse_temperature("77F").unwrap();
        assert_eq!(temperature, 25.0);

        let temperature = super::UnitState::parse_temperature("25X");
        assert!(temperature.is_err());
    }

    #[test]
    #[ignore]
    fn test_parse_unit_state() {
        let state = super::UnitState::from_str("L4.001 OFF 19.0C 23.0C High Heat OK   - 0").unwrap();
        assert_eq!(state.unit, "L4.001");
        assert_eq!(state.power, false);
        assert_eq!(state.target_temperature, 19.0);
        assert_eq!(state.temperature, 23.0);
        assert_eq!(state.fan_speed, super::FanSpeed::High);
        assert_eq!(state.operation_mode, super::OperationMode::Heat);
        assert_eq!(state.failure_code, None);
        assert_eq!(state.filter_change, false);
        assert_eq!(state.demand, false);
    }

}