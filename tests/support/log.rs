//! The tests' log capture: one global recorder (installed once, before a test's bridge starts)
//! that keeps every event carrying a `kind`, with its level and its `unit`. The driver logs on the
//! runtime's worker thread, so a per-thread capture would miss it; a test looks only at the kinds
//! and units its own scenario makes.

use std::sync::{Mutex, Once};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// One event with a `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub level: Level,
    pub kind: String,
    pub unit: Option<String>,
}

static LOGGED: Mutex<Vec<Logged>> = Mutex::new(Vec::new());

/// Install the recorder, once; later calls do nothing.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Recorder));
    });
}

/// The events of `kind` about `unit`, so far.
pub fn logged(kind: &str, unit: &str) -> Vec<Logged> {
    LOGGED.lock().unwrap().iter().filter(|l| l.kind == kind && l.unit.as_deref() == Some(unit)).cloned().collect()
}

struct Recorder;

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(kind) = fields.kind {
            LOGGED.lock().unwrap().push(Logged { level: *event.metadata().level(), kind, unit: fields.unit });
        }
    }
}

#[derive(Default)]
struct Fields {
    kind: Option<String>,
    unit: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl Fields {
    fn record(&mut self, field: &Field, value: String) {
        match field.name() {
            "kind" => self.kind = Some(value),
            "unit" => self.unit = Some(value),
            _ => {}
        }
    }
}
