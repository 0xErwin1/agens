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
use std::time::Duration;

use ratatui::backend::TestBackend;

use crate::{Engine, FrameSchedule, RatatuiRenderer, Tui, TuiSubmissionOutcome};

pub use scenarios::SCENARIOS;

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
        self.now = self.now.saturating_add(Duration::from_millis(1));
        crate::render_progress_frame(
            &mut self.tui,
            &mut self.renderer,
            &mut self.schedule,
            self.now,
            dirty,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agens_perf::{Record, Recorder, RecorderConfig};

    use super::SCENARIOS;

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
