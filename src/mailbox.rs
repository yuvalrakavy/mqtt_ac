//! The Coolmaster's commands (the owner's device-down policy, 2026-10-02; Store no-hang §14.3).
//!
//! The MQTT session and the polling worker post here and never wait; the Coolmaster worker takes
//! from here only while the Coolmaster is connected. A bounded queue in its place parked the
//! session once it was full while the Coolmaster was down, and the session is what reads the
//! broker's CONNACK: a broker blip was then never announced or resubscribed (review B3).
//!
//! Each command is one of three kinds:
//! - **State** (`SetUnitPower`, `SetUnitMode`, `SetFanSpeed`, `SetTargetTemperature`), keyed by
//!   unit and property. A post replaces a waiting command with the same key and moves it to the
//!   back, so the Coolmaster receives the latest value per unit and property, in the order the
//!   values were last set. Kept while the Coolmaster is down, for its return.
//! - **Momentary** (`ResetFilter`): queued in order while the Coolmaster is connected, at most
//!   `MOMENTARY_CAP` of them (past it, refused); refused at once while it is down — by the time it
//!   is back the command would mean nothing.
//! - **Read** (`PublishUnitState`, `PublishUnitsState`): coalesced per target, and moved to the
//!   back like a state; dropped while the Coolmaster is down — its return publishes every unit's
//!   observed state.
//!
//! So the mailbox holds at most one state per unit and property, one read per unit and one for
//! all of them, and `MOMENTARY_CAP` momentary commands.
//!
//! The states kept while the Coolmaster was down are **held**: when it is back the worker takes
//! them first, one at a time (`take_held`), and they stay in the mailbox until then — so a value
//! set meanwhile replaces its held one, which is never sent (re-review C3). Taken out all at once,
//! a held value already superseded was applied anyway, however long the catch-up took.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tracing::{debug, info};

use crate::messages::ToCoolmasterMessage;

/// Momentary commands that may wait for a connected Coolmaster; past it, one is refused.
pub const MOMENTARY_CAP: usize = 64;

/// What became of a post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// Waiting for the Coolmaster worker.
    Queued,
    /// A read while the Coolmaster is down: its return publishes every unit's state anyway.
    Dropped,
    /// A momentary command that will not be sent. The poster reports it on the error topic.
    Refused(Refusal),
}

/// Why a momentary command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The Coolmaster is not connected.
    CoolmasterDown,
    /// `MOMENTARY_CAP` momentary commands are already waiting.
    QueueFull,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::CoolmasterDown => write!(f, "the Coolmaster is not connected"),
            Refusal::QueueFull => write!(
                f,
                "{MOMENTARY_CAP} momentary commands are already waiting for the Coolmaster"
            ),
        }
    }
}

/// The text published on the error topic for a refused command.
pub fn refusal_text(command: &ToCoolmasterMessage, why: Refusal) -> String {
    format!("{command:?} refused: {why}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Property {
    Power,
    Mode,
    Fan,
    Setpoint,
}

/// What a waiting command is coalesced by. A momentary command has none.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    State(String, Property),
    /// One unit's, or (`None`) every unit's.
    Read(Option<String>),
}

enum Class {
    State(Key),
    Momentary,
    Read(Key),
}

fn classify(command: &ToCoolmasterMessage) -> Class {
    use ToCoolmasterMessage::*;
    let state = |unit: &String, property| Class::State(Key::State(unit.clone(), property));
    match command {
        SetUnitPower(unit, _) => state(unit, Property::Power),
        SetUnitMode(unit, _) => state(unit, Property::Mode),
        SetFanSpeed(unit, _) => state(unit, Property::Fan),
        SetTargetTemperature(unit, _) => state(unit, Property::Setpoint),
        ResetFilter(_) => Class::Momentary,
        PublishUnitState(unit) => Class::Read(Key::Read(Some(unit.clone()))),
        PublishUnitsState => Class::Read(Key::Read(None)),
    }
}

/// See the module documentation.
#[derive(Debug)]
pub struct Mailbox {
    state: Mutex<State>,
    posted: Notify,
}

#[derive(Debug)]
struct State {
    connected: bool,
    pending: VecDeque<Entry>,
    /// Momentary commands in `pending`.
    momentary: usize,
    /// Momentary commands refused since the Coolmaster went down, for the recovery line.
    refused: u32,
    /// This outage's first refusal has been logged at INFO.
    refusal_logged: bool,
    /// The first refusal for a full momentary queue has been logged at INFO; cleared when the
    /// queue drains, so each such episode is said once.
    full_logged: bool,
}

#[derive(Debug)]
struct Entry {
    key: Option<Key>,
    command: ToCoolmasterMessage,
    /// A state kept while the Coolmaster was down, for the worker to take first when it is back.
    /// The held entries are always at the front: everything posted while it is connected goes
    /// behind them.
    held: bool,
}

impl State {
    /// Replace a waiting command with the same key, and move it to the back — held if the
    /// Coolmaster is down.
    fn replace(&mut self, key: Key, command: ToCoolmasterMessage) {
        if let Some(i) = self
            .pending
            .iter()
            .position(|e| e.key.as_ref() == Some(&key))
        {
            self.pending.remove(i);
        }
        let held = !self.connected;
        self.pending.push_back(Entry {
            key: Some(key),
            command,
            held,
        });
    }

    /// Note a refusal; `true` when it is the first of its episode — the outage, or the full
    /// queue. Only an outage's refusals are counted, for its recovery line.
    fn refuse(&mut self, why: Refusal) -> bool {
        match why {
            Refusal::CoolmasterDown => {
                self.refused += 1;
                !std::mem::replace(&mut self.refusal_logged, true)
            }
            Refusal::QueueFull => !std::mem::replace(&mut self.full_logged, true),
        }
    }
}

/// The first refusal of an episode is an INFO, the rest DEBUG; the recovery INFO counts them.
/// Called with the mailbox's lock released.
fn log_refusal(command: &ToCoolmasterMessage, why: Refusal, first: bool) {
    if first {
        info!(kind = "command_refused", command = ?command, reason = %why, "Coolmaster command refused");
    } else {
        debug!(command = ?command, reason = %why, "Coolmaster command refused");
    }
}

impl Mailbox {
    /// A mailbox for a Coolmaster not connected yet: until the worker's first connect, states are
    /// held for it, momentary commands refused and reads dropped, as in any outage (re-review C10:
    /// started as connected, a momentary command posted before the first connect was queued, and
    /// applied whenever that connect came).
    pub fn new() -> Arc<Mailbox> {
        Arc::new(Mailbox {
            state: Mutex::new(State {
                connected: false,
                pending: VecDeque::new(),
                momentary: 0,
                refused: 0,
                refusal_logged: false,
                full_logged: false,
            }),
            posted: Notify::new(),
        })
    }

    /// Post a command. Never waits: see the module documentation for what becomes of it.
    pub fn post(&self, command: ToCoolmasterMessage) -> Posted {
        let class = classify(&command);
        let mut refusal = None;
        let posted = {
            // WAIT: coolmaster-mailbox-lock
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match class {
                Class::State(key) => {
                    state.replace(key, command.clone());
                    Posted::Queued
                }
                Class::Read(key) if state.connected => {
                    state.replace(key, command.clone());
                    Posted::Queued
                }
                Class::Read(_) => Posted::Dropped,
                Class::Momentary => {
                    let why = if !state.connected {
                        Some(Refusal::CoolmasterDown)
                    } else if state.momentary >= MOMENTARY_CAP {
                        Some(Refusal::QueueFull)
                    } else {
                        None
                    };
                    match why {
                        None => {
                            state.momentary += 1;
                            state.pending.push_back(Entry {
                                key: None,
                                command: command.clone(),
                                held: false,
                            });
                            Posted::Queued
                        }
                        Some(why) => {
                            refusal = Some((why, state.refuse(why)));
                            Posted::Refused(why)
                        }
                    }
                }
            }
        };
        // Logged after the lock is released: nothing waits, and nothing logs, under it.
        if let Some((why, first)) = refusal {
            log_refusal(&command, why, first);
        }
        if posted == Posted::Queued {
            self.posted.notify_one();
        }
        posted
    }

    /// The Coolmaster is connected: posts are queued for it again, behind the state held while it
    /// was down, which the worker takes first (`take_held`). Returns how many momentary commands
    /// were refused while it was down (none, if it was never down).
    pub fn connected(&self) -> u32 {
        // WAIT: coolmaster-mailbox-lock
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.connected {
            return 0;
        }
        state.connected = true;
        state.refusal_logged = false;
        std::mem::take(&mut state.refused)
    }

    /// The next held state — the oldest kept while the Coolmaster was down and not replaced since
    /// — or `None` when none is left. Never waits.
    pub fn take_held(&self) -> Option<ToCoolmasterMessage> {
        // WAIT: coolmaster-mailbox-lock
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.pending.front().is_some_and(|e| e.held) {
            state.pending.pop_front().map(|e| e.command)
        } else {
            None
        }
    }

    /// The Coolmaster is not connected: states keep waiting for it, waiting reads are dropped, and
    /// waiting momentary commands are refused — returned, for the worker to report.
    pub fn disconnected(&self) -> Vec<ToCoolmasterMessage> {
        let mut refused = Vec::new();
        let first = {
            // WAIT: coolmaster-mailbox-lock
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if !state.connected {
                return refused;
            }
            state.connected = false;
            state.refused = 0;
            state.refusal_logged = false;
            state.momentary = 0;
            state.full_logged = false;
            let waiting = std::mem::take(&mut state.pending);
            for entry in waiting {
                match entry.key {
                    Some(Key::State(..)) => state.pending.push_back(Entry { held: true, ..entry }),
                    Some(Key::Read(_)) => {}
                    None => refused.push(entry.command),
                }
            }
            let mut first = false;
            for _ in &refused {
                first |= state.refuse(Refusal::CoolmasterDown);
            }
            first
        };
        for (i, command) in refused.iter().enumerate() {
            log_refusal(command, Refusal::CoolmasterDown, first && i == 0);
        }
        refused
    }

    /// States the worker took but could not apply, the connection having failed: back at the
    /// front, in order, held for the next connection, unless a newer value for the same unit and
    /// property is already waiting. Anything else is not put back.
    pub fn restore(&self, commands: Vec<ToCoolmasterMessage>) {
        // WAIT: coolmaster-mailbox-lock
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for command in commands.into_iter().rev() {
            if let Class::State(key) = classify(&command) {
                if !state.pending.iter().any(|e| e.key.as_ref() == Some(&key)) {
                    state.pending.push_front(Entry {
                        key: Some(key),
                        command,
                        held: true,
                    });
                }
            }
        }
    }

    /// The next command, waiting for a post if there is none. The worker calls it only while the
    /// Coolmaster is connected. Cancellation-safe: nothing is taken before the only await.
    pub async fn take(&self) -> ToCoolmasterMessage {
        loop {
            {
                // WAIT: coolmaster-mailbox-lock
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(entry) = state.pending.pop_front() {
                    if entry.key.is_none() {
                        state.momentary -= 1;
                        if state.momentary == 0 {
                            state.full_logged = false;
                        }
                    }
                    return entry.command;
                }
            }
            // WAIT: coolmaster-mailbox
            self.posted.notified().await;
        }
    }

    /// The commands waiting, in order (tests).
    #[cfg(test)]
    pub fn waiting(&self) -> Vec<ToCoolmasterMessage> {
        let state = self.state.lock().unwrap();
        state.pending.iter().map(|e| e.command.clone()).collect()
    }
}

#[cfg(test)]
mod tests;
