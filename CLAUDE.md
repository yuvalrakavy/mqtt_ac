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

Three async worker tasks communicate via bounded `async-channel` channels (capacity 10):

```
Polling Worker ──→ ToCoolmasterMessage channel ──→ Coolmaster Worker ←──→ TCP (port 10102)
MQTT Subscriber ──→ ToCoolmasterMessage channel ──↗        │
      ↑                                             ToMqttPublisherMessage channel
  Pump (polls rumqttc; unbounded queue)                     ↓
      ↑                                             MQTT Publisher ──→ MQTT Broker
  MQTT Broker
```

**Workers** (spawned in `service.rs` via `tokio::task::JoinSet`):
- **Coolmaster Worker** (`coolmaster.rs`): TCP connection to Coolmaster controller. Receives commands from the channel, executes them, sends results back. Auto-reconnects on failure (5s retry).
- **MQTT Worker** (`service.rs`): Manages MQTT connection lifecycle. Spawns publisher and subscriber as sub-tasks. Auto-reconnects on failure (10s retry).
- **Polling Worker** (`polling.rs`): Sends `PublishUnitsState` every N seconds (default 4s).

**The MQTT loop** (no-hang §14.3): rumqttc's request channel drains only while its event loop is
polled, so the event loop is polled by `Pump` (`mqtt_pump.rs`), a task that waits on nothing else —
it has no client and forwards CONNACKs and publishes on an unbounded queue (WARN
`mqtt_backlog_high` past 1000 unread, INFO `mqtt_backlog_drained` back under 100). The subscriber
session reads that queue and does the work: it announces (active flag, version, subscription) on
every CONNACK, since rumqttc reconnects inside one event loop and the broker keeps no session, and
it sends commands on to the Coolmaster worker. Nothing the pump forwards to can stop it polling.
The pump also clears a rumqttc 0.25 packet-id collision left standing by a failed poll, which
would otherwise stop the event loop taking requests for good.

**Key modules:**
- `ac_unit.rs` — `UnitState` model and Coolmaster response parsing (power, temp, fan speed, mode)
- `messages.rs` — `ToCoolmasterMessage` and `ToMqttPublisherMessage` enums for inter-worker IPC
- `mqtt_publisher.rs` — Publishes state changes as JSON; deduplicates (only publishes when state differs)
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
- **Message passing**: All inter-worker communication goes through async channels, no shared mutable state
- **Coolmaster protocol**: TCP text protocol — send command + `\r`, read until `>` prompt, parse `OK` / `ERROR` status line

## Logging

Log levels follow the fleet policy at `~/Documents/Projects/Store/docs/guides/logging-policy.md` (portable core also in the user-level `logging-policy` skill):

- **ERROR** — a code change is warranted; never for external/environmental failures
- **WARN** — operator must act; must be zero at idle; always include a `kind` field
- **INFO** — notable lifecycle events (connect/disconnect, orderly shutdown)
- **DEBUG** — per-iteration detail (polling ticks, message receipt)

External outages (MQTT broker unreachable, Coolmaster TCP errors) are `kind = "external_failure"` at INFO per iteration and promoted to WARN only when a threshold is crossed. Orderly shutdown of channels is INFO (not WARN/ERROR).

This bridge joins distributed traces via MQTT v5 `traceparent` user properties:
- **Inbound**: `traceparent` user property extracted from each Publish packet; span created with `tracing_init::traceparent::set_remote_parent` before entering
- **Outbound**: `tracing_init::traceparent::current()` stamped as a `traceparent` user property on every publish

GELF and OTel destinations are deploy-time config (`LOG_DESTINATION` env var / config file). The `otel` feature in `tracing-init` must be enabled (already in `Cargo.toml`).
