# The wait registry

Every place mqtt_ac waits on something else — a lock, a channel, the broker, the Coolmaster, a
dependency's `async fn` — carries a tag naming a row here, on a line of its own directly above the
statement (a tag trailing an opening `{` is moved into the block by rustfmt, and lost):

```text
// WAIT: mqtt-pump-queue
let event = self.rx.recv().await;
```

The row says, once, why that wait cannot hang the bridge: **`acyclic`** (nothing it waits on can
wait back on any of its waiters) or **`bounded`** (it ends within a stated bound, and the row says
what happens on expiry). The rule and the tag format are Store's no-hang spec, §13.3 and §14
(`Store/docs/superpowers/specs/2026-09-30-no-hang-wait-graph-design.md`); the check is the
`wait-lint` crate, run by `tests/wait_registry.rs`.

It does not prove the order locks are taken in, nor see a wait reached through a call to this
crate's own `async fn`; the arguments are checked by reading, and the waiters block below makes
every new waiting function a reviewed diff. Regenerate it after a change, from a checkout of
tracing-init:

```text
cargo run --manifest-path wait-lint/Cargo.toml -- --root <mqtt_ac> --src src --registry docs/wait-registry.md --write
```

**The graph, in one paragraph.** `main` runs `run` on the runtime and then shuts the runtime
down within a bound. `run` takes the stop signals first, starts the logging on a blocking thread
raced against them, then runs the service until a stop signal or the end of a worker, and its
bounded shutdown. The pump (`Pump::run`) polls rumqttc's event loop and waits on nothing else in
this process. The subscriber session reads the pump's unbounded queue, posts the Coolmaster's
commands to its mailbox — a post never waits — and sends a refused or malformed command's error to
the publisher's queue (10); the polling worker posts to the mailbox too. The Coolmaster worker takes
from the mailbox only while the Coolmaster is connected, talks to it (each exchange under one 10 s
deadline) and hands what it reports to `Reports` — observed state folded into the retained model,
errors queued — which never waits either: the worker waits on nothing but the Coolmaster, its
mailbox and its pacing (re-review C1). The publisher takes from `Reports` and its queue, and it and
the session's announcement wait on rumqttc's request channel, which the pump drains. Every edge
leads toward the pump, and the pump waits on nothing here — no cycle. Nothing waits on the
Coolmaster worker, so a Coolmaster that is down holds up nothing but its own commands (the owner's
device-down policy, 2026-10-02), and a broker that is down or stalled holds up nothing on the
Coolmaster's side.

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-poll` | acyclic | the broker, over the network | The pump waits on nothing in this process: it has no client, and its one send is to an unbounded queue. Its wait is on the broker alone, and this code does not bound it: rumqttc writes and flushes inside `poll` with no timeout, so a broker that stops reading holds the poll until the kernel gives the connection up (TCP retransmission, ~15 min on Linux), or for as long as a live peer keeps advertising a zero window; keep-alive (5 s) notices a silent peer only while writes go through. A connect is bounded by rumqttc's connect timeout (5 s). After a failed poll rumqttc reconnects on the next one; after five failures in a row the pump ends the session. |
| `mqtt-pump-queue` | acyclic | the pump's forward queue | Unbounded (no-hang §14.6), with a high-water WARN (`mqtt_backlog_high`): the pump never waits on the session, so the session's wait ends with the next event, or with `Ended` when the connection has failed five times running — the session returns and the MQTT worker starts a new one. If the pump task is gone the queue closes and the wait returns `None`. The queue's depth and its high-water flag are atomics, so neither side waits on them, and nothing is logged under a lock. |
| `mqtt-request` | acyclic | rumqttc's request channel (`publish`, `subscribe`) | The channel drains only while the event loop is polled, and the pump polls it in a task of its own. So a full channel is back-pressure on the publisher and the session, never a cycle: with the broker stalled they wait until it recovers, the pump reading its acks meanwhile (`a_burst_of_bad_commands_…`, `a_burst_of_unit_commands_…`; both failed when the subscriber session polled). Nothing on the Coolmaster's side waits behind them (`the_coolmaster_worker_takes_commands_while_the_broker_is_stalled`). The announcement on CONNACK is made by the session, not the pump (`a_connection_lost_while_saturated_is_restored_and_resubscribed`), and so is the publisher's publishing again, after a CONNACK, of the whole retained model (`a_state_a_reconnect_dropped_is_published_again`). When the pump ends, the event loop is dropped and a waiting publish fails, ending the session; what it was publishing is still in the model, for the next session (`a_report_cut_short_…`). rumqttc's own stall — a packet-id collision left standing across a reconnect, after which it takes no request — is cleared by the pump (`a_collision_standing_when_the_connection_drops_…`). |
| `publisher-queue` | acyclic | the publisher's queue (10), from the session | The publisher waits only on this queue, on `Reports`, on the session's notice of a CONNACK (`publisher-messages`) and on the request channel (`mqtt-request`), which the pump drains; it never waits on a producer. Only the session sends here — the error of a malformed or refused command — and nothing waits on the session but its own MQTT session. Between MQTT sessions (the MQTT worker's 10 s retry) nobody reads it, and the session that sends is gone with its session. The Coolmaster worker does not send here (re-review C1): it reports through `Reports`, which never waits. |
| `publisher-messages` | acyclic | the publisher's queue, read; the worker's reports; the session's notice of a CONNACK | An idle wait for work: it waits only while there is none — the queue empty, nothing reported — when no producer is waiting on it, and for the session's notice (`Notify::notify_one`, which never waits and keeps a permit when nobody is waiting). The end of its MQTT session drops it (`session-join`). |
| `reports-ready` | acyclic | the Coolmaster worker's next report (`Reports::reported`) | An idle wait for work, the publisher's. A report never waits — a short lock, then `Notify::notify_one`, which keeps a permit when nobody is waiting — so the worker never waits on the publisher, and the wait ends with the next report. Observed state coalesces in the model, so what waits stays bounded by the number of units; errors are queued, at most 64, the oldest displaced past it (WARN `error_report_dropped`, once per episode; INFO `error_report_drop_ended` when the publisher takes them again). |
| `reports-lock` | acyclic | the reports' state (a std mutex) | Held only to change or copy what waits for the publisher: never across an await, no other lock taken under it, and nothing logged under it — the displacement lines are logged after the guard is dropped (`errors_past_the_cap_displace_the_oldest_said_once_per_episode`). |
| `coolmaster-mailbox` | acyclic | the Coolmaster's mailbox, read by the Coolmaster worker (`Mailbox::take`) | An idle wait for work. A post never waits — `Mailbox::post` is synchronous: a short lock, then a notify — so no poster ever waits on the worker, and the wait ends with the next post. The worker takes only while the Coolmaster is connected; while it is down the worker is in its reconnect loop and takes nothing: states wait in the mailbox (the latest per unit and property, so it stays bounded), momentary commands are refused at once, reads dropped (`a_broker_blip_while_the_coolmaster_is_down_is_announced_and_resubscribed`). |
| `coolmaster-mailbox-lock` | acyclic | the mailbox's state (a std mutex) | Held only to read or change the waiting commands: never across an await, no other lock taken under it, and nothing logged under it — a refusal is logged after the guard is dropped (`nothing_is_logged_under_the_mailbox_lock`). |
| `coolmaster-io` | bounded | the Coolmaster, over TCP: an exchange — a connect and its prompt, or a command, its CR and its reply | `COMMAND_TIMEOUT`, 10 s, per exchange as a whole (`timed`, over `Coolmaster::open` for a connect and `Coolmaster::exchange` for a command): the steps inside — the TCP connect, the prompt, each write, the reply — are bounded by their exchange's deadline, not one each (re-review C7: one each made a command up to 30 s and a connect up to 20 s). On expiry the exchange fails with `Timeout`: a failed connect is reported (once per outage while its error does not change) and retried after 5 s; a command whose connection failed is reported, the Coolmaster flag goes `false` at once, and the worker drops the connection and reconnects no sooner than 5 s after its last connect — a state it was applying waits in the mailbox, held, for the next connection (`a_connection_lost_during_the_catch_up_keeps_every_held_state`). A command that failed on its own (refused, or answered with something unusable) is reported and passed over on the same connection, never retried: retrying it would reconnect for good (`a_command_answered_with_something_unusable_…`, `a_coolmaster_that_closes_on_every_command_…`). |
| `session-join` | acyclic | the publisher's and the subscriber's halves of one MQTT session (`select!`) | Neither waits on the MQTT worker. The subscriber session ends when the pump does (the connection failed five times running), the publisher when a publish fails once the event loop is dropped; the first to end ends the session, and the other is dropped with it. |
| `task-shutdown` | acyclic | `JoinSet::shutdown`: the service's workers | Abort ends each task at its next await point, and no task here runs blocking code; nothing aborted waits on the caller. |
| `worker-join` | acyclic | the service's workers ending (`Service::worker_ended`, `JoinSet::join_next_with_id`) | No worker waits on `run`, and none ends on its own: the wait ends only with a panic or an early return — the end of the bridge (`bridge-end`) — or is dropped when a stop arrives first. A set with no worker waits for good rather than taking "no worker" for a death (`a_worker_that_dies_ends_the_bridge_with_a_failure`). |
| `bridge-end` | acyclic | a stop signal, or a worker's end (`serve`'s `select!`) | Neither waits on `run` (`stop-signal`, `worker-join`); the first to come ends the wait, and the bounded shutdown follows (`shutdown`). A worker's end is one ERROR `worker_died` naming it and a failure status, for the service manager (`Restart=always`) to restart the bridge (re-review C5). |
| `shutdown` | bounded | the service stopping, at process exit (`stop_within`) | `SHUTDOWN_GRACE`, 5 s. Stopping aborts the workers (`task-shutdown`), so the bound is a backstop: on expiry one WARN, the fleet's `shutdown_timeout`, names it, and `run` returns anyway — dropping tracing-init's guard — and `main` shuts the runtime down within its own bound (`runtime`) (`a_shutdown_past_its_bound_is_cut_short_with_one_warn`). |
| `runtime` | bounded | `run`, on the runtime (`Runtime::block_on`); then the runtime's threads (`Runtime::shutdown_timeout`) | `run` ends with a stop signal or a worker's end and its bounded shutdown (`bridge-end`, `shutdown`), or at once on a stop during the logging start (`logging-start`). Then `shut_down` gives the runtime's threads `RUNTIME_GRACE`, 1 s; on expiry those still in a synchronous call — a DNS lookup of the broker's or the Coolmaster's name (tokio and rumqttc resolve names on blocking threads; no bound of their own), tracing-init's start (5 s per stalled destination) — are abandoned, and the process exits (re-review C6, the fleet's F1: `sigterm_during_a_held_logging_start_stops_the_bridge_within_its_bound`, whose control drops the runtime and waits out the start). Dropping the runtime instead waits for them: a lookup without limit. tracing-init's writers (console, file, GELF) are non-blocking and lossy since 97eebba (WARN `log_lines_dropped`), and dropping its guard is bounded (about 4 s), so a stalled log destination holds no thread here, the shutdown included. |
| `logging-start` | bounded | tracing-init's start (`start_logging`), on a blocking thread | It reads its configuration, opens today's log file and resolves the GELF host, synchronously. Since 97eebba tracing-init bounds the open and the lookup at 5 s each; on expiry it starts without that destination (WARN `log_destination_skipped`) and abandons the thread stuck in it. Only `run` waits on the start, before the service starts, and races it against the stop signals: a stop ends the wait at once, and the blocking thread still in the start is abandoned within `runtime`'s grace (F1, F3: `sigterm_during_a_held_logging_start_…`; its controls take the signals after the start, or wait the start out). Without the stop signals (they could not be taken, and the bridge will not start) `run` waits on it alone, within tracing-init's bounds. In debug builds only, the start first reads the file `MQTT_AC_TEST_LOGGING_GATE` names, if set — the test seam that holds the start for as long as a test likes (a FIFO whose write end it holds); the deployed release build has none. The start's thread is also the only place the bridge writes to stderr itself (the logging's own failure), so a stderr nobody drains holds that thread, never the stop (`a_stdout_and_stderr_nobody_reads_hold_neither_the_start_nor_the_stop`). |
| `stop-signal` | acyclic | the operating system's SIGINT or SIGTERM | Nothing in this process is waited on. Both are registered first thing in `run`, before the logging starts (the fleet's F3), so from then on either one is a stop request — the bounded shutdown above — rather than the end of the process (`sigterm_stops_the_bridge_cleanly`, `sigterm_during_a_held_logging_start_…`). |

## Settings

```wait-lint
# rumqttc's client calls, which wait on its request channel, and its event loop's poll.
wait-methods = publish, publish_with_properties, subscribe, unsubscribe, disconnect, poll
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
# This code's own async methods, awaited on a receiver other than `self` (checked: the
# Coolmaster's own).
local-methods = connect_to, handle_message, catch_up
```

## Waiters (generated)

```wait-lint-waiters
bridge-end src/main.rs serve
coolmaster-io src/coolmaster.rs Coolmaster::command
coolmaster-io src/coolmaster.rs Coolmaster::get_reply_from_coolmaster
coolmaster-io src/coolmaster.rs Coolmaster::open
coolmaster-io src/coolmaster.rs Coolmaster::send_to_coolmaster
coolmaster-io src/coolmaster.rs timed
coolmaster-mailbox src/coolmaster.rs Coolmaster::coolmaster_worker
coolmaster-mailbox src/mailbox.rs Mailbox::take
coolmaster-mailbox-lock src/mailbox.rs Mailbox::connected
coolmaster-mailbox-lock src/mailbox.rs Mailbox::disconnected
coolmaster-mailbox-lock src/mailbox.rs Mailbox::post
coolmaster-mailbox-lock src/mailbox.rs Mailbox::restore
coolmaster-mailbox-lock src/mailbox.rs Mailbox::take
coolmaster-mailbox-lock src/mailbox.rs Mailbox::take_held
logging-start src/main.rs run
mqtt-poll src/mqtt_pump.rs Pump::run
mqtt-pump-queue src/mqtt_pump.rs Incoming::recv
mqtt-pump-queue src/mqtt_subscriber.rs session
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_coolmaster_connected
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_error
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_unit_state
mqtt-request src/mqtt_subscriber.rs announce
publisher-messages src/mqtt_publisher.rs MqttPublisher::session
publisher-queue src/mqtt_subscriber.rs session
publisher-queue src/mqtt_subscriber.rs submit
reports-lock src/reports.rs Reports::locked
reports-ready src/reports.rs Reports::reported
runtime src/main.rs main
session-join src/service.rs Service::mqtt_session
shutdown src/main.rs stop_within
stop-signal src/main.rs StopSignals::next
task-shutdown src/service.rs Service::stop
worker-join src/service.rs Service::worker_ended
```
