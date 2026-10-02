//! The mailbox on its own: the device-down policy's three kinds of command.

use std::time::Duration;

use super::{Mailbox, Posted, Refusal, MOMENTARY_CAP};
use crate::ac_unit::OperationMode;
use crate::messages::ToCoolmasterMessage::{self, *};
use crate::test_support::Capture;

const U1: &str = "L1.001";
const U2: &str = "L1.002";

fn power(unit: &str, on: bool) -> ToCoolmasterMessage {
    SetUnitPower(unit.to_owned(), on)
}

fn temperature(unit: &str, t: f32) -> ToCoolmasterMessage {
    SetTargetTemperature(unit.to_owned(), t)
}

/// A mailbox whose worker has found the Coolmaster down.
fn down() -> std::sync::Arc<Mailbox> {
    let mailbox = Mailbox::new();
    assert!(mailbox.disconnected().is_empty());
    mailbox
}

/// A mailbox whose worker has connected to the Coolmaster.
fn up() -> std::sync::Arc<Mailbox> {
    let mailbox = Mailbox::new();
    assert_eq!(mailbox.connected(), 0);
    mailbox
}

/// Until the worker's first connect, the Coolmaster is down (C10): a momentary command is refused
/// and a state is held; the first connect hands the held state to the catch-up.
#[test]
fn a_new_mailbox_is_down_until_the_first_connect() {
    let mailbox = Mailbox::new();
    assert_eq!(
        mailbox.post(ResetFilter(U1.to_owned())),
        Posted::Refused(Refusal::CoolmasterDown),
        "a momentary command was taken before the first connect"
    );
    assert_eq!(mailbox.post(PublishUnitsState), Posted::Dropped);
    assert_eq!(mailbox.post(power(U1, true)), Posted::Queued);
    assert_eq!(mailbox.connected(), 1);
    assert_eq!(held(&mailbox), [power(U1, true)]);
}

/// Every held state, taken as the worker's catch-up takes them.
fn held(mailbox: &Mailbox) -> Vec<ToCoolmasterMessage> {
    std::iter::from_fn(|| mailbox.take_held()).collect()
}

#[test]
fn state_set_while_down_keeps_the_latest_value_per_unit_and_property_in_the_order_last_set() {
    let mailbox = down();
    for command in [
        power(U1, true),
        temperature(U1, 20.0),
        power(U2, true),
        SetUnitMode(U2.to_owned(), OperationMode::Heat),
        power(U1, false),
        temperature(U1, 22.0),
    ] {
        assert_eq!(mailbox.post(command), Posted::Queued);
    }
    assert_eq!(mailbox.connected(), 0);
    assert_eq!(
        held(&mailbox),
        [
            power(U2, true),
            SetUnitMode(U2.to_owned(), OperationMode::Heat),
            power(U1, false),
            temperature(U1, 22.0),
        ],
        "the Coolmaster should get the latest value per unit and property, in the order last set"
    );
    assert!(
        mailbox.waiting().is_empty(),
        "the held state was taken and also left waiting"
    );
}

/// The held state stays in the mailbox until the worker takes it, so a value posted once the
/// Coolmaster is back replaces its held one, and goes behind the rest of the held state (C3).
#[test]
fn a_value_posted_after_the_return_replaces_its_held_one() {
    let mailbox = down();
    mailbox.post(power(U1, true));
    mailbox.post(temperature(U1, 20.0));
    mailbox.post(power(U2, true));
    mailbox.connected();
    assert_eq!(mailbox.take_held(), Some(power(U1, true)));
    mailbox.post(temperature(U1, 22.0));
    assert_eq!(
        held(&mailbox),
        [power(U2, true)],
        "a held value replaced since the Coolmaster came back was still taken as held"
    );
    assert_eq!(mailbox.waiting(), [temperature(U1, 22.0)]);
}

#[test]
fn a_momentary_command_is_refused_while_the_coolmaster_is_down() {
    let mailbox = down();
    assert_eq!(
        mailbox.post(ResetFilter(U1.to_owned())),
        Posted::Refused(Refusal::CoolmasterDown)
    );
    assert!(
        mailbox.waiting().is_empty(),
        "a refused momentary command was kept"
    );
    assert_eq!(
        mailbox.connected(),
        1,
        "the refusal was not counted for the recovery line"
    );
    assert_eq!(
        mailbox.post(ResetFilter(U1.to_owned())),
        Posted::Queued,
        "a momentary command was refused while the Coolmaster was connected"
    );
}

#[test]
fn reads_are_dropped_while_down_and_coalesced_while_up() {
    let mailbox = down();
    assert_eq!(
        mailbox.post(PublishUnitState(U1.to_owned())),
        Posted::Dropped
    );
    assert_eq!(mailbox.post(PublishUnitsState), Posted::Dropped);
    assert_eq!(mailbox.connected(), 0);
    assert!(mailbox.take_held().is_none());

    mailbox.post(PublishUnitState(U1.to_owned()));
    mailbox.post(PublishUnitsState);
    mailbox.post(power(U1, true));
    mailbox.post(PublishUnitState(U1.to_owned()));
    assert_eq!(
        mailbox.waiting(),
        [
            PublishUnitsState,
            power(U1, true),
            PublishUnitState(U1.to_owned())
        ],
        "a read already waiting should answer the next one too, moved behind what was set since"
    );
}

#[test]
fn momentary_commands_past_the_cap_are_refused() {
    let mailbox = up();
    for _ in 0..MOMENTARY_CAP {
        assert_eq!(mailbox.post(ResetFilter(U1.to_owned())), Posted::Queued);
    }
    assert_eq!(
        mailbox.post(ResetFilter(U1.to_owned())),
        Posted::Refused(Refusal::QueueFull)
    );
    // States are not counted against the cap.
    assert_eq!(mailbox.post(power(U1, true)), Posted::Queued);
}

#[test]
fn momentary_commands_waiting_when_the_coolmaster_goes_down_are_refused() {
    let mailbox = up();
    mailbox.post(ResetFilter(U1.to_owned()));
    mailbox.post(PublishUnitState(U1.to_owned()));
    mailbox.post(power(U1, true));
    assert_eq!(
        mailbox.disconnected(),
        [ResetFilter(U1.to_owned())],
        "the momentary command waiting at the disconnect was not refused"
    );
    assert_eq!(
        mailbox.waiting(),
        [power(U1, true)],
        "only the state should wait for the Coolmaster's return"
    );
    assert_eq!(mailbox.connected(), 1);
    assert_eq!(held(&mailbox), [power(U1, true)]);
}

#[test]
fn state_that_could_not_be_applied_goes_back_unless_superseded() {
    let mailbox = down();
    mailbox.post(temperature(U1, 23.0));
    mailbox.restore(vec![
        power(U1, true),
        temperature(U1, 20.0),
        ResetFilter(U1.to_owned()),
        power(U2, false),
    ]);
    assert_eq!(
        mailbox.waiting(),
        [power(U1, true), power(U2, false), temperature(U1, 23.0)],
        "restored states go back to the front in order, except one superseded meanwhile; nothing else goes back"
    );
}

#[test]
fn the_first_refusal_of_an_outage_is_logged_at_info_and_the_rest_at_debug() {
    let log = Capture::start();
    let mailbox = down();
    for _ in 0..3 {
        mailbox.post(ResetFilter(U1.to_owned()));
    }
    let refused = log.of_kind("command_refused");
    assert!(
        refused.len() == 1 && refused[0].level == tracing::Level::INFO,
        "an outage's refusals were not one INFO then DEBUG; the log: {:#?}",
        log.records()
    );
    assert_eq!(mailbox.connected(), 3);
    mailbox.disconnected();
    mailbox.post(ResetFilter(U1.to_owned()));
    assert_eq!(
        log.of_kind("command_refused").len(),
        2,
        "the next outage's first refusal was not logged at INFO"
    );
}

/// A full momentary queue is an episode of its own: its first refusal is an INFO, the rest DEBUG,
/// until it drains; and its refusals are not an outage's, so the recovery line does not count them.
#[tokio::test]
async fn a_full_momentary_queue_is_said_once_per_episode() {
    let log = Capture::start();
    let mailbox = up();
    for _ in 0..MOMENTARY_CAP + 3 {
        mailbox.post(ResetFilter(U1.to_owned()));
    }
    assert_eq!(
        log.of_kind("command_refused").len(),
        1,
        "a full queue's refusals were not one INFO; the log: {:#?}",
        log.records()
    );
    for _ in 0..MOMENTARY_CAP {
        let taken = tokio::time::timeout(Duration::from_secs(5), mailbox.take()).await;
        assert!(taken.is_ok(), "a waiting momentary command was not taken");
    }
    mailbox.post(ResetFilter(U1.to_owned()));
    for _ in 0..MOMENTARY_CAP {
        mailbox.post(ResetFilter(U1.to_owned()));
    }
    assert_eq!(
        log.of_kind("command_refused").len(),
        2,
        "the queue's next episode, after it drained, was not said at INFO; the log: {:#?}",
        log.records()
    );
    mailbox.disconnected();
    assert_eq!(
        mailbox.connected(),
        MOMENTARY_CAP as u32,
        "the recovery line should count the commands refused for the outage (those waiting at the disconnect), not \
         those refused for a full queue"
    );
}

/// Every log line is written with the mailbox's lock released: the posters and the worker would
/// otherwise wait on the log's writers (synchronous, for the console and the file) under it.
#[test]
fn nothing_is_logged_under_the_mailbox_lock() {
    use std::cell::Cell;
    use std::rc::Rc;

    let mailbox = up();
    // Each event as it is emitted: was the mailbox's lock held?
    let (under_lock, events) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let _watch = {
        let (mailbox, under_lock, events) = (mailbox.clone(), under_lock.clone(), events.clone());
        crate::test_support::watch(move |_| {
            events.set(events.get() + 1);
            if mailbox.state.try_lock().is_err() {
                under_lock.set(under_lock.get() + 1);
            }
        })
    };

    // Refusals for a full queue, at the disconnect, and while down: every path that logs.
    for _ in 0..MOMENTARY_CAP + 2 {
        mailbox.post(ResetFilter(U1.to_owned()));
    }
    mailbox.disconnected();
    mailbox.post(ResetFilter(U1.to_owned()));
    mailbox.post(ResetFilter(U2.to_owned()));
    mailbox.connected();

    assert!(
        events.get() >= 4,
        "the mailbox logged nothing, so this test proves nothing"
    );
    assert_eq!(
        under_lock.get(),
        0,
        "the mailbox logged while holding its lock"
    );
}

/// `take` waits for a post, and a post wakes it; a post never waits, however many there are and
/// whether or not anything takes them.
#[tokio::test]
async fn take_waits_for_a_post_and_a_post_never_waits() {
    let mailbox = up();
    let taker = {
        let mailbox = mailbox.clone();
        tokio::spawn(async move { mailbox.take().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!taker.is_finished(), "take returned with nothing posted");
    mailbox.post(power(U1, true));
    let taken = tokio::time::timeout(Duration::from_secs(5), taker).await;
    assert_eq!(
        taken.ok().and_then(Result::ok),
        Some(power(U1, true)),
        "a post did not wake the waiting take"
    );
    for i in 0..10_000 {
        mailbox.post(temperature(U1, (i % 30) as f32));
        mailbox.post(PublishUnitsState);
    }
    assert_eq!(
        mailbox.waiting().len(),
        2,
        "the mailbox grew past one entry per key"
    );
}
