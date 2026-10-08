//! The driver against the stand-in CoolMaster, through the real bridge and `FakeBroker` (see
//! `support`): what the Store asks on the Bridge Kit grammar becomes CoolMaster commands, and
//! what the CoolMaster says comes back as `State` or `Error`.

mod support;

use std::time::Duration;

use serde_json::json;
use support::*;

/// A target-level `Desired` (the Store's `desire_many`) becomes the CoolMaster's commands, once,
/// in the order it needs — power on first, then the mode, the setpoint and the fan speed — and the
/// unit's `State` follows (the driver reads it back). The property copies the Store publishes
/// after it are not applied again.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_level_desired_becomes_coolmaster_commands_in_order_and_state_follows() {
    let h = Harness::start("target").await;
    assert!(h.connected().await, "the bridge never connected to the CoolMaster");
    assert_eq!(h.state("L1.001").unwrap()["power"], false);
    let before = h.coolmaster.commands().len();
    let values = json!({"fan_speed": "Low", "target_temperature": 24, "power": true, "operation_mode": "Heat"});
    assert!(h.desire_many("L1.001", values, REQUEST));
    let followed = h
        .eventually(BOUND, |h| {
            h.state("L1.001").is_some_and(|s| {
                s["power"] == true && s["operation_mode"] == "Heat" && s["target_temperature"] == 24.0 && s["fan_speed"] == "Low"
            })
        })
        .await;
    assert!(followed, "the unit's State did not follow the request: {:?}; the commands: {:?}", h.state("L1.001"), h.coolmaster.commands());
    h.settle(SETTLE).await;
    assert_eq!(
        h.coolmaster.commands_since(before),
        ["on L1.001", "heat L1.001", "temp L1.001 24.0", "fspeed L1.001 l", "ls2 L1.001"],
        "the request did not reach the CoolMaster once, in its order, and then a read-back"
    );
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.stop().await);
}

/// A property write (`Desired/{instance}/{unit}/{property}`) becomes its one command, and the
/// unit's `State` follows.
#[tokio::test(flavor = "multi_thread")]
async fn a_property_write_becomes_its_command() {
    let h = Harness::start("property").await;
    assert!(h.connected().await);
    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.002", "fan_speed", json!("Top"), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.002").is_some_and(|s| s["fan_speed"] == "Top")).await,
        "the unit's State did not follow: {:?}; the commands: {:?}",
        h.state("L1.002"),
        h.coolmaster.commands()
    );
    h.settle(SETTLE).await;
    assert_eq!(h.coolmaster.commands_since(before), ["fspeed L1.002 t", "ls2 L1.002"]);
    assert_eq!(h.coolmaster.unit("L1.002").unwrap().fan, "Top");
    assert!(h.stop().await);
}

/// `ResetFilter` on `Command` resets the unit's filter flag (`filt <unit>`), and its `State`'s
/// `filter_change` follows.
#[tokio::test(flavor = "multi_thread")]
async fn reset_filter_resets_the_units_filter() {
    let h = Harness::start("filter").await;
    assert!(h.connected().await);
    assert_eq!(h.state("L1.002").unwrap()["filter_change"], true);
    let before = h.coolmaster.commands().len();
    assert!(h.command(json!({"target": "L1.002", "command": "ResetFilter"})));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.002").is_some_and(|s| s["filter_change"] == false)).await,
        "the filter flag was not reset: {:?}; the commands: {:?}",
        h.state("L1.002"),
        h.coolmaster.commands()
    );
    assert_eq!(h.coolmaster.commands_since(before), ["filt L1.002", "ls2 L1.002"]);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.stop().await);
}

/// A value the CoolMaster refuses (a setpoint its unit cannot take) is an `Error` naming the unit
/// and that property only, `"reason": "rejected"`: the rest of the request is still sent, and
/// stands as applied — turning the unit off is not lost to the refused setpoint, and is confirmed
/// through `State`, never reported failed (`OpError::Partial`). The link stays up. A value outside
/// its property's vocabulary is refused the same way, without being sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_value_is_an_error_and_the_rest_still_applies() {
    let h = Harness::start("rejected").await;
    assert!(h.connected().await);
    let before = h.coolmaster.commands().len();
    // 40 °C: in the bridge's range, past the (stand-in) unit's 10–35.
    assert!(h.desire_many("L1.002", json!({"target_temperature": 40, "power": false}), REQUEST));
    assert!(
        h.eventually(BOUND, |h| !h.errors_for("L1.002", "target_temperature").is_empty()).await,
        "the refused setpoint got no Error: {:?}",
        h.errors()
    );
    let error = &h.errors_for("L1.002", "target_temperature")[0];
    assert_eq!(error["reason"], "rejected", "{error}");
    // The write's Errors go out together: by now one for the power-off would have too.
    assert!(h.errors_for("L1.002", "power").is_empty(), "the power-off, which the CoolMaster took, was reported failed: {:?}", h.errors());
    assert_eq!(
        error["error"], "the CoolMaster answered `ERROR: 4`",
        "the Error does not say, alone, what the CoolMaster answered the setpoint: {error}"
    );
    assert!(
        h.eventually(BOUND, |h| h.state("L1.002").is_some_and(|s| s["power"] == false)).await,
        "turning the unit off was lost to the refused setpoint: {:?}; the commands: {:?}",
        h.state("L1.002"),
        h.coolmaster.commands()
    );
    assert_eq!(h.coolmaster.commands_since(before), ["temp L1.002 40.0", "off L1.002", "ls2 L1.002"]);
    assert_eq!(h.coolmaster.unit("L1.002").unwrap().setpoint, 24.0);

    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.001", "fan_speed", json!("Turbo"), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.errors_for("L1.001", "fan_speed").first().is_some_and(|e| e["reason"] == "rejected")).await,
        "a value outside its vocabulary got no rejection: {:?}",
        h.errors()
    );
    h.settle(SETTLE).await;
    assert!(h.coolmaster.commands_since(before).is_empty(), "a value outside its vocabulary was sent: {:?}", h.coolmaster.commands());
    assert_eq!(h.coolmaster.served(), 1, "a rejection was taken for a lost link: the bridge reconnected");
    assert_eq!(h.status().as_deref(), Some("connected"));
    assert!(h.stop().await);
}

/// A reply the bridge cannot use (not text) is an `Error`, `"reason": "unusable"`, and the request
/// is passed over: never a reason to reconnect (no-hang 3b: reconnecting for it was a hot loop).
/// The next request goes through on the same connection.
#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_reply_is_passed_over_on_the_same_connection() {
    let h = Harness::start("unusable").await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("on", Answer::Unusable);
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.errors_for("L1.001", "power").first().is_some_and(|e| e["reason"] == "unusable")).await,
        "the unusable reply got no Error: {:?}",
        h.errors()
    );
    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.001", "operation_mode", json!("Dry"), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["operation_mode"] == "Dry")).await,
        "the request after the unusable reply did not go through: {:?}",
        h.coolmaster.commands()
    );
    assert_eq!(h.coolmaster.commands_since(before), ["dry L1.001", "ls2 L1.001"]);
    assert_eq!(h.coolmaster.served(), 1, "an unusable reply was taken for a lost link: the bridge reconnected");
    assert!(h.stop().await);
}

/// The link lost, a request made meanwhile is held — nothing reaches the CoolMaster while it is
/// down — and applied when it is back (the runtime's mailbox and catch-up, through this driver).
#[tokio::test(flavor = "multi_thread")]
async fn a_request_held_through_a_lost_link_is_applied_on_recovery() {
    let h = Harness::start_with("held", quick(Some(Duration::from_millis(200)))).await;
    assert!(h.connected().await);
    assert_eq!(h.state("L1.001").unwrap()["power"], false);
    let attempts_before = h.coolmaster.accepted();
    let outage = std::time::Instant::now();
    h.coolmaster.set_up(false);
    assert!(
        h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await,
        "the lost link was never noticed: {:?}",
        h.statuses()
    );
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    h.settle(Duration::from_millis(1500)).await;
    assert!(!h.coolmaster.commands().iter().any(|c| c == "on L1.001"), "a request reached a CoolMaster that was down");
    assert!(!h.coolmaster.unit("L1.001").unwrap().power, "the unit was turned on while the CoolMaster was down");
    // Every attempt counts, refused or not: at most one per retry interval (300 ms), plus one.
    let attempts = h.coolmaster.accepted() - attempts_before;
    let paced = (outage.elapsed().as_millis() / 300) as usize + 1;
    assert!(attempts >= 2 && attempts <= paced, "{attempts} connect attempts in an outage of {:?}: not paced at 300 ms", outage.elapsed());
    h.coolmaster.set_up(true);
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["power"] == true)).await,
        "the held request was not applied on recovery: {:?}; the commands: {:?}",
        h.state("L1.001"),
        h.coolmaster.commands()
    );
    assert_eq!(h.coolmaster.commands().iter().filter(|c| *c == "on L1.001").count(), 1);
    assert_eq!(h.status().as_deref(), Some("connected"));
    assert!(h.coolmaster.served() <= 3, "the bridge reconnected {} times for one outage", h.coolmaster.served());
    assert!(h.stop().await);
}

/// A reply far past any CoolMaster's (no prompt within 64 KiB) has lost its framing: the link is
/// dropped as soon as the limit is read — not read on, buffered, until the operation's deadline —
/// and the request is applied on the next connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_past_its_byte_limit_is_a_lost_link_at_once() {
    let mut timing = quick(None);
    timing.operation = Some(Duration::from_secs(3));
    let h = Harness::start_with("flood", timing).await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("on", Answer::Flood);
    let asked = std::time::Instant::now();
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    assert!(h.eventually(BOUND, |h| h.coolmaster.served() == 2).await, "the flooded link was never dropped");
    let took = asked.elapsed();
    assert!(took < Duration::from_millis(1500), "an over-long reply was read on until the operation's deadline: {took:?}");
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["power"] == true)).await,
        "the request was not applied after the reconnect: {:?}",
        h.coolmaster.commands()
    );
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.stop().await);
}

/// A CoolMaster that closes the connection instead of answering a command has lost the link —
/// not given an unusable answer: the request is held, and applied on the next connection, with
/// no `Error`.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_closed_mid_command_is_a_lost_link_and_the_request_is_applied_on_reconnect() {
    let h = Harness::start("closed").await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("on", Answer::Close);
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["power"] == true)).await,
        "the request was not applied after the reconnect: {:?}; the commands: {:?}; the errors: {:?}",
        h.state("L1.001"),
        h.coolmaster.commands(),
        h.errors()
    );
    assert_eq!(h.coolmaster.served(), 2, "the closed connection was not reconnected once");
    assert_eq!(h.coolmaster.commands().iter().filter(|c| *c == "on L1.001").count(), 2);
    assert!(h.errors().is_empty(), "a lost link was reported as the request's failure: {:?}", h.errors());
    assert!(h.stop().await);
}

/// One `ls2` line the bridge cannot use — a unit the CoolMaster lists garbled — is passed over: the
/// other units' `State` is published from the first listing on, and a poll still carries their
/// changes. v1 failed the whole listing on one bad line, so no unit's `State` changed at all. The
/// bad line is no reason to reconnect either.
#[tokio::test(flavor = "multi_thread")]
async fn one_bad_ls2_line_does_not_fail_the_listing() {
    let h = Harness::start_prepared("badline", quick(Some(Duration::from_millis(200))), |coolmaster| {
        coolmaster.set_line("L1.002", Some("L1.002 ON 24.0C ??? Low Heat OK # 1"));
    })
    .await;
    assert!(
        h.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("L1.001").is_some()).await,
        "L1.001's State was never published beside a bad line for L1.002: {:?}; the commands: {:?}",
        h.state("L1.001"),
        h.coolmaster.commands()
    );
    h.coolmaster.change("L1.001", |unit| unit.power = true);
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["power"] == true)).await,
        "a poll beside a bad line did not carry L1.001's change: {:?}",
        h.state("L1.001")
    );
    assert!(h.state("L1.002").is_none(), "a line that cannot be used was published: {:?}", h.state("L1.002"));
    assert_eq!(h.coolmaster.served(), 1, "a bad line was taken for a lost link: the bridge reconnected");
    assert!(h.stop().await);
}

/// A unit that goes into failure keeps its `State` current: its `failure_code` is the
/// CoolMaster's own code (`"U4"`), and a change at the unit meanwhile still shows. A code read as a
/// number only (v1) made the line unusable, and the unit's State froze while it was in failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_in_failure_keeps_its_state_current() {
    let h = Harness::start_with("failure", quick(Some(Duration::from_millis(200)))).await;
    assert!(h.connected().await);
    assert!(h.state("L1.001").unwrap()["failure_code"].is_null());
    h.coolmaster.change("L1.001", |unit| unit.failure = Some("U4"));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["failure_code"] == "U4")).await,
        "a unit in failure with an alphanumeric code was not published: {:?}",
        h.state("L1.001")
    );
    h.coolmaster.change("L1.001", |unit| unit.room = 27.5);
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["temperature"] == 27.5 && s["failure_code"] == "U4")).await,
        "the State of a unit in failure froze: {:?}",
        h.state("L1.001")
    );
    h.coolmaster.change("L1.001", |unit| unit.failure = None);
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["failure_code"].is_null())).await,
        "the failure did not clear: {:?}",
        h.state("L1.001")
    );
    assert!(h.stop().await);
}

/// A unit the CoolMaster no longer lists (unbound at the controller) has its retained `State`
/// retracted — an empty retained payload — so the broker keeps no ghost of it; listed again, it is
/// published again. The other unit is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_gone_from_the_listing_has_its_state_retracted() {
    let h = Harness::start_with("gone", quick(Some(Duration::from_millis(200)))).await;
    assert!(h.connected().await);
    let topic = h.topics.state("L1.002");
    assert!(h.broker.retained_on(&topic).is_some_and(|p| !p.is_empty()));
    let unit = h.coolmaster.remove_unit("L1.002").expect("the stand-in's L1.002");
    let retracted = h.eventually(BOUND, |h| h.broker.received_on(&topic).last().is_some_and(|r| r.payload.is_empty() && r.retain)).await;
    assert!(
        retracted && h.broker.retained_on(&topic).is_none(),
        "the State of a unit gone from the listing was not retracted: {:?}; retained: {:?}",
        h.broker.received_on(&topic).last(),
        h.broker.retained_on(&topic)
    );
    assert!(h.broker.retained_on(&h.topics.state("L1.001")).is_some_and(|p| !p.is_empty()), "the unit still listed lost its State");
    h.coolmaster.add_unit("L1.002", unit);
    assert!(
        h.eventually(BOUND, |h| h.broker.retained_on(&topic).is_some_and(|p| !p.is_empty())).await,
        "a unit listed again was not published again: {:?}",
        h.broker.received_on(&topic).last()
    );
    assert!(h.stop().await);
}

/// A line whose unit address itself is garbled says nothing about which unit it was: no unit is
/// retracted for it, and the garbage never becomes a topic of its own — neither while it lasts nor
/// once the line is clean again.
#[tokio::test(flavor = "multi_thread")]
async fn a_garbled_unit_address_retracts_nothing() {
    let h = Harness::start_with("garble", quick(Some(Duration::from_millis(200)))).await;
    assert!(h.connected().await);
    let topic = h.topics.state("L1.002");
    h.coolmaster.set_line("L1.002", Some("L1.0\u{1}2 ON 24.0C ??? Low Heat OK # 1"));
    h.settle(Duration::from_millis(1000)).await;
    assert!(
        h.broker.retained_on(&topic).is_some_and(|p| !p.is_empty()),
        "a unit was retracted because its address was listed garbled: {:?}",
        h.broker.received_on(&topic).last()
    );
    h.coolmaster.set_line("L1.002", None);
    h.settle(Duration::from_millis(1000)).await;
    let ghost = h.topics.state("L1.0\u{1}2");
    assert!(h.broker.received_on(&ghost).is_empty(), "a garbled address was published as a unit: {:?}", h.broker.received_on(&ghost));
    assert!(h.broker.retained_on(&topic).is_some_and(|p| !p.is_empty()));
    assert!(h.stop().await);
}

/// A listing that says nothing usable about the units — empty (a CoolMaster rebooting, or still
/// scanning its line), or with no line that can be read — retracts no unit, however long it lasts.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_or_unusable_listing_retracts_nothing() {
    let h = Harness::start_with("emptylist", quick(Some(Duration::from_millis(200)))).await;
    assert!(h.connected().await);
    let retained = |h: &Harness| ["L1.001", "L1.002"].map(|u| h.broker.retained_on(&h.topics.state(u)).is_some_and(|p| !p.is_empty()));
    let a = h.coolmaster.remove_unit("L1.001").unwrap();
    let b = h.coolmaster.remove_unit("L1.002").unwrap();
    h.settle(Duration::from_millis(1000)).await;
    assert_eq!(retained(&h), [true, true], "an empty listing retracted units");
    h.coolmaster.add_unit("L1.001", a);
    h.coolmaster.add_unit("L1.002", b);
    h.coolmaster.set_line("L1.001", Some("garbage"));
    h.coolmaster.set_line("L1.002", Some("more garbage"));
    h.settle(Duration::from_millis(1000)).await;
    assert_eq!(retained(&h), [true, true], "a listing with no usable line retracted units");
    assert!(h.stop().await);
}

/// The read-back after a confirmed command has a bound of its own: a CoolMaster that stalls on
/// it (mid-exchange, `Gated`) never turns the confirmed `ResetFilter` into a failure. The stalled
/// link is dropped and reported lost, and the bridge reconnects.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_read_back_leaves_a_confirmed_command_confirmed() {
    let h = Harness::start("rbstall").await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("ls2 L1.002", Answer::Gated);
    assert!(h.command(json!({"target": "L1.002", "command": "ResetFilter"})));
    assert!(
        h.eventually(BOUND, |h| h.coolmaster.served() == 2 && h.status().as_deref() == Some("connected")).await,
        "the stalled link was not dropped and reconnected: served {}, statuses {:?}",
        h.coolmaster.served(),
        h.statuses()
    );
    assert!(!h.coolmaster.unit("L1.002").unwrap().filter, "the filter was not reset");
    assert!(h.errors().is_empty(), "a confirmed command was reported failed: {:?}", h.errors());
    assert!(h.stop().await);
}

/// The same for a partly refused write: a stalled read-back keeps the write's own outcome — the
/// refused setpoint's `Error`, the power-off applied — and the write is never applied again.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_read_back_keeps_a_partial_writes_outcome() {
    let h = Harness::start("rbpartial").await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("ls2 L1.002", Answer::Gated);
    assert!(h.desire_many("L1.002", json!({"target_temperature": 40, "power": false}), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.errors_for("L1.002", "target_temperature").first().is_some_and(|e| e["reason"] == "rejected")).await,
        "the refused setpoint got no rejection: {:?}",
        h.errors()
    );
    assert!(h.eventually(BOUND, |h| h.coolmaster.served() == 2 && h.status().as_deref() == Some("connected")).await);
    h.settle(SETTLE).await;
    let temps = h.coolmaster.commands().iter().filter(|c| c.starts_with("temp ")).count();
    assert_eq!(temps, 1, "the write was applied again after its read-back stalled: {:?}", h.coolmaster.commands());
    assert!(h.errors_for("L1.002", "power").is_empty(), "the applied power-off was reported failed: {:?}", h.errors());
    assert!(!h.coolmaster.unit("L1.002").unwrap().power);
    assert!(h.stop().await);
}

/// A read-back whose link closes, with polling off: the command stays confirmed, and the link is
/// reported lost — the bridge never stays "connected" on a dead socket, waiting for a request to
/// find out.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_back_that_finds_the_link_dead_reports_it_lost() {
    let h = Harness::start("rbclosed").await;
    assert!(h.connected().await);
    let statuses = h.statuses().len();
    h.coolmaster.answer_once("ls2 L1.002", Answer::Close);
    assert!(h.command(json!({"target": "L1.002", "command": "ResetFilter"})));
    assert!(
        h.eventually(BOUND, |h| h.statuses()[statuses..].iter().any(|s| s == "unreachable")).await,
        "the bridge still says connected on a dead link: {:?}",
        h.statuses()
    );
    assert!(h.eventually(BOUND, |h| h.coolmaster.served() == 2 && h.status().as_deref() == Some("connected")).await);
    assert!(h.errors().is_empty(), "a confirmed command was reported failed: {:?}", h.errors());
    assert!(h.stop().await);
}

/// A unit the CoolMaster reads in °F: its State is in °C, as every unit's, and a setpoint the
/// Store asks in °C goes to it in °F (23 °C as `temp 73.4`) — never as 23 °F, which the unit
/// would refuse or take for a near-freezing setpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_in_fahrenheit_gets_its_setpoint_in_fahrenheit() {
    let h = Harness::start_prepared("fahrenheit", quick(None), |coolmaster| {
        coolmaster.change("L1.001", |unit| {
            unit.fahrenheit = true;
            unit.setpoint = 72.0;
            unit.room = 77.0;
        });
    })
    .await;
    assert!(h.connected().await);
    assert_eq!(h.state("L1.001").unwrap()["temperature"], 25.0, "a °F reading was not shown in °C");
    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.001", "target_temperature", json!(23), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.coolmaster.commands_since(before).iter().any(|c| c.starts_with("temp "))).await,
        "no setpoint was sent: {:?}",
        h.coolmaster.commands()
    );
    let sent: Vec<String> = h.coolmaster.commands_since(before).into_iter().filter(|c| c.starts_with("temp ")).collect();
    assert_eq!(sent, ["temp L1.001 73.4"], "a °C setpoint was not sent to a °F unit in °F");
    assert!(
        h.eventually(BOUND, |h| h
            .state("L1.001")
            .is_some_and(|s| s["target_temperature"].as_f64().is_some_and(|t| (t - 23.0).abs() < 0.01)))
            .await,
        "the unit's State did not follow in °C: {:?}",
        h.state("L1.001")
    );
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.stop().await);
}

/// The request as the Store's S1 driver sends it today: one target-level `Desired`, not retained,
/// with no message expiry and no `conflict`, and no property copies after it. It is applied.
#[tokio::test(flavor = "multi_thread")]
async fn the_stores_own_request_shape_is_applied() {
    let h = Harness::start("storeshape").await;
    assert!(h.connected().await);
    let before = h.coolmaster.commands().len();
    let topic = h.topics.desired_target("L1.001");
    assert!(h.broker.send_with(&topic, json!({"power": true, "operation_mode": "Heat"}).to_string(), &SendOptions::default()));
    assert!(
        h.eventually(BOUND, |h| h.state("L1.001").is_some_and(|s| s["power"] == true && s["operation_mode"] == "Heat")).await,
        "the Store's request was not applied: {:?}; the errors: {:?}",
        h.coolmaster.commands(),
        h.errors()
    );
    assert_eq!(h.coolmaster.commands_since(before), ["on L1.001", "heat L1.001", "ls2 L1.001"]);
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(h.stop().await);
}

/// A command the CoolMaster never answers (it stalls mid-exchange, `Gated`) is a lost link at the
/// operation's deadline: the request is held and applied on the next connection, once, and no
/// stale reply of the stalled connection is taken for a later command's.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_stalled_mid_exchange_is_a_lost_link_and_applied_after_reconnect() {
    let h = Harness::start("stalled").await;
    assert!(h.connected().await);
    h.coolmaster.answer_once("on", Answer::Gated);
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.coolmaster.served() == 2 && h.state("L1.001").is_some_and(|s| s["power"] == true)).await,
        "the stalled request was not applied after the reconnect: served {}, {:?}",
        h.coolmaster.served(),
        h.coolmaster.commands()
    );
    h.coolmaster.open_gate();
    h.settle(SETTLE).await;
    assert!(h.statuses().contains(&"unreachable".to_owned()), "the stalled link was never taken for lost: {:?}", h.statuses());
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert_eq!(h.status().as_deref(), Some("connected"));
    assert!(h.stop().await);
}

/// `ResetFilter` names its unit: one with no `target` is refused (`filt` with no unit would reset
/// every unit's filter), and nothing is sent.
#[tokio::test(flavor = "multi_thread")]
async fn reset_filter_without_a_unit_is_refused_unsent() {
    let h = Harness::start("filternone").await;
    assert!(h.connected().await);
    let before = h.coolmaster.commands().len();
    assert!(h.command(json!({"command": "ResetFilter"})));
    assert!(
        h.eventually(BOUND, |h| h.errors().iter().any(|e| e["command"] == "ResetFilter" && e["reason"] == "rejected")).await,
        "a ResetFilter with no unit was not refused: {:?}",
        h.errors()
    );
    h.settle(SETTLE).await;
    assert!(h.coolmaster.commands_since(before).is_empty(), "a ResetFilter with no unit was sent: {:?}", h.coolmaster.commands());
    assert!(h.stop().await);
}

/// A unit whose scale is not known yet (no usable line of it listed): before its setpoint is
/// sent, the unit is read to learn it. A read that cannot tell refuses the setpoint — rejected,
/// "scale unknown", nothing sent — and never guesses °C; once its line can be read, the setpoint
/// goes in the unit's own scale.
#[tokio::test(flavor = "multi_thread")]
async fn a_setpoint_for_a_unit_of_unknown_scale_learns_it_first_and_never_guesses() {
    let h = Harness::start_prepared("unknownscale", quick(None), |coolmaster| {
        coolmaster.change("L1.001", |unit| {
            unit.fahrenheit = true;
            unit.setpoint = 72.0;
        });
        coolmaster.set_line("L1.001", Some("L1.001 ON 72.0F ??? High Cool OK - 0"));
    })
    .await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("L1.002").is_some()).await);
    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.001", "target_temperature", json!(23), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.errors_for("L1.001", "target_temperature").first().is_some_and(|e| e["reason"] == "rejected")).await,
        "a setpoint for a unit of unknown scale was not refused: {:?}; the commands: {:?}",
        h.errors(),
        h.coolmaster.commands()
    );
    let sent = h.coolmaster.commands_since(before);
    assert!(!sent.iter().any(|c| c.starts_with("temp ")), "a setpoint was sent to a unit of unknown scale: {sent:?}");
    assert!(sent.iter().any(|c| c == "ls2 L1.001"), "the unit was not read to learn its scale: {sent:?}");
    let error = &h.errors_for("L1.001", "target_temperature")[0];
    assert!(error["error"].as_str().is_some_and(|e| e.contains("scale")), "{error}");
    h.coolmaster.set_line("L1.001", None);
    let before = h.coolmaster.commands().len();
    assert!(h.desire("L1.001", "target_temperature", json!(23), REQUEST));
    assert!(
        h.eventually(BOUND, |h| h.coolmaster.commands_since(before).iter().any(|c| c.starts_with("temp "))).await,
        "the setpoint was not sent once the scale could be learnt: {:?}",
        h.coolmaster.commands()
    );
    let temps: Vec<String> = h.coolmaster.commands_since(before).into_iter().filter(|c| c.starts_with("temp ")).collect();
    assert_eq!(temps, ["temp L1.001 73.4"], "the learnt scale was not used");
    assert!(h.stop().await);
}
