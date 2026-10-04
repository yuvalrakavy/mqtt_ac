# mqtt_ac v2 — the CoolMaster bridge

`mqtt_ac` bridges a CoolMaster (CoolMasterNet) air-conditioning controller to MQTT for the Store.
Version 2 is the CoolMaster's protocol as a `DeviceDriver` on the shared **Bridge Runtime**
(tracing-init `mqtt-bridge-kit`), speaking the **Bridge Kit** topic grammar.

> **v2 is incompatible with v1 on purpose.** The old home system keeps using v1: branch `v1`,
> tag `v0.2.1` (critical fixes only). v2 serves only the new Store. The two systems use separate
> brokers, so v2 keeps `Aircondition` as its default root. v1 and v2 cannot drive the same
> CoolMaster at once: a site moves from v1 to v2 as a cutover.

## The contract

The wire contract is the Bridge Kit's, owned by the Store:

- **The grammar, the lanes and the request rules:** Store
  `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md` §3 (topic grammar), §4 (the
  `Desired` lane) and §9 (this pilot); the mandated contract is Store
  `docs/guides/mqtt-bridge-contract.md`, "The CoolMaster pair".
- **What the bridge side does with them:** tracing-init
  `docs/superpowers/specs/2026-10-04-bridge-runtime-design.md` §4–§6 (the driver API, what the
  runtime owns, the wire details fixed while building it).

One process fronts one CoolMaster: the controller is the `{instance}`, an indoor unit's address
(`L7.400`) the `{target}`. `{Root}` is `Aircondition` unless `--root` says otherwise.

| Topic | Direction | Retained | Payload |
|---|---|---|---|
| `{Root}/Active/{controller}` | bridge → Store | yes; last will `false` | `true` / `false` (plain text) |
| `{Root}/Status/{controller}` | bridge → Store | yes | `connecting` · `connected` · `unreachable` (plain text): the TCP link to the CoolMaster |
| `{Root}/Version/{controller}` | bridge → Store | yes | `mqtt_ac 2.0.0` (plain text) |
| `{Root}/State/{controller}/{unit}` | bridge → Store | yes | the unit document (below) |
| `{Root}/Desired/{controller}/{unit}/{property}` | Store → bridge | yes; empty retracts | a JSON scalar: one property's requested value |
| `{Root}/Desired/{controller}/{unit}` | Store → bridge | never | `{property: value, …}`: several at once |
| `{Root}/Command/{controller}` | Store → bridge | never; 30 s expiry | `{"target": unit, "command": "ResetFilter"}`; the standard `refresh` and `assume` |
| `{Root}/Config/{controller}/{unit}` | Store → bridge | yes | accepted; the CoolMaster needs no setup per unit |
| `{Root}/Event/{controller}` | bridge → Store | no | replies to commands that ask for one (`"kind": "reply"`) |
| `{Root}/Error/{controller}` | bridge → Store | no | `{error, reason, target?, property?, command?}` |

QoS 1 everywhere. Requests carry the Store's `RequestExpiry` as their MQTT message expiry and its
`OutageConflict` as the user property `conflict` (a missing one is `DeviceWins`); the runtime holds a
request while the CoolMaster is down, drops it at its expiry, and decides an outage conflict against
what the unit was before.

### The unit document (`State`)

v1's field names and value strings, so the Store's driver maps them as it did:

```json
{"unit": "L7.400", "power": true, "target_temperature": 23.5, "temperature": 24.1,
 "fan_speed": "High", "operation_mode": "Heat", "failure_code": null,
 "filter_change": false, "demand": true}
```

| Field | Values |
|---|---|
| `unit` | the unit address (also the topic's `{unit}`) |
| `power` | `true` / `false` |
| `target_temperature`, `temperature` | numbers, °C (a CoolMaster reporting °F is converted) |
| `fan_speed` | `VLow` `Low` `Medium` `High` `Top` `Auto` |
| `operation_mode` | `Cool` `Heat` `Dry` `Fan` `Auto` |
| `failure_code` | `null` when the unit is `OK`; otherwise the code exactly as the CoolMaster prints it, a string (`"A3"`, `"U4"`, `"12"`) |
| `filter_change` | `true` when the filter wants changing |
| `demand` | `true` / `false` |

### What a request can set (`Desired`)

`power` (`true`/`false`), `operation_mode`, `fan_speed` and `target_temperature` (a number), with the
document's own vocabulary. A target-level write sends its properties together, in the order the
CoolMaster needs: **power on first**; then the mode, the setpoint and the fan speed (the mode decides
whether the others apply); **power off last**. Each is one CoolMaster command (`on`/`off`,
`cool`/`heat`/`dry`/`fan`/`auto`, `temp <unit> <t>`, `fspeed <unit> v|l|m|h|t|a`); the unit is read
back afterwards (`ls2 <unit>`), so its `State` follows at once. The bridge polls every unit every 4 s
(`ls2`).

### The command

`ResetFilter` (`filt <unit>`), momentary: refused while the CoolMaster is down, never retried.

### Errors

Every `Error` carries a `reason`:

| Reason | When |
|---|---|
| `rejected` | the CoolMaster answered a command with an error status (a setpoint its unit cannot take, an unknown unit); a value outside its property's vocabulary; a target that is not a unit address |
| `unusable` | the CoolMaster's reply could not be read (not text, no status) — reported and passed over, never a reason to reconnect |
| `unsupported` | a property no request can set, or a command the bridge does not have |
| `refused` | a momentary command while the CoolMaster is down |
| `superseded` | a unit changed at the wall during an outage won over a request made in the same outage |
| `link_lost` | the link failed while a command ran (it is not retried) |
| `malformed` | a payload or topic the bridge cannot read |

A property that fails on its own is passed over and the rest of the request still sent: a refused
setpoint does not cost a power-off. One `ls2` line that cannot be read costs only its own unit.

## Running it

```text
mqtt_ac --instance <controller> --broker <host[:port]> --coolmaster <host[:port]> [--root Aircondition]
```

- `--instance` — the controller's name: the `{instance}` of every topic.
- `--broker` — the MQTT broker (port 1883 by default).
- `--coolmaster` — the CoolMaster (port 10102 by default).
- `--root` — the topics' root (default `Aircondition`).
- The runtime's timing options: `--connect-timeout` (10 s), `--operation-timeout` (10 s), `--retry`
  (5 s), `--outage-warn` (30 s), `--poll` (4 s, or `off`), and the MQTT side's `--mqtt-*`.
  `--log-config <path>` names tracing-init's `logging.toml` (otherwise it is found upward from the
  working directory). `--help` lists them all.

On the Pi it runs under systemd (`ac.service`, made from `ac_Template.service` by `install_to_pi`).
SIGTERM and SIGINT stop it within its bound (`Active=false`, then DISCONNECT).

## Building and testing

```bash
cargo build                                   # the Pi: armv7-unknown-linux-musleabihf (.cargo/config.toml)
cargo test --target aarch64-apple-darwin      # on the Mac
cargo clippy --target aarch64-apple-darwin --all-targets
```

The tests run the real bridge against the fleet's `FakeBroker` and a stand-in CoolMaster on
127.0.0.1 — never the house broker, the LAN or a real CoolMaster. The runtime's own behaviour is
covered by its conformance suite; these cover the driver. Negative controls:
`docs/v2-driver-controls.toml`. Every wait is registered: `docs/wait-registry.md`.

## From v1 to v2

| v1 (old system) | v2 |
|---|---|
| `mqtt_ac <name> <mqtt> <coolmaster> [--polling n]` | `--instance`, `--broker`, `--coolmaster`, `--root`, the runtime's timing options |
| `Aircondition/` hard-coded | `--root` (default `Aircondition`) |
| `Aircondition/Coolmaster/{ctrl}` `true`/`false` | `Status/{ctrl}` `connected` / `connecting` / `unreachable` |
| `Command/{ctrl}`: `SetPower`, `TargetTemperature`, `SetMode`, `SetFanSpeed`, `ResetFilter` (`{"unit", "operation"}`) | `Desired/{ctrl}/{unit}[/{power \| target_temperature \| operation_mode \| fan_speed}]`; `Command` keeps only `ResetFilter` (`{"target", "command"}`) |
| `Error/{ctrl}` retained, a bare string | not retained, `{error, reason, target, property?, command?}` |
| a request while the CoolMaster is down: held only while the bridge runs | held to its expiry, also across a bridge restart (retained `Desired`), with the outage conflict rule |
| one bad `ls2` line failed the whole listing | it costs only its own unit |
| `failure_code` a number: an alphanumeric code (`A3`) made the line unusable | a string, exactly as the CoolMaster prints it, or `null` |
| `Version/{ctrl}` `mqtt_ac: 0.2.1 (built at …)` | `Version/{ctrl}` `mqtt_ac 2.0.0`, retained |
| its own pump, mailbox, reports and process code | the Bridge Runtime; what remains is the CoolMaster protocol as a `DeviceDriver` |
