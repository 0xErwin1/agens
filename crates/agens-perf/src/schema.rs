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
