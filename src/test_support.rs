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
/// connections, as a broker that is down would.
pub struct Relay {
    pub address: String,
    links: Arc<Mutex<Vec<JoinHandle<()>>>>,
    refusing: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

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
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if refuse.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let upstream = upstream.clone();
                let link = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = server.set_nodelay(true);
                        let _ = client.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                held.lock().unwrap().push(link);
            }
        });
        Relay { address, links, refusing, task }
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
}

/// The units the stand-in Coolmaster lists.
pub const UNITS: [&str; 2] = ["L1.001", "L1.002"];

/// A Coolmaster stand-in on `127.0.0.1`: the `>` prompt, then one reply per `\r`-terminated
/// command. `ls2` lists every unit of `UNITS` and `ls2 <unit>` that one; every other command
/// answers `OK`. By default the room temperature moves on every listing, so each one is a state
/// change, which the publisher publishes (it publishes only what changed); a steady one lists the
/// same state every time. While it is down it takes each connection and closes it at once, before
/// the prompt — a Coolmaster that cannot be reached — and the connections it held are cut. Its
/// listings can be garbled: answered `OK`, with lines no unit state parses from.
pub struct FakeCoolmaster {
    pub address: String,
    shared: Arc<CoolmasterShared>,
    task: JoinHandle<()>,
}

struct CoolmasterShared {
    up: AtomicBool,
    steady: bool,
    garbled: AtomicBool,
    listings: AtomicUsize,
    /// Connections served (taken while up).
    served: AtomicUsize,
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
            garbled: AtomicBool::new(false),
            listings: AtomicUsize::new(0),
            served: AtomicUsize::new(0),
            commands: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
        });
        let serving = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if !serving.up.load(Ordering::SeqCst) {
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

    /// Garble its listings from now on, or stop.
    pub fn garble_listings(&self, garbled: bool) {
        self.shared.garbled.store(garbled, Ordering::SeqCst);
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
        let reply = match command.strip_prefix("ls2") {
            Some(_) if shared.garbled.load(Ordering::SeqCst) => "#garbled#\r\nOK\r\n>".to_owned(),
            Some(unit) => {
                let n = shared.listings.fetch_add(1, Ordering::SeqCst);
                let room = if shared.steady { 25 } else { 10 + n % 50 };
                let unit = unit.trim();
                let lines: Vec<String> = UNITS
                    .iter()
                    .filter(|u| unit.is_empty() || **u == unit)
                    .map(|u| format!("{u} ON 22.0C {room}.0C High Cool OK - 0"))
                    .collect();
                format!("{}\r\nOK\r\n>", lines.join("\r\n"))
            }
            None => "OK\r\n>".to_owned(),
        };
        if wr.write_all(reply.as_bytes()).await.is_err() {
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
