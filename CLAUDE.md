# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**mqtt_ac** is a Rust service that bridges MQTT messaging with Coolmaster AC controllers over TCP. It runs on Raspberry Pi (ARMv7) and enables remote air conditioning control via MQTT topics.

## Build & Development

```bash
# Build (default target is armv7-unknown-linux-musleabihf per .cargo/config.toml)
cargo build

# Build for local development (override the ARM default target)
cargo build --target aarch64-apple-darwin

# Test on the Mac: an in-process fake broker and a stand-in Coolmaster on a local socket
cargo test --target aarch64-apple-darwin

# Check without building
cargo check

# Lint
cargo clippy --target aarch64-apple-darwin --all-targets

# Format
cargo fmt

# Deploy to Raspberry Pi (Fish shell script)
./install_to_pi <pi_ip> <mqtt_broker> <controller_name> <coolmaster_ip>
```

The `#[ignore]`d tests in `coolmaster.rs` and `ac_unit.rs` talk to the real Coolmaster (10.0.1.70):
run them by hand only, never in a gate. `--mqtt` takes `host` or `host:port` (default 1883).

Every wait carries a `// WAIT: <row>` tag naming a row of `docs/wait-registry.md`, which
`tests/wait_registry.rs` checks (Store's no-hang spec §13.3, §14). Negative controls for the
no-hang tests: `docs/no-hang-3b-controls.toml`, run with Store's `scripts/negative-control.py`.

## Architecture

Three async worker tasks. Commands for the Coolmaster go through its mailbox, which never waits;
what the Coolmaster worker reports goes through `Reports`, which never waits either; the session's
errors go through the publisher's bounded `async-channel` queue (capacity 10):

```
Polling Worker ──→ Mailbox (post never waits) ──→ Coolmaster Worker ←──→ TCP (port 10102)
MQTT Subscriber ──↗                                        │
      ↑     └── errors: ToMqttPublisherMessage ──┐   Reports (never waits: the retained
  Pump (polls rumqttc; unbounded queue)          │            model, and capped errors)
      ↑                                          ↓          ↓
  MQTT Broker  ←─────────────────────────── MQTT Publisher
```

**Workers** (spawned in `service.rs` via `tokio::task::JoinSet`):
- **Coolmaster Worker** (`coolmaster.rs`): TCP connection to Coolmaster controller. Takes commands from the mailbox only while connected, executes them (each exchange — a connect and its prompt, a command and its reply — under one 10 s deadline), and hands what it observes to `Reports`, never waiting on MQTT: a stalled broker holds up none of its work. Auto-reconnects on failure (5s retry, and never sooner than 5s after its last connect); on each reconnect it applies the state held while it was down — taken from the mailbox one at a time, so a value set meanwhile replaces its held one — then lists every unit and publishes its observed state. The Coolmaster flag (`Aircondition/Coolmaster/{controller}`) says `true` only once that catch-up has had answers, and `false` as soon as a connection is found gone. Only a failed *connection* is a reconnect: a command the Coolmaster refuses (WARN `command_rejected`) or answers with something unusable is reported on the error topic and passed over on the same connection, never retried. While the Coolmaster is unreachable its error is published on the first attempt and then only when it changes.
- **MQTT Worker** (`service.rs`): Manages MQTT connection lifecycle: a session after another (10s retry), each with a fresh client, its publisher half and its subscriber half. The publisher, `Reports` (the retained model: the Coolmaster flag and every unit's latest state) and the broker's outage state outlive the sessions; after every CONNACK the publisher publishes the whole model again.
- **Polling Worker** (`polling.rs`): Posts `PublishUnitsState` every N seconds (default 4s).

None of the workers ends on its own: one that does (a panic, an early return) ends the bridge with ERROR `worker_died` and a failure status, for systemd (`Restart=always`) to restart it.

**Commands while the Coolmaster is down** (the owner's device-down policy, 2026-10-02; `mailbox.rs`):
the four `Set*` are **states**, kept per unit and property (a newer value replaces the waiting one
and moves it to the back), applied in that order on reconnect; `ResetFilter` is **momentary**,
refused at once with an error on `Aircondition/Error/{controller}` while the Coolmaster is down
(queued, up to 64, while it is up); `PublishUnitState`/`PublishUnitsState` are **reads**,
coalesced, dropped while it is down. The Coolmaster counts as down until the worker's first
connect, so the same holds before it. An outage is logged as one episode: INFO
`device_connection_lost`, one WARN `device_unreachable` past 30 s, INFO `device_recovered`
(`down_for_ms`, `applied`, `refused`); the first refusal of an outage is an INFO `command_refused`.

**The MQTT loop** (no-hang §14.3): rumqttc's request channel drains only while its event loop is
polled, so the event loop is polled by `Pump` (`mqtt_pump.rs`), a task that waits on nothing else —
it has no client and forwards CONNACKs and publishes on an unbounded queue (WARN
`mqtt_backlog_high` past 1000 unread, INFO `mqtt_backlog_drained` back under 100). The subscriber
session reads that queue and does the work: it announces (subscription, active flag, version) on
every CONNACK, since rumqttc reconnects inside one event loop and the broker keeps no session, then
tells the publisher to publish again the retained state it knows (a reconnect without a session
dropped whatever rumqttc still held, and the publisher otherwise publishes only what changed); and
it posts commands to the Coolmaster's mailbox, never waiting on the Coolmaster. Nothing the pump
forwards to can stop it polling. The pump also clears a rumqttc 0.25 packet-id collision left
standing by a failed poll, which would otherwise stop the event loop taking requests for good. A
broker outage is one episode however many sessions it spans: INFO `connection_lost`, one WARN
`external_failure` past 30 s, INFO `external_recovered` (`down_for_ms`, `attempts`) once a
connection has lasted 5 s — one lost sooner is another failed attempt of the same outage. SIGINT
and SIGTERM are taken first thing, before the logging starts (on a blocking thread, raced against
them), and both run the bounded shutdown (5 s; past it, WARN `shutdown_timeout`); `main` then shuts
the runtime down within 1 s, abandoning any thread still in a synchronous call (a DNS lookup,
tracing-init's start on a stalled file system, which tracing-init bounds at 5 s per destination)
instead of waiting for it.

**Key modules:**
- `ac_unit.rs` — `UnitState` model and Coolmaster response parsing (power, temp, fan speed, mode)
- `messages.rs` — `ToCoolmasterMessage` and `ToMqttPublisherMessage` enums for inter-worker IPC
- `mailbox.rs` — the Coolmaster's `Mailbox`: posts never wait; states, momentary commands and reads classified per the device-down policy
- `reports.rs` — `Reports`: what the Coolmaster worker reports, handed over without waiting — the retained model (coalesced; a state is published only when it changed) and errors (at most 64 waiting; past it the oldest go, WARN `error_report_dropped` once per episode)
- `mqtt_publisher.rs` — Publishes `Reports`' changes and the session's errors as JSON; the whole model again after every CONNACK
- `mqtt_subscriber.rs` — The MQTT session: receives JSON commands from `Aircondition/Command/{name}` topic (via the pump), announces on every CONNACK
- `mqtt_pump.rs` — `Pump`: polls rumqttc's event loop in a task of its own; the forward queue's high-water flag (`Backlog`)
- `error.rs` — `CoolmasterError` and `MqttError` types using `error-stack`

**MQTT Topics:**
- `Aircondition/State/{controller}/{unit}` — Unit state JSON (retained)
- `Aircondition/Command/{controller}` — Incoming commands
- `Aircondition/Active/{controller}` — Service alive (MQTT last-will sets to "false")
- `Aircondition/Coolmaster/{controller}` — TCP connection status
- `Aircondition/Version/{controller}` — Build version

## Design Patterns

- **Type-state pattern**: `Service<Stopped>` / `Service<Started>` using phantom types
- **Message passing**: inter-worker communication goes through the Coolmaster's mailbox, `Reports` and the publisher's channel; no other shared mutable state
- **Coolmaster protocol**: TCP text protocol — send command + `\r`, read until `>` prompt, parse `OK` / `ERROR` status line

## Logging

Log levels follow the fleet policy at `~/Documents/Projects/Store/docs/guides/logging-policy.md` (portable core also in the user-level `logging-policy` skill):

- **ERROR** — a code change is warranted; never for external/environmental failures
- **WARN** — operator must act; must be zero at idle; always include a `kind` field
- **INFO** — notable lifecycle events (connect/disconnect, orderly shutdown)
- **DEBUG** — per-iteration detail (polling ticks, message receipt)

External outages (MQTT broker unreachable, Coolmaster unreachable) are each one episode: an INFO when it starts, retries at DEBUG, one WARN once it has lasted 30 s, an INFO with its length when it ends (kinds above). Orderly shutdown of channels is INFO (not WARN/ERROR).

The bridge writes nothing to stdout or stderr itself: a supervisor's pipe that stopped draining would hold the writer outside every bound. Lifecycle messages (the logging summary, `Logging started`) go through tracing-init's lossy sinks; the logging's own failure is written by the thread of the raced logging start, which a stop ends regardless; anything earlier goes nowhere but the exit status. The one exemption is `rustop`'s own command-line output (`--help`, a usage error), before anything starts.

Kinds this bridge emits: broker — `connection_lost` (INFO), `external_failure` (WARN, the broker outage only), `external_recovered` (INFO); Coolmaster — `device_connection_lost` (INFO), `device_unreachable` (WARN), `device_recovered` (INFO), `command_refused` (INFO, a momentary command while the Coolmaster is down), `command_rejected` (WARN, a command the Coolmaster refused: fix the unit id or the config); MQTT queue — `mqtt_backlog_high` (WARN), `mqtt_backlog_drained` (INFO), `mqtt_commands_discarded` (WARN); the worker's error reports — `error_report_dropped` (WARN, older ones displaced while MQTT takes none) and `error_report_drop_ended` (INFO, `dropped`, `lasted_ms`); `validation_rejected` (INFO, a malformed command payload); `worker_died` (ERROR, a worker ended: the bridge ends); `shutdown_timeout` (WARN); `signal_handler_unavailable` (WARN).

## Not done (deliberately, for later)

- **The error topic is retained** (`Aircondition/Error/{controller}` is published with retain), though the house topic rules never retain `Error`. It changes with the topic-grammar migration of this bridge and its SDL driver, together.
- **No explicit `Active=false` or DISCONNECT at shutdown.** The bounded shutdown aborts the workers, and the connection closes without a DISCONNECT, so the broker publishes the last will (`Active=false`, retained). A clean DISCONNECT would suppress the will and need an explicit `Active=false` publish first.

This bridge joins distributed traces via MQTT v5 `traceparent` user properties:
- **Inbound**: `traceparent` user property extracted from each Publish packet; span created with `tracing_init::traceparent::set_remote_parent` before entering
- **Outbound**: `tracing_init::traceparent::current()` stamped as a `traceparent` user property on every publish

GELF and OTel destinations are deploy-time config (`LOG_DESTINATION` env var / config file). The `otel` feature in `tracing-init` must be enabled (already in `Cargo.toml`).
