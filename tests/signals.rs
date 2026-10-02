//! The bridge process and its stop signals (Store no-hang 3b, finding C-7), against an in-process
//! fake broker — never the shared one — and a Coolmaster address on loopback that refuses.
//!
//! A service manager stops a process with SIGTERM (systemd's `systemctl stop`, and `Restart=`
//! cycles). The bridge must take it as it takes ctrl-c: stop its workers within its shutdown
//! bound and return from `main`, so tracing-init's guard is dropped and flushes the log. Killed by
//! the signal instead, it skips both, and the log loses whatever it had not written yet.

use std::io::{ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
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

/// A stream whose buffer is already full, as a supervisor's stdout that stopped draining: a write
/// to it waits until someone reads, and nobody does. Returns the full end, for the bridge, and the
/// other, to hold unread until the bridge has exited.
fn full_stream() -> (UnixStream, UnixStream) {
    let (full, unread) = UnixStream::pair().expect("a socket pair");
    full.set_nonblocking(true).expect("a non-blocking socket");
    // Large writes until it takes no more, then single bytes: a large write can be refused while
    // some room is left (the low-water mark), a single byte only when none is.
    for chunk in [&[b'x'; 4096][..], &b"x"[..]] {
        loop {
            match (&full).write(chunk) {
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling the socket: {e}"),
            }
        }
    }
    full.set_nonblocking(false).expect("a blocking socket again");
    (full, unread)
}

/// The bridge's stdout and stderr are a pipe its supervisor no longer drains (re-review B2): a
/// line the bridge writes there itself waits until someone reads, outside every bound, so its
/// lifecycle messages go through tracing-init's lossy sinks instead. The bridge must start —
/// subscribe — and stop on SIGTERM, cleanly, with both full and unread throughout. Printing its
/// logging summary to stdout itself, it never gets past its start, and SIGTERM cannot end it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stdout_and_stderr_nobody_reads_hold_neither_the_start_nor_the_stop() {
    let broker = FakeBroker::start().await;
    let dir = workdir("full_stdout");
    let (full, unread) = full_stream();
    let stdout = OwnedFd::from(full.try_clone().expect("a second handle on the full socket"));
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqtt_ac"))
        .args([NAME, broker.address().as_str(), refused_address().as_str()])
        .current_dir(&dir)
        .env("LOG_CONFIG", dir.join("logging.toml"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(OwnedFd::from(full)))
        .kill_on_drop(true)
        .spawn()
        .expect("start the bridge");

    let started = broker
        .wait_for_subscription(&format!("Aircondition/Command/{NAME}"), Duration::from_secs(10))
        .await;
    let pid = bridge.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill").args(["-TERM", &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "could not send SIGTERM to the bridge");
    let exited = match tokio::time::timeout(Duration::from_secs(15), bridge.wait()).await {
        Ok(status) => Some(status.expect("the bridge's exit status")),
        Err(_) => {
            let _ = bridge.start_kill();
            None
        }
    };
    // Unread until the bridge has exited.
    drop(unread);
    assert!(
        started,
        "the bridge never subscribed: its start waited on a stdout or stderr nobody reads"
    );
    assert!(
        exited.is_some_and(|status| status.success()),
        "the bridge did not stop cleanly on SIGTERM with a stdout and stderr nobody reads: {exited:?}"
    );
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
/// configuration and opens today's log file. Here that log file is a FIFO nobody reads, so
/// opening it for writing never returns — as on a stalled file system. (A FIFO as the
/// configuration does not stall it: tracing-init 97eebba takes a configuration only from a regular
/// file, and falls back to `logging.toml` past anything else.) tracing-init gives the open 5 s and
/// then starts without the file (`log_destination_skipped`), so the start ends by itself 5 s after
/// it began, and so no sooner than 5 s after the spawn. SIGTERM, sent 1 s in, must stop the bridge
/// cleanly before that:
/// - the stop signals are taken before the logging starts — taken after it, SIGTERM kills the
///   stalled bridge (signal 15);
/// - the start is raced against the stop — not raced, the bridge waits out the start;
/// - the runtime is shut down within its 1 s grace, abandoning the blocking thread still in the
///   start — dropped, it waits for that thread (and, in production, for a DNS lookup of the broker's
///   or the Coolmaster's name as long as the resolver takes).
///
/// The deadline is counted from the spawn, not from the signal: the bridge must be gone within
/// 4.5 s of it, which waiting out the start can never meet, however late the signal goes out — a
/// delay can fail the test, never pass a regression (re-review B3: counted from the signal, a
/// signal late by 2 s let a dropped runtime pass). And the test proves the start was still held
/// when the signal came: the bridge had not reached its service (no connection to the broker), and
/// its exit used up the runtime's grace (a thread was still in the start).
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
    let spawned = std::time::Instant::now();
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

    // Time to start, take the stop signals and reach the open that does not return.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let held = broker.connections() == 0;
    let pid = bridge.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill").args(["-TERM", &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "could not send SIGTERM to the bridge");
    let signalled = std::time::Instant::now();

    let status = match tokio::time::timeout(Duration::from_secs(15), bridge.wait()).await {
        Ok(status) => status.expect("the bridge's exit status"),
        Err(_) => {
            let _ = bridge.start_kill();
            panic!("the bridge did not exit within 15 s of SIGTERM while its logging start was stalled");
        }
    };
    let (since_spawn, since_signal) = (spawned.elapsed(), signalled.elapsed());
    // The FIFO stays unopened until the bridge is gone (it is removed with the directory next run).
    assert!(
        held && broker.connections() == 0,
        "the bridge reached its service, so its logging start was not held and this test proves nothing"
    );
    assert!(
        status.success() && since_spawn < Duration::from_millis(4500),
        "the bridge did not stop cleanly within its bound on SIGTERM while its logging start was stalled: {status}, \
         {since_spawn:?} after its spawn and {since_signal:?} after the signal (it must be gone within 4.5 s of its \
         spawn; waiting out the start takes at least 5 s)"
    );
    assert!(
        since_signal >= Duration::from_millis(900),
        "the bridge exited {since_signal:?} after the signal, before the runtime's 1 s grace ran out: no thread was \
         still in the logging start, so this test proves nothing"
    );
}
