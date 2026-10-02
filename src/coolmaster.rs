use error_stack::{Report, ResultExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};

use tracing::{debug, info, warn};

use crate::ac_unit::{self, UnitState};
use crate::error::CoolmasterError;
use crate::mailbox::{refusal_text, Mailbox, Refusal};
use crate::messages::ToCoolmasterMessage;
use crate::reports::Reports;

/// The bound on one exchange with the Coolmaster, as a whole: a connect and its prompt, or a
/// command — its write and its CR — and its reply (re-review C7: each step had its own, so a
/// command could take 30 s and a connect 20 s).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// One exchange with the Coolmaster, within `COMMAND_TIMEOUT` as a whole.
async fn timed<T>(
    exchange: impl std::future::Future<Output = Result<T, Report<CoolmasterError>>>,
    description: &str,
) -> Result<T, Report<CoolmasterError>> {
    // WAIT: coolmaster-io
    match timeout(COMMAND_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(CoolmasterError::Timeout(description.to_string()).into()),
    }
}

/// How the worker paces its attempts to reach the Coolmaster, and when an outage is a WARN.
#[derive(Debug, Clone, Copy)]
pub struct DeviceTiming {
    pub retry: Duration,
    pub warn_after: Duration,
}

impl Default for DeviceTiming {
    fn default() -> DeviceTiming {
        DeviceTiming {
            retry: Duration::from_secs(5),
            warn_after: Duration::from_secs(30),
        }
    }
}

/// The Coolmaster's reachability as the worker sees it. The worker outlives every connection, so
/// an outage is one episode however many attempts it takes (the logging policy, and the owner's
/// device-down policy): an INFO when it starts, one WARN once it has lasted `warn_after`, the
/// attempts in between counted at DEBUG, and an INFO with its length when the Coolmaster is back.
struct DeviceOutage {
    warn_after: Duration,
    current: Option<Episode>,
}

struct Episode {
    since: Instant,
    attempts: u32,
    warned: bool,
    /// The error last published on the error topic for this outage.
    reported: Option<String>,
}

impl DeviceOutage {
    fn new(warn_after: Duration) -> DeviceOutage {
        DeviceOutage {
            warn_after,
            current: None,
        }
    }

    /// The connection was lost, or an attempt to connect failed.
    fn failed(&mut self, error: &Report<CoolmasterError>) {
        let Some(episode) = &mut self.current else {
            self.current = Some(Episode {
                since: Instant::now(),
                attempts: 1,
                warned: false,
                reported: None,
            });
            info!(kind = "device_connection_lost", error = %error, "Coolmaster unreachable; retrying");
            return;
        };
        episode.attempts += 1;
        let down_for_ms = episode.since.elapsed().as_millis() as u64;
        if !episode.warned && episode.since.elapsed() >= self.warn_after {
            episode.warned = true;
            warn!(
                kind = "device_unreachable",
                error = %error,
                attempts = episode.attempts,
                down_for_ms,
                "Coolmaster unreachable; still retrying"
            );
        } else {
            debug!(error = %error, attempts = episode.attempts, down_for_ms, "Coolmaster still unreachable");
        }
    }

    /// An attempt's error, to publish on the error topic unless this outage already published it:
    /// the first attempt's, then only a change — not one per retry for as long as it lasts.
    fn unreported(&mut self, error: String) -> Option<String> {
        let Some(episode) = &mut self.current else {
            return Some(error);
        };
        if episode.reported.as_ref() == Some(&error) {
            return None;
        }
        episode.reported = Some(error.clone());
        Some(error)
    }

    /// Connected, with the state set meanwhile applied and the units' observed state published.
    fn recovered(&mut self, applied: usize, refused: u32) {
        match self.current.take() {
            Some(episode) => info!(
                kind = "device_recovered",
                down_for_ms = episode.since.elapsed().as_millis() as u64,
                attempts = episode.attempts,
                applied,
                refused,
                "Coolmaster reachable again"
            ),
            None => info!("Connected to coolmaster controller"),
        }
    }
}

/// Report, on the error topic, the momentary commands the mailbox refused when the connection
/// went down: they were waiting, and will not be sent.
fn report_refused(reports: &Reports, refused: Vec<ToCoolmasterMessage>) {
    for command in refused {
        reports.error(refusal_text(&command, Refusal::CoolmasterDown));
    }
}

/// A command the Coolmaster answered with an error status: the command failed, the connection
/// did not.
fn rejected(e: &Report<CoolmasterError>) -> Option<&str> {
    match e.downcast_ref::<CoolmasterError>() {
        Some(CoolmasterError::CoolmasterCommandError(status)) => Some(status),
        _ => None,
    }
}

/// A command failed: report it on the error topic, once. Returns whether the connection itself
/// failed — then it is lost, and the worker reconnects. Otherwise only the command failed, and it
/// is passed over on the same connection: the Coolmaster refused it (an unknown unit, say: a WARN,
/// someone has to fix it), or answered with something the bridge cannot use. Neither is retried:
/// the same command would fail the same way, and a worker that reconnected to retry it would do
/// so for good.
fn command_failed(command: &ToCoolmasterMessage, e: &Report<CoolmasterError>, reports: &Reports) -> bool {
    reports.error(format!("Failed to handle {command:#?} - {e}"));
    if connection_failed(e) {
        return true;
    }
    match rejected(e) {
        Some(status) => {
            warn!(kind = "command_rejected", error = %status, command = ?command, "Coolmaster command rejected")
        }
        None => debug!(error = %e, command = ?command, "Coolmaster reply unusable; command passed over"),
    }
    false
}

/// Whether the connection itself failed — an I/O error, a timeout, the Coolmaster closing it —
/// rather than the Coolmaster answering with something the bridge could not use.
fn connection_failed(e: &Report<CoolmasterError>) -> bool {
    e.frames()
        .filter_map(|frame| frame.downcast_ref::<CoolmasterError>())
        .any(|error| {
            matches!(
                error,
                CoolmasterError::IoError(_)
                    | CoolmasterError::Timeout(_)
                    | CoolmasterError::ConnectionClosed
                    | CoolmasterError::NotConnected
            )
        })
}

pub struct Coolmaster {
    stream: Option<BufReader<TcpStream>>,
}

#[allow(dead_code)]
impl Coolmaster {
    /// Owns the Coolmaster connection. It takes commands from the mailbox only while connected
    /// (the owner's device-down policy): on each connect it applies the state held while it was
    /// down, then reads and publishes every unit's observed state. Nothing waits on it — the
    /// mailbox never does — and it waits on nothing but the Coolmaster (under `COMMAND_TIMEOUT`),
    /// its mailbox and its pacing: its reports go to `Reports`, which never waits, so a stalled
    /// broker holds up none of its work (re-review C1). It connects at most once per
    /// `timing.retry`: a Coolmaster that takes connections and drops them is not reconnected to in
    /// a loop. The Coolmaster flag says `false` until the Coolmaster has answered, `true` once it
    /// has (the catch-up's commands and listing), and `false` again as soon as the connection is
    /// found gone, before the pacing (re-review C4).
    pub async fn coolmaster_worker(
        coolmaster_address: &str,
        mailbox: Arc<Mailbox>,
        reports: Arc<Reports>,
        timing: DeviceTiming,
    ) {
        let mut coolmaster = Coolmaster::new();
        let mut outage = DeviceOutage::new(timing.warn_after);
        let reports = &*reports;
        let mut connected_at: Option<Instant> = None;
        reports.coolmaster_connected(false);

        loop {
            // A connection lost soon after it was made: the next waits out the retry.
            if let Some(at) = connected_at {
                tokio::time::sleep_until(at + timing.retry).await;
            }

            while let Err(e) = coolmaster.connect_to(coolmaster_address).await {
                outage.failed(&e);
                report_refused(reports, mailbox.disconnected());
                if let Some(text) = outage.unreported(format!("{e:#?}")) {
                    reports.error(text);
                }
                tokio::time::sleep(timing.retry).await;
            }
            connected_at = Some(Instant::now());

            let refused = mailbox.connected();
            match coolmaster.catch_up(&mailbox, reports).await {
                Ok(applied) => {
                    // The Coolmaster has answered: now it is connected.
                    reports.coolmaster_connected(true);
                    outage.recovered(applied, refused);
                }
                Err(e) => {
                    coolmaster.lost(&e, &mailbox, reports, &mut outage);
                    continue;
                }
            }

            loop {
                // WAIT: coolmaster-mailbox
                let message = mailbox.take().await;
                if let Err(e) = coolmaster.handle_message(&message, reports).await {
                    if command_failed(&message, &e, reports) {
                        // A state is idempotent: what the lost connection did not apply waits for
                        // the next one.
                        mailbox.restore(vec![message]);
                        coolmaster.lost(&e, &mailbox, reports, &mut outage);
                        break;
                    }
                }
            }
        }
    }

    /// Just connected: apply the state held while the Coolmaster was down, then read and publish
    /// every unit's observed state, so the Store shows what is true. Returns how many held states
    /// were applied. The held states are taken one at a time and stay in the mailbox until then,
    /// so one replaced meanwhile by a newer value is never sent (re-review C3). An error is the
    /// connection failing; the state in flight goes back to the mailbox, held, ahead of the held
    /// states not yet taken (re-review C8). A command that fails on its own — refused, or answered
    /// with something the bridge cannot use, the listing included — is reported and passed over
    /// (`command_failed`), so it never keeps the worker from its mailbox.
    async fn catch_up(&mut self, mailbox: &Mailbox, reports: &Reports) -> Result<usize, Report<CoolmasterError>> {
        let mut applied = 0;
        while let Some(command) = mailbox.take_held() {
            match self.handle_message(&command, reports).await {
                Ok(()) => applied += 1,
                Err(e) => {
                    if command_failed(&command, &e, reports) {
                        mailbox.restore(vec![command]);
                        return Err(e);
                    }
                }
            }
        }
        let list = ToCoolmasterMessage::PublishUnitsState;
        if let Err(e) = self.handle_message(&list, reports).await {
            if command_failed(&list, &e, reports) {
                return Err(e);
            }
        }
        Ok(applied)
    }

    /// The connection failed: drop it, say so at once, and stop taking from the mailbox until the
    /// next connect.
    fn lost(&mut self, error: &Report<CoolmasterError>, mailbox: &Mailbox, reports: &Reports, outage: &mut DeviceOutage) {
        self.stream = None;
        reports.coolmaster_connected(false);
        outage.failed(error);
        report_refused(reports, mailbox.disconnected());
    }

    fn new() -> Self {
        Coolmaster { stream: None }
    }

    fn split_host_port(host_port: &str) -> Result<(String, u16), Report<CoolmasterError>> {
        let mut host_port_parts = host_port.split(':');

        let host = host_port_parts
            .next()
            .ok_or_else(|| CoolmasterError::InvalidCoolmasterAddress(host_port.to_string()))?;
        let port_string = host_port_parts.next().unwrap_or("10102");
        let port = port_string
            .parse::<u16>()
            .map_err(|_| CoolmasterError::InvalidCoolmasterPort(host_port.to_string()))?;

        Ok((host.to_string(), port))
    }

    async fn connect_to(&mut self, host: &str) -> Result<(), Report<CoolmasterError>> {
        let into_context =
            || CoolmasterError::Context(format!("Connecting to coolmaster controller at {host}"));
        let (host, port) = Coolmaster::split_host_port(host).change_context_lazy(into_context)?;

        let reader = timed(Coolmaster::open(&host, port), "connect and read the prompt")
            .await
            .change_context_lazy(into_context)?;
        self.stream = Some(reader);
        Ok(())
    }

    /// A TCP connection, up to the Coolmaster's initial `>` prompt (bounded by `connect_to`).
    async fn open(host: &str, port: u16) -> Result<BufReader<TcpStream>, Report<CoolmasterError>> {
        // WAIT: coolmaster-io
        let stream = TcpStream::connect(format!("{host}:{port}"))
            .await
            .map_err(CoolmasterError::IoError)?;
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        // WAIT: coolmaster-io
        let n = reader
            .read_until(b'>', &mut bytes)
            .await
            .map_err(CoolmasterError::IoError)?;
        if n == 0 {
            return Err(CoolmasterError::ConnectionClosed.into());
        }
        Ok(reader)
    }

    async fn handle_message(
        &mut self,
        message: &ToCoolmasterMessage,
        reports: &Reports,
    ) -> Result<(), Report<CoolmasterError>> {
        match message {
            ToCoolmasterMessage::SetUnitPower(unit, power) => {
                self.set_unit_power(unit, *power).await?
            }

            ToCoolmasterMessage::PublishUnitState(unit) => {
                let unit_state = self.get_unit_state(unit).await?;
                reports.observed([unit_state]);
            }

            ToCoolmasterMessage::PublishUnitsState => {
                let units_state = self.get_units_state().await?;
                reports.observed(units_state);
            }
            ToCoolmasterMessage::SetUnitMode(unit, mode) => {
                self.set_unit_mode(unit, mode.clone()).await?
            }
            ToCoolmasterMessage::SetFanSpeed(unit, fan_speed) => {
                self.set_unit_fan_speed(unit, fan_speed).await?
            }
            ToCoolmasterMessage::SetTargetTemperature(unit, temperature) => {
                self.set_unit_target_temperature(unit, *temperature).await?
            }
            ToCoolmasterMessage::ResetFilter(unit) => self.reset_filter(unit).await?,
        }

        Ok(())
    }

    // Commands

    async fn set_unit_power(&mut self, unit: &str, power: bool) -> Result<(), Report<CoolmasterError>> {
        let _ = match power {
            true => self.command(&format!("on {unit}")).await?,
            false => self.command(&format!("off {unit}")).await?,
        };
        Ok(())
    }

    async fn set_unit_mode(
        &mut self,
        unit: &str,
        mode: ac_unit::OperationMode,
    ) -> Result<(), Report<CoolmasterError>> {
        let _ = match mode {
            ac_unit::OperationMode::Cool => self.command(&format!("cool {unit}")).await?,
            ac_unit::OperationMode::Heat => self.command(&format!("heat {unit}")).await?,
            ac_unit::OperationMode::Dry => self.command(&format!("dry {unit}")).await?,
            ac_unit::OperationMode::Fan => self.command(&format!("fan {unit}")).await?,
            ac_unit::OperationMode::Auto => self.command(&format!("auto {unit}")).await?,
        };
        Ok(())
    }

    async fn set_unit_target_temperature(
        &mut self,
        unit: &str,
        temperature: f32,
    ) -> Result<(), Report<CoolmasterError>> {
        let _ = self
            .command(&format!("temp {unit} {temperature}"))
            .await?;
        Ok(())
    }

    async fn set_unit_fan_speed(
        &mut self,
        unit: &str,
        speed: &ac_unit::FanSpeed,
    ) -> Result<(), Report<CoolmasterError>> {
        let speed = match speed {
            ac_unit::FanSpeed::VLow => "v",
            ac_unit::FanSpeed::Low => "l",
            ac_unit::FanSpeed::Medium => "m",
            ac_unit::FanSpeed::High => "h",
            ac_unit::FanSpeed::Top => "t",
            ac_unit::FanSpeed::Auto => "a",
        };

        self.command(&format!("fspeed {unit} {speed}")).await?;
        Ok(())
    }

    async fn reset_filter(&mut self, unit: &str) -> Result<(), Report<CoolmasterError>> {
        let _ = self.command(&format!("filt {unit}")).await?;
        Ok(())
    }

    async fn get_unit_state(&mut self, unit: &str) -> Result<UnitState, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context(format!("Getting state for unit {unit}"));
        let reply = self.command(&format!("ls2 {unit}")).await?;

        UnitState::from_str(&reply).change_context_lazy(into_context)
    }

    async fn get_units_state(&mut self) -> Result<Vec<UnitState>, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context("Getting states for all units".to_string());
        let reply = self.command("ls2").await?;
        let states = reply
            .lines()
            .map(UnitState::from_str)
            .collect::<Result<Vec<UnitState>, Report<CoolmasterError>>>()
            .change_context_lazy(into_context)?;

        Ok(states)
    }

    // Lower level functions to communicate with coolmaster controller

    async fn send_to_coolmaster(&mut self, command: &str) -> Result<(), Report<CoolmasterError>> {
        let into_context =
            || CoolmasterError::Context(format!("Sending command to coolmaster: '{command}'"));
        let reader = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let stream = reader.get_mut();

        // WAIT: coolmaster-io
        stream
            .write_all(command.as_bytes())
            .await
            .map_err(CoolmasterError::IoError)
            .change_context_lazy(into_context)?;

        if !command.ends_with('\r') && !command.ends_with('\n') {
            // WAIT: coolmaster-io
            stream
                .write_all(b"\r")
                .await
                .map_err(CoolmasterError::IoError)
                .change_context_lazy(into_context)?;
        }

        Ok(())
    }

    async fn get_reply_from_coolmaster(&mut self) -> Result<String, Report<CoolmasterError>> {
        let into_context = || CoolmasterError::Context("Getting reply from coolmaster".to_string());
        let reader = self.stream.as_mut().ok_or(CoolmasterError::NotConnected)?;
        let mut bytes = Vec::new();

        // WAIT: coolmaster-io
        let n = reader
            .read_until(b'>', &mut bytes)
            .await
            .map_err(CoolmasterError::IoError)
            .change_context_lazy(into_context)?;

        if n == 0 {
            return Err(CoolmasterError::ConnectionClosed.into());
        }

        if let Some(last_byte) = bytes.last() {
            if *last_byte == b'>' {
                bytes.pop();
            }
        }

        let reply = String::from_utf8(bytes).change_context_lazy(into_context)?;
        Coolmaster::parse_reply(&reply)
    }

    /// A command and its reply, within `COMMAND_TIMEOUT` as a whole.
    async fn command(&mut self, command: &str) -> Result<String, Report<CoolmasterError>> {
        // WAIT: coolmaster-io
        timed(self.exchange(command), "send a command and read its reply").await
    }

    async fn exchange(&mut self, command: &str) -> Result<String, Report<CoolmasterError>> {
        self.send_to_coolmaster(command).await?;
        self.get_reply_from_coolmaster().await
    }

    fn parse_reply(reply: &str) -> Result<String, Report<CoolmasterError>> {
        let reply = reply.trim();
        let body_status_split = reply.rsplit_once("\r\n");

        match body_status_split {
            Some((body, status)) => Coolmaster::parse_status(body, status),
            None => Coolmaster::parse_status("", reply), // Reply body is empty, so the whole reply is the status
        }
    }

    fn parse_status(body: &str, status: &str) -> Result<String, Report<CoolmasterError>> {
        match status {
            "OK" | "ERROR: 0" => Ok(body.to_string()),
            _ => Err(CoolmasterError::CoolmasterCommandError(status.to_string()).into()),
        }
    }
}

#[cfg(test)]
mod worker_tests;

#[cfg(test)]
mod tests {
    const COOLMASTER_ADDRESS: &str = "10.0.1.70";

    fn set_logger() {
        _ = tracing_init::TracingInit::builder("mqtt_ac")
            .log_to_console(true)
            .init();
    }

    // Talks to the real Coolmaster, like the rest of this module's tests: run by hand only.
    #[tokio::test]
    #[ignore]
    async fn test_coolmaster_worker() {
        set_logger();

        let mailbox = crate::mailbox::Mailbox::new();
        let reports = crate::reports::Reports::new();

        let worker_mailbox = mailbox.clone();
        let worker_reports = reports.clone();
        let handle = tokio::spawn(async move {
            super::Coolmaster::coolmaster_worker(COOLMASTER_ADDRESS, worker_mailbox, worker_reports, Default::default())
                .await;
        });

        tokio::spawn(async move {
            loop {
                reports.reported().await;
                println!("MQTT message: {:?}", reports.take());
            }
        });

        mailbox.post(super::ToCoolmasterMessage::PublishUnitsState);
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        handle.abort();
        println!("Test done");
    }

    #[tokio::test]
    #[ignore]
    async fn test_split_host_port() {
        let (host, port) = super::Coolmaster::split_host_port("10.0.1.70").unwrap();
        assert_eq!(host, "10.0.1.70");
        assert_eq!(port, 10102);

        let (host, port) = super::Coolmaster::split_host_port("10.0.1.70:7777").unwrap();
        assert_eq!(host, "10.0.1.70");
        assert_eq!(port, 7777);
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("ls").await.unwrap();

        println!("Reply: {reply}");
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_bad_command_get_reply() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let reply = coolmaster.command("abracadabra").await;

        println!("Reply: {reply:?}");
        assert!(reply.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_send_command_empty_reply_body() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();

        let reply = coolmaster.command("on L7.400").await.unwrap();
        assert!(reply.is_empty());

        let reply = coolmaster.command("off L7.400").await.unwrap();
        assert!(reply.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_state() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let state = coolmaster.get_unit_state("L7.400").await.unwrap();

        println!("State: {state:?}");
        let state = coolmaster.get_unit_state("Invalid").await;
        assert!(state.is_err());
        print!("State: {state:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_unit_states() {
        let mut coolmaster = super::Coolmaster::new();
        coolmaster.connect_to(COOLMASTER_ADDRESS).await.unwrap();
        let states = coolmaster.get_units_state().await.unwrap();

        println!("States: {states:?}");
    }
}
