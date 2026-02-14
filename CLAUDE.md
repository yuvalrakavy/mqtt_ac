# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**mqtt_ac** is a Rust service that bridges MQTT messaging with Coolmaster AC controllers over TCP. It runs on Raspberry Pi (ARMv7) and enables remote air conditioning control via MQTT topics.

## Build & Development

```bash
# Build (default target is armv7-unknown-linux-musleabihf per .cargo/config.toml)
cargo build

# Build for local development (override the ARM default target)
cargo build --target x86_64-apple-darwin

# Check without building
cargo check

# Lint
cargo clippy

# Format
cargo fmt

# Deploy to Raspberry Pi (Fish shell script)
./install_to_pi <pi_ip> <mqtt_broker> <controller_name> <coolmaster_ip>
```

There are no tests in this project.

## Architecture

Three async worker tasks communicate via bounded `async-channel` channels (capacity 10):

```
Polling Worker ──→ ToCoolmasterMessage channel ──→ Coolmaster Worker ←──→ TCP (port 10102)
MQTT Subscriber ──→ ToCoolmasterMessage channel ──↗        │
                                                    ToMqttPublisherMessage channel
                                                            ↓
                                                    MQTT Publisher ──→ MQTT Broker
```

**Workers** (spawned in `service.rs` via `tokio::task::JoinSet`):
- **Coolmaster Worker** (`coolmaster.rs`): TCP connection to Coolmaster controller. Receives commands from the channel, executes them, sends results back. Auto-reconnects on failure (5s retry).
- **MQTT Worker** (`service.rs`): Manages MQTT connection lifecycle. Spawns publisher and subscriber as sub-tasks. Auto-reconnects on failure (10s retry).
- **Polling Worker** (`polling.rs`): Sends `PublishUnitsState` every N seconds (default 4s).

**Key modules:**
- `ac_unit.rs` — `UnitState` model and Coolmaster response parsing (power, temp, fan speed, mode)
- `messages.rs` — `ToCoolmasterMessage` and `ToMqttPublisherMessage` enums for inter-worker IPC
- `mqtt_publisher.rs` — Publishes state changes as JSON; deduplicates (only publishes when state differs)
- `mqtt_subscriber.rs` — Receives JSON commands from `Aircondition/Command/{name}` topic
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
