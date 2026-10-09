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
use std::time::{Duration, Instant};

use mqtt_bridge_kit::{
    CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, FailureClass, Feedback, LinkContext, LinkError, LinkHandle, OpError,
    PropertyFailure, PropertyMap, Reporter, Scope, TargetState, Timing, Value, Writes,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::protocol;

/// What the CoolMaster driver declares (spec §4.4): acknowledged writes (an `OK` is the
/// CoolMaster confirming the command), observed feedback (`ls2`), one momentary command,
/// `ResetFilter`, and v1's timing — a connect and its prompt within 10 s, a retry every 5 s, one
/// WARN past 30 s down, a listing every 4 s. v1 bounded each exchange at 10 s; the runtime bounds
/// each operation, an `apply`'s few exchanges together, which a CoolMaster answers in milliseconds.
/// Anything else `DriverInfo` holds keeps its default.
// Every field is named today (the runtime dropped `restore`); the update keeps it so when it adds one.
#[allow(clippy::needless_update)]
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
    /// The current link's handle: its reporter, and its `link_lost` for a read-back that finds the
    /// link dead after its command was confirmed.
    handle: Option<LinkHandle>,
    /// The runtime's bound on each operation, as the command line set it: a read-back keeps within
    /// what its operation has left (see [`Coolmaster::read_back`]).
    operation_bound: Duration,
    /// The units whose listing line cannot be used now, as last said.
    passed_over: BTreeSet<String>,
    /// The units the full listings have named, for telling one gone (see [`Coolmaster::gone`]).
    known: BTreeMap<String, Known>,
    /// Each unit's scale at the CoolMaster, from its last usable line: a setpoint for a unit in
    /// °F goes in °F. For a unit not here, `apply` reads it first, and never guesses.
    scales: BTreeMap<String, protocol::Scale>,
}

/// A unit the listings have named.
#[derive(Debug, Default)]
struct Known {
    /// Its State was reported (a line of it parsed), so there is a State to say it lost power in.
    reported: bool,
    /// Usable full listings that have named it, up to [`GONE_AFTER`]: at `GONE_AFTER` it is
    /// established — a unit of the CoolMaster's, which can lose its power. One named by fewer (a
    /// garble that read as an address, in one listing) is a ghost when it vanishes.
    listed: u32,
    /// Usable full listings in a row that have omitted it, up to [`GONE_AFTER`].
    missed: u32,
    /// When the run of omissions began (the first listing that omitted it).
    first_missed: Option<Instant>,
    /// Since when it has had no power — its first omission — during a power-loss episode.
    lost_since: Option<Instant>,
}

/// What a listing says about the units' power ([`Coolmaster::observe`]).
#[derive(Debug, Default, PartialEq)]
struct Observed {
    /// Established units that have just lost their power.
    lost: Vec<String>,
    /// Units never established that are gone: ghosts, to retract quietly.
    vanished: Vec<String>,
    /// Units whose power is back, and how long it was gone.
    restored: Vec<(String, Duration)>,
}

/// Usable full listings in a row that must omit a unit before it is taken for one that lost its
/// power: one listing can be cut short or come mid-scan; two in a row (8 s at the default poll)
/// say it left. With polling off (`--poll off`), full listings come only at a connect or a
/// `refresh`, so it is two of those.
pub const GONE_AFTER: u32 = 2;

/// The most a prompt or a reply may be, in bytes, before its `>`: far past any CoolMaster's (a
/// listing of hundreds of units is some tens of KiB at most). A reply past it has lost its
/// framing — the link is dropped at once — and is never buffered on until the deadline.
pub const MAX_REPLY: u64 = 64 * 1024;

impl Coolmaster {
    pub fn new(address: Address) -> Coolmaster {
        Coolmaster {
            address,
            link: None,
            dropped: None,
            handle: None,
            operation_bound: info().timing.operation,
            passed_over: BTreeSet::new(),
            known: BTreeMap::new(),
            scales: BTreeMap::new(),
        }
    }

    /// The runtime's bound on each operation, when the command line changed it
    /// (`--operation-timeout`): `main` passes the bridge's own.
    pub fn operation_bound(mut self, bound: Duration) -> Coolmaster {
        self.operation_bound = bound;
        self
    }

    fn report(&self) -> Option<&Reporter> {
        self.handle.as_ref().map(LinkHandle::report)
    }

    /// What a listing (`full`: of every unit) says about the units' power. A unit the CoolMaster
    /// stops listing is one whose power went (owner, 2026-10-09), never a unit to retract: its
    /// State is kept, marked `"powered": false`, until it is listed again.
    ///
    /// - **Presence:** every unit a listing names — a usable line, or a bad one under its own
    ///   address — is there, whatever listing it is (a full one, `ls2 <unit>`, a read-back): its
    ///   run of omissions starts over. A usable line of a unit that had lost its power ends the
    ///   episode (`restored`, with how long it was down).
    /// - **Omissions** count only on a usable full listing that can say who is missing: never an
    ///   empty one (a CoolMaster rebooting, or still scanning its line), one with no usable line,
    ///   or one with a line whose address itself is garbled (which unit it was, nobody can tell).
    ///   After [`GONE_AFTER`] in a row a unit is gone: one **established** — named by at least
    ///   `GONE_AFTER` usable full listings — has lost its power (`lost`, once per episode, from its
    ///   first omission); one never established is a ghost (a garble that read as an address, in a
    ///   listing or so), retracted quietly (`vanished`) if its State was reported, and one known
    ///   only from a garbled line is just forgotten.
    ///
    /// What it cannot tell: a garble that reads as an address in `GONE_AFTER` listings or more is
    /// established like any unit. With polling off, full listings come only at a connect or a
    /// `refresh`, so "two in a row" is two of those.
    fn observe(&mut self, listing: &protocol::Listing, full: bool) -> Observed {
        let mut observed = Observed::default();
        let valid_bad = listing.bad.iter().filter(|b| protocol::valid_unit(&b.unit));
        let present: BTreeSet<&str> = listing.states.iter().map(|s| s.target.as_str()).chain(valid_bad.map(|b| b.unit.as_str())).collect();
        for unit in &present {
            let known = self.known.entry((*unit).to_owned()).or_default();
            known.missed = 0;
            known.first_missed = None;
        }
        for state in &listing.states {
            let known = self.known.entry(state.target.clone()).or_default();
            known.reported = true;
            if let Some(since) = known.lost_since.take() {
                observed.restored.push((state.target.clone(), since.elapsed()));
            }
        }
        let certain = listing.bad.iter().all(|b| protocol::valid_unit(&b.unit));
        if !full || !certain || listing.states.is_empty() {
            return observed;
        }
        for unit in &present {
            if let Some(known) = self.known.get_mut(*unit) {
                known.listed = (known.listed + 1).min(GONE_AFTER);
            }
        }
        let now = Instant::now();
        self.known.retain(|unit, known| {
            if present.contains(unit.as_str()) {
                return true;
            }
            if known.missed == 0 {
                known.first_missed = Some(now);
            }
            known.missed = (known.missed + 1).min(GONE_AFTER);
            if known.missed < GONE_AFTER || known.lost_since.is_some() {
                return true;
            }
            if known.reported && known.listed >= GONE_AFTER {
                known.lost_since = Some(known.first_missed.unwrap_or(now));
                observed.lost.push(unit.clone());
                return true;
            }
            if known.reported {
                observed.vanished.push(unit.clone());
            }
            false
        });
        for unit in observed.lost.iter().chain(&observed.vanished) {
            self.passed_over.remove(unit);
        }
        for unit in &observed.vanished {
            self.scales.remove(unit);
        }
        observed
    }

    /// A full listing's power losses (tests).
    #[cfg(test)]
    fn gone(&mut self, listing: &protocol::Listing) -> Vec<String> {
        self.observe(listing, true).lost
    }

    /// A per-unit listing's restorations (tests).
    #[cfg(test)]
    fn restored(&mut self, listing: &protocol::Listing) -> Vec<(String, Duration)> {
        self.observe(listing, false).restored
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
        (&mut *link).take(MAX_REPLY).read_until(b'>', &mut reply).await.map_err(lost)?;
        if reply.last() != Some(&b'>') && reply.len() as u64 >= MAX_REPLY {
            return Err(OpError::Link(LinkError::new(format!("a reply of {MAX_REPLY} bytes and no prompt: its framing is lost"))));
        }
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
        let started = Instant::now();
        let command = protocol::reset_filter(unit)?;
        self.exchange(&command).await?;
        self.read_back(unit, started).await;
        Ok(None)
    }

    /// After a command: read the unit back, so its `State` follows at once rather than at the next
    /// poll. Its outcome never replaces the command's own — the command was confirmed already — so
    /// it has a bound of its own, within what the operation (begun at `started`) has left, less a
    /// margin: the runtime's deadline on the operation never falls in it. A listing that is refused
    /// or unusable waits for the poll. A link that fails here, or a read-back that overruns its
    /// bound (the exchange cut midway, the connection in an unknown state), is dropped and reported
    /// lost, so the runtime reconnects at once rather than at the next operation — with polling off,
    /// that could be never.
    async fn read_back(&mut self, unit: &str, started: Instant) {
        let budget = self.operation_bound.saturating_sub(started.elapsed()).saturating_sub(self.operation_bound / 5);
        if budget.is_zero() {
            debug!(unit, "No time left for a unit's read-back after its command; the next poll reads it");
            return;
        }
        // WAIT: coolmaster-read-back
        let read = tokio::time::timeout(budget, self.read_state(Scope::Target(unit.to_owned()))).await;
        let lost = match read {
            Ok(Ok(states)) => {
                if let Some(report) = self.report() {
                    for state in states {
                        report.state(&state.target, state.values, false);
                    }
                }
                return;
            }
            Ok(Err(OpError::Link(e))) => e.reason().to_owned(),
            Ok(Err(e)) => {
                debug!(unit, error = %e, "A unit's read-back after its command failed; the next poll reads it");
                return;
            }
            Err(_) => {
                self.link = None;
                format!("a unit's read-back after its command overran its bound ({} ms)", budget.as_millis())
            }
        };
        self.dropped = Some(lost.clone());
        if let Some(handle) = &self.handle {
            handle.link_lost(lost);
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
        self.handle = Some(ctx.handle());
        let Address { host, port } = &self.address;
        // WAIT: coolmaster-io
        let stream = TcpStream::connect((host.as_str(), *port)).await?;
        let _ = stream.set_nodelay(true);
        let mut link = BufReader::new(stream);
        let mut prompt = Vec::new();
        // WAIT: coolmaster-io
        (&mut link).take(MAX_REPLY).read_until(b'>', &mut prompt).await?;
        if prompt.last() != Some(&b'>') {
            return Err(LinkError::new(if prompt.len() as u64 >= MAX_REPLY {
                "the CoolMaster sent no prompt within its byte limit"
            } else {
                "the CoolMaster closed the connection before its prompt"
            }));
        }
        self.link = Some(link);
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.link = None;
        self.dropped = None;
        self.handle = None;
    }

    /// `ls2`: every unit (`Scope::All`, and `Scope::Any`), or one. A line that cannot be used costs
    /// only its own unit; a listing with no usable line is `Unusable`. What the listing says of the
    /// units' power ([`Coolmaster::observe`]) is reported here: a unit that lost its power keeps
    /// its State, marked `"powered": false`, with one `unit_lost_power` Event and WARN; one whose
    /// power is back gets its fresh State first, then one `unit_power_restored` Event and an INFO;
    /// a ghost (never established) is retracted quietly.
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let unit = match &scope {
            Scope::Target(unit) => Some(unit.as_str()),
            Scope::All | Scope::Any => None,
        };
        let command = protocol::list(unit)?;
        let body = self.exchange(&command).await?;
        let listing = protocol::parse_listing(&body);
        self.note(&listing);
        self.scales.extend(listing.scales.iter().map(|(unit, scale)| (unit.clone(), *scale)));
        let observed = self.observe(&listing, unit.is_none());
        if let Some(report) = self.report() {
            for (unit, down) in &observed.restored {
                let down_for_ms = down.as_millis() as u64;
                info!(kind = "unit_power_restored", unit = %unit, down_for_ms, "A unit the CoolMaster had stopped listing is listed again: its power is back");
                // Its fresh State (`powered: true`) goes first, then the Event.
                if let Some(state) = listing.states.iter().find(|s| &s.target == unit) {
                    report.state(unit, state.values.clone(), false);
                }
                report.event(json!({ "kind": "unit_power_restored", "target": unit, "down_for_ms": down_for_ms }));
            }
            for unit in &observed.lost {
                // Once per episode: `observe` names a unit only as its power goes.
                warn!(kind = "unit_lost_power", unit = %unit, "The CoolMaster no longer lists a unit: it has lost its power");
                // Its last values stay (the runtime merges); only `powered` changes.
                report.state(unit, PropertyMap::from_iter([(protocol::POWERED.to_owned(), Value::from(false))]), false);
                report.event(json!({ "kind": "unit_lost_power", "target": unit }));
            }
            for unit in &observed.vanished {
                debug!(unit = %unit, "A unit never established is no longer listed: a ghost, retracted");
                report.remove(unit);
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
    ///
    /// A setpoint goes in the unit's own scale: for a unit whose scale is not known yet (no usable
    /// line of it listed), the unit is read first (`ls2 <unit>`) to learn it; a read that cannot
    /// tell refuses the setpoint ("scale unknown"), never a guess of °C.
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
        let started = Instant::now();
        let mut failed = Vec::new();
        let mut sent = false;
        if values.contains_key(protocol::TARGET_TEMPERATURE) && !self.scales.contains_key(target) && protocol::valid_unit(target) {
            match self.read_state(Scope::Target(target.to_owned())).await {
                Ok(states) => {
                    if let Some(report) = self.report() {
                        for state in states {
                            report.state(&state.target, state.values, false);
                        }
                    }
                }
                Err(OpError::Link(e)) => return Err(OpError::Link(e)),
                Err(e) => debug!(unit = target, error = %e, "A unit's scale could not be read before its setpoint"),
            }
        }
        let scale = self.scales.get(target).copied();
        for step in protocol::plan_scaled(target, values, scale)? {
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
            self.read_back(target, started).await;
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

    /// A unit has lost its power only once two usable full listings in a row omit it, only if it
    /// was ever reported, and once per episode; listed usably again, it is restored, once. One
    /// listed garbled under its own address is still there; a line whose address is garbled, an
    /// unusable listing and an empty one change nothing.
    #[test]
    fn a_unit_is_lost_only_after_two_usable_listings_omit_it() {
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
                "a garbled address took a unit for lost"
            );
            assert!(driver.gone(&listing("garbage\r\nmore garbage")).is_empty(), "an unusable listing took a unit for lost");
            assert!(driver.gone(&listing("")).is_empty(), "an empty listing took a unit for lost");
        }
        // Omitted by one usable listing: not yet; by a second in a row: lost.
        assert!(driver.gone(&listing(a)).is_empty(), "one omission took a unit for lost");
        // (The unusable listing's `garbage` and `more` read as addresses, were never reported, and
        // are forgotten here, not taken for lost.)
        assert_eq!(
            driver.gone(&listing(a)),
            ["L7.401"],
            "only L7.401 is lost: a unit never reported was taken for lost, or the lost one was not"
        );
        assert!(driver.gone(&listing(a)).is_empty(), "a unit was taken for lost twice in one episode");
        assert!(driver.restored(&listing(a)).is_empty(), "a unit still unlisted was restored");
        // Listed again: restored, once.
        let restored: Vec<String> = driver.restored(&listing(both)).into_iter().map(|(unit, _)| unit).collect();
        assert_eq!(restored, ["L7.401"], "a unit listed again was not restored");
        assert!(driver.restored(&listing(both)).is_empty(), "a unit was restored twice");
        // One omission, then listed again: the count starts over.
        let started_over = "a unit listed again was taken for lost: its earlier omission was not forgotten";
        assert!(driver.gone(&listing(both)).is_empty(), "{started_over}");
        assert!(driver.gone(&listing(a)).is_empty(), "{started_over}");
        assert!(driver.gone(&listing(both)).is_empty(), "{started_over}");
        assert!(driver.gone(&listing(a)).is_empty(), "{started_over}");
        // Known only from a garbled line under its address, never reported: forgotten, not lost.
        assert!(driver.gone(&listing(&format!("{both}\r\nL7.402 ON ??? garbled"))).is_empty());
        assert!(driver.gone(&listing(both)).is_empty());
        assert!(driver.gone(&listing(both)).is_empty(), "a unit never reported was taken for lost");
        // Omitted once, then named by a listing that cannot say who is missing (a garbled address
        // beside it): it was there, so the run of omissions starts over.
        assert!(driver.gone(&listing(a)).is_empty());
        assert!(driver.gone(&listing(&format!("{both}\r\nL7.4\u{1}2 OFF 22.0C 25.0C High Cool OK - 0"))).is_empty());
        assert!(driver.gone(&listing(a)).is_empty(), "a unit named by the listing in between was taken for lost");
        // Lost, then seen again only by a per-unit read (`ls2 L7.401`, a read-back): restored, and
        // its omissions start over — one full listing omitting it is not a new loss.
        assert_eq!(driver.gone(&listing(a)), ["L7.401"]);
        let per_unit = listing("L7.401 OFF 22.0C 25.0C High Cool OK - 0");
        assert_eq!(driver.restored(&per_unit).len(), 1, "a per-unit read did not restore the unit");
        assert!(driver.gone(&listing(a)).is_empty(), "one omission after a per-unit restore was a new loss");
        // A ghost: a garble that read as an address, in one listing. Never established, so when it
        // vanishes it is retracted quietly — never a unit that lost its power.
        let ghost = format!("{both}\r\nL7.4O1 ON 22.0C 25.0C High Cool OK - 0");
        assert!(driver.observe(&listing(&ghost), true).lost.is_empty());
        assert!(driver.observe(&listing(both), true).vanished.is_empty(), "a ghost vanished after one omission");
        let observed = driver.observe(&listing(both), true);
        assert!(observed.lost.is_empty(), "a ghost from one garbled listing was taken for a unit that lost its power");
        assert_eq!(observed.vanished, ["L7.4O1"], "a ghost was not retracted");
        assert!(!driver.known.contains_key("L7.4O1"), "a ghost stayed known");
        // Established but never reported (listed only garbled under its own address): forgotten.
        let garbled = format!("{both}\r\nL7.403 ON ??? garbled");
        driver.observe(&listing(&garbled), true);
        driver.observe(&listing(&garbled), true);
        driver.observe(&listing(both), true);
        assert!(driver.observe(&listing(both), true).lost.is_empty(), "a unit never reported was taken for lost");
        assert!(!driver.known.contains_key("L7.403"));
        // The counts stay within GONE_AFTER, however long a unit is omitted or listed.
        for _ in 0..5 {
            driver.observe(&listing(a), true);
        }
        let known = &driver.known["L7.401"];
        assert_eq!((known.missed, known.listed), (GONE_AFTER, GONE_AFTER), "the counts are not capped");
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
