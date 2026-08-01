//! Owned trace schema and reader for the workspace performance-auditing
//! mechanism.
//!
//! This crate has no workspace-internal dependencies: it is a leaf, meant to
//! be adopted by any crate that wants to emit or read a trace. Instrumentation
//! (the `enabled` feature) and the diff tool build on top of the types here in
//! later increments; this module only owns the record shapes and the reader
//! that turns a file back into them.

mod diff;
mod reader;
mod schema;

pub use diff::{
    AdvisoryFinding, CompareError, DiffReport, SpanAggregate, SpanFinding, TraceAssemblyError,
    TraceSide, compare, render_text,
};
pub use reader::{TraceReadError, read_trace};
pub use schema::{Record, RunMetadata, SCHEMA_VERSION, SpanRecord};
