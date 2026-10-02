//! The Coolmaster worker against a stand-in Coolmaster on a local socket that can be taken down:
//! the device-down policy as the worker carries it out, and how an outage is logged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Coolmaster, DeviceTiming};
use crate::mailbox::{Mailbox, Posted, Refusal};
use crate::messages::ToCoolmasterMessage::{self, *};
use crate::reports::Reports;
use crate::test_support::{Answer, Capture, FakeCoolmaster, UNITS};

const UNIT: &str = UNITS[0];

/// The worker's pacing in the tests that count its connections.
const RETRY: Duration = Duration::from_millis(300);

/// The connections a stand-in serves in the tests where a regression reconnects in a loop: enough
/// for the worker as it should be, few enough to catch the loop at once.
const SPIN_CAP: usize = 12;

fn paced() -> DeviceTiming {
    DeviceTiming {
        retry: RETRY,
        warn_after: Duration::from_secs(30),
    }
}

/// How many of these the Coolmaster received.
fn received(coolmaster: &FakeCoolmaster, command: &str) -> usize {
    coolmaster
        .commands()
        .iter()
        .filter(|c| *c == command)
        .count()
}

/// Wait until `ready` holds, or `within` passes. Returns whether it held.
async fn eventually(within: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while !ready() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// The worker, with a mailbox, and what it hands the publisher (as text: `CoolmasterConnected(..)`,
/// `UnitState(..)` for each unit, `Error(..)`), taken as a publisher takes it.
struct Rig {
    mailbox: Arc<Mailbox>,
    published: Arc<Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Rig {
    fn start(address: String, timing: DeviceTiming) -> Rig {
        let mailbox = Mailbox::new();
        let reports = Reports::new();
        let published = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let (mailbox, reports) = (mailbox.clone(), reports.clone());
            tokio::spawn(async move {
                Coolmaster::coolmaster_worker(&address, mailbox, reports, timing).await;
            })
        };
        let reader = {
            let published = published.clone();
            tokio::spawn(async move {
                loop {
                    reports.reported().await;
                    let changes = reports.take();
                    let mut published = published.lock().unwrap();
                    if let Some(connected) = changes.connected {
                        published.push(format!("CoolmasterConnected({connected})"));
                    }
                    for unit in changes.units {
                        published.push(format!("UnitState({unit:?})"));
                    }
                    for error in changes.errors {
                        published.push(format!("Error({error:?})"));
                    }
                }
            })
        };
        Rig {
            mailbox,
            published,
            tasks: vec![worker, reader],
        }
    }

    fn published(&self) -> Vec<String> {
        self.published.lock().unwrap().clone()
    }
}

/// A Coolmaster outage, start to end, through the worker: the connection drops under a command,
/// the Coolmaster stays away past the WARN threshold, and comes back.
///
/// - The state in flight when the connection dropped, and the state set while it was down, are
///   applied when it is back — the latest value per unit and property, in the order last set —
///   and then the units are listed (their observed state published).
/// - The momentary command waiting when the connection dropped, and the one posted while it was
///   down, are refused (the first with an error publish by the worker, the second by the poster)
///   and never sent.
/// - The log tells it as one episode: INFO `device_connection_lost`, one WARN
///   `device_unreachable` however many attempts, INFO `device_recovered` with its length and how
///   many states were applied and commands refused.
#[tokio::test]
async fn a_coolmaster_outage_is_applied_refused_and_logged_as_one_episode() {
    let coolmaster = FakeCoolmaster::start().await;
    let log = Capture::start();
    let timing = DeviceTiming {
        retry: Duration::from_millis(100),
        warn_after: Duration::from_millis(500),
    };
    let rig = Rig::start(coolmaster.address.clone(), timing);
    assert!(
        eventually(Duration::from_secs(5), || coolmaster
            .commands()
            .iter()
            .any(|c| c == "ls2"))
        .await,
        "the worker never connected and listed the units"
    );

    coolmaster.set_up(false);
    // Back to back, so both are waiting when the worker takes the first and finds the connection
    // gone.
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), true));
    rig.mailbox.post(ResetFilter(UNIT.to_owned()));
    assert!(
        eventually(Duration::from_secs(5), || !log
            .of_kind("device_connection_lost")
            .is_empty())
        .await,
        "the worker never noticed the connection was gone"
    );
    assert_eq!(
        rig.mailbox.post(ResetFilter(UNIT.to_owned())),
        Posted::Refused(Refusal::CoolmasterDown),
        "a momentary command was taken while the Coolmaster was down"
    );
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 22.0));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before_return = coolmaster.commands().len();
    coolmaster.set_up(true);
    assert!(
        eventually(Duration::from_secs(5), || !log
            .of_kind("device_recovered")
            .is_empty())
        .await,
        "the worker never recovered once the Coolmaster was back; the log: {:#?}",
        log.records()
    );
    let after_return: Vec<String> = coolmaster.commands()[before_return..].to_vec();
    let published = rig.published();
    drop(rig);

    assert_eq!(
        after_return,
        ["on L1.001", "temp L1.001 22", "ls2"],
        "on its return the Coolmaster should get the state in flight at the drop and the state set meanwhile, in \
         order, and then a listing of the units"
    );
    assert!(
        published.iter().any(|p| p.contains("ResetFilter") && p.contains("refused")),
        "the momentary command waiting at the drop was refused without an error publish: {published:#?}"
    );
    assert!(
        published.iter().any(|p| p.starts_with("UnitState")),
        "the units' observed state was not published after the return: {published:#?}"
    );

    let lost = log.of_kind("device_connection_lost");
    assert!(
        lost.len() == 1 && lost[0].level == tracing::Level::INFO,
        "the outage's start was not one INFO; the log: {:#?}",
        log.records()
    );
    let warnings = log.at_least(tracing::Level::WARN);
    assert!(
        warnings.len() == 1
            && warnings[0].kind == "device_unreachable"
            && warnings[0].field("down_for_ms").is_some(),
        "the outage was not one WARN past its threshold; the log: {:#?}",
        log.records()
    );
    let recovered = log.of_kind("device_recovered");
    let field = |name: &str| {
        recovered
            .first()
            .and_then(|r| r.field(name).map(str::to_owned))
    };
    assert!(
        recovered.len() == 1
            && recovered[0].level == tracing::Level::INFO
            && field("applied").as_deref() == Some("2")
            && field("refused").as_deref() == Some("2")
            && field("down_for_ms").and_then(|ms| ms.parse::<u64>().ok()).is_some_and(|ms| ms >= 500),
        "the outage's end was not one INFO with its length, the states applied (2) and the commands refused (2); the \
         log: {:#?}",
        log.records()
    );
    let refusals = log.of_kind("command_refused");
    assert!(
        refusals.len() == 1 && refusals[0].level == tracing::Level::INFO,
        "the outage's refusals were not one INFO (the rest at DEBUG); the log: {:#?}",
        log.records()
    );
}

/// A listing the bridge cannot use (lines no unit state parses from) is not a lost connection: the
/// worker reports it and goes on to its mailbox. Taken for one, it reconnects at once, lists
/// again and fails again, for good — a loop as fast as the Coolmaster answers that never takes a
/// command, so a momentary one is refused with the Coolmaster up.
#[tokio::test]
async fn a_listing_the_bridge_cannot_use_does_not_keep_the_worker_from_its_mailbox() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.answer("ls2", Answer::Unusable);
    let timing = DeviceTiming {
        retry: Duration::from_millis(100),
        warn_after: Duration::from_secs(30),
    };
    let rig = Rig::start(coolmaster.address.clone(), timing);
    assert!(
        eventually(Duration::from_secs(5), || coolmaster
            .commands()
            .iter()
            .any(|c| c == "ls2"))
        .await,
        "the worker never connected and listed the units"
    );
    // Time for a reconnect loop, if there is one, to show.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let posted = rig.mailbox.post(ResetFilter(UNIT.to_owned()));
    let sent = eventually(Duration::from_secs(5), || {
        coolmaster.commands().iter().any(|c| c == "filt L1.001")
    })
    .await;
    let served = coolmaster.served();
    let published = rig.published();
    drop(rig);
    assert!(
        posted == Posted::Queued && sent && served == 1,
        "a listing the bridge could not use was taken for a lost connection: {served} connections, and the ResetFilter \
         posted with the Coolmaster up was {posted:?} and {} it",
        if sent { "reached" } else { "never reached" }
    );
    assert!(
        published
            .iter()
            .any(|p| p.starts_with("Error") && p.contains("PublishUnitsState")),
        "the listing that could not be used was not reported on the error topic: {published:#?}"
    );
}

/// A Coolmaster that is down from the start: nothing is refused before the worker has tried, and
/// once it has, reads are dropped and momentary commands refused; states wait.
#[tokio::test]
async fn a_coolmaster_down_from_the_start_holds_state_only() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.set_up(false);
    let timing = DeviceTiming {
        retry: Duration::from_millis(100),
        warn_after: Duration::from_secs(30),
    };
    let rig = Rig::start(coolmaster.address.clone(), timing);
    assert!(
        eventually(Duration::from_secs(5), || rig
            .published()
            .iter()
            .any(|p| p.starts_with("Error")))
        .await,
        "the worker never reported the Coolmaster down"
    );
    let posted: Vec<Posted> = [
        PublishUnitsState,
        ResetFilter(UNIT.to_owned()),
        SetUnitPower(UNIT.to_owned(), false),
    ]
    .into_iter()
    .map(|c: ToCoolmasterMessage| rig.mailbox.post(c))
    .collect();
    assert_eq!(
        posted,
        [
            Posted::Dropped,
            Posted::Refused(Refusal::CoolmasterDown),
            Posted::Queued
        ],
        "with the Coolmaster down, a read should be dropped, a momentary command refused and a state kept"
    );
    coolmaster.set_up(true);
    assert!(
        eventually(Duration::from_secs(5), || coolmaster
            .commands()
            .iter()
            .any(|c| c == "ls2"))
        .await,
        "the worker never listed the units once the Coolmaster was up"
    );
    let commands = coolmaster.commands();
    drop(rig);
    assert_eq!(
        commands,
        ["off L1.001", "ls2"],
        "only the state should have waited for the Coolmaster"
    );
}

/// A command the Coolmaster answers with something the bridge cannot use (a reply that is not
/// text) failed; the connection did not. It is reported once and passed over, on the same
/// connection, and the next command is served. Taken for a lost connection, it is put back and
/// tried again first thing on a reconnect made at once: the worker spins — connect, apply, fail —
/// as fast as the Coolmaster answers, and never reaches the next command.
#[tokio::test]
async fn a_command_answered_with_something_unusable_is_passed_over_on_the_same_connection() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.answer("on ", Answer::Unusable);
    let rig = Rig::start(coolmaster.address.clone(), paced());
    assert!(
        eventually(Duration::from_secs(5), || received(&coolmaster, "ls2") > 0).await,
        "the worker never connected and listed the units"
    );
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), true));
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 22.0));
    let next = eventually(Duration::from_secs(3), || {
        received(&coolmaster, "temp L1.001 22") > 0
    })
    .await;
    // A window for a loop, if there is one, to show.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (served, tries) = (coolmaster.served(), received(&coolmaster, "on L1.001"));
    let reports = rig
        .published()
        .iter()
        .filter(|p| p.starts_with("Error") && p.contains("SetUnitPower"))
        .count();
    drop(rig);
    assert!(
        next && served == 1 && tries == 1 && reports == 1,
        "a command answered with something unusable was not passed over on its connection: {served} connections, \
         tried {tries} times, reported {reports} times, and the next command {} the Coolmaster",
        if next { "reached" } else { "never reached" }
    );
}

/// A Coolmaster that takes the connection and closes it on the first command, every time: the
/// worker reconnects at its retry pace, no faster, and once the Coolmaster answers again the
/// mailbox is served. Reconnecting at once, the worker spins as fast as the Coolmaster closes.
#[tokio::test]
async fn a_coolmaster_that_closes_on_every_command_is_reconnected_to_at_the_retry_pace() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.answer("", Answer::Close);
    let rig = Rig::start(coolmaster.address.clone(), paced());
    assert!(
        eventually(Duration::from_secs(5), || coolmaster.served() > 0).await,
        "the worker never connected"
    );
    // Counted from the first connection (the cap stops a loop at SPIN_CAP).
    let window = Duration::from_millis(1500);
    tokio::time::sleep(window).await;
    let in_window = coolmaster.served();
    coolmaster.answer_normally();
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), false));
    let next = eventually(Duration::from_secs(3), || {
        received(&coolmaster, "off L1.001") > 0
    })
    .await;
    drop(rig);
    // One connection per RETRY at most: the first and 5 more in the window, and a margin.
    assert!(
        in_window <= 7,
        "the worker reconnected {in_window} times in {window:?} to a Coolmaster that closed on every command — \
         at once each time, not at its {RETRY:?} retry pace"
    );
    assert!(
        next,
        "the Coolmaster answered again, but the state posted was never applied"
    );
}

/// An outage's error is published on the first failed attempt, and again only if it changes —
/// not on every retry for as long as the Coolmaster is away.
#[tokio::test]
async fn a_coolmaster_outage_publishes_its_error_once_while_it_does_not_change() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.set_up(false);
    let timing = DeviceTiming {
        retry: Duration::from_millis(30),
        warn_after: Duration::from_secs(30),
    };
    let rig = Rig::start(coolmaster.address.clone(), timing);
    // Some twenty attempts.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let reports: Vec<String> = rig
        .published()
        .into_iter()
        .filter(|p| p.starts_with("Error") && p.contains("Connection closed by remote"))
        .collect();
    drop(rig);
    assert!(
        reports.len() == 1,
        "an outage whose error did not change published it {} times, once per retry: {reports:#?}",
        reports.len()
    );
}

/// The worker has found the Coolmaster down and said so on the error topic.
async fn reported_down(rig: &Rig) -> bool {
    eventually(Duration::from_secs(5), || {
        rig.published().iter().any(|p| p.starts_with("Error"))
    })
    .await
}

/// The device-down policy's "latest value per unit and property" holds during the catch-up too
/// (re-review C3): a value set while the held state is being applied replaces the held one for its
/// unit and property, which is then never sent. Taken out of the mailbox all at once when the
/// Coolmaster is back, the held values are applied however long the catch-up takes, and a value
/// already superseded reaches the Coolmaster — ON then OFF, a setpoint and then another.
#[tokio::test]
async fn a_value_set_during_the_catch_up_supersedes_the_held_one() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.set_up(false);
    let rig = Rig::start(coolmaster.address.clone(), paced());
    assert!(
        reported_down(&rig).await,
        "the worker never reported the Coolmaster down"
    );
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), true));
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 20.0));
    // The Coolmaster holds its answer to the first held state until the test lets it go: the
    // catch-up has begun, and the held setpoint is not applied yet.
    coolmaster.answer_once("on ", Answer::Gated);
    coolmaster.set_up(true);
    assert!(
        eventually(Duration::from_secs(5), || received(&coolmaster, "on L1.001") == 1).await,
        "the worker never applied the held power once the Coolmaster was back"
    );
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 22.0));
    coolmaster.open_gate();
    let newest = eventually(Duration::from_secs(5), || {
        received(&coolmaster, "temp L1.001 22") == 1
    })
    .await;
    let commands = coolmaster.commands();
    drop(rig);
    assert!(
        newest,
        "the setpoint set during the catch-up never reached the Coolmaster: {commands:?}"
    );
    assert!(
        !commands.iter().any(|c| c == "temp L1.001 20"),
        "a held setpoint superseded during the catch-up was applied anyway, before the newer one: {commands:?}"
    );
}

/// The catch-up's connection fails under the first held state (re-review C8): that state, and
/// every held state after it, wait for the next connection and are applied there, in order. Not
/// put back, they are lost: the Coolmaster never gets what was set while it was away.
#[tokio::test]
async fn a_connection_lost_during_the_catch_up_keeps_every_held_state() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.set_up(false);
    let rig = Rig::start(coolmaster.address.clone(), paced());
    assert!(
        reported_down(&rig).await,
        "the worker never reported the Coolmaster down"
    );
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), true));
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 22.0));
    // Back, but the first command of the catch-up loses the connection.
    coolmaster.answer_once("on ", Answer::Close);
    coolmaster.set_up(true);
    let listed = eventually(Duration::from_secs(5), || received(&coolmaster, "ls2") > 0).await;
    let commands = coolmaster.commands();
    drop(rig);
    assert!(
        listed,
        "the worker never listed the units once the Coolmaster answered: {commands:?}"
    );
    assert_eq!(
        commands,
        ["on L1.001", "on L1.001", "temp L1.001 22", "ls2"],
        "the held states the failed catch-up had not applied were not all applied on the next connection"
    );
}

/// A command the Coolmaster refuses (an unknown unit, say) is someone's to fix: one WARN of its
/// own kind, `command_rejected` — not `external_failure`, the broker outage's — and the connection
/// stays, serving the next command.
#[tokio::test]
async fn a_command_the_coolmaster_rejects_is_one_warn_command_rejected_and_the_connection_stays() {
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(SPIN_CAP);
    coolmaster.answer("on ", Answer::Rejected);
    let log = Capture::start();
    let rig = Rig::start(coolmaster.address.clone(), paced());
    assert!(
        eventually(Duration::from_secs(5), || received(&coolmaster, "ls2") > 0).await,
        "the worker never connected and listed the units"
    );
    rig.mailbox.post(SetUnitPower(UNIT.to_owned(), true));
    rig.mailbox
        .post(SetTargetTemperature(UNIT.to_owned(), 22.0));
    let next = eventually(Duration::from_secs(3), || {
        received(&coolmaster, "temp L1.001 22") > 0
    })
    .await;
    let served = coolmaster.served();
    drop(rig);
    let warnings = log.at_least(tracing::Level::WARN);
    assert!(
        warnings.len() == 1 && warnings[0].kind == "command_rejected",
        "the rejected command was not one WARN `command_rejected`; the log: {:#?}",
        log.records()
    );
    assert!(
        next && served == 1,
        "after a rejected command the connection should stay and serve the next one ({served} connections; the next \
         command {} the Coolmaster)",
        if next { "reached" } else { "never reached" }
    );
}
