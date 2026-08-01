//! The low-water reference scenario: a handful of turns, then rest.
//!
//! The resting tail is the only place the gate's skip path is reachable:
//! every other scenario either dirties the state or keeps a turn in flight,
//! and a turn in flight makes the gate render unconditionally so the live
//! indicators can animate. Without these ticks `tui.frame.gate` and
//! `tui.frame` would have identical counts everywhere and the gate span would
//! be measuring nothing.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};

const TURNS: usize = 4;
const LINES_PER_TURN: usize = 3;
const IDLE_TICKS: usize = 32;

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

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for _ in 0..IDLE_TICKS {
        ctx.render_frame(false)?;
    }

    Ok(())
}
