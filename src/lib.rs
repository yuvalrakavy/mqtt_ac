//! mqtt_ac v2: the CoolMaster bridge on the Bridge Runtime (tracing-init `mqtt-bridge-kit`;
//! its spec is `docs/superpowers/specs/2026-10-04-bridge-runtime-design.md` there), speaking the
//! Bridge Kit grammar (Store `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md` §3, §4;
//! the CoolMaster pair in Store's `docs/guides/mqtt-bridge-contract.md`).
//!
//! The runtime owns everything that is policy: the topics, the MQTT pump and session, the device
//! mailbox, outages, reports, replies and the process. This crate keeps only the CoolMaster's
//! protocol ([`protocol`]) and the driver that speaks it over TCP ([`driver`]).
//!
//! v2 is incompatible with v1 on purpose: v1 (branch `v1`, tag `v0.2.1`) keeps serving the old
//! home system.

pub mod driver;
pub mod protocol;

pub use driver::{info, Address, Coolmaster};

/// The device family: the default root of every topic (`--root`).
pub const FAMILY: &str = "Aircondition";

/// The bridge's retained `Version` (plain text).
pub const VERSION: &str = concat!("mqtt_ac ", env!("CARGO_PKG_VERSION"));
