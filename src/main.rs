//! `mqtt_ac` v2: the CoolMaster bridge, on the Bridge Runtime.
//!
//! ```text
//! mqtt_ac --instance <controller> --broker <host[:port]> --coolmaster <host[:port]> [--root Aircondition] [timing options]
//! ```
//!
//! Everything but `--coolmaster` is the runtime's own command line (`--help` lists it all); the
//! runtime also owns the process — the stop signals, the logging start, the bounded shutdown and
//! the exit status — and says the command line's problems: `--coolmaster` is required
//! (`Bridge::require`), and an address it cannot use is a usage error (`Bridge::usage_error`),
//! both on stderr with the usage text, status 2, before any driver is built.

use std::process::ExitCode;

use mqtt_ac::{Address, Coolmaster, FAMILY, VERSION};
use mqtt_bridge_kit::Bridge;

/// The driver's option: the CoolMaster's address.
const COOLMASTER: &str = "coolmaster";
const COOLMASTER_HELP: &str = "the CoolMaster, host[:port] (port 10102 by default)";

fn main() -> ExitCode {
    let bridge =
        Bridge::new(FAMILY).app_name("mqtt_ac").version(VERSION).option(COOLMASTER, COOLMASTER_HELP).require(COOLMASTER).from_cli();
    let address = bridge.value(COOLMASTER).map(Address::parse);
    let bridge = match &address {
        Some(Err(problem)) => bridge.usage_error(problem.as_str()),
        _ => bridge,
    };
    if let Some(code) = bridge.usage_exit() {
        return code;
    }
    match address {
        Some(Ok(address)) => bridge.run(Coolmaster::new(address)),
        // Not reached: without the option, `require` made it a usage error, said above.
        _ => ExitCode::from(2),
    }
}
