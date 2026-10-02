//! The bridge process and its stop signals (Store no-hang 3b, finding C-7), against an in-process
//! fake broker — never the shared one — and a Coolmaster address on loopback that refuses.
//!
//! A service manager stops a process with SIGTERM (systemd's `systemctl stop`, and `Restart=`
//! cycles). The bridge must take it as it takes ctrl-c: stop its workers within its shutdown
//! bound and return from `main`, so tracing-init's guard is dropped and flushes the log. Killed by
//! the signal instead, it skips both, and the log loses whatever it had not written yet.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mqtt_test_broker::FakeBroker;

const NAME: &str = "Signals";
const BOUND: Duration = Duration::from_secs(20);

/// A working directory of the test's own, with a `logging.toml` that keeps the bridge's logs in a
/// file there: GELF goes to the discard port on loopback, and nothing to logmon.
fn workdir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's working directory");
    std::fs::write(
        dir.join("logging.toml"),
        "[logging]\non_destination_error = \"skip\"\n\n[logging.gelf]\naddress = \"127.0.0.1:9\"\n",
    )
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

/// A loopback address nothing listens on: the bridge's Coolmaster, unreachable.
fn refused_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let address = listener.local_addr().expect("its address").to_string();
    drop(listener);
    address
}

async fn stops_cleanly_on(signal: &str, dir_name: &str) {
    let broker = FakeBroker::start().await;
    let dir = workdir(dir_name);
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac"))
        .args([NAME, broker.address().as_str(), refused_address().as_str()])
        .current_dir(&dir)
        .env("LOG_CONFIG", dir.join("logging.toml"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the bridge");

    assert!(
        broker
            .wait_for_subscription(&format!("Aircondition/Command/{NAME}"), BOUND)
            .await,
        "the bridge never subscribed"
    );

    let pid = bridge.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill")
        .args([signal, &pid])
        .status();
    assert!(
        sent.is_ok_and(|s| s.success()),
        "could not send {signal} to the bridge"
    );

    let status = match tokio::time::timeout(Duration::from_secs(15), bridge.wait()).await {
        Ok(status) => status.expect("the bridge's exit status"),
        Err(_) => {
            let _ = bridge.start_kill();
            panic!("the bridge did not exit within 15 s of {signal}");
        }
    };
    let log = logged(&dir);
    assert!(
        status.success(),
        "the bridge did not stop cleanly on {signal}: {status} (killed by the signal, it skips its shutdown and the \
         log's flush); the log:\n{log}"
    );
    assert!(
        log.contains("Service stopped"),
        "the bridge exited without its bounded shutdown (`Service stopped` is its last line before `main` returns, \
         dropping tracing-init's guard); the log:\n{log}"
    );
}

/// The failing-first case: a bridge that waits on ctrl-c alone is killed by SIGTERM.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_stops_the_bridge_cleanly() {
    stops_cleanly_on("-TERM", "sigterm").await;
}

/// The control: ctrl-c stopped the bridge cleanly before the fix too, and must still.
#[tokio::test(flavor = "multi_thread")]
async fn sigint_stops_the_bridge_cleanly() {
    stops_cleanly_on("-INT", "sigint").await;
}

/// The whole process stops within its bound, whatever a blocking thread is doing (re-review C6,
/// the fleet's F1 and F3). The bridge's file-system work is tracing-init's: it reads its logging
/// configuration and opens today's log file, synchronously. Here that log file is a FIFO nobody
/// reads, so opening it for writing never returns — as on a stalled file system. SIGTERM must
/// still stop the bridge, cleanly and promptly: the stop signals are taken before anything else,
/// the logging starts off the runtime's workers, and the runtime is shut down with a bound instead
/// of dropped (dropping it waits for every blocking thread: here for good, and for a DNS lookup of
/// the broker's or the Coolmaster's name as long as the resolver takes). With the signals taken
/// after the logging start, SIGTERM kills the stalled bridge instead.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_a_stalled_logging_start_stops_the_bridge_within_its_bound() {
    let broker = FakeBroker::start().await;
    let dir = workdir("stalled_start");
    // tracing-appender's daily file: `{prefix}.{UTC date}.{suffix}`, in the bridge's `logs`.
    let today = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .expect("today's UTC date");
    let today = String::from_utf8_lossy(&today.stdout).trim().to_owned();
    std::fs::create_dir_all(dir.join("logs")).expect("the log directory");
    let fifo = dir.join("logs").join(format!("ac.{today}.log"));
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.is_ok_and(|s| s.success()), "could not make the FIFO");
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac"))
        .args([NAME, broker.address().as_str(), refused_address().as_str()])
        .current_dir(&dir)
        .env("LOG_CONFIG", dir.join("logging.toml"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the bridge");

    // Time to start and reach the read that never returns.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let pid = bridge.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill").args(["-TERM", &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "could not send SIGTERM to the bridge");

    let started = std::time::Instant::now();
    let status = match tokio::time::timeout(Duration::from_secs(15), bridge.wait()).await {
        Ok(status) => status.expect("the bridge's exit status"),
        Err(_) => {
            let _ = bridge.start_kill();
            panic!("the bridge did not exit within 15 s of SIGTERM while its logging start was stalled");
        }
    };
    let took = started.elapsed();
    assert!(
        status.success() && took < Duration::from_secs(10),
        "the bridge did not stop cleanly within its bound on SIGTERM while its logging start was stalled: {status} \
         after {took:?}"
    );
}
