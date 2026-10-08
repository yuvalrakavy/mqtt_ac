# The wait registry

Every place mqtt_ac waits on something else — a lock, a channel, the CoolMaster, a dependency's
`async fn` — carries a tag naming a row here, on a line of its own directly above the statement (a
tag trailing an opening `{` is moved into the block by rustfmt, and lost):

```text
// WAIT: coolmaster-io
link.read_until(b'>', &mut reply).await.map_err(lost)?;
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

**The graph, in one paragraph.** v2 is a driver on the Bridge Runtime (tracing-init
`mqtt-bridge-kit`), which owns every other wait — the MQTT pump and session, the mailbox, the
reports, the worker, the process — and registers them in its own registry
(`mqtt-bridge-kit/docs/wait-registry.md`). What is left here is the driver's own I/O with the
CoolMaster. The runtime calls the driver from the device worker, on a thread of its own, one
operation at a time, each under a deadline (`device-op` there), and nothing the driver awaits
waits on the runtime: its reports go to the runtime's `Reports`, which never waits. So the driver
waits on nothing but the CoolMaster, and the runtime's deadline bounds that.

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `coolmaster-io` | bounded | the CoolMaster, over TCP: a connect and its first prompt, or a command's write and its reply up to the next prompt | The runtime's deadline on the driver operation it is part of: `connect` (10 s, `--connect-timeout`) for the connect and its prompt; every other operation (10 s, `--operation-timeout`) for an `ls2`, or for an `apply`'s commands and its read-back together. On expiry the runtime drops the operation's future, mid-exchange if need be, takes it for a lost link — it calls `disconnect`, which drops the connection, holds the state requests, refuses the momentary ones — and reconnects at most once per retry interval (5 s). Nothing in the driver waits on the runtime, so the bound is the whole of it. A name lookup of the CoolMaster's host runs on the worker runtime's blocking pool, and is abandoned with it at the deadline (Bridge Runtime spec §4.2, §5). |
| `coolmaster-read-back` | bounded | the read-back (`ls2 <unit>`) after a confirmed command or `apply` | What the operation has left of its bound (`--operation-timeout`, which `main` hands the driver), less a fifth of it, so the runtime's own deadline never falls inside it and the read-back can never replace the command's outcome. On expiry the exchange is cut midway: the driver drops the connection (its state unknown), reports the link lost through its `LinkHandle`, and returns the command's own outcome; the runtime then reconnects at its paced rate. With no time left, the read-back is skipped for the next poll. |

## Settings

```wait-lint
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
```

## Waiters (generated)

```wait-lint-waiters
coolmaster-io src/driver.rs <Coolmaster as DeviceDriver>::connect
coolmaster-io src/driver.rs Coolmaster::exchange_raw
coolmaster-read-back src/driver.rs Coolmaster::read_back
```
