//! Owned trace schema and reader for the workspace performance-auditing
//! mechanism.
//!
//! This crate has no workspace-internal dependencies: it is a leaf, meant to
//! be adopted by any crate that wants to emit or read a trace. Instrumentation
//! (the `enabled` feature) and the diff tool build on top of the types here in
//! later increments; this module only owns the record shapes and the reader
//! that turns a file back into them.

mod diff;
mod guard;
mod reader;
mod schema;

#[cfg(feature = "enabled")]
mod recorder;
#[cfg(feature = "enabled")]
mod tracing_layer;

pub use diff::{
    AdvisoryFinding, CompareError, DiffReport, SpanAggregate, SpanFinding, TraceAssemblyError,
    TraceSide, compare, render_text,
};
pub use guard::{Guard, Pending};
pub use reader::{TraceReadError, read_trace};
pub use schema::{
    FIELD_SCENARIO, FIELD_TERMINAL_SIZE, FIELD_TRANSCRIPT_LINES, Record, RunMetadata,
    SCHEMA_VERSION, SpanRecord,
};

#[cfg(feature = "enabled")]
pub use recorder::{PerfError, Recorder, RecorderConfig, TracePaths};

/// Re-exported so the [`span!`] and [`field!`] macros can reach `tracing`
/// through `$crate` from any crate that calls them, without requiring that
/// crate to declare `tracing` as its own dependency.
#[cfg(feature = "enabled")]
#[doc(hidden)]
pub use tracing;
