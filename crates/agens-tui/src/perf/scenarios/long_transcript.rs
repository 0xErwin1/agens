//! A long session as the reader actually pays for it: scrolling the window.
//!
//! History elision keeps only the last few settled turns on screen, so a
//! single paint of 120 turns costs almost the same as a short transcript.
//! This scenario still seats a long session — so the fixture is honest —
//! then scrolls the viewport. That is the cost a long session adds: not
//! assembling every turn, but walking the window.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};
use crate::{Action, Event, MouseWheelDirection};

const TURNS: usize = 120;
const LINES_PER_TURN: usize = 30;
const SCROLL_STEPS: usize = 24;

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
        iterations = SCROLL_STEPS as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for _ in 0..SCROLL_STEPS {
        let action = ctx.handle(Event::MouseWheel(MouseWheelDirection::Up));
        ctx.render_frame(matches!(action, Action::Render))?;
    }

    Ok(())
}
