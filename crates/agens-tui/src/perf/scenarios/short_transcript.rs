//! The low-water reference scenario: a handful of turns.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};
use crate::{Conversation, TuiPresentation, TuiSubmissionOutcome};

const TURNS: usize = 4;
const LINES_PER_TURN: usize = 3;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "short_transcript",
    run,
};

fn run(ctx: &mut ScenarioContext) -> io::Result<()> {
    let fixture = fixtures::transcript(TURNS, LINES_PER_TURN);

    let _root = agens_perf::span!(
        "perf.scenario",
        scenario = SCENARIO.name,
        width = ctx.width(),
        height = ctx.height(),
        transcript_lines = fixture.lines as u64,
        iterations = 1u64,
    );

    let history = Conversation::from_messages(&fixture.messages).map_err(|error| {
        io::Error::other(format!("fixture built an invalid conversation: {error:?}"))
    })?;

    ctx.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session.".to_owned(),
        presentation: TuiPresentation::new("provider", "model", "session"),
        history,
        draft: None,
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    ctx.render_frame(true)?;

    Ok(())
}
