//! Power-loss tracking across a restart of the bridge: the runtime's read-back of the previous
//! run's retained `State` (`ctx.read_back()`, spec §4.2) seeds what the driver knows of its units.
//! A unit retained `"powered": false` starts in an open power-loss episode; one retained
//! `"powered": true` counts as established. The retained values are the previous run's reports, not
//! evidence from the CoolMaster: they never produce a restored Event for a unit that was not
//! unpowered.
//!
//! Each test names its own unit (the log recorder is global to the binary).

mod support;

use std::time::Duration;

use mqtt_bridge_kit::topics::Topics;
use serde_json::json;
use support::coolmaster::units;
use support::*;

/// The `Event`s published, as JSON.
fn events(h: &Harness) -> Vec<serde_json::Value> {
    h.broker.received_on(&h.topics.event()).iter().filter_map(|r| serde_json::from_slice(&r.payload).ok()).collect()
}

fn restored(h: &Harness) -> Vec<serde_json::Value> {
    events(h).into_iter().filter(|e| e["kind"] == "unit_power_restored").collect()
}

/// What the previous run last retained for `unit`.
fn retain_state(broker: &FakeBroker, topics: &Topics, unit: &str, powered: bool) {
    let state = json!({
        "unit": unit, "power": true, "target_temperature": 24.0, "temperature": 21.5, "fan_speed": "Low",
        "operation_mode": "Heat", "failure_code": null, "filter_change": false, "demand": true, "powered": powered
    });
    broker.retain(&topics.state(unit), state.to_string(), &SendOptions::default().retained());
}

/// The previous run left a unit retained `"powered": false`: the process restarted in the middle of
/// its power loss. When the CoolMaster lists it, the driver says it is back (`unit_power_restored`
/// with `down_for_ms` from the restart and `"since_restart": true`, the INFO, `powered: true`) and
/// never a `unit_lost_power`: that loss was the previous run's.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_retained_unpowered_is_restored_when_it_is_listed_after_a_restart() {
    const UNIT: &str = "L1.071";
    let started = std::time::Instant::now();
    let h = Harness::start_restarted(
        "restart-restore",
        quick(Some(Duration::from_millis(200))),
        |cm| {
            cm.remove_unit("L1.002");
        },
        |broker, topics| {
            retain_state(broker, topics, "L1.001", true);
            retain_state(broker, topics, UNIT, false);
        },
    )
    .await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("L1.001").is_some()).await);
    // Several listings that omit it: nothing is said of it again.
    h.settle(Duration::from_millis(900)).await;
    assert!(events(&h).is_empty(), "a unit already without power at the restart was reported: {:?}", events(&h));
    h.coolmaster.add_unit(UNIT, units().remove("L1.002").unwrap());
    assert!(
        h.eventually(BOUND, |h| !restored(h).is_empty()).await,
        "no unit_power_restored Event for a unit retained unpowered: {:?}",
        events(&h)
    );
    let elapsed = started.elapsed().as_millis() as u64;
    h.settle(Duration::from_millis(600)).await;
    let restored = restored(&h);
    assert_eq!(restored.len(), 1, "not one unit_power_restored: {restored:?}");
    assert_eq!(restored[0]["target"], UNIT);
    assert_eq!(restored[0]["since_restart"], true, "the restore was not counted from the restart: {:?}", restored[0]);
    let down = restored[0]["down_for_ms"].as_u64().expect("down_for_ms");
    assert!((800..=elapsed).contains(&down), "down_for_ms {down} is not counted from the restart (it was {elapsed} ms ago)");
    assert!(h.state(UNIT).is_some_and(|s| s["powered"] == true), "the unit's power was not published back: {:?}", h.state(UNIT));
    assert!(events(&h).iter().all(|e| e["kind"] != "unit_lost_power"), "the previous run's loss was reported again: {:?}", events(&h));
    let infos = log::logged("unit_power_restored", UNIT);
    assert!(infos.len() == 1 && infos[0].level == tracing::Level::INFO, "the return was not one INFO: {infos:?}");
    assert!(log::logged("unit_lost_power", UNIT).is_empty(), "a WARN for a loss the previous run reported");
    assert!(h.stop().await);
}

/// A unit retained `"powered": true` counts as established: if the CoolMaster, listing fully and
/// usably, keeps omitting it, it lost its power while the bridge was down: `powered: false`, one
/// Event, one WARN.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_retained_powered_that_the_coolmaster_omits_is_reported_lost() {
    const UNIT: &str = "L1.072";
    let h = Harness::start_restarted(
        "restart-lost",
        quick(Some(Duration::from_millis(200))),
        |cm| {
            cm.remove_unit("L1.002");
        },
        |broker, topics| {
            retain_state(broker, topics, "L1.001", true);
            retain_state(broker, topics, UNIT, true);
        },
    )
    .await;
    assert!(
        h.eventually(BOUND, |h| h.state(UNIT).is_some_and(|s| s["powered"] == false)).await,
        "a unit that lost power while the bridge was down was not reported unpowered: {:?}",
        h.state(UNIT)
    );
    assert!(h.eventually(BOUND, |h| events(h).iter().any(|e| e["kind"] == "unit_lost_power")).await, "no unit_lost_power Event");
    h.settle(Duration::from_millis(600)).await;
    let kinds: Vec<_> = events(&h).iter().map(|e| (e["kind"].clone(), e["target"].clone())).collect();
    assert_eq!(kinds, [(json!("unit_lost_power"), json!(UNIT))], "not one Event, for the one unit");
    let warns = log::logged("unit_lost_power", UNIT);
    assert!(warns.len() == 1 && warns[0].level == tracing::Level::WARN, "the loss was not one WARN: {warns:?}");
    assert!(h.state("L1.001").is_some_and(|s| s["powered"] == true), "the unit still listed changed");
    assert!(h.stop().await);
}

/// Units retained powered and listed again say nothing: the retained values are the previous run's
/// reports, not evidence from the CoolMaster, so they never produce a restored Event.
#[tokio::test(flavor = "multi_thread")]
async fn units_retained_powered_and_listed_say_nothing_after_a_restart() {
    let h = Harness::start_restarted(
        "restart-quiet",
        quick(Some(Duration::from_millis(200))),
        // The CoolMaster is down until the bridge has subscribed to requests, which it does only
        // once the read-back is complete: the first listing meets a seeded driver.
        |cm| cm.set_up(false),
        |broker, topics| {
            retain_state(broker, topics, "L1.001", true);
            retain_state(broker, topics, "L1.002", true);
        },
    )
    .await;
    h.coolmaster.set_up(true);
    assert!(h.connected().await);
    h.settle(Duration::from_millis(1000)).await;
    assert!(events(&h).is_empty(), "units listed after a restart produced Events: {:?}", events(&h));
    assert!(h.stop().await);
}

/// The read-back is not complete when the CoolMaster is first connected (the broker is slow to
/// acknowledge the bridge's subscription): nothing is seeded then, so a unit the listings omit is
/// not lost yet; once the read-back is complete, a later poll seeds it, once, and the unit retained
/// powered, still omitted, is lost.
#[tokio::test(flavor = "multi_thread")]
async fn an_incomplete_read_back_is_seeded_at_the_poll_that_finds_it_complete() {
    const UNIT: &str = "L1.073";
    let h = Harness::start_restarted_unwaited(
        "restart-late",
        quick(Some(Duration::from_millis(200))),
        |cm| {
            cm.remove_unit("L1.002");
        },
        |broker, topics| {
            retain_state(broker, topics, "L1.001", true);
            retain_state(broker, topics, UNIT, true);
            broker.hold_subacks();
        },
    )
    .await;
    // The session publishes nothing while it waits for the read-back (5 s at most), but the driver
    // polls: let it list the CoolMaster several times, the read-back pending at each.
    let listings = |h: &Harness| h.coolmaster.commands().iter().filter(|c| c.starts_with("ls2")).count();
    assert!(h.eventually(BOUND, |h| listings(h) >= 5).await, "the driver did not poll while the read-back was pending");
    h.broker.release_subacks();
    assert!(
        h.eventually(BOUND, |h| h.state(UNIT).is_some_and(|s| s["powered"] == false)).await,
        "the read-back, complete at a later poll, was never seeded: {:?}",
        h.state(UNIT)
    );
    h.settle(Duration::from_millis(800)).await;
    let lost = events(&h).into_iter().filter(|e| e["kind"] == "unit_lost_power" && e["target"] == UNIT).count();
    assert_eq!(lost, 1, "not one unit_lost_power Event: {:?}", events(&h));
    assert!(h.stop().await);
}
