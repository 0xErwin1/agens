//! The pointer moving across a long turn.
//!
//! Movement events arrive continuously — dozens per second for a single
//! gesture — and each one has to decide which block sits under the pointer.
//! That makes hover the densest input the TUI receives, and the one where any
//! per-event work over the whole transcript is felt immediately.

use std::io;

use crate::Event;
use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};

const CALLS: usize = 200;
const MOVES: usize = 60;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "pointer_hover",
    run,
};

fn run(ctx: &mut ScenarioContext) -> io::Result<()> {
    let fixture = fixtures::tool_heavy_turn(CALLS);

    let _root = agens_perf::span!(
        "perf.scenario",
        scenario = SCENARIO.name,
        width = ctx.width(),
        height = ctx.height(),
        transcript_lines = fixture.lines as u64,
        iterations = MOVES as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    let height = ctx.height();

    for step in 0..MOVES {
        let row = 2 + (step as u16 % height.saturating_sub(4).max(1));
        ctx.handle(Event::MouseMove { column: 8, row });
        ctx.render_frame(true)?;
    }

    Ok(())
}
