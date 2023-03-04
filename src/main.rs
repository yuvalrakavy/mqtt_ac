
mod error;
mod messages;
mod ac_unit;
mod coolmaster;
mod service;
mod mqtt_publisher;
mod mqtt_subscriber;
mod polling;

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

    env_logger::init();

    let config = ServiceConfig {
        controller_name: args.controller_name,
        mqtt_broker_address: args.mqtt,
        coolmaster_address: args.coolmaster_address,
        polling_period: tokio::time::Duration::from_secs(args.polling as u64),
    };

    let service = service::Service::new(config);

    let service = service.start().await;

    tokio::signal::ctrl_c().await.unwrap();
    let _ = service.stop().await;
}

// fn set_logger() {
//     let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).filter_module("rumqttc", log::LevelFilter::Off).try_init();
// }
