//! `mqtt_ac` v2: the CoolMaster bridge, on the Bridge Runtime.
//!
//! ```text
//! mqtt_ac --instance <controller> --broker <host[:port]> --coolmaster <host[:port]> [--root Aircondition] [timing options]
//! ```
//!
//! Everything but `--coolmaster` is the runtime's own command line (`--help` lists it all); the
//! runtime also owns the process — the stop signals, the logging start, the bounded shutdown and
//! the exit status. The only thing this binary says itself is a usage error of `--coolmaster`,
//! on stderr before anything has started (the command line's own output, as the runtime's
//! `--help` and usage errors are).

use std::process::ExitCode;

use mqtt_ac::{Address, Coolmaster, FAMILY, VERSION};
use mqtt_bridge_kit::config::{self, Settings};
use mqtt_bridge_kit::Bridge;

/// The driver's option: the CoolMaster's address.
const COOLMASTER: &str = "coolmaster";
const COOLMASTER_HELP: &str = "the CoolMaster, host[:port] (port 10102 by default)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bridge = Bridge::new(FAMILY).app_name("mqtt_ac").version(VERSION).option(COOLMASTER, COOLMASTER_HELP).args(args.clone());
    match bridge.value(COOLMASTER).map(Address::parse) {
        Some(Ok(address)) => bridge.run(Coolmaster::new(address)),
        // `--help`, or a usage error of the runtime's own options: the runtime says it, and no
        // driver starts (it is never called).
        _ if runtime_refuses(args) => bridge.run(Coolmaster::new(Address { host: String::new(), port: 0 })),
        Some(Err(problem)) => usage_error(&problem),
        None => usage_error("`--coolmaster` is required"),
    }
}

fn declared() -> [(String, String); 1] {
    [(COOLMASTER.to_owned(), COOLMASTER_HELP.to_owned())]
}

/// Whether the runtime itself refuses the command line (the runtime keeps its verdict private).
fn runtime_refuses(args: Vec<String>) -> bool {
    config::parse(args, &mut Settings::new(FAMILY), &declared()).is_err()
}

/// A usage error of `--coolmaster`, said as the runtime says its own: on stderr, status 2.
fn usage_error(problem: &str) -> ExitCode {
    let program = std::env::args_os()
        .next()
        .and_then(|a| std::path::Path::new(&a).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "mqtt_ac".to_owned());
    eprint!("{program}: {problem}\n\n{}", config::usage(&program, FAMILY, &declared()));
    ExitCode::from(2)
}
