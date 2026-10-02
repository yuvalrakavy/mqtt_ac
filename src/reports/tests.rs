//! The worker's reports on their own: the retained model, and the error queue's cap.

use super::{Changes, Model, Reports, ERROR_CAP};
use crate::ac_unit::{FanSpeed, OperationMode, UnitState};

fn state(unit: &str, temperature: f32) -> UnitState {
    UnitState {
        unit: unit.to_owned(),
        power: true,
        target_temperature: 22.0,
        temperature,
        fan_speed: FanSpeed::High,
        operation_mode: OperationMode::Cool,
        failure_code: None,
        filter_change: false,
        demand: false,
    }
}

/// Between two takes a unit's states coalesce to its latest, a state that did not change is not
/// taken again, and the flag is taken only when it changed.
#[test]
fn observed_state_coalesces_to_the_latest_and_an_unchanged_one_is_not_taken_again() {
    let reports = Reports::new();
    reports.coolmaster_connected(false);
    reports.observed([state("L1.001", 20.0), state("L1.002", 20.0)]);
    reports.observed([state("L1.001", 21.0)]);
    assert_eq!(
        reports.take(),
        Changes {
            connected: Some(false),
            units: vec![state("L1.001", 21.0), state("L1.002", 20.0)],
            errors: vec![],
        },
        "the changes should be the latest state per unit, in the order the units first changed"
    );
    reports.coolmaster_connected(false);
    reports.observed([state("L1.001", 21.0), state("L1.002", 20.0)]);
    assert!(
        !reports.untaken(),
        "what did not change is waiting to be taken again"
    );
    assert_eq!(
        reports.take(),
        Changes::default(),
        "what did not change was taken again"
    );
}

/// After a CONNACK the publisher publishes everything known again, and what had changed goes out
/// with it.
#[test]
fn the_model_is_everything_known_and_takes_what_changed_with_it() {
    let reports = Reports::new();
    reports.coolmaster_connected(true);
    reports.observed([state("L1.002", 20.0), state("L1.001", 20.0)]);
    assert!(reports.take().units.len() == 2);
    reports.observed([state("L1.001", 23.0)]);
    reports.error("an error".to_owned());
    assert_eq!(
        reports.model(),
        Model {
            connected: Some(true),
            units: vec![state("L1.001", 23.0), state("L1.002", 20.0)],
        }
    );
    assert_eq!(
        reports.take(),
        Changes {
            connected: None,
            units: vec![],
            errors: vec!["an error".to_owned()],
        },
        "after the model, only the errors should be left to take"
    );
}

/// The error queue never waits and never grows past its cap: the oldest are displaced, said once
/// per episode at WARN, and the episode's end (the publisher took the queue) once at INFO with how
/// many were lost. Nothing is logged under the lock.
#[test]
fn errors_past_the_cap_displace_the_oldest_said_once_per_episode() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let reports = Reports::new();
    // Each event as it is emitted, and whether the reports' lock was held then.
    let (records, under_lock) = (Rc::new(RefCell::new(Vec::new())), Rc::new(Cell::new(0)));
    let _watch = {
        let (reports, records, under_lock) = (reports.clone(), records.clone(), under_lock.clone());
        crate::test_support::watch(move |record| {
            if reports.state.try_lock().is_err() {
                under_lock.set(under_lock.get() + 1);
            }
            records.borrow_mut().push(record);
        })
    };
    for i in 0..ERROR_CAP + 10 {
        reports.error(format!("error {i}"));
    }
    let taken = reports.take();
    assert_eq!(
        taken.errors.len(),
        ERROR_CAP,
        "the error queue grew past its cap"
    );
    assert_eq!(
        taken.errors.first().map(String::as_str),
        Some("error 10"),
        "the oldest errors were not the ones displaced"
    );
    let records = records.borrow();
    let of_kind = |kind: &str| {
        records
            .iter()
            .filter(|r| r.kind == kind)
            .collect::<Vec<_>>()
    };
    let (dropped, ended) = (
        of_kind("error_report_dropped"),
        of_kind("error_report_drop_ended"),
    );
    assert!(
        dropped.len() == 1
            && dropped[0].level == tracing::Level::WARN
            && ended.len() == 1
            && ended[0].level == tracing::Level::INFO
            && ended[0].field("dropped") == Some("10"),
        "the displacement was not one WARN and one INFO with how many were lost; the log: {records:#?}"
    );
    assert_eq!(
        under_lock.get(),
        0,
        "the reports logged while holding their lock"
    );
}
