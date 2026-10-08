//! The bridge process: the real binary against `FakeBroker` and the stand-in CoolMaster on
//! 127.0.0.1 — never the house broker — with its logging in a directory of the test's own (GELF to
//! the discard port on loopback, nothing to logmon). The process itself (signals, the logging
//! start, the bounded end) is the runtime's, and its conformance suite covers it in depth; this
//! proves the binary is wired to it, and that its own command line says what is wrong.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use support::*;

const INSTANCE: &str = "proc";
const PROC_ROOT: &str = "AcProc";

/// A working directory of the test's own, with a `logging.toml` that keeps the bridge's logs in a
/// file there.
fn workdir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("process-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's working directory");
    std::fs::write(dir.join("logging.toml"), "[logging]\non_destination_error = \"skip\"\n\n[logging.gelf]\naddress = \"127.0.0.1:9\"\n")
        .expect("the test's logging.toml");
    dir
}

/// Everything the bridge wrote to its log files in `dir`.
fn logged(dir: &Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("logs")) {
        for entry in entries.flatten() {
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    text
}

/// The child's exit within `bound`, or `None` (it is then killed).
async fn exit_within(child: &mut tokio::process::Child, bound: Duration) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(bound, child.wait()).await {
        Ok(status) => Some(status.expect("the bridge's exit status")),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
    }
}

/// SIGTERM — `kill`, systemd — stops the bridge through the runtime's bounded shutdown:
/// `Active=false`, DISCONNECT, exit 0, the log flushed. Before it, the binary has done its job:
/// connected to the CoolMaster named by `--coolmaster` and published its units under `--root`.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_stops_the_bridge_cleanly() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(20);
    let dir = workdir("sigterm");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac"))
        .args(["--instance", INSTANCE, "--broker", &broker.address(), "--coolmaster", &coolmaster.address, "--root", PROC_ROOT])
        .arg("--log-config")
        .arg(dir.join("logging.toml"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the bridge");
    let active = format!("{PROC_ROOT}/Active/{INSTANCE}");
    let status = format!("{PROC_ROOT}/Status/{INSTANCE}");
    let version = format!("{PROC_ROOT}/Version/{INSTANCE}");
    let state = format!("{PROC_ROOT}/State/{INSTANCE}/L1.001");
    let working = broker
        .wait_until(BOUND, |b| {
            b.received_on(&active).iter().any(|r| &r.payload[..] == b"true")
                && b.received_on(&status).last().is_some_and(|r| &r.payload[..] == b"connected")
                && !b.received_on(&state).is_empty()
        })
        .await;
    assert!(working, "the bridge never connected and published its units; the log:\n{}", logged(&dir));
    let versions = broker.received_on(&version);
    assert_eq!(versions.last().map(|r| String::from_utf8_lossy(&r.payload).into_owned()).as_deref(), Some("mqtt_ac 2.0.0"));

    let pid = child.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill").args(["-TERM", &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "could not send SIGTERM to the bridge");
    let exited = exit_within(&mut child, Duration::from_secs(15)).await;
    let log = logged(&dir);
    assert!(exited.is_some_and(|s| s.success()), "the bridge did not stop cleanly on SIGTERM: {exited:?}; the log:\n{log}");
    assert!(log.contains("Bridge stopped"), "the bridge exited without its bounded shutdown; the log:\n{log}");
    let last_active = broker.received_on(&active).last().map(|r| r.payload.clone());
    assert_eq!(last_active.as_deref(), Some(&b"false"[..]), "the bridge did not say Active=false on its way out");
    assert!(broker.events().iter().any(|e| matches!(e, BrokerEvent::Disconnected)), "the bridge did not DISCONNECT");
}

/// The binary hands the driver the operation bound its command line set (`--operation-timeout`),
/// so a read-back keeps within it: with a 1 s bound and the read-back after a confirmed
/// `ResetFilter` stalled, the command stays confirmed (the stalled link is dropped and reconnected)
/// — never `link_lost` because the driver kept to its default 10 s.
#[tokio::test(flavor = "multi_thread")]
async fn the_binary_keeps_a_read_back_within_its_operation_timeout() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let coolmaster = FakeCoolmaster::start().await;
    coolmaster.cap_connections(20);
    coolmaster.answer_once("ls2 L1.002", Answer::Gated);
    let dir = workdir("operation_timeout");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac"))
        .args(["--instance", INSTANCE, "--broker", &broker.address(), "--coolmaster", &coolmaster.address, "--root", PROC_ROOT])
        .args(["--operation-timeout", "1s", "--poll", "off", "--retry", "300ms"])
        .arg("--log-config")
        .arg(dir.join("logging.toml"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the bridge");
    let state = format!("{PROC_ROOT}/State/{INSTANCE}/L1.002");
    let errors = format!("{PROC_ROOT}/Error/{INSTANCE}");
    assert!(broker.wait_until(BOUND, |b| !b.received_on(&state).is_empty()).await, "the bridge never published its units");
    let command = format!("{PROC_ROOT}/Command/{INSTANCE}");
    let payload = serde_json::json!({"target": "L1.002", "command": "ResetFilter"}).to_string();
    assert!(broker.send_with(&command, payload, &SendOptions::default().expiry(30)));
    let reconnected = broker_wait(&coolmaster, |c| c.served() == 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let errors = broker.received_on(&errors);
    let _ = child.start_kill();
    let _ = child.wait().await;
    assert!(reconnected, "the stalled read-back's link was not dropped and reconnected");
    assert!(
        errors.is_empty(),
        "a confirmed command was reported failed (the read-back outran the operation bound): {:?}",
        errors.iter().map(|r| String::from_utf8_lossy(&r.payload).into_owned()).collect::<Vec<_>>()
    );
    assert!(!coolmaster.unit("L1.002").unwrap().filter);
}

/// Wait until `ready` holds for the stand-in, up to `BOUND`.
async fn broker_wait(coolmaster: &FakeCoolmaster, ready: impl Fn(&FakeCoolmaster) -> bool) -> bool {
    let deadline = std::time::Instant::now() + BOUND;
    while std::time::Instant::now() < deadline {
        if ready(coolmaster) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ready(coolmaster)
}

async fn usage(args: &[&str]) -> std::process::Output {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac")).args(args).stdin(Stdio::null()).output();
    tokio::time::timeout(BOUND, output).await.expect("the bridge did not exit on a usage error").expect("run the bridge")
}

/// `--coolmaster` is required (`Bridge::require`), and must be an address (`Bridge::usage_error`):
/// otherwise a usage error on stderr — `mqtt_ac: <problem>`, then the usage text, the command
/// line's own output before anything has started — and status 2, as the runtime's own options say
/// theirs; a problem of the runtime's own options comes first. `--help` lists the option.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_bad_coolmaster_address_is_a_usage_error() {
    for (args, says) in [
        (&["--instance", "i", "--broker", "127.0.0.1:9"][..], "mqtt_ac: `--coolmaster` is required"),
        (&["--instance", "i", "--broker", "127.0.0.1:9", "--coolmaster", "host:port"][..], "mqtt_ac: `host:port`: `port` is not a port"),
        (&["--broker", "127.0.0.1:9", "--coolmaster", "host:port"][..], "mqtt_ac: `--instance` is required"),
    ] {
        let output = usage(args).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.starts_with(says), "{args:?} did not say `{says}` first: {stderr}");
        assert!(stderr.contains("Usage: mqtt_ac") && stderr.contains("--coolmaster <value>"), "{args:?}: no usage text: {stderr}");
        assert!(output.stdout.is_empty(), "{args:?}: a usage error wrote to stdout");
    }
    let help = usage(&["--help"]).await;
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("--coolmaster <value>") && text.contains("the CoolMaster, host[:port]") && text.contains("Aircondition"),
        "{text}"
    );
}
