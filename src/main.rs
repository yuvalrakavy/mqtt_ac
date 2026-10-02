
mod error;
mod messages;
mod ac_unit;
mod coolmaster;
mod mailbox;
mod reports;
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
use service::{Service, ServiceConfig, Started};
use tracing::{error, info, warn};
use tracing_init::types::OnDestinationError;
use tracing_init::TracingGuard;

fn main() -> ExitCode {
    let (args, _) = opts! {
        synopsis "MQTT Coolmaster (aircondition) Controller";
        param controller_name:String, desc: "Controller name";
        param mqtt:String, desc: "MQTT broker to connect";
        param coolmaster_address:String, desc: "Coolmaster address";
        opt polling: u16=4, desc: "Polling period (in seconds)";
    }.parse_or_exit();
    let config = ServiceConfig {
        controller_name: args.controller_name,
        mqtt_broker_address: args.mqtt,
        coolmaster_address: args.coolmaster_address,
        polling_period: Duration::from_secs(args.polling as u64),
        timing: Default::default(),
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("The async runtime cannot start ({e}); not starting");
            return ExitCode::FAILURE;
        }
    };
    // WAIT: runtime
    let exit = runtime.block_on(run(config));
    // The runtime is shut down with a bound, never dropped (re-review C6, the fleet's F1):
    // dropping it waits, without limit, for every thread still in a synchronous call — a DNS
    // lookup of the broker's or the Coolmaster's name (tokio and rumqttc resolve names on blocking
    // threads), tracing-init's start on a stalled file system. Past the bound they are abandoned,
    // and the process exits.
    shut_down(runtime);
    exit
}

/// How long the runtime's threads get, once `run` has returned, before the process exits anyway.
const RUNTIME_GRACE: Duration = Duration::from_secs(2);

/// Shut the runtime down within `RUNTIME_GRACE`, whatever its threads are doing.
fn shut_down(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_timeout(RUNTIME_GRACE);
}

/// The bridge, from its stop signals to its bounded shutdown. tracing-init's guard lives here and
/// is dropped when `run` returns, inside the runtime: dropping it flushes the log and shuts down
/// tracing-init's OpenTelemetry providers (guard.rs). `main` returns afterwards, never calling
/// `process::exit`, which would skip every drop.
async fn run(config: ServiceConfig) -> ExitCode {
    // The stop signals before anything else (the fleet's F3): from here on SIGINT and SIGTERM stop
    // the bridge through its bounded shutdown, whatever the start is doing. A failure to take them
    // is logged once the logging has started.
    let mut signals = StopSignals::register();

    // tracing-init's start reads its configuration and opens today's log file, synchronously: on a
    // blocking thread, raced against a stop, so a stalled file system holds the start, never the
    // stop (F1). The thread, if it never returns, is abandoned by `shut_down`.
    let starting = tokio::task::spawn_blocking(start_logging);
    let logging = match &mut signals {
        Ok(signals) => {
            // WAIT: logging-start
            tokio::select! {
                started = starting => started,
                signal = signals.next() => {
                    eprintln!("Stop requested ({signal}) before the logging started; stopping");
                    return ExitCode::SUCCESS;
                }
            }
        }
        Err(_) => {
            // WAIT: logging-start
            starting.await
        }
    };
    let _logging: Option<TracingGuard> = match logging {
        Ok(Ok(guard)) => {
            println!("Logging: {guard}");
            Some(guard)
        }
        Ok(Err(e)) => {
            eprintln!("Logging did not start ({e}); running without it");
            None
        }
        Err(e) => {
            eprintln!("Logging did not start ({e}); running without it");
            None
        }
    };

    error_stack::Report::set_color_mode(error_stack::fmt::ColorMode::None);

    let mut signals = match signals {
        Ok(signals) => signals,
        Err(e) => {
            warn!(kind = "signal_handler_unavailable", error = %e, "The stop signals cannot be handled; not starting");
            eprintln!("The stop signals cannot be handled ({e}); not starting");
            return ExitCode::FAILURE;
        }
    };

    let service = Service::new(config).start();
    match serve(service, signals.next()).await {
        Ending::Stopped => ExitCode::SUCCESS,
        Ending::WorkerDied => ExitCode::FAILURE,
    }
}

/// tracing-init's start, synchronous.
fn start_logging() -> Result<TracingGuard, String> {
    tracing_init::TracingInit::builder("mqtt_ac")
        .log_to_file(true)
        .log_to_gelf_server(true)
        .file_prefix("ac")
        .file_path("logs")
        // Telemetry never stops the bridge: a destination that fails is skipped whatever
        // logging.toml says, and if the logging cannot start at all the bridge runs without it.
        .on_destination_error(OnDestinationError::Skip)
        .init()
        .map_err(|e| e.to_string())
}

/// How the bridge ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// A stop signal.
    Stopped,
    /// A worker the bridge cannot run without ended; the service manager restarts the bridge.
    WorkerDied,
}

/// The started service, until a stop signal or the end of any of its workers — none of them ends
/// on its own, so one that does (a panic, an early return) ends the bridge, with a failure status
/// for the service manager to restart it (re-review C5) — then its bounded shutdown.
async fn serve(mut service: Service<Started>, stop: impl Future<Output = &'static str>) -> Ending {
    // WAIT: bridge-end
    let ending = tokio::select! {
        signal = stop => {
            info!(signal, "Stop requested; shutting down");
            Ending::Stopped
        }
        (worker, cause) = service.worker_ended() => {
            error!(
                kind = "worker_died",
                worker,
                error = %cause,
                "A worker of the bridge ended; shutting down, for the service manager to restart the bridge"
            );
            Ending::WorkerDied
        }
    };
    // Stopping aborts the workers; the bound makes the whole shutdown finite whatever they do.
    stop_within(SHUTDOWN_GRACE, service.stop()).await;
    ending
}

/// How long the aborted workers get to end before the process exits regardless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The shutdown, for at most `grace`. Past it, one WARN with the fleet's kind, `shutdown_timeout`,
/// and `run` returns anyway; `main` then shuts the runtime down within its own bound
/// (`RUNTIME_GRACE`), abandoning what is left. Returns whether it ended in time.
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
/// registered first thing in `run`, so from then on either is taken as a stop request rather than
/// killing the process: killed, the bridge skips its bounded shutdown and the log's flush.
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

    /// The next stop signal to arrive, by name. Cancellation-safe.
    #[cfg(unix)]
    async fn next(&mut self) -> &'static str {
        // WAIT: stop-signal
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }

    #[cfg(not(unix))]
    async fn next(&mut self) -> &'static str {
        // WAIT: stop-signal
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
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

    /// A worker the bridge cannot run without that ends — a panic, as `ac_unit` had on a
    /// multibyte temperature — ends the bridge (re-review C5): one ERROR `worker_died` naming it,
    /// the bounded shutdown, and a failure status, so the service manager restarts it. Unobserved,
    /// the bridge goes on without it for good, `Active` still true.
    #[tokio::test]
    async fn a_worker_that_dies_ends_the_bridge_with_a_failure() {
        let broker = mqtt_test_broker::FakeBroker::start().await;
        let coolmaster = crate::test_support::FakeCoolmaster::start().await;
        let log = Capture::start();
        let mut service = crate::service::Service::new(crate::service::ServiceConfig {
            controller_name: "Dies".to_owned(),
            mqtt_broker_address: broker.address(),
            coolmaster_address: coolmaster.address.clone(),
            polling_period: Duration::from_secs(3600),
            timing: Default::default(),
        })
        .start();
        service.spawn_worker("doomed", async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            panic!("the doomed worker's panic");
        });
        let never = std::future::pending::<&'static str>();
        let ended = tokio::time::timeout(Duration::from_secs(10), super::serve(service, never)).await;
        assert_eq!(
            ended,
            Ok(super::Ending::WorkerDied),
            "a worker died, and the bridge went on without it"
        );
        let errors = log.at_least(tracing::Level::ERROR);
        assert!(
            errors.len() == 1 && errors[0].kind == "worker_died" && errors[0].field("worker") == Some("doomed"),
            "the worker's death was not one ERROR `worker_died` naming it; the log: {:#?}",
            log.records()
        );
    }
}
