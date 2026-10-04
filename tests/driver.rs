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
        ["on L1.001", "heat L1.001", "temp L1.001 24", "fspeed L1.001 l", "ls2 L1.001"],
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
    assert!(h.desire_many("L1.002", json!({"target_temperature": 99, "power": false}), REQUEST));
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
    assert_eq!(h.coolmaster.commands_since(before), ["temp L1.002 99", "off L1.002", "ls2 L1.002"]);
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
    h.coolmaster.set_up(false);
    assert!(
        h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await,
        "the lost link was never noticed: {:?}",
        h.statuses()
    );
    assert!(h.desire("L1.001", "power", json!(true), REQUEST));
    h.settle(SETTLE).await;
    assert!(!h.coolmaster.commands().iter().any(|c| c == "on L1.001"), "a request reached a CoolMaster that was down");
    assert!(!h.coolmaster.unit("L1.001").unwrap().power, "the unit was turned on while the CoolMaster was down");
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
