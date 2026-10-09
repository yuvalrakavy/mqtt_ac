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
cargo build --release                                    # the Pi (armv7-unknown-linux-musleabihf, .cargo/config.toml)
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
  back (`ls2 <unit>`) so State follows at once. **A unit the CoolMaster stops listing has lost its
  power** (owner, 2026-10-09) and is never retracted: after `GONE_AFTER` (2) usable full listings
  omit it, its State says `"powered": false` (last values kept), one Event `unit_lost_power`; listed
  again, fresh State (`"powered": true` is in every listed document) and one Event
  `unit_power_restored` with `down_for_ms`.
- `src/main.rs` — the runtime's command line plus `--coolmaster` (a driver option, made required
  with `Bridge::require`; an address it cannot use is `Bridge::usage_error`, said by `usage_exit`).

The runtime calls the driver one operation at a time, each under its deadline (an overrun is a lost
link). The driver's one timeout of its own is the read-back after a command: within what the
operation has left (`main` hands it `--operation-timeout`), so it never replaces the command's
outcome; a read-back that fails or overruns drops the link and reports it lost (`LinkHandle`).

**Error classes** (spec §4.3): any status but `OK` (`ERROR: n`, or a firmware's words such as
`Unknown command`, as v1 took them), an out-of-vocabulary or out-of-range value, a setpoint for a
unit whose scale cannot be learnt, and a target that is not a unit address are `Rejected`; a reply
that is not text or is empty, and a listing with no usable line, are `Unusable`; an unknown property or command is
`Unsupported`; I/O, a closed connection, EOF before the prompt and a prompt or reply past
`MAX_REPLY` (64 KiB) are `Link`.

Built against the runtime after its R1 re-gates (tracing-init `feat/bridge-runtime` 3128c96):
`execute` takes `Option<&str>` (`ResetFilter` with none is Rejected), `DriverInfo` is built with
`..DriverInfo::default()`, and `Bridge::run` installs the panic hook.

## Tests

- `tests/support/` — the harness: the real bridge in-process (`Bridge::start`) against
  `FakeBroker` and a stand-in CoolMaster on 127.0.0.1 (`tests/support/coolmaster.rs`: units that
  keep their state, answer rules, a down mode, and a **connection cap** of 50 — keep it: an uncapped
  stand-in once let a reconnect loop exhaust the Mac's ephemeral ports).
- `tests/driver.rs` — what the driver adds: Desired to commands (order, read-back), ResetFilter,
  a partly rejected write (Error for the refused property only), unusable replies, a lost link (its
  connect attempts paced), a closed connection, a command stalled mid-exchange, an over-long reply,
  a bad `ls2` line, a unit in failure, a unit losing its power and getting it back (and a garbled
  address, an empty or unusable listing changing no unit), the read-back stalled or dead, a °F
  unit, and the Store's own request shape.
- `tests/process.rs` — the binary: SIGTERM, `--coolmaster` usage errors, and `main` handing the
  driver `--operation-timeout` (a stalled read-back with a 1 s bound stays within it).
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
an INFO when a unit's `ls2` line first cannot be used (DEBUG while it stays so, INFO when it is
usable again), one WARN `unit_lost_power` per power-loss episode, and an INFO
`unit_power_restored` (`down_for_ms`) when it ends. The log file is `logs/mqtt_ac.*` (the
runtime's file prefix is the application name); `logging.toml` adds GELF and OpenTelemetry.

## Not done (deliberately, for later)

- **A garble that still reads as an address** (`L7.4O1` for `L7.401`) cannot be told from a real
  unit (re-gate R2, accepted): listed twice in a row in place of the real one, it is published as a
  unit and the real one taken for unpowered, until the garble ends.

**Temperatures:** State is in °C. A unit the CoolMaster lists in °F is converted, and a setpoint
(asked in °C, 0–50) is sent to it in °F (`Scale`, from its last usable line). For a unit whose
scale is not known yet, `apply` reads it first (`ls2 <unit>`); a read that cannot tell refuses the
setpoint (Partial, Rejected, "scale unknown") — never a guess of °C. A setpoint goes at the
CoolMaster's step, 0.1°, with one decimal (`21.7`).

`failure_code` is a string (the code as the CoolMaster prints it: `"A3"`, `"12"`) or null for
`OK`. v1 took numbers only, so an alphanumeric code froze a unit's State while it was in failure;
never narrow it back.
