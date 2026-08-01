//! Streaming with the live indicators animating alongside it.
//!
//! Same delta shape as `streaming_response`, with the turn marked in flight
//! and the clock advanced a full animation period per delta. Diffing the two
//! scenarios isolates what the spinner and the elapsed counters cost, because
//! everything else about them matches.

use std::io;
use std::time::Duration;

use agens_core::{MessagePart, TurnEvent};

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};
use crate::widgets::StatusGlyph;

const BACKLOG_TURNS: usize = 8;
const BACKLOG_LINES_PER_TURN: usize = 4;
const DELTAS: usize = 240;

/// Ticks between deltas that carry no new content, only elapsed time.
///
/// These are the frames that answer the scenario's actual question: with
/// nothing dirty, does the animation force a repaint of its own? Driving
/// every tick as dirty would have made the gate's skip path unreachable and
/// the spinner's cost indistinguishable from the streaming cost.
const IDLE_TICKS_PER_DELTA: usize = 3;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "streaming_with_spinner",
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
    ctx.set_running(true);
    ctx.render_frame(true)?;

    let step = Duration::from_millis(StatusGlyph::FRAME_PERIOD_MS as u64);

    for delta in 0..DELTAS {
        ctx.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(delta_text(
            delta,
        ))));
        ctx.render_frame_after(step, true)?;

        for _ in 0..IDLE_TICKS_PER_DELTA {
            ctx.render_frame_after(step, false)?;
        }
    }

    ctx.set_running(false);

    Ok(())
}

fn delta_text(index: usize) -> String {
    if index % 8 == 7 {
        format!(" chunk {index}\n")
    } else {
        format!(" chunk {index}")
    }
}
