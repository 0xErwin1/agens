//! The high-water scenario: several thousand lines of transcript.
//!
//! `LINES_PER_TURN * TURNS` is chosen to clear the 2000-line floor with
//! headroom, so the scenario stays meaningful even as fixture content shifts
//! slightly over time.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};

const TURNS: usize = 120;
const LINES_PER_TURN: usize = 30;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "long_transcript",
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

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    Ok(())
}
