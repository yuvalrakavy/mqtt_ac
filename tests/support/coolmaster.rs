//! A CoolMaster stand-in on `127.0.0.1` (from v1's `test_support.rs`, now with units that keep
//! their state): the `>` prompt, then one reply per `\r`-terminated command, as a CoolMasterNet
//! answers. `ls2` lists every unit (`ls2 <unit>` one); `on`/`off`, the mode commands, `temp`,
//! `fspeed` and `filt` change a unit, so a listing after them shows the change. A `temp` outside
//! 10–35 °C is refused (`ERROR: 4`), as a CoolMaster refuses a setpoint its unit cannot take; an
//! unknown unit is `ERROR: 3`, an unknown command `ERROR: 1`. A command can be given another
//! answer (`answer`), and a unit's listing line replaced verbatim (`set_line`).
//!
//! While it is down it takes each connection and closes it at once, before the prompt — a
//! CoolMaster that cannot be reached — and the connections it held are cut.
//!
//! It serves at most `CONNECTION_CAP` connections (fewer with `cap_connections`) and closes the
//! rest at once, as while down. A bridge that reconnects in a loop is then caught after a few
//! connections: uncapped, one opened thousands a second, and their TIME_WAIT sockets used up the
//! machine's ephemeral ports — every process's on this host, for half a minute.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The connections the stand-in serves at most, by default.
pub const CONNECTION_CAP: usize = 50;

/// One indoor unit, as the stand-in keeps it: the words `ls2` shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub power: bool,
    pub setpoint: f64,
    pub room: f64,
    /// `VLow`, `Low`, `Med`, `High`, `Top` or `Auto`.
    pub fan: &'static str,
    /// `Cool`, `Heat`, `Dry`, `Fan` or `Auto`.
    pub mode: &'static str,
    /// The failure code as the CoolMaster prints it (`A3`, `12`); `None` lists `OK`.
    pub failure: Option<&'static str>,
    pub filter: bool,
    pub demand: bool,
}

impl Unit {
    fn line(&self, unit: &str) -> String {
        format!(
            "{unit} {} {:.1}C {:.1}C {} {} {} {} {}",
            if self.power { "ON" } else { "OFF" },
            self.setpoint,
            self.room,
            self.fan,
            self.mode,
            self.failure.unwrap_or("OK"),
            if self.filter { "#" } else { "-" },
            if self.demand { "1" } else { "0" },
        )
    }
}

/// The units a stand-in starts with: `L1.001` off, cooling; `L1.002` on, heating, its filter due.
pub fn units() -> BTreeMap<String, Unit> {
    BTreeMap::from([
        (
            "L1.001".to_owned(),
            Unit { power: false, setpoint: 22.0, room: 25.0, fan: "High", mode: "Cool", failure: None, filter: false, demand: false },
        ),
        (
            "L1.002".to_owned(),
            Unit { power: true, setpoint: 24.0, room: 21.5, fan: "Low", mode: "Heat", failure: None, filter: true, demand: true },
        ),
    ])
}

/// How the stand-in answers a command, in place of its own answer.
#[derive(Debug, Clone, Copy)]
pub enum Answer {
    /// `OK`, after a body that is not text: nothing the bridge can use, and not a refusal either.
    Unusable,
    /// An error status: the CoolMaster refused the command.
    Rejected,
    /// No answer: the connection is closed.
    Close,
    /// The usual answer, once the test lets it go (`open_gate`).
    Gated,
}

struct Rule {
    prefix: String,
    answer: Answer,
    remaining: Option<usize>,
}

struct Shared {
    up: AtomicBool,
    units: Mutex<BTreeMap<String, Unit>>,
    /// Units whose listing line is this, verbatim, in place of their state.
    lines: Mutex<BTreeMap<String, String>>,
    /// Commands answered otherwise, by the prefix they start with; the first match wins. A rule
    /// with a count answers that many commands, and is then gone.
    answers: Mutex<Vec<Rule>>,
    gate: tokio::sync::Semaphore,
    served: AtomicUsize,
    cap: AtomicUsize,
    commands: Mutex<Vec<String>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

/// See the module documentation.
pub struct FakeCoolmaster {
    pub address: String,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for FakeCoolmaster {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl FakeCoolmaster {
    pub async fn start() -> FakeCoolmaster {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake CoolMaster");
        let address = listener.local_addr().expect("the fake CoolMaster's address").to_string();
        let shared = Arc::new(Shared {
            up: AtomicBool::new(true),
            units: Mutex::new(units()),
            lines: Mutex::new(BTreeMap::new()),
            answers: Mutex::new(Vec::new()),
            gate: tokio::sync::Semaphore::new(0),
            served: AtomicUsize::new(0),
            cap: AtomicUsize::new(CONNECTION_CAP),
            commands: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
        });
        let serving = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let capped = serving.served.load(Ordering::SeqCst) >= serving.cap.load(Ordering::SeqCst);
                if capped || !serving.up.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                serving.served.fetch_add(1, Ordering::SeqCst);
                let connection = tokio::spawn(serve(stream, serving.clone()));
                serving.connections.lock().unwrap().push(connection);
            }
        });
        FakeCoolmaster { address, shared, task }
    }

    /// Bring the CoolMaster up, or take it down: its connections are cut and new ones closed.
    pub fn set_up(&self, up: bool) {
        self.shared.up.store(up, Ordering::SeqCst);
        if !up {
            self.cut();
        }
    }

    /// Take a unit off the CoolMaster's list (unbound at the controller); its state, to put back.
    pub fn remove_unit(&self, unit: &str) -> Option<Unit> {
        self.shared.units.lock().unwrap().remove(unit)
    }

    /// Put a unit on the CoolMaster's list.
    pub fn add_unit(&self, unit: &str, state: Unit) {
        self.shared.units.lock().unwrap().insert(unit.to_owned(), state);
    }

    /// Every command the CoolMaster has received, in order.
    pub fn commands(&self) -> Vec<String> {
        self.shared.commands.lock().unwrap().clone()
    }

    /// The commands received since the first `skip`.
    pub fn commands_since(&self, skip: usize) -> Vec<String> {
        self.commands().into_iter().skip(skip).collect()
    }

    /// A unit as the CoolMaster has it now.
    pub fn unit(&self, unit: &str) -> Option<Unit> {
        self.shared.units.lock().unwrap().get(unit).cloned()
    }

    /// Change a unit at the CoolMaster, as a person at the wall does.
    pub fn change(&self, unit: &str, change: impl FnOnce(&mut Unit)) {
        change(self.shared.units.lock().unwrap().get_mut(unit).expect("a unit of the stand-in"));
    }

    /// List `unit` as `line`, verbatim, in place of its state; `None` lists its state again.
    pub fn set_line(&self, unit: &str, line: Option<&str>) {
        let mut lines = self.shared.lines.lock().unwrap();
        match line {
            Some(line) => lines.insert(unit.to_owned(), line.to_owned()),
            None => lines.remove(unit),
        };
    }

    /// From now on, answer the commands that start with `prefix` (every one, for `""`) so.
    pub fn answer(&self, prefix: &str, answer: Answer) {
        self.shared.answers.lock().unwrap().push(Rule { prefix: prefix.to_owned(), answer, remaining: None });
    }

    /// Answer the next command that starts with `prefix` so, and the ones after it as before.
    pub fn answer_once(&self, prefix: &str, answer: Answer) {
        self.shared.answers.lock().unwrap().push(Rule { prefix: prefix.to_owned(), answer, remaining: Some(1) });
    }

    /// Let one `Answer::Gated` reply go.
    pub fn open_gate(&self) {
        self.shared.gate.add_permits(1);
    }

    /// Serve at most `cap` connections in all (`CONNECTION_CAP` by default).
    pub fn cap_connections(&self, cap: usize) {
        self.shared.cap.store(cap, Ordering::SeqCst);
    }

    /// How many connections it has served.
    pub fn served(&self) -> usize {
        self.shared.served.load(Ordering::SeqCst)
    }

    fn cut(&self) {
        for connection in self.shared.connections.lock().unwrap().drain(..) {
            connection.abort();
        }
    }
}

/// How to answer `command`: the first rule whose prefix it starts with, counted down.
fn answer_for(shared: &Shared, command: &str) -> Option<Answer> {
    let mut answers = shared.answers.lock().unwrap();
    let i = answers.iter().position(|rule| command.starts_with(rule.prefix.as_str()))?;
    let answer = answers[i].answer;
    if let Some(remaining) = &mut answers[i].remaining {
        *remaining -= 1;
        if *remaining == 0 {
            answers.remove(i);
        }
    }
    Some(answer)
}

/// The CoolMaster's own answer to `command`: a body and a status.
fn reply(shared: &Shared, command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    let mut units = shared.units.lock().unwrap();
    let ok = |body: String| if body.is_empty() { "OK\r\n".to_owned() } else { format!("{body}\r\nOK\r\n") };
    match words[..] {
        ["ls2"] | ["ls2", _] => {
            let lines = shared.lines.lock().unwrap();
            let wanted = words.get(1).copied();
            if wanted.is_some_and(|u| !units.contains_key(u)) {
                return "ERROR: 3\r\n".to_owned();
            }
            let listing: Vec<String> = units
                .iter()
                .filter(|(u, _)| wanted.is_none_or(|w| w == u.as_str()))
                .map(|(u, unit)| lines.get(u).cloned().unwrap_or_else(|| unit.line(u)))
                .collect();
            ok(listing.join("\r\n"))
        }
        [verb, unit, ref rest @ ..] => {
            let Some(state) = units.get_mut(unit) else {
                return "ERROR: 3\r\n".to_owned();
            };
            match (verb, rest) {
                ("on", []) => state.power = true,
                ("off", []) => state.power = false,
                ("cool", []) => state.mode = "Cool",
                ("heat", []) => state.mode = "Heat",
                ("dry", []) => state.mode = "Dry",
                ("fan", []) => state.mode = "Fan",
                ("auto", []) => state.mode = "Auto",
                ("filt", []) => state.filter = false,
                ("temp", [t]) => match t.parse::<f64>() {
                    Ok(t) if (10.0..=35.0).contains(&t) => state.setpoint = t,
                    _ => return "ERROR: 4\r\n".to_owned(),
                },
                ("fspeed", [speed]) => {
                    state.fan = match *speed {
                        "v" => "VLow",
                        "l" => "Low",
                        "m" => "Med",
                        "h" => "High",
                        "t" => "Top",
                        "a" => "Auto",
                        _ => return "ERROR: 4\r\n".to_owned(),
                    }
                }
                _ => return "ERROR: 1\r\n".to_owned(),
            }
            ok(String::new())
        }
        _ => "ERROR: 1\r\n".to_owned(),
    }
}

async fn serve(stream: TcpStream, shared: Arc<Shared>) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    if wr.write_all(b">").await.is_err() {
        return;
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        match rd.read_until(b'\r', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = String::from_utf8_lossy(&line).trim().to_owned();
        shared.commands.lock().unwrap().push(command.clone());
        let answer = answer_for(&shared, &command);
        if let Some(Answer::Gated) = answer {
            match shared.gate.acquire().await {
                Ok(permit) => permit.forget(),
                Err(_) => return,
            }
        }
        let reply = match answer {
            Some(Answer::Close) => return,
            Some(Answer::Unusable) => b"\xff\xfe\r\nOK\r\n>".to_vec(),
            Some(Answer::Rejected) => b"ERROR: 1\r\n>".to_vec(),
            None | Some(Answer::Gated) => format!("{}>", reply(&shared, &command)).into_bytes(),
        };
        if wr.write_all(&reply).await.is_err() {
            return;
        }
    }
}
