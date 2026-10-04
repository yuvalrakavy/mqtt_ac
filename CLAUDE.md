# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**mqtt_ac v2** bridges a CoolMaster (CoolMasterNet) AC controller to MQTT for the new Store. It is
the CoolMaster's protocol as a `DeviceDriver` on the shared **Bridge Runtime** (tracing-init
`mqtt-bridge-kit`), speaking the **Bridge Kit** grammar. It runs on a Raspberry Pi (ARMv7, musl).

**v1 lives on branch `v1` (tag `v0.2.1`) and serves the old home system** — critical fixes only,
there. v2 (this branch line, version 2.x) is incompatible with it on purpose. Before changing v1,
ask: does the old system see this on the wire? Never change v1's MQTT contract.

The contract (see `README.md` for the CoolMaster pair's topics and payloads):
- Store `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md` — §3 the grammar, §4 the
  `Desired` lane, §9 this pilot's v1-to-v2 table; the mandated contract is Store
  `docs/guides/mqtt-bridge-contract.md`, "The CoolMaster pair".
- tracing-init `docs/superpowers/specs/2026-10-04-bridge-runtime-design.md` — §4 the driver API,
  §5 what the runtime owns, §5.1 the wire details, §6 the Rust binding.
- The fleet's rules: the `developing-mqtt-bridges` skill.

## Build & Development

```bash
cargo build                                              # the Pi (armv7-unknown-linux-musleabihf, .cargo/config.toml)
cargo check --target armv7-unknown-linux-musleabihf      # the Pi, explicitly
cargo test --target aarch64-apple-darwin                 # on the Mac
cargo clippy --target aarch64-apple-darwin --all-targets # must EXIT 0 (deny-by-default lints are errors)
cargo fmt                                                # rustfmt.toml: wide, like the runtime
./install_to_pi <pi_ip> <mqtt_broker> <controller_name> <coolmaster_ip>
```

The runtime, `mqtt-test-broker` and `wait-lint` are path dependencies on a tracing-init checkout
for now (`Cargo.toml`, `TODO: pin to the tracing-init git rev once R1 merges`).

## Architecture

The runtime owns everything that is policy — the topics from `--root`/`--instance`, the MQTT pump
and session, the device mailbox (requests held to their expiry, the outage conflict rule,
momentary commands refused while down), outages and their log episodes, reports that never wait,
replies, the process (signals first, the logging start raced against the stop, the bounded
shutdown, the exit status), and logging. This crate keeps only the CoolMaster:

- `src/protocol.rs` — the CoolMaster's ASCII protocol, no I/O: a reply's status, `ls2` lines as
  State documents (one bad line costs only its unit), the commands, the apply order
  (`plan`: power on first; mode, setpoint, fan; power off last), the unit-address guard, and the
  error classification. Unit tests in `src/protocol/tests.rs`.
- `src/driver.rs` — the `DeviceDriver`: `info()` (acknowledged writes, observed feedback,
  `ResetFilter` momentary, v1's timing: connect 10 s, operation 10 s, retry 5 s, WARN at 30 s,
  poll 4 s); one TCP connection, one exchange at a time (command + `\r`, reply up to the `>`
  prompt); `apply` passes over a property that fails on its own and sends the rest, returning
  `OpError::Partial` naming only the failed ones (each property is its own CoolMaster command,
  confirmed or refused alone, nothing rolled back), and a lost link ends it; each command is read
  back (`ls2 <unit>`) so State follows at once; a unit a full listing no longer names has its
  retained State retracted (`Reporter::remove`).
- `src/main.rs` — the runtime's command line plus `--coolmaster` (a driver option, made required
  with `Bridge::require`; an address it cannot use is `Bridge::usage_error`, said by `usage_exit`).

The runtime calls the driver one operation at a time, each under its deadline (an overrun is a lost
link), so the driver has no timeouts of its own.

**Error classes** (spec §4.3): an error status, an out-of-vocabulary value and a target that is not
a unit address are `Rejected`; a reply that is not text or has no status, and a listing with no
usable line, are `Unusable`; an unknown property or command is `Unsupported`; I/O, a closed
connection and EOF before the prompt are `Link`.

Built against the runtime after its R1 gate (tracing-init `feat/bridge-runtime` 67afa2d):
`execute` takes `Option<&str>` (`ResetFilter` with none is Rejected), `DriverInfo` is built with
`..DriverInfo::default()` (`restore` stays false), and `Bridge::run` installs the panic hook.

## Tests

- `tests/support/` — the harness: the real bridge in-process (`Bridge::start`) against
  `FakeBroker` and a stand-in CoolMaster on 127.0.0.1 (`tests/support/coolmaster.rs`: units that
  keep their state, answer rules, a down mode, and a **connection cap** of 50 — keep it: an uncapped
  stand-in once let a reconnect loop exhaust the Mac's ephemeral ports).
- `tests/driver.rs` — what the driver adds: Desired to commands (order, read-back), ResetFilter,
  a partly rejected write (Error for the refused property only), unusable replies, a lost link and
  a closed connection, a bad `ls2` line, a unit in failure, a unit leaving the listing.
- `tests/process.rs` — the binary: SIGTERM, and `--coolmaster` usage errors.
- **Never** the live broker (`localhost:1883` on the Mac is the house), the LAN, or a real
  CoolMaster. Several agents share this Mac: run timing-sensitive tests alone before concluding.
- Negative controls: `docs/v2-driver-controls.toml`, run from Store with
  `python3 scripts/negative-control.py --root ../Home/mqtt_ac-v2 ../Home/mqtt_ac-v2/docs/v2-driver-controls.toml`.
  Every row must come back RED. Fixes go in failing-first.
- Every wait carries a `// WAIT: <row>` tag naming a row of `docs/wait-registry.md`, which
  `tests/wait_registry.rs` checks; regenerate the waiters block with wait-lint after a change.

## Logging

The runtime logs with the fleet's kinds (Store `docs/guides/logging-policy.md`): the broker and
device outage episodes (`connection_lost`, `external_failure`, `external_recovered`,
`device_connection_lost`, `device_unreachable`, `device_recovered`), `command_refused`,
`command_rejected` (WARN: the CoolMaster refused a command — fix the unit id or the value),
`validation_rejected`, `worker_died`, `shutdown_timeout`, and the rest of its own. The driver adds
only an INFO when a unit's `ls2` line first cannot be used (DEBUG while it stays so, INFO when it is
usable again) and an INFO when a unit leaves the listing (its State retracted). The log file is `logs/mqtt_ac.*` (the
runtime's file prefix is the application name); `logging.toml` adds GELF and OpenTelemetry.

## Not done (deliberately, for later)

- **`temp` sends the Store's number as is.** A CoolMaster set to °F would read a °C setpoint as °F
  (v1 the same); `ls2`'s °F readings are converted to °C.

`failure_code` is a string (the code as the CoolMaster prints it: `"A3"`, `"12"`) or null for
`OK`. v1 took numbers only, so an alphanumeric code froze a unit's State while it was in failure;
never narrow it back.
