//! The CoolMaster as a [`DeviceDriver`] (Bridge Runtime spec §4, §6): one TCP connection to the
//! controller, one exchange at a time — the runtime calls the driver one operation at a time, so
//! the protocol's strict command-then-reply sequencing comes for free.
//!
//! The runtime bounds every operation (`connect` and the rest, [`info`]'s timing) and takes an
//! overrun for a lost link; so this driver has no timeouts of its own. What it maps:
//! - the targets are unit addresses (`L7.400`);
//! - `read_state` is an `ls2` listing, of every unit or one;
//! - `apply` sends a command per property, in the order [`protocol::plan`] gives, then reads the
//!   unit back so its `State` follows at once (v1 read a unit after its commands too);
//! - `execute` is `ResetFilter` (`filt <unit>`), also read back;
//! - a failure is classified as [`protocol`] says; I/O, a closed connection and EOF before the
//!   prompt are the link's ([`LinkError`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use mqtt_bridge_kit::{
    CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, FailureClass, Feedback, LinkContext, LinkError, OpError,
    PropertyFailure, PropertyMap, Reporter, Scope, TargetState, Timing, Value, Writes,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::protocol;

/// What the CoolMaster driver declares (spec §4.4): acknowledged writes (an `OK` is the
/// CoolMaster confirming the command), observed feedback (`ls2`), one momentary command,
/// `ResetFilter`, and v1's timing — a connect and its prompt within 10 s, a retry every 5 s, one
/// WARN past 30 s down, a listing every 4 s. v1 bounded each exchange at 10 s; the runtime bounds
/// each operation, an `apply`'s few exchanges together, which a CoolMaster answers in milliseconds.
/// Anything else `DriverInfo` holds keeps its default: `restore` stays off (the CoolMaster is read
/// back, never restored from the read-back).
pub fn info() -> DriverInfo {
    DriverInfo {
        writes: Writes::Acknowledged,
        feedback: Feedback::Observed,
        commands: vec![CommandSpec::new(protocol::RESET_FILTER, CommandClass::Momentary)],
        timing: Timing {
            connect: Duration::from_secs(10),
            operation: Duration::from_secs(10),
            retry: Duration::from_secs(5),
            outage_warn: Duration::from_secs(30),
            poll: Some(Duration::from_secs(4)),
            ..Timing::default()
        },
        ..DriverInfo::default()
    }
}

/// The CoolMaster's address: `host` or `host:port` (port 10102 by default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub host: String,
    pub port: u16,
}

impl Address {
    pub fn parse(text: &str) -> Result<Address, String> {
        match text.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port.parse().map_err(|_| format!("`{text}`: `{port}` is not a port"))?;
                Ok(Address { host: host.to_owned(), port })
            }
            Some(_) => Err(format!("`{text}` names no host")),
            None if text.is_empty() => Err("the CoolMaster's address names no host".into()),
            None => Ok(Address { host: text.to_owned(), port: protocol::DEFAULT_PORT }),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// The driver. See the module documentation.
#[derive(Debug)]
pub struct Coolmaster {
    address: Address,
    link: Option<BufReader<TcpStream>>,
    /// Why the link was dropped between operations, for the next one to say.
    dropped: Option<String>,
    report: Option<Reporter>,
    /// The units whose listing line cannot be used now, as last said.
    passed_over: BTreeSet<String>,
    /// The units the full listings have named, for telling one gone (see [`Coolmaster::gone`]).
    known: BTreeMap<String, Known>,
}

/// A unit the full listings have named.
#[derive(Debug, Default)]
struct Known {
    /// Its State was reported (a line of it parsed), so there is a State to retract.
    reported: bool,
    /// Usable full listings in a row that have omitted it.
    missed: u32,
}

/// Usable full listings in a row that must omit a unit before it is taken for gone: one listing
/// can be cut short or come mid-scan; two in a row (8 s at the default poll) say it left.
pub const GONE_AFTER: u32 = 2;

impl Coolmaster {
    pub fn new(address: Address) -> Coolmaster {
        Coolmaster { address, link: None, dropped: None, report: None, passed_over: BTreeSet::new(), known: BTreeMap::new() }
    }

    /// A full listing: the units now gone from the CoolMaster (unbound, or the controller set up
    /// again), whose retained State is to be retracted. A unit is gone once [`GONE_AFTER`] usable
    /// full listings in a row omit it, and only a unit whose State was reported is retracted (one
    /// known only from a garbled line is just forgotten). A unit listed garbled under its own
    /// address is still there. A listing that cannot say who is there changes nothing: an empty one
    /// (a CoolMaster rebooting, or still scanning its line), one with no usable line, and one with
    /// a line whose address itself is garbled (which unit it was, nobody can tell).
    fn gone(&mut self, listing: &protocol::Listing) -> Vec<String> {
        let certain = listing.bad.iter().all(|b| protocol::valid_unit(&b.unit));
        if !certain || listing.states.is_empty() {
            return Vec::new();
        }
        let present: BTreeSet<&str> =
            listing.states.iter().map(|s| s.target.as_str()).chain(listing.bad.iter().map(|b| b.unit.as_str())).collect();
        for state in &listing.states {
            self.known.entry(state.target.clone()).or_default().reported = true;
        }
        for bad in &listing.bad {
            self.known.entry(bad.unit.clone()).or_default();
        }
        let mut gone = Vec::new();
        self.known.retain(|unit, known| {
            if present.contains(unit.as_str()) {
                known.missed = 0;
                return true;
            }
            known.missed += 1;
            if known.missed < GONE_AFTER {
                return true;
            }
            if known.reported {
                gone.push(unit.clone());
            }
            false
        });
        for unit in &gone {
            self.passed_over.remove(unit);
        }
        gone
    }

    /// Say which units a listing passed over: an INFO when a unit's line first cannot be used,
    /// DEBUG while it stays so, an INFO when it can be used again — never a line per poll (every
    /// 4 s) for as long as a unit is listed garbled.
    /// The line is logged escaped: a garbled one may hold control characters.
    fn note(&mut self, listing: &protocol::Listing) {
        for bad in &listing.bad {
            let (unit, line, why) = (bad.unit.escape_debug(), bad.line.escape_debug(), bad.why.escape_debug());
            if self.passed_over.insert(bad.unit.clone()) {
                info!(unit = %unit, line = %line, error = %why, "A unit's ls2 line cannot be used; the unit is passed over");
            } else {
                debug!(unit = %unit, line = %line, error = %why, "A unit's ls2 line still cannot be used");
            }
        }
        for state in &listing.states {
            if self.passed_over.remove(&state.target) {
                info!(unit = %state.target, "A unit's ls2 line can be used again");
            }
        }
    }

    /// A command and its reply (bounded by the runtime's deadline on the operation): the reply's
    /// body, or how it failed. A failure of the link drops it, so nothing more is written into a
    /// connection in an unknown state.
    async fn exchange(&mut self, command: &str) -> Result<String, OpError> {
        let reply = self.exchange_raw(command).await;
        if let Err(OpError::Link(e)) = &reply {
            self.link = None;
            self.dropped = Some(e.reason().to_owned());
        }
        protocol::parse_reply(&reply?)
    }

    async fn exchange_raw(&mut self, command: &str) -> Result<Vec<u8>, OpError> {
        let Some(link) = self.link.as_mut() else {
            let why = self.dropped.as_deref().unwrap_or("not connected");
            return Err(OpError::Link(LinkError::new(format!("the CoolMaster link is gone: {why}"))));
        };
        let line = format!("{command}\r");
        // WAIT: coolmaster-io
        link.get_mut().write_all(line.as_bytes()).await.map_err(lost)?;
        let mut reply = Vec::new();
        // WAIT: coolmaster-io
        link.read_until(b'>', &mut reply).await.map_err(lost)?;
        // A reply that ends without its prompt ended with the connection.
        if reply.pop() != Some(b'>') {
            return Err(OpError::Link(LinkError::new("the CoolMaster closed the connection")));
        }
        Ok(reply)
    }

    /// A momentary command: `ResetFilter` (`filt <unit>`), then the unit read back. It names a
    /// unit — `filt` with none would reset every unit's filter.
    async fn command(&mut self, unit: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError> {
        if call.command != protocol::RESET_FILTER {
            return Err(OpError::Unsupported(format!("the CoolMaster has no command `{}`", call.command)));
        }
        let Some(unit) = unit else {
            return Err(OpError::Rejected(format!("`{}` names the unit it resets", protocol::RESET_FILTER)));
        };
        let command = protocol::reset_filter(unit)?;
        self.exchange(&command).await?;
        self.read_back(unit).await;
        Ok(None)
    }

    /// After a command: read the unit back, so its `State` follows at once rather than at the next
    /// poll. A courtesy — the command was confirmed already: a listing that is refused or
    /// unusable waits for the poll, and a link lost here is dropped, for the next operation to
    /// find.
    async fn read_back(&mut self, unit: &str) {
        match self.read_state(Scope::Target(unit.to_owned())).await {
            Ok(states) => {
                if let Some(report) = &self.report {
                    for state in states {
                        report.state(&state.target, state.values, false);
                    }
                }
            }
            Err(e) => debug!(unit, error = %e, "A unit's read-back after its command failed; the next poll reads it"),
        }
    }
}

fn lost(e: std::io::Error) -> OpError {
    OpError::Link(LinkError::from(e))
}

/// One property's failure in an `apply`, with its own class and message.
fn failure(property: String, e: OpError) -> PropertyFailure {
    let (class, error) = match e {
        OpError::Rejected(m) => (FailureClass::Rejected, m),
        OpError::Unusable(m) => (FailureClass::Unusable, m),
        OpError::Unsupported(m) => (FailureClass::Unsupported, m),
        // Neither comes from one property's command: a lost link ends the apply first, and only an
        // apply's outcome is partial.
        other @ (OpError::Link(_) | OpError::Partial(_)) => (FailureClass::Rejected, other.to_string()),
    };
    PropertyFailure { property, class, error }
}

/// An `apply`'s outcome: `Ok`, or the properties that failed on their own, each with its class
/// (`OpError::Partial`) — every other property of the write stands as applied, and the CoolMaster
/// has it: each property is a command of its own, confirmed or refused alone, with nothing rolled
/// back. The Store hears an `Error` for the failed ones only.
fn outcome(failed: Vec<PropertyFailure>) -> Result<(), OpError> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(OpError::Partial(failed))
    }
}

impl DeviceDriver for Coolmaster {
    fn info(&self) -> DriverInfo {
        info()
    }

    /// A TCP connection, up to the CoolMaster's first `>` prompt.
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        self.link = None;
        self.dropped = None;
        self.report = Some(ctx.report.clone());
        let Address { host, port } = &self.address;
        // WAIT: coolmaster-io
        let stream = TcpStream::connect((host.as_str(), *port)).await?;
        let _ = stream.set_nodelay(true);
        let mut link = BufReader::new(stream);
        let mut prompt = Vec::new();
        // WAIT: coolmaster-io
        link.read_until(b'>', &mut prompt).await?;
        if prompt.last() != Some(&b'>') {
            return Err(LinkError::new("the CoolMaster closed the connection before its prompt"));
        }
        self.link = Some(link);
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.link = None;
        self.dropped = None;
    }

    /// `ls2`: every unit (`Scope::All`, and `Scope::Any`), or one. A line that cannot be used costs
    /// only its own unit; a listing with no usable line is `Unusable`. A unit a full listing no
    /// longer names has its retained `State` retracted, so the broker keeps no ghost of it.
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let unit = match &scope {
            Scope::Target(unit) => Some(unit.as_str()),
            Scope::All | Scope::Any => None,
        };
        let command = protocol::list(unit)?;
        let body = self.exchange(&command).await?;
        let listing = protocol::parse_listing(&body);
        self.note(&listing);
        if unit.is_none() {
            for unit in self.gone(&listing) {
                info!(unit = %unit, "A unit is no longer listed by the CoolMaster; its State is retracted");
                if let Some(report) = &self.report {
                    report.remove(&unit);
                }
            }
        }
        listing.into_states()
    }

    /// A command per property, in the CoolMaster's order ([`protocol::plan`]). A property that
    /// fails on its own — refused by the CoolMaster, answered with something unusable, or not one
    /// a request can set — is passed over and the rest still sent (turning a unit off is not lost
    /// to a refused setpoint); the outcome names only the failed ones (`OpError::Partial`). A lost
    /// link ends it at once, and the runtime holds the request for the next connection. Then the
    /// unit is read back. A target that is not a unit address fails the whole write (`Rejected`).
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
        let mut failed = Vec::new();
        let mut sent = false;
        for step in protocol::plan(target, values)? {
            let done = match step.command {
                Ok(command) => {
                    sent = true;
                    self.exchange(&command).await.map(drop)
                }
                Err(refused) => Err(refused),
            };
            match done {
                Ok(()) => {}
                Err(OpError::Link(e)) => return Err(OpError::Link(e)),
                Err(e) => failed.push(failure(step.property, e)),
            }
        }
        if sent {
            self.read_back(target).await;
        }
        outcome(failed)
    }

    /// `ResetFilter` (`filt <unit>`), then the unit read back.
    async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError> {
        self.command(target, call).await
    }

    /// The CoolMaster needs no setup per unit; a `Config`'s `feedback` is the runtime's.
    async fn configure(&mut self, _target: &str, _config: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_defaults_to_port_10102() {
        assert_eq!(Address::parse("10.0.1.70"), Ok(Address { host: "10.0.1.70".into(), port: 10102 }));
        assert_eq!(Address::parse("10.0.1.70:7777"), Ok(Address { host: "10.0.1.70".into(), port: 7777 }));
        assert_eq!(Address::parse("coolmaster.local:1"), Ok(Address { host: "coolmaster.local".into(), port: 1 }));
        for bad in ["", ":10102", "host:", "host:port", "host:99999"] {
            assert!(Address::parse(bad).is_err(), "{bad}");
        }
    }

    /// A unit is gone — its State to be retracted — only once two usable full listings in a row
    /// omit it, and only if it was ever reported. One listed garbled under its own address is still
    /// there; a line whose address is garbled, an unusable listing and an empty one change nothing.
    #[test]
    fn a_unit_is_gone_only_after_two_usable_listings_omit_it() {
        let mut driver = Coolmaster::new(Address::parse("127.0.0.1").unwrap());
        let listing = |body: &str| protocol::parse_listing(body);
        let a = "L7.400 ON 22.0C 25.0C High Cool OK - 0";
        let both = "L7.400 ON 22.0C 25.0C High Cool OK - 0\r\nL7.401 OFF 22.0C 25.0C High Cool OK - 0";
        assert!(driver.gone(&listing(both)).is_empty());
        // Garbled under its own address: still there, however long.
        for _ in 0..3 {
            assert!(driver.gone(&listing(&format!("{a}\r\nL7.401 ON ??? garbled"))).is_empty());
        }
        // A garbled address, an unusable listing, an empty one: no inference at all.
        for _ in 0..3 {
            assert!(
                driver.gone(&listing(&format!("{a}\r\nL7.4\u{1}1 OFF 22.0C 25.0C High Cool OK - 0"))).is_empty(),
                "a garbled address retracted a unit"
            );
            assert!(driver.gone(&listing("garbage\r\nmore garbage")).is_empty(), "an unusable listing retracted a unit");
            assert!(driver.gone(&listing("")).is_empty(), "an empty listing retracted a unit");
        }
        // Omitted by one usable listing: not yet; by a second in a row: gone.
        assert!(driver.gone(&listing(a)).is_empty(), "one omission retracted a unit");
        assert_eq!(driver.gone(&listing(a)), ["L7.401"]);
        assert!(driver.gone(&listing(a)).is_empty(), "a unit was retracted twice");
        // One omission, then listed again: the count starts over.
        assert!(driver.gone(&listing(both)).is_empty());
        assert!(driver.gone(&listing(a)).is_empty());
        assert!(driver.gone(&listing(both)).is_empty());
        assert!(driver.gone(&listing(a)).is_empty());
        // Known only from a garbled line under its address, never reported: forgotten, not retracted.
        assert!(driver.gone(&listing(&format!("{both}\r\nL7.402 ON ??? garbled"))).is_empty());
        assert!(driver.gone(&listing(both)).is_empty());
        assert!(driver.gone(&listing(both)).is_empty(), "a unit never reported was retracted");
    }

    /// A unit address is the target as the CoolMaster lists it — `L7.400` unchanged, never a `/`,
    /// `+` or `#` (the runtime drops a target holding one).
    #[test]
    fn a_unit_address_is_the_target_unchanged() {
        let states = protocol::parse_listing("L7.400 ON 22.0C 25.0C High Cool OK - 0").into_states().unwrap();
        assert_eq!(states[0].target, "L7.400");
        assert_eq!(states[0].values["unit"], "L7.400");
        for bad in ["L7/400 ON 22.0C 25.0C High Cool OK - 0", "L7+400 ON 22.0C 25.0C High Cool OK - 0", "# ON 22.0C 25.0C High Cool OK - 0"]
        {
            assert!(protocol::parse_listing(bad).into_states().is_err(), "{bad}");
        }
    }

    /// An `apply` that partly failed is `Partial`, naming each failed property with its own class
    /// and message; one that did not fail is `Ok`.
    #[test]
    fn an_apply_that_partly_failed_names_each_property_with_its_class() {
        assert_eq!(outcome(Vec::new()), Ok(()));
        let failed = vec![
            failure("target_temperature".to_owned(), OpError::Rejected("the CoolMaster answered `ERROR: 1`".into())),
            failure("fan_speed".to_owned(), OpError::Unusable("the CoolMaster's reply is not text".into())),
            failure("swing".to_owned(), OpError::Unsupported("`swing` is not a property a request can set".into())),
        ];
        let failure = |property: &str, class, error: &str| PropertyFailure { property: property.into(), class, error: error.into() };
        assert_eq!(
            outcome(failed),
            Err(OpError::Partial(vec![
                failure("target_temperature", FailureClass::Rejected, "the CoolMaster answered `ERROR: 1`"),
                failure("fan_speed", FailureClass::Unusable, "the CoolMaster's reply is not text"),
                failure("swing", FailureClass::Unsupported, "`swing` is not a property a request can set"),
            ]))
        );
    }
}
