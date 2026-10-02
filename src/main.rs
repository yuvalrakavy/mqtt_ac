
mod error;
mod messages;
mod ac_unit;
mod coolmaster;
mod mailbox;
mod service;
mod mqtt_publisher;
mod mqtt_pump;
mod mqtt_subscriber;
mod polling;
#[cfg(test)]
mod test_support;

use std::future::Future;
use std::process::ExitCode;
use std::time::Duration;

use rustop::opts;
use service::ServiceConfig;
use tracing::{info, warn};
use tracing_init::types::OnDestinationError;

#[tokio::main]
async fn main() -> ExitCode {
    let (args, _) = opts! {
        synopsis "MQTT Coolmaster (aircondition) Controller";
        param controller_name:String, desc: "Controller name";
        param mqtt:String, desc: "MQTT broker to connect";
        param coolmaster_address:String, desc: "Coolmaster address";
        opt polling: u16=4, desc: "Polling period (in seconds)";
    }.parse_or_exit();

    // Keep the guard for all of main, and leave main by returning, never by `process::exit`:
    // dropping the guard flushes the log and shuts down tracing-init's OpenTelemetry providers
    // (guard.rs), and `process::exit` skips every drop.
    let logging = tracing_init::TracingInit::builder("mqtt_ac")
        .log_to_file(true)
        .log_to_gelf_server(true)
        .file_prefix("ac")
        .file_path("logs")
        // Telemetry never stops the bridge: a destination that fails is skipped whatever
        // logging.toml says, and if the logging cannot start at all the bridge runs without it.
        .on_destination_error(OnDestinationError::Skip)
        .init();
    match &logging {
        Ok(guard) => println!("Logging: {guard}"),
        Err(e) => eprintln!("Logging did not start ({e}); running without it"),
    }

    error_stack::Report::set_color_mode(error_stack::fmt::ColorMode::None);

    let stop = match StopSignals::register() {
        Ok(stop) => stop,
        Err(e) => {
            warn!(kind = "signal_handler_unavailable", error = %e, "The stop signals cannot be handled; not starting");
            eprintln!("The stop signals cannot be handled ({e}); not starting");
            return ExitCode::FAILURE;
        }
    };

    let config = ServiceConfig {
        controller_name: args.controller_name,
        mqtt_broker_address: args.mqtt,
        coolmaster_address: args.coolmaster_address,
        polling_period: Duration::from_secs(args.polling as u64),
        timing: Default::default(),
    };

    let service = service::Service::new(config).start();

    let signal = stop_requested(stop).await;
    info!(signal, "Stop requested; shutting down");
    // Stopping aborts the workers; the bound makes the whole shutdown finite whatever they do.
    stop_within(SHUTDOWN_GRACE, service.stop()).await;
    ExitCode::SUCCESS
}

/// How long the aborted workers get to end before the process exits regardless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The shutdown, for at most `grace`. Past it, one WARN with the fleet's kind, `shutdown_timeout`,
/// and `main` returns anyway: the runtime drops what is left. Returns whether it ended in time.
async fn stop_within<T>(grace: Duration, stop: impl Future<Output = T>) -> bool {
    // WAIT: shutdown
    let ended = tokio::time::timeout(grace, stop).await.is_ok();
    if !ended {
        let grace_ms = grace.as_millis() as u64;
        warn!(kind = "shutdown_timeout", grace_ms, "Workers did not stop within the shutdown bound; exiting anyway");
    }
    ended
}

/// What stops the bridge: ctrl-c (SIGINT), and SIGTERM, which `kill` and systemd send. Both are
/// registered at startup, so from then on either is taken as a stop request rather than killing
/// the process: killed, the bridge skips its bounded shutdown and the log's flush.
struct StopSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl StopSignals {
    #[cfg(unix)]
    fn register() -> std::io::Result<StopSignals> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(StopSignals {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(not(unix))]
    fn register() -> std::io::Result<StopSignals> {
        Ok(StopSignals {})
    }
}

/// The first stop signal to arrive, by name.
#[cfg(unix)]
async fn stop_requested(mut signals: StopSignals) -> &'static str {
    // WAIT: stop-signal
    tokio::select! {
        _ = signals.interrupt.recv() => "SIGINT",
        _ = signals.terminate.recv() => "SIGTERM",
    }
}

#[cfg(not(unix))]
async fn stop_requested(_signals: StopSignals) -> &'static str {
    // WAIT: stop-signal
    let _ = tokio::signal::ctrl_c().await;
    "ctrl-c"
}

pub fn get_version() -> String {
    format!("mqtt_ac: {} (built at {})", built_info::PKG_VERSION, built_info::BUILT_TIME_UTC)
}

// Include the generated-file as a separate module
pub mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::test_support::Capture;

    /// A shutdown that does not end within its bound is cut short, with one WARN of the fleet's
    /// kind (`shutdown_timeout`, not an ERROR: nothing in the code is wrong when a stop is slow).
    #[tokio::test]
    async fn a_shutdown_past_its_bound_is_cut_short_with_one_warn() {
        let log = Capture::start();
        let never = std::future::pending::<()>();
        let stop = super::stop_within(Duration::from_millis(50), never);
        let ended = tokio::time::timeout(Duration::from_secs(5), stop).await;
        assert_eq!(
            ended,
            Ok(false),
            "a shutdown that never ends was not cut short at its bound"
        );
        let warnings = log.at_least(tracing::Level::WARN);
        assert!(
            warnings.len() == 1
                && warnings[0].level == tracing::Level::WARN
                && warnings[0].kind == "shutdown_timeout"
                && warnings[0].field("grace_ms") == Some("50"),
            "the cut-short shutdown was not one WARN `shutdown_timeout` with its bound; the log: {:#?}",
            log.records()
        );
    }
}
