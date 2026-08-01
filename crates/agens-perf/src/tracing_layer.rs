//! The `tracing_subscriber::Layer` that turns entered/exited spans into
//! [`SpanRecord`]s and writes them out as JSONL.
//!
//! Span identity is a load-bearing invariant here: `Registry` recycles a
//! `tracing::span::Id` once the span it named has closed, so persisting that
//! id would make `(span_id, parent_span_id)` ambiguous across a trace file —
//! two unrelated spans could end up sharing the same closed id. `JsonlLayer`
//! therefore mints its own monotonic id per span in `on_new_span`, stores it
//! in the span's extensions, and resolves every parent reference through
//! that store rather than through `tracing`'s own id.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::schema::{Record, SpanRecord};

thread_local! {
    static THREAD_ID: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(0);

/// A small, sequential, process-local thread identifier. `std::thread::ThreadId`
/// exposes no stable numeric conversion, so this crate mints its own,
/// matching the pattern `tracing-chrome` uses for the same reason.
fn thread_id() -> u64 {
    THREAD_ID.with(|cell| match cell.get() {
        Some(id) => id,
        None => {
            let id = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
            cell.set(Some(id));
            id
        }
    })
}

struct SpanState {
    id: u64,
    parent_id: Option<u64>,
    name: &'static str,
    target: &'static str,
    start_ns: u64,
    fields: serde_json::Map<String, serde_json::Value>,
}

struct FieldVisitor<'a> {
    fields: &'a mut serde_json::Map<String, serde_json::Value>,
}

impl Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::json!(format!("{value:?}")),
        );
    }
}

/// Writes one JSONL line per closed span. Holds the shared writer that the
/// run-metadata record is also written through, so the run record and every
/// span record land in the same file in a single, consistent stream.
pub(crate) struct JsonlLayer<W: io::Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    write_error: Arc<Mutex<Option<io::Error>>>,
    next_span_id: AtomicU64,
    epoch: Instant,
}

impl<W: io::Write + Send + 'static> JsonlLayer<W> {
    pub(crate) fn new(
        writer: Arc<Mutex<W>>,
        write_error: Arc<Mutex<Option<io::Error>>>,
        epoch: Instant,
    ) -> Self {
        Self {
            writer,
            write_error,
            next_span_id: AtomicU64::new(1),
            epoch,
        }
    }

    fn elapsed_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn write_record(&self, record: &Record) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                self.store_error(io::Error::new(io::ErrorKind::InvalidData, error));
                return;
            }
        };

        let mut writer = self.writer.lock().unwrap();
        if let Err(error) = writeln!(writer, "{line}") {
            drop(writer);
            self.store_error(error);
        }
    }

    fn store_error(&self, error: io::Error) {
        let mut slot = self.write_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

impl<S, W> Layer<S> for JsonlLayer<W>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: io::Write + Send + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(id) else {
            return;
        };

        let own_id = self.next_span_id.fetch_add(1, Ordering::Relaxed);
        let parent_id = span_ref
            .parent()
            .and_then(|parent| parent.extensions().get::<SpanState>().map(|state| state.id));

        let mut fields = serde_json::Map::new();
        attrs.record(&mut FieldVisitor {
            fields: &mut fields,
        });

        let state = SpanState {
            id: own_id,
            parent_id,
            name: span_ref.metadata().name(),
            target: span_ref.metadata().target(),
            start_ns: self.elapsed_ns(),
            fields,
        };

        span_ref.extensions_mut().insert(state);
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(id) else {
            return;
        };

        let mut extensions = span_ref.extensions_mut();
        if let Some(state) = extensions.get_mut::<SpanState>() {
            values.record(&mut FieldVisitor {
                fields: &mut state.fields,
            });
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(&id) else {
            return;
        };

        let now_ns = self.elapsed_ns();
        let extensions = span_ref.extensions();
        let Some(state) = extensions.get::<SpanState>() else {
            return;
        };

        let record = SpanRecord {
            span_id: state.id,
            parent_span_id: state.parent_id,
            name: state.name.to_string(),
            target: state.target.to_string(),
            thread: thread_id(),
            start_ns: state.start_ns,
            dur_ns: now_ns.saturating_sub(state.start_ns),
            fields: state.fields.clone(),
        };
        drop(extensions);

        self.write_record(&Record::Span(record));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use tracing_subscriber::layer::SubscriberExt;

    use super::JsonlLayer;
    use crate::schema::Record;

    fn recorded_lines(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<Record> {
        let contents = buffer.lock().unwrap();
        String::from_utf8_lossy(&contents)
            .lines()
            .map(|line| serde_json::from_str(line).expect("layer writes valid JSON per line"))
            .collect()
    }

    fn install_layer() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let write_error = Arc::new(Mutex::new(None));
        let layer = JsonlLayer::new(buffer.clone(), write_error, Instant::now());
        let subscriber = tracing_subscriber::registry().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        (buffer, guard)
    }

    #[test]
    fn nested_spans_record_their_parent_by_our_own_id_not_a_reused_tracing_id() {
        let (buffer, _guard) = install_layer();

        {
            let outer = tracing::info_span!("outer");
            let _outer_entered = outer.enter();
            {
                let inner = tracing::info_span!("inner");
                let _inner_entered = inner.enter();
            }
        }
        {
            let outer_again = tracing::info_span!("outer");
            let _entered = outer_again.enter();
        }

        let records = recorded_lines(&buffer);
        let spans: Vec<_> = records
            .into_iter()
            .map(|record| match record {
                Record::Span(span) => span,
                Record::Run(_) => panic!("no run record was written in this test"),
            })
            .collect();

        // Close order: `inner` closes first, then the first `outer`, then
        // the second `outer`, so this order is the order the layer wrote.
        assert_eq!(spans.len(), 3);
        let inner = &spans[0];
        let first_outer = &spans[1];
        let second_outer = &spans[2];

        assert_eq!(inner.name, "inner");
        assert_eq!(first_outer.name, "outer");
        assert_eq!(second_outer.name, "outer");

        assert_eq!(
            inner.parent_span_id,
            Some(first_outer.span_id),
            "inner's parent must resolve to the first outer span by our own id"
        );
        assert!(first_outer.parent_span_id.is_none());
        assert!(second_outer.parent_span_id.is_none());

        assert_ne!(
            first_outer.span_id, second_outer.span_id,
            "a closed span's id must never be reused for a later, unrelated span"
        );
    }

    #[test]
    fn a_field_recorded_after_open_reaches_the_written_record() {
        let (buffer, _guard) = install_layer();

        {
            let span = tracing::info_span!(
                "tui.transcript.settled_turn",
                cache_hit = tracing::field::Empty
            );
            let _entered = span.enter();
            span.record("cache_hit", false);
        }

        let records = recorded_lines(&buffer);
        let span = records
            .into_iter()
            .find_map(|record| match record {
                Record::Span(span) if span.name == "tui.transcript.settled_turn" => Some(span),
                _ => None,
            })
            .expect("the span was written");

        assert_eq!(
            span.fields.get("cache_hit"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}
