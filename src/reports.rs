//! What the Coolmaster worker reports for MQTT, handed over without waiting (Store no-hang 3b
//! re-review C1, C2; the fleet's rule: the device side never waits on MQTT).
//!
//! The worker hands its reports here and goes on; the publisher takes them when the broker lets
//! it. A bounded queue in its place made the worker wait on MQTT: with the broker stalled the
//! queue filled, and the worker took no command — catch-up included — until the broker recovered.
//!
//! - **Observed state** is the bridge's retained model: the Coolmaster flag, and each unit's latest
//!   state. A report is folded into it at once, before anything waits, so what the publisher
//!   publishes is always the latest, and a publish cut short (its MQTT session ended) loses nothing:
//!   the next session publishes the whole model again after its CONNACK (`model`). Between takes a
//!   unit's states coalesce to its latest; one that did not change is not published again.
//! - **Errors** are events, queued in order: at most `ERROR_CAP`, the oldest displaced past it —
//!   one WARN (`error_report_dropped`) when an episode of displacement starts, and an INFO
//!   (`error_report_drop_ended`, with how many and how long) once the publisher takes the queue.
//!
//! The lock is held only to change or copy what is waiting: never across an await, no other lock
//! under it, and nothing logged under it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Notify;
use tracing::{info, warn};

use crate::ac_unit::UnitState;

/// Errors that may wait for the publisher; past it, the oldest is displaced.
pub const ERROR_CAP: usize = 64;

/// See the module documentation.
#[derive(Debug, Default)]
pub struct Reports {
    state: Mutex<State>,
    ready: Notify,
}

#[derive(Debug, Default)]
struct State {
    /// The Coolmaster flag, once known, and whether it changed since the publisher last took it.
    connected: Option<bool>,
    connected_changed: bool,
    /// Every unit's latest observed state.
    units: BTreeMap<String, UnitState>,
    /// The units whose state changed since the publisher last took them, in the order they did.
    changed: Vec<String>,
    errors: VecDeque<String>,
    /// The episode of displacement under way: when it began, and how many errors it displaced.
    displacing: Option<(Instant, u64)>,
}

/// What changed since the publisher last took it.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub connected: Option<bool>,
    pub units: Vec<UnitState>,
    pub errors: Vec<String>,
}

/// Everything known: what the publisher publishes again after a CONNACK.
#[derive(Debug, Default, PartialEq)]
pub struct Model {
    pub connected: Option<bool>,
    pub units: Vec<UnitState>,
}

impl Reports {
    pub fn new() -> Arc<Reports> {
        Arc::new(Reports::default())
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        // WAIT: reports-lock
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The Coolmaster is connected — it has answered — or it is not. Never waits.
    pub fn coolmaster_connected(&self, connected: bool) {
        let changed = {
            let mut state = self.locked();
            let changed = state.connected != Some(connected);
            if changed {
                state.connected = Some(connected);
                state.connected_changed = true;
            }
            changed
        };
        if changed {
            self.ready.notify_one();
        }
    }

    /// Units' observed states, from a listing. Never waits.
    pub fn observed(&self, states: impl IntoIterator<Item = UnitState>) {
        let changed = {
            let mut state = self.locked();
            let mut changed = false;
            for unit_state in states {
                if state.units.get(&unit_state.unit) == Some(&unit_state) {
                    continue;
                }
                if !state.changed.contains(&unit_state.unit) {
                    state.changed.push(unit_state.unit.clone());
                }
                state.units.insert(unit_state.unit.clone(), unit_state);
                changed = true;
            }
            changed
        };
        if changed {
            self.ready.notify_one();
        }
    }

    /// An error for the error topic. Never waits: past `ERROR_CAP` waiting, the oldest goes.
    pub fn error(&self, text: String) {
        let displacing = {
            let mut state = self.locked();
            state.errors.push_back(text);
            if state.errors.len() > ERROR_CAP {
                state.errors.pop_front();
                match &mut state.displacing {
                    Some((_, displaced)) => {
                        *displaced += 1;
                        false
                    }
                    None => {
                        state.displacing = Some((Instant::now(), 1));
                        true
                    }
                }
            } else {
                false
            }
        };
        if displacing {
            warn!(
                kind = "error_report_dropped",
                waiting = ERROR_CAP,
                "Coolmaster error reports are displacing older ones: MQTT is not taking them"
            );
        }
        self.ready.notify_one();
    }

    /// What changed since the last take, for the publisher; the errors are taken out of the queue.
    pub fn take(&self) -> Changes {
        let (changes, ended) = {
            let mut state = self.locked();
            let connected = if std::mem::take(&mut state.connected_changed) {
                state.connected
            } else {
                None
            };
            let units = std::mem::take(&mut state.changed)
                .into_iter()
                .filter_map(|unit| state.units.get(&unit).cloned())
                .collect();
            let errors = state.errors.drain(..).collect();
            let ended = state.displacing.take();
            (
                Changes {
                    connected,
                    units,
                    errors,
                },
                ended,
            )
        };
        if let Some((since, dropped)) = ended {
            info!(
                kind = "error_report_drop_ended",
                dropped,
                lasted_ms = since.elapsed().as_millis() as u64,
                "Coolmaster error reports are taken again"
            );
        }
        changes
    }

    /// Everything known, for the publisher to publish again after a CONNACK; what changed is then
    /// published with it, so it is no longer waiting. Errors stay queued: they are not state.
    pub fn model(&self) -> Model {
        let mut state = self.locked();
        state.connected_changed = false;
        state.changed.clear();
        Model {
            connected: state.connected,
            units: state.units.values().cloned().collect(),
        }
    }

    /// Wait for a report. Cancellation-safe: a report made meanwhile leaves a permit, and `take`
    /// is what removes it from waiting.
    pub async fn reported(&self) {
        // WAIT: reports-ready
        self.ready.notified().await;
    }

    /// Whether anything is waiting for the publisher (tests).
    #[cfg(test)]
    pub fn untaken(&self) -> bool {
        let state = self.locked();
        state.connected_changed || !state.changed.is_empty() || !state.errors.is_empty()
    }
}

#[cfg(test)]
mod tests;
