
mod error;
mod messages;
mod ac_unit;
mod coolmaster;
mod service;
mod mqtt_publisher;
mod mqtt_pump;
mod mqtt_subscriber;
mod polling;
#[cfg(test)]
mod test_support;

use rustop::opts;
use service::ServiceConfig;

#[tokio::main]
async fn main() {
    let (args, _) = opts! {
        synopsis "MQTT Coolmaster (aircondition) Controller";
        param controller_name:String, desc: "Controller name";
        param mqtt:String, desc: "MQTT broker to connect";
        param coolmaster_address:String, desc: "Coolmaster address";
        opt polling: u16=4, desc: "Polling period (in seconds)";
    }.parse_or_exit();

    // Keep the guard for all of main: dropping it shuts down tracing-init's OpenTelemetry providers
    // (guard.rs), so spans and OTLP logs would stop right after startup.
    let d = tracing_init::TracingInit::builder("mqtt_ac")
        .log_to_file(true)
        .log_to_gelf_server(true)
        .file_prefix("ac")
        .file_path("logs")
        .init()
        .unwrap();

    println!("Logging: {d}");

    error_stack::Report::set_color_mode(error_stack::fmt::ColorMode::None);

    let config = ServiceConfig {
        controller_name: args.controller_name,
        mqtt_broker_address: args.mqtt,
        coolmaster_address: args.coolmaster_address,
        polling_period: tokio::time::Duration::from_secs(args.polling as u64),
    };

    let service = service::Service::new(config).start();

    tokio::signal::ctrl_c().await.unwrap(); // WAIT: ctrl-c
    // Stopping aborts the workers; the bound makes the whole shutdown finite whatever they do.
    // WAIT: shutdown
    if tokio::time::timeout(SHUTDOWN_GRACE, service.stop()).await.is_err() {
        tracing::error!(kind = "shutdown_overrun", grace_ms = SHUTDOWN_GRACE.as_millis() as u64, "Workers did not stop after being aborted; exiting anyway");
    }
}

/// How long the aborted workers get to end before the process exits regardless.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub fn get_version() -> String {
    format!("mqtt_ac: {} (built at {})", built_info::PKG_VERSION, built_info::BUILT_TIME_UTC)
}

// Include the generated-file as a separate module
pub mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}
