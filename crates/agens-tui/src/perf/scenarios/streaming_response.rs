//! A long assistant response arriving one delta at a time.
//!
//! The transcript underneath is deliberately modest: this scenario is about
//! the per-delta cost of the live turn, which is the one conversation the
//! settled cache can never serve, and a large backlog would bury that cost
//! under settled-turn work that `long_transcript` already measures.

use std::io;

use agens_core::{MessagePart, TurnEvent};

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};

const BACKLOG_TURNS: usize = 8;
const BACKLOG_LINES_PER_TURN: usize = 4;
const DELTAS: usize = 240;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "streaming_response",
    run,
};

fn run(ctx: &mut ScenarioContext) -> io::Result<()> {
    let fixture = fixtures::transcript(BACKLOG_TURNS, BACKLOG_LINES_PER_TURN);

    let _root = agens_perf::span!(
        "perf.scenario",
        scenario = SCENARIO.name,
        width = ctx.width(),
        height = ctx.height(),
        transcript_lines = fixture.lines as u64,
        iterations = DELTAS as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for delta in 0..DELTAS {
        ctx.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(delta_text(
            delta,
        ))));
        ctx.render_frame(true)?;
    }

    Ok(())
}

/// Every eighth delta closes a line, so the live turn grows in height rather
/// than only in width and the reflow work stays representative.
fn delta_text(index: usize) -> String {
    if index % 8 == 7 {
        format!(" chunk {index}\n")
    } else {
        format!(" chunk {index}")
    }
}
