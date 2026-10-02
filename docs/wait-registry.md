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

**The graph, in one paragraph.** The pump (`Pump::run`) polls rumqttc's event loop and waits on
nothing else in this process. The subscriber session reads the pump's unbounded queue, posts the
Coolmaster's commands to its mailbox — a post never waits — and sends a refused or malformed
command's error to the publisher's queue (10); the polling worker posts to the mailbox too. The
Coolmaster worker takes from the mailbox only while the Coolmaster is connected, talks to it
(every exchange under a 10 s timeout) and sends to the publisher's queue. The publisher and the
session's announcement wait on rumqttc's request channel, which the pump drains. Every edge leads
toward the pump, and the pump waits on nothing here — no cycle. Nothing waits on the Coolmaster
worker, so a Coolmaster that is down holds up nothing but its own commands (the owner's
device-down policy, 2026-10-02).

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-poll` | acyclic | the broker, over the network | The pump waits on nothing in this process: it has no client, and its one send is to an unbounded queue. Its wait is on the broker alone, and this code does not bound it: rumqttc writes and flushes inside `poll` with no timeout, so a broker that stops reading holds the poll until the kernel gives the connection up (TCP retransmission, ~15 min on Linux), or for as long as a live peer keeps advertising a zero window; keep-alive (5 s) notices a silent peer only while writes go through. A connect is bounded by rumqttc's connect timeout (5 s). After a failed poll rumqttc reconnects on the next one; after five failures in a row the pump ends the session. |
| `mqtt-pump-queue` | acyclic | the pump's forward queue | Unbounded (no-hang §14.6), with a high-water WARN (`mqtt_backlog_high`): the pump never waits on the session, so the session's wait ends with the next event, or with `Ended` when the connection has failed five times running — the session returns and the MQTT worker starts a new one. If the pump task is gone the queue closes and the wait returns `None`. The queue's depth and its high-water flag are atomics, so neither side waits on them, and nothing is logged under a lock. |
| `mqtt-request` | acyclic | rumqttc's request channel (`publish`, `subscribe`) | The channel drains only while the event loop is polled, and the pump polls it in a task of its own. So a full channel is back-pressure on the publisher and the session, never a cycle: with the broker stalled they wait until it recovers, the pump reading its acks meanwhile (`a_burst_of_bad_commands_…`, `a_burst_of_unit_commands_…`; both failed when the subscriber session polled). The announcement on CONNACK is made by the session, not the pump (`a_connection_lost_while_saturated_is_restored_and_resubscribed`), and so is the publisher's publishing again, after a CONNACK, of the retained state it knows (`a_state_a_reconnect_dropped_is_published_again`). When the pump ends, the event loop is dropped and a waiting publish fails, ending the session. rumqttc's own stall — a packet-id collision left standing across a reconnect, after which it takes no request — is cleared by the pump (`a_collision_standing_when_the_connection_drops_…`). |
| `publisher-queue` | acyclic | the publisher's queue (10), from the Coolmaster worker and the session | The publisher waits only on this queue, on the session's notice of a CONNACK (`publisher-messages`) and on the request channel (`mqtt-request`), which the pump drains; it never waits on a producer. The session sends here only the error of a malformed or refused command; the Coolmaster worker its states, its connection status and its errors, and nothing waits on the worker. Between MQTT sessions (the MQTT worker's 10 s retry) nobody reads it, and a producer waits for the next session's publisher. |
| `publisher-messages` | acyclic | the publisher's queue, read; the session's notice of a CONNACK | An idle wait for work: it waits only while the queue is empty, when no producer is waiting on it, and for the session's notice (`Notify::notify_one`, which never waits and keeps a permit when nobody is waiting). The end of its MQTT session drops it (`session-join`). |
| `coolmaster-mailbox` | acyclic | the Coolmaster's mailbox, read by the Coolmaster worker (`Mailbox::take`) | An idle wait for work. A post never waits — `Mailbox::post` is synchronous: a short lock, then a notify — so no poster ever waits on the worker, and the wait ends with the next post. The worker takes only while the Coolmaster is connected; while it is down the worker is in its reconnect loop and takes nothing: states wait in the mailbox (the latest per unit and property, so it stays bounded), momentary commands are refused at once, reads dropped (`a_broker_blip_while_the_coolmaster_is_down_is_announced_and_resubscribed`). |
| `coolmaster-mailbox-lock` | acyclic | the mailbox's state (a std mutex) | Held only to read or change the waiting commands: never across an await, no other lock taken under it, and nothing logged under it — a refusal is logged after the guard is dropped (`nothing_is_logged_under_the_mailbox_lock`). |
| `coolmaster-io` | bounded | the Coolmaster, over TCP: connect, each write, each reply | `COMMAND_TIMEOUT`, 10 s, per exchange. On expiry the exchange fails with `Timeout`: a failed connect is reported and retried after 5 s; a failed command is reported, and the worker drops the connection and reconnects — a state it was applying waits in the mailbox for the next connection. |
| `session-join` | acyclic | the publisher's and the subscriber's halves of one MQTT session (`select!`) | Neither waits on the MQTT worker. The subscriber session ends when the pump does (the connection failed five times running), the publisher when a publish fails once the event loop is dropped; the first to end ends the session, and the other is dropped with it. |
| `task-shutdown` | acyclic | `JoinSet::shutdown`: the service's workers | Abort ends each task at its next await point, and no task here runs blocking code; nothing aborted waits on the caller. |
| `shutdown` | bounded | the service stopping, at process exit (`stop_within`) | `SHUTDOWN_GRACE`, 5 s. Stopping aborts the workers (`task-shutdown`), so the bound is a backstop: on expiry one WARN, the fleet's `shutdown_timeout`, names it, and `main` returns anyway — dropping tracing-init's guard — and the runtime drops what is left (`a_shutdown_past_its_bound_is_cut_short_with_one_warn`). |
| `stop-signal` | acyclic | the operating system's SIGINT or SIGTERM | Nothing in this process is waited on. Both are registered before the service starts, so from then on either one is a stop request — the bounded shutdown above — rather than the end of the process (`sigterm_stops_the_bridge_cleanly`). |

## Settings

```wait-lint
# rumqttc's client calls, which wait on its request channel, and its event loop's poll.
wait-methods = publish, publish_with_properties, subscribe, unsubscribe, disconnect, poll
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
# This code's own async methods, awaited on a receiver other than `self` (checked: the
# Coolmaster's own).
local-methods = connect_to, handle_message, catch_up, lost
```

## Waiters (generated)

```wait-lint-waiters
coolmaster-io src/coolmaster.rs timed
coolmaster-mailbox src/coolmaster.rs Coolmaster::coolmaster_worker
coolmaster-mailbox src/mailbox.rs Mailbox::take
coolmaster-mailbox-lock src/mailbox.rs Mailbox::connected
coolmaster-mailbox-lock src/mailbox.rs Mailbox::disconnected
coolmaster-mailbox-lock src/mailbox.rs Mailbox::post
coolmaster-mailbox-lock src/mailbox.rs Mailbox::restore
coolmaster-mailbox-lock src/mailbox.rs Mailbox::take
mqtt-poll src/mqtt_pump.rs Pump::run
mqtt-pump-queue src/mqtt_pump.rs Incoming::recv
mqtt-pump-queue src/mqtt_subscriber.rs session
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_coolmaster_connected
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_unit_state
mqtt-request src/mqtt_publisher.rs MqttPublisher::session
mqtt-request src/mqtt_subscriber.rs announce
publisher-messages src/mqtt_publisher.rs MqttPublisher::session
publisher-queue src/coolmaster.rs Coolmaster::catch_up
publisher-queue src/coolmaster.rs Coolmaster::coolmaster_worker
publisher-queue src/coolmaster.rs Coolmaster::handle_message
publisher-queue src/coolmaster.rs report_refused
publisher-queue src/mqtt_subscriber.rs session
publisher-queue src/mqtt_subscriber.rs submit
session-join src/service.rs Service::mqtt_session
shutdown src/main.rs stop_within
stop-signal src/main.rs stop_requested
task-shutdown src/service.rs Service::stop
```
