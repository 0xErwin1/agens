//! Call counters that production code increments under `cfg(test)`.
//!
//! These live apart from `test_support` on purpose. Production functions call
//! into this module, so whatever holds the counters becomes a dependency of
//! every module that is instrumented. `test_support` carries fixtures that
//! reach into the TUI, the headless runtime and the dispatcher, so leaving the
//! counters there made those three a dependency of the modules they are built
//! from — a cycle that only exists at test-build time and that no crate split
//! can survive. This module depends on nothing.
#![cfg(test)]

thread_local! {
    static TUI_RESUME_LOAD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TUI_RESUME_PROJECTION_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_TOOL_RUNTIME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_PROVIDER_RUNTIME_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

pub(crate) fn note_tui_resume_load() {
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_tui_resume_projection() {
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_production_tool_runtime() {
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_production_provider_runtime() {
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn reset_tui_resume_test_counters() {
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(0));
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(0));
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(0));
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(0));
}

pub(crate) fn tui_resume_test_counters() -> (usize, usize, usize, usize) {
    (
        TUI_RESUME_LOAD_CALLS.with(std::cell::Cell::get),
        TUI_RESUME_PROJECTION_CALLS.with(std::cell::Cell::get),
        PRODUCTION_TOOL_RUNTIME_CALLS.with(std::cell::Cell::get),
        PRODUCTION_PROVIDER_RUNTIME_CALLS.with(std::cell::Cell::get),
    )
}
