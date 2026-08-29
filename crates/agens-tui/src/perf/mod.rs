//! Performance-audit scenario harness.
//!
//! Lives inside this crate's lib, not in a `[[bin]]`, because the render-skip
//! gate it measures (`render_progress_frame`) and the frame schedule it
//! drives (`FrameSchedule`) are both private to this crate. A binary that
//! linked `agens-tui` as an external dependency would only see `pub` items
//! and would have to reimplement the gate to drive it at all — at which
//! point it would be measuring its own reimplementation, not the real thing.

pub mod fixtures;
mod scenarios;

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Duration;

use agens_core::TurnEvent;
use ratatui::backend::TestBackend;

use crate::{
    Action, Conversation, Engine, Event, FrameSchedule, RatatuiRenderer, Tui, TuiPresentation,
    TuiSubmissionOutcome, ViewState,
};

pub use scenarios::SCENARIOS;

/// The terminal the audit reports its numbers against. Scenarios that resize
/// start here and return here, so a resize is a departure from a known shape
/// rather than an arbitrary one.
pub const BASE_WIDTH: u16 = 120;
pub const BASE_HEIGHT: u16 = 40;

/// A named unit of the audit. Registering one requires only a new module and
/// one entry in [`SCENARIOS`] — no change to the harness, the writer, the
/// schema, or the comparison tool.
pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&mut ScenarioContext) -> io::Result<()>,
}

/// An engine handle with no provider or session logic of its own, standing
/// in for the real composition root so a scenario can drive [`Tui`] without
/// pulling in `agens-tui-app`.
struct PerfEngine;

impl Engine for PerfEngine {
    fn cancel(&mut self) {}
}

/// Everything one scenario needs to drive the real render-skip gate: the TUI
/// state machine, a `TestBackend` renderer, the frame schedule the real
/// event loop uses, and a synthetic clock so frame counts stay deterministic
/// while measured wall-clock time stays real.
pub struct ScenarioContext {
    tui: Tui<PerfEngine>,
    renderer: RatatuiRenderer<TestBackend>,
    schedule: FrameSchedule,
    now: Duration,
    width: u16,
    height: u16,
}

impl ScenarioContext {
    pub fn new(width: u16, height: u16) -> io::Result<Self> {
        let terminal = ratatui::Terminal::new(TestBackend::new(width, height))
            .expect("TestBackend never fails to construct a terminal");
        Ok(Self {
            tui: Tui::new(PerfEngine),
            renderer: RatatuiRenderer::new(terminal),
            schedule: FrameSchedule::default(),
            now: Duration::ZERO,
            width,
            height,
        })
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub fn apply_submission_outcome(&mut self, outcome: TuiSubmissionOutcome) {
        self.tui.apply_submission_outcome(outcome);
    }

    /// Advances the synthetic clock by one millisecond and drives one pass
    /// of the real render-skip gate, exactly as `run_with_default_progress_submit_with_permissions_and_task_controls`
    /// does per loop iteration.
    pub fn render_frame(&mut self, dirty: bool) -> io::Result<bool> {
        self.render_frame_after(Duration::from_millis(1), dirty)
    }

    /// Drives one pass of the gate after advancing the synthetic clock by
    /// `step`.
    ///
    /// The step is a scenario's only lever over time-driven work: the spinner
    /// and the elapsed-time counters redraw on clock movement alone, so a
    /// scenario that wants to measure them has to advance far enough for the
    /// animation to reach its next frame.
    pub fn render_frame_after(&mut self, step: Duration, dirty: bool) -> io::Result<bool> {
        self.now = self.now.saturating_add(step);

        crate::render_progress_frame(
            &mut self.tui,
            &mut self.renderer,
            &mut self.schedule,
            self.now,
            dirty,
        )
    }

    pub fn handle(&mut self, event: Event) -> Action {
        self.tui.handle(event)
    }

    pub fn apply_progress(&mut self, event: TurnEvent) {
        self.tui.apply_progress(event);
    }

    /// Marks the turn as in flight, which is what keeps the spinner and the
    /// elapsed counters alive across frames.
    pub fn set_running(&mut self, running: bool) {
        self.tui.set_running(running);
    }

    pub fn view(&self) -> ViewState<'_> {
        self.tui.view()
    }

    /// Resizes both the TUI state and the backing test terminal.
    ///
    /// Resizing only the TUI would leave the renderer drawing into a buffer
    /// of the old size, so the settled-conversation cache would be keyed on a
    /// width the frame never actually used.
    pub fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        self.width = width;
        self.height = height;

        self.renderer.terminal.backend_mut().resize(width, height);

        self.tui.handle(Event::Resize { width, height });

        Ok(())
    }

    /// Seats a fixture transcript as resumed session history.
    pub fn load_transcript(&mut self, messages: &[agens_core::Message]) -> io::Result<()> {
        let history = Conversation::from_messages(messages).map_err(|error| {
            io::Error::other(format!("fixture built an invalid conversation: {error:?}"))
        })?;

        self.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message: "Resumed session.".to_owned(),
            presentation: TuiPresentation::new("provider", "model", "session"),
            history,
            draft: None,
            staged_media: Vec::new(),
            resume_error: None,
            file_candidates: Vec::new(),
            palette_entries: Vec::new(),
            extension_notice: None,
        });

        Ok(())
    }
}

/// What one audit run produced.
///
/// `failed` is not an error: a scenario that panics is named and skipped so
/// the remaining scenarios still contribute their spans, and the caller
/// decides the exit status. A run that hid a panic to keep a clean exit code
/// would present a partial trace as a complete one.
pub struct AuditOutcome {
    pub paths: agens_perf::TracePaths,
    pub failed: Vec<&'static str>,
}

/// Runs every registered scenario against a single recorder.
///
/// The recorder is installed once for the whole run, never once per scenario:
/// installation is process-global and one-shot, and the event loop this
/// harness drives spawns worker threads that a thread-local subscriber would
/// silently drop. Scenario identity therefore travels as a field on each
/// scenario's root span.
pub fn run_all(trace_dir: &Path, run_id: &str) -> io::Result<AuditOutcome> {
    let config = agens_perf::RecorderConfig::new(trace_dir, run_id)
        .with_scenario("audit")
        .with_terminal_size(BASE_WIDTH, BASE_HEIGHT);

    let recorder = agens_perf::Recorder::install(config)
        .map_err(|error| io::Error::other(format!("could not install the recorder: {error}")))?;

    let failed = run_scenarios(SCENARIOS);

    let paths = recorder
        .finish()
        .map_err(|error| io::Error::other(format!("could not finish the trace: {error}")))?;

    Ok(AuditOutcome { paths, failed })
}

/// Runs each scenario in isolation and returns the names that did not finish.
///
/// A scenario that panics must not take the run down with it: the ones after
/// it still have spans worth recording, and a run that stopped at the first
/// panic would look identical to a run that measured less.
fn run_scenarios(scenarios: &[Scenario]) -> Vec<&'static str> {
    let mut failed = Vec::new();

    for scenario in scenarios {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut context = ScenarioContext::new(BASE_WIDTH, BASE_HEIGHT)?;
            (scenario.run)(&mut context)
        }));

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("scenario {} failed: {error}", scenario.name);
                failed.push(scenario.name);
            }
            Err(_) => {
                eprintln!("scenario {} panicked", scenario.name);
                failed.push(scenario.name);
            }
        }
    }

    failed
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agens_perf::{Record, Recorder, RecorderConfig};

    use super::SCENARIOS;

    #[test]
    fn a_panicking_scenario_is_named_without_stopping_the_ones_after_it() {
        fn healthy(_: &mut super::ScenarioContext) -> std::io::Result<()> {
            Ok(())
        }

        fn explodes(_: &mut super::ScenarioContext) -> std::io::Result<()> {
            panic!("this scenario is supposed to blow up");
        }

        let scenarios = [
            super::Scenario {
                name: "first_healthy",
                run: healthy,
            },
            super::Scenario {
                name: "explodes",
                run: explodes,
            },
            super::Scenario {
                name: "last_healthy",
                run: healthy,
            },
        ];

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let failed = super::run_scenarios(&scenarios);
        std::panic::set_hook(previous_hook);

        assert_eq!(
            failed,
            vec!["explodes"],
            "only the panicking scenario should be reported as failed"
        );
    }

    #[test]
    fn expand_collapse_does_not_move_the_scroll_offset() {
        use crate::perf::fixtures;
        use crate::{Event, Key, MouseWheelDirection};

        let mut ctx = super::ScenarioContext::new(super::BASE_WIDTH, super::BASE_HEIGHT)
            .expect("test backend always builds");

        let fixture = fixtures::transcript(40, 12);
        ctx.load_transcript(&fixture.messages)
            .expect("fixture is a valid conversation");
        ctx.render_frame(true).expect("first frame renders");

        for _ in 0..4 {
            ctx.handle(Event::MouseWheel(MouseWheelDirection::Up));
        }
        ctx.render_frame(true).expect("scrolled frame renders");

        let anchored = ctx.view().scroll_offset;
        assert!(
            !ctx.view().following_bottom,
            "the scroll test is meaningless while the view is pinned to the bottom"
        );

        for _ in 0..3 {
            ctx.handle(Event::Key(Key::CtrlT));
            ctx.render_frame(true).expect("toggled frame renders");

            assert_eq!(
                ctx.view().scroll_offset,
                anchored,
                "changing tool detail moved the ground under the reader"
            );
        }
    }

    #[test]
    fn long_transcript_measures_scroll_on_a_long_session() {
        let mut ctx = super::ScenarioContext::new(super::BASE_WIDTH, super::BASE_HEIGHT)
            .expect("test backend always builds");

        (SCENARIOS
            .iter()
            .find(|scenario| scenario.name == "long_transcript")
            .expect("long_transcript is registered")
            .run)(&mut ctx)
        .expect("scenario runs to completion");

        assert!(
            !ctx.view().following_bottom,
            "a long-session measurement that stays pinned to the bottom is still a single paint"
        );
    }

    #[test]
    fn a_resize_rebuilds_the_visible_window_not_the_session() {
        use crate::perf::fixtures;
        use crate::render::{
            reset_settled_conversation_test_state, settled_conversation_test_renders,
        };

        reset_settled_conversation_test_state();
        let mut ctx = super::ScenarioContext::new(super::BASE_WIDTH, super::BASE_HEIGHT)
            .expect("test backend always builds");
        let fixture = fixtures::transcript(40, 12);
        ctx.load_transcript(&fixture.messages)
            .expect("fixture is a valid conversation");
        ctx.render_frame(true).expect("first frame renders");
        let after_load = settled_conversation_test_renders();
        assert!(
            after_load > 0,
            "the first paint has to go through the settled cache"
        );
        assert!(
            after_load < 40,
            "elision should keep the first paint far below the session, got {after_load}"
        );

        ctx.resize(80, super::BASE_HEIGHT)
            .expect("test backend resizes");
        ctx.render_frame(true).expect("resized frame renders");
        let after_resize = settled_conversation_test_renders();
        assert!(
            after_resize < 40,
            "one resize must not rebuild the hidden session, renders {after_resize} after {after_load}"
        );
    }

    #[test]
    fn every_registered_scenario_has_a_unique_name() {
        let mut seen = std::collections::HashSet::new();
        for scenario in SCENARIOS {
            assert!(
                seen.insert(scenario.name),
                "duplicate scenario name: {}",
                scenario.name
            );
        }
        assert!(!SCENARIOS.is_empty());
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agens-tui-perf-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn each_registered_scenario_emits_at_least_one_tui_frame_span() {
        let dir = unique_temp_dir("scenario-frame-span");
        let recorder = Recorder::install(RecorderConfig::new(&dir, "test-run"))
            .expect("this is the only test in this binary that installs a recorder");

        for scenario in SCENARIOS {
            let mut ctx = super::ScenarioContext::new(80, 24).expect("test backend always builds");
            (scenario.run)(&mut ctx).expect("scenario runs to completion");
        }

        let paths = recorder
            .finish()
            .expect("canonical trace writer did not fail");
        let records = agens_perf::read_trace(&paths.jsonl).expect("trace is well-formed");

        let mut spans_by_id = HashMap::new();
        for record in &records {
            if let Record::Span(span) = record {
                spans_by_id.insert(span.span_id, span);
            }
        }

        let root_span_id_for = |name: &str| -> u64 {
            spans_by_id
                .values()
                .find(|span| {
                    span.name == "perf.scenario"
                        && span.fields.get("scenario")
                            == Some(&serde_json::Value::String(name.to_string()))
                })
                .map(|span| span.span_id)
                .unwrap_or_else(|| panic!("no perf.scenario span recorded for {name}"))
        };

        let ancestor_root = |mut id: u64| -> u64 {
            while let Some(parent) = spans_by_id.get(&id).and_then(|span| span.parent_span_id) {
                id = parent;
            }
            id
        };

        for scenario in SCENARIOS {
            let root_id = root_span_id_for(scenario.name);
            let has_frame_span = spans_by_id
                .values()
                .any(|span| span.name == "tui.frame" && ancestor_root(span.span_id) == root_id);
            assert!(
                has_frame_span,
                "scenario {} recorded no tui.frame span under its perf.scenario root",
                scenario.name
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
