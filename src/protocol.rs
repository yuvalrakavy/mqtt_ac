//! The CoolMaster's ASCII protocol (CoolMasterNet, TCP port 10102), as v1 spoke it. No I/O here:
//! what goes on the wire and what comes back, so every rule is a unit test. The link itself is
//! [`crate::driver`]'s.
//!
//! - **An exchange:** a command and `\r`; the reply is everything up to the next `>` prompt — a
//!   body, then a status line. `OK` is success (and `ERROR: 0`, which v1 also took for success);
//!   any other status is the CoolMaster refusing the command.
//! - **`ls2`** lists every unit (`ls2`) or one (`ls2 <unit>`), a line each:
//!   `L1.001 ON 22.0C 25.0C High Cool OK - 0` — the unit, its power, the setpoint, the room
//!   temperature, the fan speed, the mode, the failure code (`OK`, or the code: `A3`, `12`), the filter flag
//!   (`-`, or `#` when the filter wants changing) and demand (`0` or `1`).
//! - **The commands:** `on`/`off <unit>`, `cool`/`heat`/`dry`/`fan`/`auto <unit>`,
//!   `temp <unit> <t>`, `fspeed <unit> v|l|m|h|t|a`, `filt <unit>`.
//!
//! **How a failure is classified** (Bridge Runtime spec §4.3): an error status is
//! [`OpError::Rejected`], and so is a value outside its property's vocabulary or a target that is
//! not a unit address (the bridge refuses it for the CoolMaster); a reply that cannot be read — not
//! text, no status, a listing line that does not parse — is [`OpError::Unusable`]; a property or
//! command this bridge does not know is [`OpError::Unsupported`]. I/O, a timeout and a closed
//! connection are the link's, [`mqtt_bridge_kit::LinkError`], raised by the driver.
//!
//! **The State document** keeps v1's field names and value strings, so the Store's driver maps
//! them as it did: `unit`, `power`, `target_temperature` and `temperature` (°C), `fan_speed`
//! (`VLow` `Low` `Medium` `High` `Top` `Auto`), `operation_mode` (`Cool` `Heat` `Dry` `Fan`
//! `Auto`), `failure_code` (the code as the CoolMaster prints it, a string — `"A3"`, `"12"` — or
//! `null` for `OK`), `filter_change` and `demand`. The four a request may set are `power`,
//! `operation_mode`, `fan_speed` and `target_temperature`.

use mqtt_bridge_kit::{OpError, PropertyMap, TargetState, Value};

/// The CoolMaster's port when the address names none.
pub const DEFAULT_PORT: u16 = 10102;

/// The State document's fields.
pub const UNIT: &str = "unit";
pub const POWER: &str = "power";
pub const OPERATION_MODE: &str = "operation_mode";
pub const FAN_SPEED: &str = "fan_speed";
pub const TARGET_TEMPERATURE: &str = "target_temperature";
pub const TEMPERATURE: &str = "temperature";
pub const FAILURE_CODE: &str = "failure_code";
pub const FILTER_CHANGE: &str = "filter_change";
pub const DEMAND: &str = "demand";

/// The one momentary command: reset a unit's filter flag (`filt <unit>`).
pub const RESET_FILTER: &str = "ResetFilter";

/// The modes: State's value (also `ls2`'s word), and the command that sets it.
const MODES: [(&str, &str); 5] = [("Cool", "cool"), ("Heat", "heat"), ("Dry", "dry"), ("Fan", "fan"), ("Auto", "auto")];

/// The fan speeds: State's value, `ls2`'s word, and `fspeed`'s letter.
const FAN_SPEEDS: [(&str, &str, &str); 6] =
    [("VLow", "VLow", "v"), ("Low", "Low", "l"), ("Medium", "Med", "m"), ("High", "High", "h"), ("Top", "Top", "t"), ("Auto", "Auto", "a")];

/// Whether `unit` can be sent as a unit address: `L7.400`, `L1.001`, `100`. A target comes from a
/// topic segment, which may hold a space or a `\r` — sent as is, it would end the command early
/// and start another one on the CoolMaster (`on L1.001\roff L1.002`); and an empty one would make
/// `filt` reset every unit's filter.
pub fn valid_unit(unit: &str) -> bool {
    !unit.is_empty() && unit.len() <= 32 && unit.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn unit_address(unit: &str) -> Result<&str, OpError> {
    if valid_unit(unit) {
        Ok(unit)
    } else {
        Err(OpError::Rejected(format!("`{unit}` is not a CoolMaster unit address")))
    }
}

/// A reply, as read up to its prompt (the `>` taken off): its body, when the status says success.
pub fn parse_reply(reply: &[u8]) -> Result<String, OpError> {
    let Ok(reply) = std::str::from_utf8(reply) else {
        return Err(OpError::Unusable("the CoolMaster's reply is not text".into()));
    };
    let reply = reply.trim();
    // The status is the last line; a reply of one line is its status alone.
    let (body, status) = match reply.rsplit_once('\n') {
        Some((body, status)) => (body.trim_end(), status.trim()),
        None => ("", reply),
    };
    match status {
        "OK" | "ERROR: 0" => Ok(body.to_owned()),
        "" => Err(OpError::Unusable("the CoolMaster's reply has no status".into())),
        status => Err(OpError::Rejected(format!("the CoolMaster answered `{status}`"))),
    }
}

/// A temperature as `ls2` gives it (`22.0C`, `72F`), in °C.
fn temperature(text: &str) -> Result<f64, String> {
    // The value, and its scale: the last character, whatever its width in bytes — a byte slice
    // at `len() - 1` panics inside a multibyte one (`25°`; v1's re-review C5).
    let mut chars = text.chars();
    let scale = chars.next_back();
    let value: f64 = chars.as_str().parse().map_err(|_| format!("`{text}` is not a temperature"))?;
    if !value.is_finite() {
        return Err(format!("`{text}` is not a temperature"));
    }
    match scale {
        Some('C') => Ok(value),
        Some('F') => Ok((value - 32.0) * 5.0 / 9.0),
        _ => Err(format!("`{text}` is not a temperature")),
    }
}

/// One unit's `ls2` line, as its State document; `Err` says why it cannot be used.
pub fn parse_line(line: &str) -> Result<TargetState, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [unit, power, setpoint, room, fan, mode, failure, filter, demand] = fields[..] else {
        return Err(format!("{} fields, not 9", fields.len()));
    };
    if !valid_unit(unit) {
        return Err(format!("`{unit}` is not a unit address"));
    }
    let power = match power {
        "ON" => true,
        "OFF" => false,
        other => return Err(format!("power `{other}`")),
    };
    let target_temperature = temperature(setpoint)?;
    let room = temperature(room)?;
    let fan = FAN_SPEEDS.iter().find(|(_, word, _)| *word == fan).map(|(value, _, _)| *value).ok_or(format!("fan speed `{fan}`"))?;
    let mode = MODES.iter().find(|(value, _)| *value == mode).map(|(value, _)| *value).ok_or(format!("mode `{mode}`"))?;
    // The code exactly as the CoolMaster prints it: vendor codes are often alphanumeric (`A3`,
    // `U4`), and a unit in failure must never freeze its State (v1 took numbers only).
    let failure = match failure {
        "OK" => Value::Null,
        code => Value::from(code),
    };
    let filter = match filter {
        "-" => false,
        "#" => true,
        other => return Err(format!("filter flag `{other}`")),
    };
    let demand = match demand {
        "0" => false,
        "1" => true,
        other => return Err(format!("demand `{other}`")),
    };
    let mut values = PropertyMap::new();
    values.insert(UNIT.into(), Value::from(unit));
    values.insert(POWER.into(), Value::from(power));
    values.insert(TARGET_TEMPERATURE.into(), Value::from(target_temperature));
    values.insert(TEMPERATURE.into(), Value::from(room));
    values.insert(FAN_SPEED.into(), Value::from(fan));
    values.insert(OPERATION_MODE.into(), Value::from(mode));
    values.insert(FAILURE_CODE.into(), failure);
    values.insert(FILTER_CHANGE.into(), Value::from(filter));
    values.insert(DEMAND.into(), Value::from(demand));
    Ok(TargetState::new(unit, values))
}

/// A listing line that cannot be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadLine {
    /// Its first word — the unit, if anything is — or, with none, the line.
    pub unit: String,
    pub line: String,
    pub why: String,
}

/// An `ls2` listing, line by line: the units whose lines parse, and the lines that do not.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Listing {
    pub states: Vec<TargetState>,
    pub bad: Vec<BadLine>,
}

impl Listing {
    /// The units' State documents. A listing none of whose lines can be used is `Unusable`; one
    /// with some is the units that can (an empty listing is no units).
    pub fn into_states(self) -> Result<Vec<TargetState>, OpError> {
        match self.bad.first() {
            Some(bad) if self.states.is_empty() => {
                Err(OpError::Unusable(format!("no line of the listing can be used (`{}`: {})", bad.line, bad.why)))
            }
            _ => Ok(self.states),
        }
    }
}

/// An `ls2` listing's body, a line at a time: one bad line costs only its own unit. (v1 failed the
/// whole listing on it, so no unit's State changed while one unit was listed garbled.)
pub fn parse_listing(body: &str) -> Listing {
    let mut listing = Listing::default();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match parse_line(line) {
            Ok(state) => listing.states.push(state),
            Err(why) => {
                let unit = line.split_whitespace().next().unwrap_or(line).to_owned();
                listing.bad.push(BadLine { unit, line: line.to_owned(), why });
            }
        }
    }
    listing
}

/// The listing command: every unit, or one.
pub fn list(unit: Option<&str>) -> Result<String, OpError> {
    match unit {
        None => Ok("ls2".to_owned()),
        Some(unit) => Ok(format!("ls2 {}", unit_address(unit)?)),
    }
}

/// `ResetFilter`'s command.
pub fn reset_filter(unit: &str) -> Result<String, OpError> {
    Ok(format!("filt {}", unit_address(unit)?))
}

/// One step of an `apply`: a property, and its command — or why it cannot be sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub property: String,
    pub command: Result<String, OpError>,
}

/// Where a property's command goes in an `apply` (see [`plan`]).
fn rank(property: &str, values: &PropertyMap) -> u8 {
    match property {
        POWER if values.get(POWER) == Some(&Value::Bool(false)) => 5,
        POWER => 0,
        OPERATION_MODE => 1,
        TARGET_TEMPERATURE => 2,
        FAN_SPEED => 3,
        _ => 4,
    }
}

/// The command that sets `property` of `unit` (an address already checked) to `value`.
fn command(unit: &str, property: &str, value: &Value) -> Result<String, OpError> {
    let refuse = |what: &str| Err(OpError::Rejected(format!("`{property}` takes {what}, not {value}")));
    match property {
        POWER => match value {
            Value::Bool(true) => Ok(format!("on {unit}")),
            Value::Bool(false) => Ok(format!("off {unit}")),
            _ => refuse("true or false"),
        },
        OPERATION_MODE => match MODES.iter().find(|(name, _)| Some(*name) == value.as_str()) {
            Some((_, word)) => Ok(format!("{word} {unit}")),
            None => refuse("Cool, Heat, Dry, Fan or Auto"),
        },
        FAN_SPEED => match FAN_SPEEDS.iter().find(|(name, _, _)| Some(*name) == value.as_str()) {
            Some((_, _, letter)) => Ok(format!("fspeed {unit} {letter}")),
            None => refuse("VLow, Low, Medium, High, Top or Auto"),
        },
        TARGET_TEMPERATURE => match value.as_f64() {
            Some(t) if t.is_finite() => Ok(format!("temp {unit} {t}")),
            _ => refuse("a number"),
        },
        other => Err(OpError::Unsupported(format!("`{other}` is not a property a request can set"))),
    }
}

/// The commands that apply `values` to `unit`, in the order the CoolMaster needs them:
/// - **power on first:** a unit that is off may not take its settings (indoor units differ);
/// - **the mode next, then the setpoint and the fan speed:** the mode decides whether the others
///   apply at all (a fan-only unit has no setpoint, dry mode keeps its own fan speed), and on some
///   indoor units which mode's setpoint a `temp` sets;
/// - **power off last:** whatever else the request sets is set while the unit is still on, and
///   the request leaves it off.
///
/// A property this bridge does not know is `Unsupported`, and a value outside its property's
/// vocabulary `Rejected` — each in its own step, so the rest still applies. A target that is not
/// a unit address refuses the whole request.
pub fn plan(unit: &str, values: &PropertyMap) -> Result<Vec<Step>, OpError> {
    let unit = unit_address(unit)?;
    let mut properties: Vec<&String> = values.keys().collect();
    properties.sort_by_key(|p| (rank(p, values), p.as_str()));
    Ok(properties.into_iter().map(|p| Step { property: p.clone(), command: command(unit, p, &values[p.as_str()]) }).collect())
}

#[cfg(test)]
mod tests;
