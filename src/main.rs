
mod error;
mod messages;
mod ac_unit;
mod coolmaster;
mod service;
mod mqtt_publisher;
mod mqtt_subscriber;
mod polling;

use service::ServiceConfig;

#[tokio::main]
async fn main() {
    let config = ServiceConfig {
        mqtt_broker_address: String::from("control-bz"),
        controller_name: String::from("BZ"),
        coolmaster_address: String::from("10.0.1.70"),
        polling_period: tokio::time::Duration::from_secs(4),
    };

    set_logger();

    let service = service::Service::new(config);

    let service = service.start().await;

    tokio::signal::ctrl_c().await.unwrap();
    let _ = service.stop().await;
}

fn set_logger() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).filter_module("rumqttc", log::LevelFilter::Off).try_init();
}
