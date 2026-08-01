//! Expanding and collapsing tool blocks in a settled transcript.
//!
//! Two questions, both of which the trace has to answer rather than assert:
//! whether a toggle is instant, and whether it moves the ground under the
//! reader. The first is the frame cost recorded under the toggle's event; the
//! second is the scroll offset, carried as a field on each toggle so a diff
//! can see it move.
//!
//! The detail level is part of the settled-conversation cache key, so a
//! toggle invalidates every settled turn — which is the cost being measured,
//! and the reason no span wraps the mode enum itself.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};
use crate::{Event, Key, MouseWheelDirection};

const TURNS: usize = 40;
const LINES_PER_TURN: usize = 12;
const TOGGLES: usize = 12;
const SCROLL_STEPS: usize = 4;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "expand_collapse",
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
        iterations = TOGGLES as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for _ in 0..SCROLL_STEPS {
        ctx.handle(Event::MouseWheel(MouseWheelDirection::Up));
    }
    ctx.render_frame(true)?;

    for _ in 0..TOGGLES {
        let before = ctx.view().scroll_offset;

        ctx.handle(Event::Key(Key::CtrlT));
        ctx.render_frame(true)?;

        let after = ctx.view().scroll_offset;

        let _toggle = agens_perf::span!(
            "tui.detail.toggle",
            scroll_before = before,
            scroll_after = after,
            anchored = before == after,
        );
    }

    Ok(())
}
