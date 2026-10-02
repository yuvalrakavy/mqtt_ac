//! Test doubles shared by the bridge's tests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// A TCP relay between the bridge and the broker that a test can cut, as a network fault would:
/// the bridge sees its connection drop, and reconnects through the relay. It can also refuse
/// connections, as a broker that is down would, or drop each one a while after it is made, as a
/// broker that takes connections and loses them does.
///
/// It takes at most `RELAY_CAP` connections, refused or not, and then stops listening: a bridge
/// that reconnects in a loop is refused by the operating system from then on, and leaves no
/// TIME_WAIT socket behind for each attempt (an uncapped spin used up this host's ephemeral ports).
pub struct Relay {
    pub address: String,
    links: Arc<Mutex<Vec<JoinHandle<()>>>>,
    refusing: Arc<AtomicBool>,
    /// Each connection made from now on is dropped this long after it is made (ms; 0: never).
    drop_after_ms: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

/// The connections a relay takes at most (see `Relay`).
pub const RELAY_CAP: usize = 100;

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl Relay {
    pub async fn start(upstream: String) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the relay");
        let address = listener.local_addr().expect("the relay's address").to_string();
        let links = Arc::new(Mutex::new(Vec::new()));
        let held = links.clone();
        let refusing = Arc::new(AtomicBool::new(false));
        let refuse = refusing.clone();
        let drop_after_ms = Arc::new(AtomicUsize::new(0));
        let drop_after = drop_after_ms.clone();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counted = accepted.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if counted.fetch_add(1, Ordering::SeqCst) + 1 >= RELAY_CAP {
                    return; // the listener is dropped: refused from now on
                }
                if refuse.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let upstream = upstream.clone();
                let lifetime = match drop_after.load(Ordering::SeqCst) {
                    0 => None,
                    ms => Some(std::time::Duration::from_millis(ms as u64)),
                };
                let link = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = server.set_nodelay(true);
                        let _ = client.set_nodelay(true);
                        let relayed = tokio::io::copy_bidirectional(&mut client, &mut server);
                        match lifetime {
                            Some(lifetime) => {
                                let _ = tokio::time::timeout(lifetime, relayed).await;
                            }
                            None => {
                                let _ = relayed.await;
                            }
                        }
                    }
                });
                held.lock().unwrap().push(link);
            }
        });
        Relay { address, links, refusing, drop_after_ms, accepted, task }
    }

    /// Drop every open connection through the relay.
    pub fn cut(&self) {
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
    }

    /// Refuse every new connection from now on (a broker that is down), or take them again.
    pub fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::SeqCst);
    }

    /// Drop each connection made from now on `after` it is made (a broker that takes connections
    /// and loses them), or (`None`) keep them.
    pub fn drop_connections_after(&self, after: Option<std::time::Duration>) {
        let ms = after.map_or(0, |after| after.as_millis().max(1) as usize);
        self.drop_after_ms.store(ms, Ordering::SeqCst);
    }

    /// How many connections the relay has taken, refused or not.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// The units the stand-in Coolmaster lists.
pub const UNITS: [&str; 2] = ["L1.001", "L1.002"];

/// The connections a stand-in Coolmaster serves at most, by default (see `FakeCoolmaster`).
pub const CONNECTION_CAP: usize = 50;

/// A Coolmaster stand-in on `127.0.0.1`: the `>` prompt, then one reply per `\r`-terminated
/// command. `ls2` lists every unit of `UNITS` and `ls2 <unit>` that one; every other command
/// answers `OK`. By default the room temperature moves on every listing, so each one is a state
/// change, which the publisher publishes (it publishes only what changed); a steady one lists the
/// same state every time. While it is down it takes each connection and closes it at once, before
/// the prompt — a Coolmaster that cannot be reached — and the connections it held are cut. A
/// command can be given another answer (`answer`).
///
/// It serves at most `CONNECTION_CAP` connections (fewer with `cap_connections`) and closes the
/// rest at once, as while down. A worker that reconnects in a loop is then caught after a few
/// connections: uncapped, it opens thousands a second, and their TIME_WAIT sockets use up the
/// machine's ephemeral ports — every process's on this host, for half a minute.
pub struct FakeCoolmaster {
    pub address: String,
    shared: Arc<CoolmasterShared>,
    task: JoinHandle<()>,
}

struct CoolmasterShared {
    up: AtomicBool,
    steady: bool,
    /// Commands answered otherwise, by the prefix they start with; the first match wins. A rule
    /// with a count answers that many commands, and is then gone.
    answers: Mutex<Vec<Rule>>,
    /// Permits for `Answer::Gated` replies.
    gate: tokio::sync::Semaphore,
    listings: AtomicUsize,
    /// Connections served (taken while up, under the cap).
    served: AtomicUsize,
    cap: AtomicUsize,
    /// Every command the Coolmaster answered, in order.
    commands: Mutex<Vec<String>>,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for FakeCoolmaster {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl FakeCoolmaster {
    pub async fn start() -> FakeCoolmaster {
        FakeCoolmaster::start_with(false).await
    }

    /// A Coolmaster whose units never change.
    pub async fn start_steady() -> FakeCoolmaster {
        FakeCoolmaster::start_with(true).await
    }

    async fn start_with(steady: bool) -> FakeCoolmaster {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake Coolmaster");
        let address = listener
            .local_addr()
            .expect("the fake Coolmaster's address")
            .to_string();
        let shared = Arc::new(CoolmasterShared {
            up: AtomicBool::new(true),
            steady,
            answers: Mutex::new(Vec::new()),
            gate: tokio::sync::Semaphore::new(0),
            listings: AtomicUsize::new(0),
            served: AtomicUsize::new(0),
            cap: AtomicUsize::new(CONNECTION_CAP),
            commands: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
        });
        let serving = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let cap = serving.cap.load(Ordering::SeqCst);
                let capped = serving.served.load(Ordering::SeqCst) >= cap;
                if capped || !serving.up.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                serving.served.fetch_add(1, Ordering::SeqCst);
                let connection = tokio::spawn(serve_coolmaster(stream, serving.clone()));
                serving.connections.lock().unwrap().push(connection);
            }
        });
        FakeCoolmaster {
            address,
            shared,
            task,
        }
    }

    /// Bring the Coolmaster up, or take it down: its connections are cut and new ones closed.
    pub fn set_up(&self, up: bool) {
        self.shared.up.store(up, Ordering::SeqCst);
        if !up {
            self.cut();
        }
    }

    /// Every command the Coolmaster has answered, in order.
    pub fn commands(&self) -> Vec<String> {
        self.shared.commands.lock().unwrap().clone()
    }

    /// From now on, answer the commands that start with `prefix` (every one, for `""`) so.
    pub fn answer(&self, prefix: &str, answer: Answer) {
        let mut answers = self.shared.answers.lock().unwrap();
        answers.push(Rule {
            prefix: prefix.to_owned(),
            answer,
            remaining: None,
        });
    }

    /// Answer the next command that starts with `prefix` so, and the ones after it as before.
    pub fn answer_once(&self, prefix: &str, answer: Answer) {
        let mut answers = self.shared.answers.lock().unwrap();
        answers.push(Rule {
            prefix: prefix.to_owned(),
            answer,
            remaining: Some(1),
        });
    }

    /// Let one `Answer::Gated` reply go.
    pub fn open_gate(&self) {
        self.shared.gate.add_permits(1);
    }

    /// From now on, answer every command as a Coolmaster does.
    pub fn answer_normally(&self) {
        self.shared.answers.lock().unwrap().clear();
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

/// How the stand-in Coolmaster answers a command, in place of its own answer.
#[derive(Debug, Clone, Copy)]
pub enum Answer {
    /// `OK`, after a body that is not text (bytes that are not UTF-8): nothing the bridge can use,
    /// and not a refusal either.
    Unusable,
    /// An error status: the Coolmaster refused the command (an unknown unit, say).
    Rejected,
    /// No answer: the connection is closed.
    Close,
    /// The usual answer, once the test lets it go (`open_gate`): the command has been received
    /// (it is in `commands`) and the bridge is waiting for its reply.
    Gated,
}

struct Rule {
    prefix: String,
    answer: Answer,
    remaining: Option<usize>,
}

/// How to answer `command`: the first rule whose prefix it starts with, counted down.
fn answer_for(shared: &CoolmasterShared, command: &str) -> Option<Answer> {
    let mut answers = shared.answers.lock().unwrap();
    let i = answers
        .iter()
        .position(|rule| command.starts_with(rule.prefix.as_str()))?;
    let answer = answers[i].answer;
    if let Some(remaining) = &mut answers[i].remaining {
        *remaining -= 1;
        if *remaining == 0 {
            answers.remove(i);
        }
    }
    Some(answer)
}

async fn serve_coolmaster(stream: TcpStream, shared: Arc<CoolmasterShared>) {
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
        let reply = match (answer, command.strip_prefix("ls2")) {
            (Some(Answer::Close), _) => return,
            (Some(Answer::Unusable), _) => b"\xff\xfe\r\nOK\r\n>".to_vec(),
            (Some(Answer::Rejected), _) => b"ERROR: 1\r\n>".to_vec(),
            (None | Some(Answer::Gated), Some(unit)) => {
                let n = shared.listings.fetch_add(1, Ordering::SeqCst);
                let room = if shared.steady { 25 } else { 10 + n % 50 };
                let unit = unit.trim();
                let lines: Vec<String> = UNITS
                    .iter()
                    .filter(|u| unit.is_empty() || **u == unit)
                    .map(|u| format!("{u} ON 22.0C {room}.0C High Cool OK - 0"))
                    .collect();
                format!("{}\r\nOK\r\n>", lines.join("\r\n")).into_bytes()
            }
            (None | Some(Answer::Gated), None) => b"OK\r\n>".to_vec(),
        };
        if wr.write_all(&reply).await.is_err() {
            return;
        }
    }
}

/// One log event: its level, its `kind` field (empty when it has none), and every field (the
/// message as `message`), as text.
#[derive(Debug, Clone)]
pub struct Record {
    pub level: Level,
    pub kind: String,
    pub fields: BTreeMap<String, String>,
}

impl Record {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Where this thread's log events go while a test watches them.
type Sink = Rc<dyn Fn(Record)>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// The tests' recorder is the process's global subscriber, installed once; each test watches its
/// own thread's events through it (`watch`). A subscriber per test (`set_default`) lost events:
/// tracing caches each callsite's interest when the callsite is first hit, computed from the
/// subscribers registered at that moment — while only one is, from the hitting thread's own — so
/// a callsite first hit by another test's thread, as this test's subscriber was being registered,
/// cached "never" for good, and this test never saw that event. With one global subscriber the
/// cache can be stale only for a callsite hit while it was being installed: after a pause for any
/// such registration to finish, the cache is rebuilt, as it is on every `watch`.
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let recorder = tracing_subscriber::registry().with(Recorder);
        let _ = tracing::subscriber::set_global_default(recorder);
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    tracing_core::callsite::rebuild_interest_cache();
}

/// Hand every log event of this thread to `on_event`, as it is emitted, until the guard is
/// dropped. A test using it runs on a current-thread runtime (the `#[tokio::test]` default), where
/// every task it spawns runs on the test's thread too.
pub fn watch(on_event: impl Fn(Record) + 'static) -> Watch {
    install();
    SINK.with(|sink| *sink.borrow_mut() = Some(Rc::new(on_event)));
    Watch(())
}

/// Stops `watch` when dropped.
pub struct Watch(());

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = SINK.try_with(|sink| sink.borrow_mut().take());
    }
}

/// The log events of this thread from `start` until it is dropped (see `watch`).
pub struct Capture {
    records: Arc<Mutex<Vec<Record>>>,
    _watch: Watch,
}

impl Capture {
    pub fn start() -> Capture {
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink = records.clone();
        let _watch = watch(move |record| sink.lock().unwrap().push(record));
        Capture { records, _watch }
    }

    /// Every event so far, in order.
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    /// The events so far that carry `kind`.
    pub fn of_kind(&self, kind: &str) -> Vec<Record> {
        self.records()
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    /// The events so far at `level` or more severe (ERROR is the most severe).
    pub fn at_least(&self, level: Level) -> Vec<Record> {
        self.records()
            .into_iter()
            .filter(|r| r.level <= level)
            .collect()
    }
}

struct Recorder;

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let Some(sink) = SINK.try_with(|sink| sink.borrow().clone()).ok().flatten() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let kind = fields.0.get("kind").cloned().unwrap_or_default();
        sink(Record {
            level: *event.metadata().level(),
            kind,
            fields: fields.0,
        });
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}
