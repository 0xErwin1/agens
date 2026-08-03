//! The owned, versioned record shapes written to and read from a trace file.
//!
//! A trace is a line-oriented stream of JSON objects (JSONL): one run-metadata
//! record followed by any number of span records, in no particular order. The
//! `record` tag distinguishes the two without heuristics, and every consumer
//! must tolerate object keys it does not recognise so the schema can grow
//! additively without a version bump.

use serde::{Deserialize, Serialize};

/// Bumped only when a change to this file breaks an existing reader.
pub const SCHEMA_VERSION: u32 = 1;

/// Conventional key, inside [`RunMetadata::fields`], for the scenario name.
///
/// The comparator refuses to compare two traces whose value under this key
/// differs. A writer and a reader that spell this key differently would turn
/// that refusal into a silent no-op, so every writer and reader in this
/// crate reads and writes the scenario name through this constant rather
/// than a string literal.
pub const FIELD_SCENARIO: &str = "scenario";

/// Conventional key, inside [`RunMetadata::fields`], for the terminal size
/// the scenario ran at. See [`FIELD_SCENARIO`] for why this is a constant
/// rather than a string literal.
pub const FIELD_TERMINAL_SIZE: &str = "terminal_size";

/// Conventional key, inside [`RunMetadata::fields`], for the line count of
/// the transcript fixture the scenario ran against. See [`FIELD_SCENARIO`]
/// for why this is a constant rather than a string literal.
pub const FIELD_TRANSCRIPT_LINES: &str = "transcript_lines";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    Run(RunMetadata),
    Span(SpanRecord),
}

/// The single record that opens every trace file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMetadata {
    pub schema_version: u32,
    pub run_id: String,
    pub started_at_unix_ms: u64,
    pub commit: Option<String>,
    pub worktree_dirty: bool,
    pub host: Option<String>,
    pub debug_assertions: bool,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// One completed span. Hierarchy is expressed only through `parent_span_id`;
/// it never depends on file order.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpanRecord {
    pub span_id: u64,
    pub parent_span_id: Option<u64>,
    pub name: String,
    pub target: String,
    pub thread: u64,
    pub start_ns: u64,
    pub dur_ns: u64,
    pub fields: serde_json::Map<String, serde_json::Value>,
}
