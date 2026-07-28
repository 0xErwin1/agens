//! Call counters that let a test assert how often a cold path ran.
//!
//! Deliberately not behind `cfg(test)`. `cfg(test)` holds only for the crate
//! being compiled as a test target, never for its dependencies, so instrumenting
//! a library this way turns into a silent no-op the moment the instrumented code
//! and the assertion end up in different crates — and the assertion then passes
//! for the wrong reason rather than failing. These count construction of a
//! runtime and resumption of a session: cold paths where a thread-local
//! increment costs nothing measurable.

use std::cell::Cell;

thread_local! {
    static SESSION_RESUME_LOADS: Cell<usize> = const { Cell::new(0) };
    static SESSION_RESUME_PROJECTIONS: Cell<usize> = const { Cell::new(0) };
    static TOOL_RUNTIME_BUILDS: Cell<usize> = const { Cell::new(0) };
    static PROVIDER_RUNTIME_BUILDS: Cell<usize> = const { Cell::new(0) };
}

/// The four counters as `(resume loads, resume projections, tool runtimes,
/// provider runtimes)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts(pub usize, pub usize, pub usize, pub usize);

pub fn note_session_resume_load() {
    SESSION_RESUME_LOADS.with(|calls| calls.set(calls.get() + 1));
}

pub fn note_session_resume_projection() {
    SESSION_RESUME_PROJECTIONS.with(|calls| calls.set(calls.get() + 1));
}

pub fn note_tool_runtime_build() {
    TOOL_RUNTIME_BUILDS.with(|calls| calls.set(calls.get() + 1));
}

pub fn note_provider_runtime_build() {
    PROVIDER_RUNTIME_BUILDS.with(|calls| calls.set(calls.get() + 1));
}

pub fn reset() {
    SESSION_RESUME_LOADS.with(|calls| calls.set(0));
    SESSION_RESUME_PROJECTIONS.with(|calls| calls.set(0));
    TOOL_RUNTIME_BUILDS.with(|calls| calls.set(0));
    PROVIDER_RUNTIME_BUILDS.with(|calls| calls.set(0));
}

pub fn counts() -> Counts {
    Counts(
        SESSION_RESUME_LOADS.with(Cell::get),
        SESSION_RESUME_PROJECTIONS.with(Cell::get),
        TOOL_RUNTIME_BUILDS.with(Cell::get),
        PROVIDER_RUNTIME_BUILDS.with(Cell::get),
    )
}
