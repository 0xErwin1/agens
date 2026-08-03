//! Installs the process-global trace subscriber and writes the run-metadata
//! record that opens every trace file.
//!
//! Installation is global, not thread-local: the TUI event loop spawns
//! worker threads, and `tracing::subscriber::set_default` only applies to
//! the thread that calls it, which would silently drop every span opened on
//! another thread. `Recorder::finish` is explicit rather than relying on
//! `Drop`, because `Drop` cannot report an I/O failure and a trace that
//! silently failed to flush is worse than no trace at all.

use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing_subscriber::layer::SubscriberExt;

use crate::schema::{
    FIELD_SCENARIO, FIELD_TERMINAL_SIZE, FIELD_TRANSCRIPT_LINES, Record, RunMetadata,
};
use crate::tracing_layer::JsonlLayer;

const MAX_ENV_VALUE_BYTES: usize = 256;

#[derive(Debug)]
pub enum PerfError {
    /// A process-global trace subscriber was already installed; only one
    /// `Recorder` may be active per process.
    AlreadyInstalled,
    Io {
        context: String,
        source: io::Error,
    },
}

impl fmt::Display for PerfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInstalled => write!(
                formatter,
                "a performance-trace subscriber is already installed for this process"
            ),
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
        }
    }
}

impl std::error::Error for PerfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyInstalled => None,
        }
    }
}

/// Configuration for [`Recorder::install`]. `fields` is the open metadata
/// map written verbatim into the run record; use [`RecorderConfig::with_scenario`],
/// [`RecorderConfig::with_terminal_size`] and [`RecorderConfig::with_transcript_lines`]
/// to populate the keys the comparison tool reads, so a caller cannot typo
/// the key apart from what the reader expects.
pub struct RecorderConfig {
    pub directory: PathBuf,
    pub run_id: String,
    pub chrome_trace: bool,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl RecorderConfig {
    pub fn new(directory: impl Into<PathBuf>, run_id: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            run_id: run_id.into(),
            chrome_trace: false,
            fields: serde_json::Map::new(),
        }
    }

    pub fn with_chrome_trace(mut self, chrome_trace: bool) -> Self {
        self.chrome_trace = chrome_trace;
        self
    }

    pub fn with_scenario(mut self, scenario: impl Into<String>) -> Self {
        self.fields.insert(
            FIELD_SCENARIO.to_string(),
            serde_json::Value::String(scenario.into()),
        );
        self
    }

    pub fn with_terminal_size(mut self, columns: u16, rows: u16) -> Self {
        self.fields.insert(
            FIELD_TERMINAL_SIZE.to_string(),
            serde_json::json!({ "columns": columns, "rows": rows }),
        );
        self
    }

    pub fn with_transcript_lines(mut self, lines: u64) -> Self {
        self.fields
            .insert(FIELD_TRANSCRIPT_LINES.to_string(), serde_json::json!(lines));
        self
    }
}

/// The files a finished [`Recorder`] wrote. `chrome` is `None` whenever the
/// secondary writer was disabled or failed to open — its absence never
/// implies the canonical trace at `jsonl` is incomplete.
pub struct TracePaths {
    pub jsonl: PathBuf,
    pub chrome: Option<PathBuf>,
}

/// Owns the process-global trace subscriber for the lifetime of a run.
pub struct Recorder {
    jsonl_path: PathBuf,
    chrome_path: Option<PathBuf>,
    writer: Arc<Mutex<BufWriter<File>>>,
    write_error: Arc<Mutex<Option<io::Error>>>,
    chrome_guard: Option<tracing_chrome::FlushGuard>,
}

impl Recorder {
    /// Installs the process-global subscriber and writes the run-metadata
    /// record. Fails if a subscriber is already installed for this process,
    /// or if the trace directory or canonical trace file cannot be created.
    pub fn install(config: RecorderConfig) -> Result<Self, PerfError> {
        std::fs::create_dir_all(&config.directory).map_err(|source| PerfError::Io {
            context: format!("creating trace directory {}", config.directory.display()),
            source,
        })?;

        let jsonl_path = config.directory.join("run.jsonl");
        let file = File::create(&jsonl_path).map_err(|source| PerfError::Io {
            context: format!("creating trace file {}", jsonl_path.display()),
            source,
        })?;
        let writer = Arc::new(Mutex::new(BufWriter::new(file)));
        let write_error = Arc::new(Mutex::new(None));

        let metadata = build_run_metadata(&config, |key| std::env::var(key).ok());
        write_run_record(&writer, &write_error, metadata);

        let epoch = Instant::now();
        let jsonl_layer = JsonlLayer::new(writer.clone(), write_error.clone(), epoch);

        let (chrome_path, chrome_layer, chrome_guard) = if config.chrome_trace {
            build_chrome_layer(&config.directory)
        } else {
            (None, None, None)
        };

        let subscriber = tracing_subscriber::registry()
            .with(jsonl_layer)
            .with(chrome_layer);

        tracing::subscriber::set_global_default(subscriber)
            .map_err(|_| PerfError::AlreadyInstalled)?;

        Ok(Self {
            jsonl_path,
            chrome_path,
            writer,
            write_error,
            chrome_guard,
        })
    }

    /// Flushes and closes the trace files, returning the paths written.
    /// Reports the first I/O failure the canonical writer hit while the
    /// recorder was active, if any. The secondary chrome writer is severable:
    /// its own failures never surface here, only its absence from
    /// [`TracePaths::chrome`].
    pub fn finish(self) -> Result<TracePaths, PerfError> {
        drop(self.chrome_guard);

        if let Some(error) = self.write_error.lock().unwrap().take() {
            return Err(PerfError::Io {
                context: format!("writing trace file {}", self.jsonl_path.display()),
                source: error,
            });
        }

        self.writer
            .lock()
            .unwrap()
            .flush()
            .map_err(|source| PerfError::Io {
                context: format!("flushing trace file {}", self.jsonl_path.display()),
                source,
            })?;

        Ok(TracePaths {
            jsonl: self.jsonl_path,
            chrome: self.chrome_path,
        })
    }
}

fn write_run_record(
    writer: &Arc<Mutex<BufWriter<File>>>,
    write_error: &Arc<Mutex<Option<io::Error>>>,
    metadata: RunMetadata,
) {
    let line = serde_json::to_string(&Record::Run(metadata))
        .expect("RunMetadata always serializes to JSON");

    let mut guard = writer.lock().unwrap();
    if let Err(error) = writeln!(guard, "{line}") {
        drop(guard);
        *write_error.lock().unwrap() = Some(error);
    }
}

type ChromeLayer = tracing_chrome::ChromeLayer<
    tracing_subscriber::layer::Layered<JsonlLayer<BufWriter<File>>, tracing_subscriber::Registry>,
>;

/// Builds the secondary chrome-trace writer. Its failure to open the output
/// file is not fatal to the run: the canonical JSONL writer is unaffected,
/// and the caller simply gets back `None` for the chrome path.
fn build_chrome_layer(
    directory: &Path,
) -> (
    Option<PathBuf>,
    Option<ChromeLayer>,
    Option<tracing_chrome::FlushGuard>,
) {
    let path = directory.join("run.chrome.json");

    match File::create(&path) {
        Ok(file) => {
            let (layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
                .writer(file)
                .include_args(true)
                .build();
            (Some(path), Some(layer), Some(guard))
        }
        Err(error) => {
            eprintln!(
                "agens-perf: could not open secondary chrome trace at {}: {error} (canonical trace is unaffected)",
                path.display()
            );
            (None, None, None)
        }
    }
}

fn cap_at_max_bytes(value: String) -> String {
    if value.len() <= MAX_ENV_VALUE_BYTES {
        return value;
    }

    let mut end = MAX_ENV_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn parse_dirty_flag(value: Option<String>) -> bool {
    match value.as_deref().map(str::trim) {
        None | Some("") | Some("0") | Some("false") => false,
        Some(_) => true,
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds the run-metadata record. `commit`, `worktree_dirty` and `host` are
/// read as opaque, untrusted strings through `lookup` — `AGENS_PERF_COMMIT`,
/// `AGENS_PERF_DIRTY` and `AGENS_PERF_HOST` in the real environment — and
/// each string value is capped at 256 bytes. An unavailable commit becomes
/// an explicit `null`, not a failed run.
fn build_run_metadata(
    config: &RecorderConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> RunMetadata {
    build_run_metadata_from(config, config.run_id.clone(), lookup)
}

fn build_run_metadata_from(
    config: &RecorderConfig,
    run_id: String,
    lookup: impl Fn(&str) -> Option<String>,
) -> RunMetadata {
    let commit = lookup("AGENS_PERF_COMMIT").map(cap_at_max_bytes);
    let worktree_dirty = parse_dirty_flag(lookup("AGENS_PERF_DIRTY"));
    let host = lookup("AGENS_PERF_HOST").map(cap_at_max_bytes);

    RunMetadata {
        schema_version: crate::schema::SCHEMA_VERSION,
        run_id,
        started_at_unix_ms: unix_ms_now(),
        commit,
        worktree_dirty,
        host,
        debug_assertions: cfg!(debug_assertions),
        fields: config.fields.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::schema::SpanRecord;
    use crate::{CompareError, Record as SchemaRecord};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agens-perf-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn commit_and_dirty_flag_come_from_the_environment_and_are_length_capped() {
        let long_commit = "a".repeat(400);
        let config = RecorderConfig::new("/unused", "run-1");

        let metadata = build_run_metadata_from(&config, "run-1".to_string(), |key| match key {
            "AGENS_PERF_COMMIT" => Some(long_commit.clone()),
            "AGENS_PERF_DIRTY" => Some("1".to_string()),
            "AGENS_PERF_HOST" => Some("dev-box".to_string()),
            _ => None,
        });

        let commit = metadata.commit.expect("commit is present");
        assert_eq!(commit.len(), MAX_ENV_VALUE_BYTES);
        assert!(long_commit.starts_with(&commit));
        assert!(metadata.worktree_dirty);
        assert_eq!(metadata.host.as_deref(), Some("dev-box"));
    }

    #[test]
    fn commit_is_null_when_unavailable_and_the_run_does_not_fail() {
        let config = RecorderConfig::new("/unused", "run-2");

        let metadata = build_run_metadata_from(&config, "run-2".to_string(), |_| None);

        assert!(metadata.commit.is_none());
        assert!(!metadata.worktree_dirty);
        assert!(metadata.host.is_none());
    }

    fn minimal_root_span() -> SchemaRecord {
        SchemaRecord::Span(SpanRecord {
            span_id: 1,
            parent_span_id: None,
            name: "perf.scenario".to_string(),
            target: "agens_perf".to_string(),
            thread: 0,
            start_ns: 0,
            dur_ns: 1,
            fields: serde_json::Map::new(),
        })
    }

    #[test]
    fn a_differing_scenario_written_by_the_recorder_is_caught_by_the_comparator() {
        let base_config = RecorderConfig::new("/unused", "base").with_scenario("scenario_a");
        let new_config = RecorderConfig::new("/unused", "new").with_scenario("scenario_b");

        let base_metadata = build_run_metadata_from(&base_config, "base".to_string(), |_| None);
        let new_metadata = build_run_metadata_from(&new_config, "new".to_string(), |_| None);

        let base_records = vec![SchemaRecord::Run(base_metadata), minimal_root_span()];
        let new_records = vec![SchemaRecord::Run(new_metadata), minimal_root_span()];

        let error = crate::compare(base_records, new_records).expect_err(
            "the comparator must see the differing scenario the recorder wrote, proving the keys agree",
        );

        assert!(matches!(error, CompareError::ScenarioMismatch { .. }));
    }

    #[test]
    fn a_failing_secondary_writer_does_not_affect_the_canonical_trace_or_comparison() {
        let dir = unique_temp_dir("severability");
        std::fs::create_dir_all(&dir).expect("temp dir created");

        // Occupy the chrome trace file's path with a directory so opening it
        // as a file fails.
        std::fs::create_dir_all(dir.join("run.chrome.json")).expect("chrome path occupied");

        let config = RecorderConfig::new(&dir, "severability-run")
            .with_chrome_trace(true)
            .with_scenario("severability_check");

        let recorder = Recorder::install(config)
            .expect("the canonical writer installs even when the chrome writer cannot open");

        {
            let _root = crate::span!("perf.scenario");
        }

        let paths = recorder
            .finish()
            .expect("finish reports no error from the canonical writer");

        assert!(
            paths.chrome.is_none(),
            "the chrome path must be absent when the secondary writer failed to open"
        );

        let records =
            crate::read_trace(&paths.jsonl).expect("the canonical trace is still readable");
        assert!(
            records.iter().any(
                |record| matches!(record, SchemaRecord::Span(span) if span.name == "perf.scenario")
            ),
            "the canonical trace still contains the spans that were opened"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
