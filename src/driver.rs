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

use std::fmt;
use std::time::Duration;

use mqtt_bridge_kit::{
    CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, Feedback, LinkContext, LinkError, OpError, PropertyMap, Reporter,
    Scope, TargetState, Timing, Value, Writes,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::debug;

use crate::protocol;

/// What the CoolMaster driver declares (spec §4.4): acknowledged writes (an `OK` is the
/// CoolMaster confirming the command), observed feedback (`ls2`), one momentary command,
/// `ResetFilter`, and v1's timing — a connect and its prompt within 10 s, a retry every 5 s, one
/// WARN past 30 s down, a listing every 4 s. v1 bounded each exchange at 10 s; the runtime bounds
/// each operation, an `apply`'s few exchanges together, which a CoolMaster answers in milliseconds.
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
}

impl Coolmaster {
    pub fn new(address: Address) -> Coolmaster {
        Coolmaster { address, link: None, dropped: None, report: None }
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

/// An error's own message, without its class.
fn message(e: &OpError) -> String {
    match e {
        OpError::Rejected(m) | OpError::Unusable(m) | OpError::Unsupported(m) => m.clone(),
        OpError::Link(e) => e.reason().to_owned(),
    }
}

/// An `apply`'s outcome: `Ok`, or the first failure's class, naming every property that failed.
/// (The runtime reports an `apply`'s failure on `Error` for each of its properties.)
fn outcome(failed: Vec<(String, OpError)>) -> Result<(), OpError> {
    let Some((_, first)) = failed.first() else {
        return Ok(());
    };
    let text = failed.iter().map(|(property, e)| format!("{property}: {}", message(e))).collect::<Vec<_>>().join("; ");
    Err(match first {
        OpError::Rejected(_) => OpError::Rejected(text),
        OpError::Unusable(_) => OpError::Unusable(text),
        OpError::Unsupported(_) => OpError::Unsupported(text),
        OpError::Link(e) => OpError::Link(e.clone()),
    })
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

    /// `ls2`: every unit (`Scope::All`, and `Scope::Any`), or one.
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let unit = match &scope {
            Scope::Target(unit) => Some(unit.as_str()),
            Scope::All | Scope::Any => None,
        };
        let command = protocol::list(unit)?;
        let body = self.exchange(&command).await?;
        protocol::parse_listing(&body)
    }

    /// A command per property, in the CoolMaster's order ([`protocol::plan`]). A property that
    /// fails on its own — refused by the CoolMaster, answered with something unusable, or not one
    /// a request can set — is passed over and the rest still sent (turning a unit off is not lost
    /// to a refused setpoint); a lost link ends it at once, and the runtime holds the request for
    /// the next connection. Then the unit is read back.
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
                Err(e) => failed.push((step.property, e)),
            }
        }
        if sent {
            self.read_back(target).await;
        }
        outcome(failed)
    }

    /// `ResetFilter` (`filt <unit>`), then the unit read back.
    async fn execute(&mut self, target: &str, call: &CommandCall) -> Result<Option<Value>, OpError> {
        if call.command != protocol::RESET_FILTER {
            return Err(OpError::Unsupported(format!("the CoolMaster has no command `{}`", call.command)));
        }
        let command = protocol::reset_filter(target)?;
        self.exchange(&command).await?;
        self.read_back(target).await;
        Ok(None)
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

    /// An `apply` that partly failed names every property that failed, in the class of the first.
    #[test]
    fn an_apply_that_partly_failed_names_each_property() {
        assert_eq!(outcome(Vec::new()), Ok(()));
        let failed = vec![
            ("target_temperature".to_owned(), OpError::Rejected("the CoolMaster answered `ERROR: 1`".into())),
            ("swing".to_owned(), OpError::Unsupported("`swing` is not a property a request can set".into())),
        ];
        assert_eq!(
            outcome(failed),
            Err(OpError::Rejected(
                "target_temperature: the CoolMaster answered `ERROR: 1`; swing: `swing` is not a property a request can set".into()
            ))
        );
    }
}
