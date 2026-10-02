# The wait registry

Every place mqtt_ac waits on something else — a lock, a channel, the broker, the Coolmaster, a
dependency's `async fn` — carries a tag naming a row here:

```text
let event = self.rx.recv().await; // WAIT: mqtt-pump-queue
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
nothing else in this process. The subscriber session reads the pump's unbounded queue and sends to
the Coolmaster worker's queue (10) and the publisher's queue (10); the polling worker sends to the
Coolmaster worker's queue; the Coolmaster worker talks to the Coolmaster (every exchange under a
10 s timeout) and sends to the publisher's queue; the publisher and the session's announcement wait
on rumqttc's request channel, which the pump drains. Every edge leads toward the pump, and the pump
waits on nothing here — no cycle.

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-poll` | acyclic | the broker, over the network | The pump waits on nothing in this process: it has no client, and its one send is to an unbounded queue. A dead connection ends the poll with an error within two keep-alive periods (10 s), a failed connect within rumqttc's 5 s connect timeout; rumqttc reconnects on the next poll, and after five errors in a row the pump ends the session. |
| `mqtt-pump-queue` | acyclic | the pump's forward queue | Unbounded (no-hang §14.6), with a high-water WARN (`mqtt_backlog_high`): the pump never waits on the session, so the session's wait ends with the next event, or with `Ended` when the connection has failed five times running — the session returns and the MQTT worker starts a new one. If the pump task is gone the queue closes and the wait returns `None`. |
| `mqtt-backlog-lock` | acyclic | the forward queue's high-water flag | Held to read or set one timestamp; nothing waits under it. |
| `mqtt-request` | acyclic | rumqttc's request channel (`publish`, `subscribe`) | The channel drains only while the event loop is polled, and the pump polls it in a task of its own. So a full channel is back-pressure on the publisher and the session, never a cycle: with the broker stalled they wait until it recovers, the pump reading its acks meanwhile (`a_burst_of_bad_commands_…`, `a_burst_of_unit_commands_…`; both failed when the subscriber session polled). The announcement on CONNACK is made by the session, not the pump (`a_connection_lost_while_saturated_is_restored_and_resubscribed`). When the pump ends, the event loop is dropped and a waiting publish fails, ending the session. rumqttc's own stall — a packet-id collision left standing across a reconnect, after which it takes no request — is cleared by the pump (`a_collision_standing_when_the_connection_drops_…`). |
| `coolmaster-queue` | acyclic | the Coolmaster worker's queue (10), from the session and the polling worker | The Coolmaster worker waits only on the Coolmaster (`coolmaster-io`, bounded) and on the publisher's queue (`publisher-queue`), whose consumer waits only on the request channel the pump drains; it never waits on a producer, and the pump waits on neither producer. A full queue holds the session back while its commands wait in the pump's queue. |
| `publisher-queue` | acyclic | the publisher's queue (10), from the Coolmaster worker and the session | The publisher waits only on this queue and on the request channel (`mqtt-request`), which the pump drains; it never waits on a producer. Between MQTT sessions (the worker's 10 s retry) nobody reads it, and a producer waits for the next session's publisher. |
| `coolmaster-commands` | acyclic | the Coolmaster worker's queue, read | An idle wait for work: it waits only while the queue is empty, when no producer is waiting on it. It ends with the next command, or with an error when every sender has gone. |
| `publisher-messages` | acyclic | the publisher's queue, read | An idle wait for work: it waits only while the queue is empty, when no producer is waiting on it. The end of its MQTT session aborts it (`session-join`, `task-shutdown`). |
| `coolmaster-io` | bounded | the Coolmaster, over TCP: connect, each write, each reply | `COMMAND_TIMEOUT`, 10 s, per exchange. On expiry the exchange fails with `Timeout`: a failed connect is reported and retried after 5 s; a failed command is reported, and the worker drops the connection and reconnects. |
| `session-join` | acyclic | the publisher and the subscriber session of one MQTT session | Neither waits on the MQTT worker. The subscriber session ends when the pump does (the connection failed five times running), the publisher when its publish fails once the event loop is dropped; the first to end ends the session. |
| `task-shutdown` | acyclic | `JoinSet::shutdown`: a session's tasks, or the service's workers | Abort ends each task at its next await point, and no task here runs blocking code; nothing aborted waits on the caller. |
| `shutdown` | bounded | the service stopping, at process exit | `SHUTDOWN_GRACE`, 5 s. Stopping aborts the workers (`task-shutdown`), so the bound is a backstop: on expiry an ERROR (`shutdown_overrun`) names it, `main` returns, and the runtime drops what is left. |
| `ctrl-c` | acyclic | the operating system's interrupt signal | Nothing in this process is waited on. |

## Settings

```wait-lint
# rumqttc's client calls, which wait on its request channel.
wait-methods = publish, publish_with_properties, subscribe, unsubscribe, disconnect
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
# This code's own async methods, awaited on a receiver other than `self` (checked: the
# Coolmaster's and the publisher's own).
local-methods = connect_to, handle_message, run_session
```

## Waiters (generated)

```wait-lint-waiters
coolmaster-commands src/coolmaster.rs Coolmaster::coolmaster_worker
coolmaster-io src/coolmaster.rs timed
coolmaster-queue src/mqtt_subscriber.rs perform_action
coolmaster-queue src/mqtt_subscriber.rs session
coolmaster-queue src/polling.rs polling_worker
ctrl-c src/main.rs main
mqtt-backlog-lock src/mqtt_pump.rs Backlog::popped
mqtt-backlog-lock src/mqtt_pump.rs Backlog::pushed
mqtt-poll src/mqtt_pump.rs Pump::run
mqtt-pump-queue src/mqtt_pump.rs Incoming::recv
mqtt-pump-queue src/mqtt_subscriber.rs session
mqtt-request src/mqtt_publisher.rs MqttPublisher::publish_unit_state
mqtt-request src/mqtt_publisher.rs MqttPublisher::run_session
mqtt-request src/mqtt_subscriber.rs announce
publisher-messages src/mqtt_publisher.rs MqttPublisher::run_session
publisher-queue src/coolmaster.rs Coolmaster::coolmaster_worker
publisher-queue src/coolmaster.rs Coolmaster::handle_message
publisher-queue src/mqtt_subscriber.rs session
session-join src/service.rs Service::mqtt_session
shutdown src/main.rs main
task-shutdown src/service.rs Service::mqtt_session
task-shutdown src/service.rs Service::stop
```
