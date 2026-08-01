//! A terminal being resized repeatedly with a full transcript behind it.
//!
//! There is no dedicated reflow span: resize has no path of its own, it is an
//! ordinary event. Its cost is read from ancestry instead — a resize event
//! leading to a frame whose settled turns all report `cache_hit=false`. The
//! settled-conversation cache is keyed partly on content width, so this
//! scenario exists to measure how often a resize actually invalidates it
//! rather than to assume it does.

use std::io;

use crate::perf::fixtures;
use crate::perf::{BASE_HEIGHT, BASE_WIDTH, Scenario, ScenarioContext};

const TURNS: usize = 60;
const LINES_PER_TURN: usize = 20;
const RESIZES: usize = 40;
const NARROWEST: u16 = 60;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "sustained_resize",
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
        iterations = RESIZES as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for step in 0..RESIZES {
        let width = sweep_width(step);
        ctx.resize(width, BASE_HEIGHT)?;
        ctx.render_frame(true)?;
    }

    ctx.resize(BASE_WIDTH, BASE_HEIGHT)?;
    ctx.render_frame(true)?;

    Ok(())
}

/// Sweeps the width down and back up rather than alternating between two
/// values, so the run cannot accidentally measure a two-entry cache serving
/// every other frame.
fn sweep_width(step: usize) -> u16 {
    let span = BASE_WIDTH - NARROWEST;
    let period = usize::from(span) * 2;
    let position = (step % period) as u16;

    if position < span {
        BASE_WIDTH - position
    } else {
        NARROWEST + (position - span)
    }
}
